// Native DOM (Stage 2): the node arena + per-instance V8 binding.
//
// This is the load-bearing core of the DOM-in-Rust rewrite. The document tree
// lives in THIS Rust structure, and app JS touches it through native-backed V8
// objects: ObjectTemplates whose accessors and interceptors read the arena
// directly in a C callback — no GVL, no host-fn marshalling, the Blink/Deno
// binding shape. Stage 1 proved the mechanism costs ~21 ns/read with the value
// hardcoded; slice 1 made it a real per-instance binding (NodeId in internal
// field 0, indexing a shared arena).
//
// Slice 2 adds the parts that make it a TREE and a faithful binding:
//   - parent/child links in the arena, built via createElement(.., parent?);
//   - WRAPPER IDENTITY — each node has exactly ONE JS object, cached in the arena,
//     so el.parentNode === parent and childNodes[i] === that child;
//   - parentNode (returns the cached parent wrapper or null);
//   - childNodes — a NodeList object backed by an INDEXED interceptor (list[i])
//     plus a length accessor.
//
// Still to come (slice 2b+): reflected-attribute SETTERS, methods (getAttribute /
// appendChild / querySelector), the named interceptor for dataset, text nodes, and
// GC-aware (weak) wrappers. Today's wrappers are strong Globals — they root every
// node for the isolate's life, which is fine while the arena only grows.

use crate::{IsolateState, istate};

// One node's data. Slice 2 carries the three reflected string attributes plus the
// tree links and the cached JS wrapper.
pub(crate) struct NodeData {
    pub(crate) tag_name: String,
    pub(crate) id: String,
    pub(crate) class_name: String,
    pub(crate) parent: Option<usize>,
    pub(crate) children: Vec<usize>,
    // The node's one JS object (identity: el.parentNode === el.parentNode). Strong
    // Global for now — roots every wrapper for the isolate life; weak/GC-aware
    // wrappers are a later slice.
    wrapper: Option<v8::Global<v8::Object>>,
    // The node's one childNodes list object, cached so el.childNodes ===
    // el.childNodes (a live list: it re-reads children on every index access).
    child_nodes_wrapper: Option<v8::Global<v8::Object>>,
}

// The per-isolate DOM: the node arena plus the cached instance templates every
// wrapper is stamped from. Lives in IsolateState, reached from any callback via
// istate!(scope). Templates are isolate-scoped (one serves every realm), built
// lazily on first install and kept as Globals.
#[derive(Default)]
pub(crate) struct Dom {
    pub(crate) nodes: Vec<NodeData>,
    element_template: Option<v8::Global<v8::ObjectTemplate>>,
    nodelist_template: Option<v8::Global<v8::ObjectTemplate>>,
}

// Install globalThis.__dom = { createElement } into |ctx|, building the templates
// on first call. Mirrors install_host_namespace's shape (its own HandleScope +
// ContextScope, safe to re-run per realm). Slice 2 keeps this under a dedicated
// __dom global so it is self-contained; a later slice folds it into the real
// document / Node surface.
pub(crate) fn install(scope: &mut v8::PinScope<'_, '_, ()>, ctx: &v8::Global<v8::Context>) {
    v8::scope!(let scope, &mut *scope);
    let context = v8::Local::new(scope, ctx);
    let scope = &mut v8::ContextScope::new(scope, context);

    ensure_templates(scope);

    let ns = v8::Object::new(scope);
    if let (Some(f), Some(k)) = (
        v8::Function::new(scope, create_element),
        v8::String::new(scope, "createElement"),
    ) {
        ns.set(scope, k.into(), f.into());
    }
    if let Some(key) = v8::String::new(scope, "__dom") {
        let global = context.global(scope);
        global.set(scope, key.into(), ns.into());
    }
}

// Build the element and NodeList instance templates once per isolate. Both reserve
// internal field 0 for a NodeId (the element's own id; the NodeList's owner id).
fn ensure_templates(scope: &mut v8::PinScope<'_, '_>) {
    if istate!(scope).dom.element_template.is_none() {
        let tmpl = v8::ObjectTemplate::new(scope);
        tmpl.set_internal_field_count(1);
        set_reader(scope, tmpl, "className", class_name_getter);
        set_reader(scope, tmpl, "id", id_getter);
        set_reader(scope, tmpl, "tagName", tag_name_getter);
        set_reader(scope, tmpl, "parentNode", parent_node_getter);
        set_reader(scope, tmpl, "childNodes", child_nodes_getter);
        let global = v8::Global::new(scope, tmpl);
        istate!(scope).dom.element_template = Some(global);
    }
    if istate!(scope).dom.nodelist_template.is_none() {
        let tmpl = v8::ObjectTemplate::new(scope);
        tmpl.set_internal_field_count(1);
        set_reader(scope, tmpl, "length", nodelist_length_getter);
        tmpl.set_indexed_property_handler(
            v8::IndexedPropertyHandlerConfiguration::new().getter(nodelist_index_getter),
        );
        let global = v8::Global::new(scope, tmpl);
        istate!(scope).dom.nodelist_template = Some(global);
    }
}

fn set_reader(
    scope: &mut v8::PinScope<'_, '_>,
    tmpl: v8::Local<'_, v8::ObjectTemplate>,
    name: &str,
    getter: impl v8::MapFnTo<v8::AccessorNameGetterCallback>,
) {
    if let Some(key) = v8::String::new(scope, name) {
        tmpl.set_accessor(key.into(), getter);
    }
}

// __dom.createElement(tagName, id, className, parent?) -> a native-backed element.
// Pushes the data into the arena, links it under `parent` when one is given, and
// returns its (cached) wrapper.
fn create_element(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let tag_name = args.get(0).to_rust_string_lossy(scope);
    let id = args.get(1).to_rust_string_lossy(scope);
    let class_name = args.get(2).to_rust_string_lossy(scope);
    let parent = node_id_of(scope, args.get(3));

    let node_id = {
        let st = istate!(scope);
        let new_id = st.dom.nodes.len();
        st.dom.nodes.push(NodeData {
            tag_name,
            id,
            class_name,
            parent,
            children: Vec::new(),
            wrapper: None,
            child_nodes_wrapper: None,
        });
        if let Some(p) = parent
            && let Some(parent_node) = st.dom.nodes.get_mut(p)
        {
            parent_node.children.push(new_id);
        }
        new_id
    };
    if let Some(obj) = element_wrapper(scope, node_id) {
        rv.set(obj.into());
    }
}

// The cached wrapper for an element node, created on first request from the element
// template with its NodeId stamped into internal field 0.
fn element_wrapper<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    node_id: usize,
) -> Option<v8::Local<'s, v8::Object>> {
    if let Some(cached) = istate!(scope)
        .dom
        .nodes
        .get(node_id)
        .and_then(|n| n.wrapper.clone())
    {
        return Some(v8::Local::new(scope, &cached));
    }
    let template = istate!(scope).dom.element_template.clone()?;
    let template = v8::Local::new(scope, &template);
    let obj = template.new_instance(scope)?;
    stamp_node_id(scope, obj, node_id);
    let global = v8::Global::new(scope, obj);
    if let Some(node) = istate!(scope).dom.nodes.get_mut(node_id) {
        node.wrapper = Some(global);
    }
    Some(obj)
}

// The cached childNodes list for a node, created on first request from the NodeList
// template with the OWNER's NodeId in internal field 0.
fn node_list_wrapper<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    owner_id: usize,
) -> Option<v8::Local<'s, v8::Object>> {
    if let Some(cached) = istate!(scope)
        .dom
        .nodes
        .get(owner_id)
        .and_then(|n| n.child_nodes_wrapper.clone())
    {
        return Some(v8::Local::new(scope, &cached));
    }
    let template = istate!(scope).dom.nodelist_template.clone()?;
    let template = v8::Local::new(scope, &template);
    let obj = template.new_instance(scope)?;
    stamp_node_id(scope, obj, owner_id);
    let global = v8::Global::new(scope, obj);
    if let Some(node) = istate!(scope).dom.nodes.get_mut(owner_id) {
        node.child_nodes_wrapper = Some(global);
    }
    Some(obj)
}

fn stamp_node_id(scope: &mut v8::PinScope<'_, '_>, obj: v8::Local<'_, v8::Object>, node_id: usize) {
    let id_value: v8::Local<v8::Value> = v8::Integer::new(scope, node_id as i32).into();
    obj.set_internal_field(0, id_value.into());
}

fn class_name_getter(
    scope: &mut v8::PinScope<'_, '_>,
    _key: v8::Local<'_, v8::Name>,
    args: v8::PropertyCallbackArguments<'_>,
    rv: v8::ReturnValue<'_, v8::Value>,
) {
    read_string(scope, &args, rv, |n| n.class_name.clone());
}

fn id_getter(
    scope: &mut v8::PinScope<'_, '_>,
    _key: v8::Local<'_, v8::Name>,
    args: v8::PropertyCallbackArguments<'_>,
    rv: v8::ReturnValue<'_, v8::Value>,
) {
    read_string(scope, &args, rv, |n| n.id.clone());
}

fn tag_name_getter(
    scope: &mut v8::PinScope<'_, '_>,
    _key: v8::Local<'_, v8::Name>,
    args: v8::PropertyCallbackArguments<'_>,
    rv: v8::ReturnValue<'_, v8::Value>,
) {
    read_string(scope, &args, rv, |n| n.tag_name.clone());
}

fn parent_node_getter(
    scope: &mut v8::PinScope<'_, '_>,
    _key: v8::Local<'_, v8::Name>,
    args: v8::PropertyCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(id) = holder_node_id(scope, &args) else {
        return;
    };
    let parent = istate!(scope).dom.nodes.get(id).and_then(|n| n.parent);
    match parent {
        Some(parent_id) => {
            if let Some(obj) = element_wrapper(scope, parent_id) {
                rv.set(obj.into());
            }
        }
        None => rv.set_null(),
    }
}

fn child_nodes_getter(
    scope: &mut v8::PinScope<'_, '_>,
    _key: v8::Local<'_, v8::Name>,
    args: v8::PropertyCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(id) = holder_node_id(scope, &args) else {
        return;
    };
    if let Some(obj) = node_list_wrapper(scope, id) {
        rv.set(obj.into());
    }
}

fn nodelist_length_getter(
    scope: &mut v8::PinScope<'_, '_>,
    _key: v8::Local<'_, v8::Name>,
    args: v8::PropertyCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(owner) = holder_node_id(scope, &args) else {
        return;
    };
    let len = istate!(scope)
        .dom
        .nodes
        .get(owner)
        .map(|n| n.children.len())
        .unwrap_or(0);
    rv.set_uint32(len as u32);
}

// list[index] -> the child wrapper, or fall through (kNo) for an out-of-range index
// so `length` and other named props still resolve normally.
fn nodelist_index_getter(
    scope: &mut v8::PinScope<'_, '_>,
    index: u32,
    args: v8::PropertyCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) -> v8::Intercepted {
    let Some(owner) = holder_node_id(scope, &args) else {
        return v8::Intercepted::kNo;
    };
    let child = istate!(scope)
        .dom
        .nodes
        .get(owner)
        .and_then(|n| n.children.get(index as usize).copied());
    match child {
        Some(child_id) => match element_wrapper(scope, child_id) {
            Some(obj) => {
                rv.set(obj.into());
                v8::Intercepted::kYes
            }
            None => v8::Intercepted::kNo,
        },
        None => v8::Intercepted::kNo,
    }
}

// Shared getter body: read the holder's NodeId, pick a string out of the arena
// (cloned so the istate borrow ends before the V8 string alloc), and set it as the
// return. An out-of-range id leaves the result unset (reads as undefined).
fn read_string(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::PropertyCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
    pick: impl Fn(&NodeData) -> String,
) {
    let Some(id) = holder_node_id(scope, args) else {
        return;
    };
    let value = {
        let st = istate!(scope);
        st.dom.nodes.get(id).map(&pick)
    };
    if let Some(s) = value
        && let Some(js) = v8::String::new(scope, &s)
    {
        rv.set(js.into());
    }
}

// The NodeId stamped into the accessor holder's internal field 0.
fn holder_node_id(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::PropertyCallbackArguments<'_>,
) -> Option<usize> {
    object_node_id(scope, args.holder())
}

// The NodeId carried by a native-backed object (internal field 0), or None if the
// value is not such an object (a plain JS value, or one built off-template).
fn node_id_of(scope: &mut v8::PinScope<'_, '_>, value: v8::Local<'_, v8::Value>) -> Option<usize> {
    let obj = v8::Local::<v8::Object>::try_from(value).ok()?;
    if obj.internal_field_count() == 0 {
        return None;
    }
    object_node_id(scope, obj)
}

fn object_node_id(scope: &mut v8::PinScope<'_, '_>, obj: v8::Local<'_, v8::Object>) -> Option<usize> {
    let data = obj.get_internal_field(scope, 0)?;
    let value = v8::Local::<v8::Value>::try_from(data).ok()?;
    Some(value.uint32_value(scope)? as usize)
}
