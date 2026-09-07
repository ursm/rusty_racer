// Native DOM (Stage 2): the node arena + per-instance V8 binding.
//
// This is the load-bearing core of the DOM-in-Rust rewrite. The document tree
// lives in THIS Rust structure, and app JS touches it through native-backed V8
// objects: ObjectTemplates whose accessors, methods and interceptors read the
// arena directly in a C callback — no GVL, no host-fn marshalling, the Blink/Deno
// binding shape.
//
// What is proven so far:
//   - slice 1: per-instance binding — NodeId in internal field 0, shared arena,
//     reflected-string reads (~21 ns hardcoded probe → ~35 ns real).
//   - slice 2: the tree + WRAPPER IDENTITY (each node caches its ONE JS object, so
//     el.parentNode === parent and childNodes[i] === child) + childNodes as a
//     native NodeList via an INDEXED interceptor.
//   - slice 2b (here): the remaining binding primitives —
//       * a generic attribute model (className/id reflect the class/id attributes,
//         not dedicated fields — no library-shaped shortcut);
//       * reflected-attribute SETTERS (el.className = x) via an accessor setter;
//       * METHODS via FunctionTemplate reading args.this() — getAttribute /
//         setAttribute / appendChild / querySelector / querySelectorAll;
//       * a NAMED interceptor — el.dataset.fooBar <-> the data-foo-bar attribute.
//
// Still to come (slice 3 = integration): text nodes, GC-aware (weak) wrappers, and
// the real question of how this replaces / bridges the JS DOM in bridge.js. Today's
// wrappers are strong Globals — they root every node for the isolate's life, which
// is fine while the arena only grows. The selector matcher here is deliberately
// minimal (tag / #id / .class compounds + descendant combinator); the real cascade
// selector engine is wired at integration.

use crate::{IsolateState, istate};

// One node's data. Attributes are the source of truth (ordered, as the DOM keeps
// them); className/id read and write the "class"/"id" attributes through it.
pub(crate) struct NodeData {
    pub(crate) tag_name: String,
    pub(crate) attributes: Vec<(String, String)>,
    pub(crate) parent: Option<usize>,
    pub(crate) children: Vec<usize>,
    // The node's one JS object (identity). Strong Global for now — roots every
    // wrapper for the isolate life; weak/GC-aware wrappers are a later slice.
    wrapper: Option<v8::Global<v8::Object>>,
    // The node's one childNodes list (live: re-reads children on each index access).
    child_nodes_wrapper: Option<v8::Global<v8::Object>>,
    // The node's one dataset object (a named-interceptor view over data-* attrs).
    dataset_wrapper: Option<v8::Global<v8::Object>>,
}

impl NodeData {
    fn get_attr(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn set_attr(&mut self, name: &str, value: String) {
        match self.attributes.iter_mut().find(|(k, _)| k == name) {
            Some(slot) => slot.1 = value,
            None => self.attributes.push((name.to_string(), value)),
        }
    }
}

// The per-isolate DOM: the node arena plus the cached instance templates every
// wrapper is stamped from. Lives in IsolateState, reached from any callback via
// istate!(scope). Templates are isolate-scoped (one serves every realm).
#[derive(Default)]
pub(crate) struct Dom {
    pub(crate) nodes: Vec<NodeData>,
    element_template: Option<v8::Global<v8::ObjectTemplate>>,
    nodelist_template: Option<v8::Global<v8::ObjectTemplate>>,
    dataset_template: Option<v8::Global<v8::ObjectTemplate>>,
}

// Install globalThis.__dom = { createElement } into |ctx|, building the templates
// on first call. Mirrors install_host_namespace's shape (its own HandleScope +
// ContextScope, safe to re-run per realm). A later slice folds this into the real
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

// Build the element / NodeList / dataset instance templates once per isolate. Each
// reserves internal field 0 for a NodeId (the element's own id; the NodeList's and
// dataset's owner id).
fn ensure_templates(scope: &mut v8::PinScope<'_, '_>) {
    if istate!(scope).dom.element_template.is_none() {
        let tmpl = v8::ObjectTemplate::new(scope);
        tmpl.set_internal_field_count(1);
        set_reflected(scope, tmpl, "className", class_name_getter, class_name_setter);
        set_reflected(scope, tmpl, "id", id_getter, id_setter);
        set_reader(scope, tmpl, "tagName", tag_name_getter);
        set_reader(scope, tmpl, "parentNode", parent_node_getter);
        set_reader(scope, tmpl, "childNodes", child_nodes_getter);
        set_reader(scope, tmpl, "dataset", dataset_getter);
        set_method(scope, tmpl, "getAttribute", get_attribute);
        set_method(scope, tmpl, "setAttribute", set_attribute);
        set_method(scope, tmpl, "appendChild", append_child);
        set_method(scope, tmpl, "querySelector", query_selector);
        set_method(scope, tmpl, "querySelectorAll", query_selector_all);
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
    if istate!(scope).dom.dataset_template.is_none() {
        let tmpl = v8::ObjectTemplate::new(scope);
        tmpl.set_internal_field_count(1);
        tmpl.set_named_property_handler(
            v8::NamedPropertyHandlerConfiguration::new()
                .getter(dataset_getter_named)
                .setter(dataset_setter_named),
        );
        let global = v8::Global::new(scope, tmpl);
        istate!(scope).dom.dataset_template = Some(global);
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

fn set_reflected(
    scope: &mut v8::PinScope<'_, '_>,
    tmpl: v8::Local<'_, v8::ObjectTemplate>,
    name: &str,
    getter: impl v8::MapFnTo<v8::AccessorNameGetterCallback>,
    setter: impl v8::MapFnTo<v8::AccessorNameSetterCallback>,
) {
    if let Some(key) = v8::String::new(scope, name) {
        tmpl.set_accessor_with_setter(key.into(), getter, setter);
    }
}

fn set_method(
    scope: &mut v8::PinScope<'_, '_>,
    tmpl: v8::Local<'_, v8::ObjectTemplate>,
    name: &str,
    callback: impl v8::MapFnTo<v8::FunctionCallback>,
) {
    let function = v8::FunctionTemplate::new(scope, callback);
    if let Some(key) = v8::String::new(scope, name) {
        tmpl.set(key.into(), function.into());
    }
}

// __dom.createElement(tagName, id, className, parent?) -> a native-backed element.
// id / className are stored as the "id" / "class" attributes (skipped when empty),
// the element is linked under `parent` when one is given, and its (cached) wrapper
// is returned.
fn create_element(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let tag_name = args.get(0).to_rust_string_lossy(scope);
    let id = args.get(1).to_rust_string_lossy(scope);
    let class_name = args.get(2).to_rust_string_lossy(scope);
    let parent = node_id_of(scope, args.get(3));

    let mut attributes = Vec::new();
    if !id.is_empty() {
        attributes.push(("id".to_string(), id));
    }
    if !class_name.is_empty() {
        attributes.push(("class".to_string(), class_name));
    }

    let node_id = {
        let st = istate!(scope);
        let new_id = st.dom.nodes.len();
        st.dom.nodes.push(NodeData {
            tag_name,
            attributes,
            parent,
            children: Vec::new(),
            wrapper: None,
            child_nodes_wrapper: None,
            dataset_wrapper: None,
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
    cached_wrapper(
        scope,
        node_id,
        |node| node.wrapper.clone(),
        |dom| dom.element_template.clone(),
        |node, global| node.wrapper = Some(global),
    )
}

// The cached childNodes list for a node (owner id in internal field 0).
fn node_list_wrapper<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    owner_id: usize,
) -> Option<v8::Local<'s, v8::Object>> {
    cached_wrapper(
        scope,
        owner_id,
        |node| node.child_nodes_wrapper.clone(),
        |dom| dom.nodelist_template.clone(),
        |node, global| node.child_nodes_wrapper = Some(global),
    )
}

// The cached dataset view for a node (owner id in internal field 0).
fn dataset_wrapper<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    owner_id: usize,
) -> Option<v8::Local<'s, v8::Object>> {
    cached_wrapper(
        scope,
        owner_id,
        |node| node.dataset_wrapper.clone(),
        |dom| dom.dataset_template.clone(),
        |node, global| node.dataset_wrapper = Some(global),
    )
}

// Return a node's cached wrapper of some kind, building it from the matching
// template (with the NodeId stamped in) and caching it on first request. One helper
// for all three wrapper kinds so identity + the stamp live in exactly one place.
fn cached_wrapper<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    node_id: usize,
    get_cached: impl Fn(&NodeData) -> Option<v8::Global<v8::Object>>,
    get_template: impl Fn(&Dom) -> Option<v8::Global<v8::ObjectTemplate>>,
    store: impl Fn(&mut NodeData, v8::Global<v8::Object>),
) -> Option<v8::Local<'s, v8::Object>> {
    if let Some(cached) = istate!(scope).dom.nodes.get(node_id).and_then(&get_cached) {
        return Some(v8::Local::new(scope, &cached));
    }
    let template = get_template(&istate!(scope).dom)?;
    let template = v8::Local::new(scope, &template);
    let obj = template.new_instance(scope)?;
    let id_value: v8::Local<v8::Value> = v8::Integer::new(scope, node_id as i32).into();
    obj.set_internal_field(0, id_value.into());
    let global = v8::Global::new(scope, obj);
    if let Some(node) = istate!(scope).dom.nodes.get_mut(node_id) {
        store(node, global);
    }
    Some(obj)
}

// ── reflected string attributes (className / id) ────────────────────────────

fn class_name_getter(
    scope: &mut v8::PinScope<'_, '_>,
    _key: v8::Local<'_, v8::Name>,
    args: v8::PropertyCallbackArguments<'_>,
    rv: v8::ReturnValue<'_, v8::Value>,
) {
    reflect_get(scope, &args, rv, "class");
}

fn class_name_setter(
    scope: &mut v8::PinScope<'_, '_>,
    _key: v8::Local<'_, v8::Name>,
    value: v8::Local<'_, v8::Value>,
    args: v8::PropertyCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, ()>,
) {
    reflect_set(scope, &args, value, "class");
}

fn id_getter(
    scope: &mut v8::PinScope<'_, '_>,
    _key: v8::Local<'_, v8::Name>,
    args: v8::PropertyCallbackArguments<'_>,
    rv: v8::ReturnValue<'_, v8::Value>,
) {
    reflect_get(scope, &args, rv, "id");
}

fn id_setter(
    scope: &mut v8::PinScope<'_, '_>,
    _key: v8::Local<'_, v8::Name>,
    value: v8::Local<'_, v8::Value>,
    args: v8::PropertyCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, ()>,
) {
    reflect_set(scope, &args, value, "id");
}

fn tag_name_getter(
    scope: &mut v8::PinScope<'_, '_>,
    _key: v8::Local<'_, v8::Name>,
    args: v8::PropertyCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(id) = holder_node_id(scope, &args) else {
        return;
    };
    let tag = istate!(scope).dom.nodes.get(id).map(|n| n.tag_name.clone());
    if let Some(tag) = tag
        && let Some(js) = v8::String::new(scope, &tag)
    {
        rv.set(js.into());
    }
}

// A reflected IDL string attribute reads "" (not undefined) when the content
// attribute is absent, matching className / id.
fn reflect_get(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::PropertyCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
    attr: &str,
) {
    let Some(id) = holder_node_id(scope, args) else {
        return;
    };
    let value = {
        let st = istate!(scope);
        st.dom
            .nodes
            .get(id)
            .map(|n| n.get_attr(attr).unwrap_or("").to_string())
            .unwrap_or_default()
    };
    if let Some(js) = v8::String::new(scope, &value) {
        rv.set(js.into());
    }
}

fn reflect_set(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::PropertyCallbackArguments<'_>,
    value: v8::Local<'_, v8::Value>,
    attr: &str,
) {
    let Some(id) = holder_node_id(scope, args) else {
        return;
    };
    let value = value.to_rust_string_lossy(scope);
    if let Some(node) = istate!(scope).dom.nodes.get_mut(id) {
        node.set_attr(attr, value);
    }
}

// ── navigation (parentNode / childNodes) ────────────────────────────────────

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

// ── methods (FunctionTemplate, reading args.this()) ─────────────────────────

fn get_attribute(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(id) = object_node_id(scope, args.this()) else {
        return;
    };
    let name = args.get(0).to_rust_string_lossy(scope);
    let value = {
        let st = istate!(scope);
        st.dom
            .nodes
            .get(id)
            .and_then(|n| n.get_attr(&name).map(str::to_string))
    };
    match value {
        Some(s) => {
            if let Some(js) = v8::String::new(scope, &s) {
                rv.set(js.into());
            }
        }
        None => rv.set_null(),
    }
}

fn set_attribute(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(id) = object_node_id(scope, args.this()) else {
        return;
    };
    let name = args.get(0).to_rust_string_lossy(scope);
    let value = args.get(1).to_rust_string_lossy(scope);
    if let Some(node) = istate!(scope).dom.nodes.get_mut(id) {
        node.set_attr(&name, value);
    }
}

// appendChild(child): detach the child from its current parent, re-parent it here,
// and return it. Slice 2b ignores the document-fragment / text-node cases.
fn append_child(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(parent) = object_node_id(scope, args.this()) else {
        return;
    };
    let Some(child) = node_id_of(scope, args.get(0)) else {
        return;
    };
    {
        let st = istate!(scope);
        if let Some(old_parent) = st.dom.nodes.get(child).and_then(|n| n.parent)
            && let Some(old) = st.dom.nodes.get_mut(old_parent)
        {
            old.children.retain(|&c| c != child);
        }
        if let Some(node) = st.dom.nodes.get_mut(child) {
            node.parent = Some(parent);
        }
        if let Some(node) = st.dom.nodes.get_mut(parent) {
            node.children.push(child);
        }
    }
    if let Some(obj) = element_wrapper(scope, child) {
        rv.set(obj.into());
    }
}

fn query_selector(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(root) = object_node_id(scope, args.this()) else {
        return;
    };
    let compounds = parse_selector(&args.get(0).to_rust_string_lossy(scope));
    let found = {
        let st = istate!(scope);
        let mut out = Vec::new();
        collect_matches(&st.dom.nodes, root, &compounds, &mut out, true);
        out.into_iter().next()
    };
    match found {
        Some(id) => {
            if let Some(obj) = element_wrapper(scope, id) {
                rv.set(obj.into());
            }
        }
        None => rv.set_null(),
    }
}

// querySelectorAll returns a plain Array for now (the spec's static NodeList is a
// slice-3 refinement); the point here is a method returning many node wrappers.
fn query_selector_all(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(root) = object_node_id(scope, args.this()) else {
        return;
    };
    let compounds = parse_selector(&args.get(0).to_rust_string_lossy(scope));
    let found = {
        let st = istate!(scope);
        let mut out = Vec::new();
        collect_matches(&st.dom.nodes, root, &compounds, &mut out, false);
        out
    };
    let array = v8::Array::new(scope, found.len() as i32);
    for (i, id) in found.into_iter().enumerate() {
        if let Some(obj) = element_wrapper(scope, id) {
            array.set_index(scope, i as u32, obj.into());
        }
    }
    rv.set(array.into());
}

// ── dataset (named interceptor over data-* attributes) ──────────────────────

fn dataset_getter(
    scope: &mut v8::PinScope<'_, '_>,
    _key: v8::Local<'_, v8::Name>,
    args: v8::PropertyCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let Some(id) = holder_node_id(scope, &args) else {
        return;
    };
    if let Some(obj) = dataset_wrapper(scope, id) {
        rv.set(obj.into());
    }
}

// dataset.fooBar -> the data-foo-bar attribute. A non-string key (a Symbol) or a
// missing attribute falls through (kNo) so normal lookup (→ undefined) applies.
fn dataset_getter_named(
    scope: &mut v8::PinScope<'_, '_>,
    key: v8::Local<'_, v8::Name>,
    args: v8::PropertyCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) -> v8::Intercepted {
    let Some(owner) = holder_node_id(scope, &args) else {
        return v8::Intercepted::kNo;
    };
    let Some(attr) = data_attr_name(scope, key) else {
        return v8::Intercepted::kNo;
    };
    let value = {
        let st = istate!(scope);
        st.dom
            .nodes
            .get(owner)
            .and_then(|n| n.get_attr(&attr).map(str::to_string))
    };
    match value {
        Some(s) => {
            if let Some(js) = v8::String::new(scope, &s) {
                rv.set(js.into());
            }
            v8::Intercepted::kYes
        }
        None => v8::Intercepted::kNo,
    }
}

fn dataset_setter_named(
    scope: &mut v8::PinScope<'_, '_>,
    key: v8::Local<'_, v8::Name>,
    value: v8::Local<'_, v8::Value>,
    args: v8::PropertyCallbackArguments<'_>,
    _rv: v8::ReturnValue<'_, ()>,
) -> v8::Intercepted {
    let Some(owner) = holder_node_id(scope, &args) else {
        return v8::Intercepted::kNo;
    };
    let Some(attr) = data_attr_name(scope, key) else {
        return v8::Intercepted::kNo;
    };
    let value = value.to_rust_string_lossy(scope);
    if let Some(node) = istate!(scope).dom.nodes.get_mut(owner) {
        node.set_attr(&attr, value);
    }
    v8::Intercepted::kYes
}

// Map a dataset key to its content-attribute name: fooBar -> data-foo-bar. Returns
// None for a non-string key (Symbol), which the interceptor treats as a miss. The
// full DOMStringMap rules (digit / "-x" edge cases) are left for a later slice.
fn data_attr_name(scope: &mut v8::PinScope<'_, '_>, key: v8::Local<'_, v8::Name>) -> Option<String> {
    let key: v8::Local<v8::Value> = key.into();
    if !key.is_string() {
        return None;
    }
    let key = key.to_rust_string_lossy(scope);
    let mut attr = String::from("data-");
    for ch in key.chars() {
        if ch.is_ascii_uppercase() {
            attr.push('-');
            attr.push(ch.to_ascii_lowercase());
        } else {
            attr.push(ch);
        }
    }
    Some(attr)
}

// ── minimal selector matcher (tag / #id / .class + descendant combinator) ───

#[derive(Default)]
struct Compound {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
}

fn parse_selector(selector: &str) -> Vec<Compound> {
    selector.split_whitespace().map(parse_compound).collect()
}

fn parse_compound(part: &str) -> Compound {
    let mut compound = Compound::default();
    // kind: 0 = type (tag / *), 1 = id (#), 2 = class (.)
    let mut kind = 0u8;
    let mut buf = String::new();
    for ch in part.chars() {
        match ch {
            '#' | '.' => {
                flush_token(&mut compound, kind, &mut buf);
                kind = if ch == '#' { 1 } else { 2 };
            }
            _ => buf.push(ch),
        }
    }
    flush_token(&mut compound, kind, &mut buf);
    compound
}

fn flush_token(compound: &mut Compound, kind: u8, buf: &mut String) {
    if buf.is_empty() {
        return;
    }
    let token = std::mem::take(buf);
    match kind {
        0 if token != "*" => compound.tag = Some(token),
        0 => {}
        1 => compound.id = Some(token),
        _ => compound.classes.push(token),
    }
}

fn matches_compound(node: &NodeData, compound: &Compound) -> bool {
    if let Some(tag) = &compound.tag
        && !node.tag_name.eq_ignore_ascii_case(tag)
    {
        return false;
    }
    if let Some(id) = &compound.id
        && node.get_attr("id") != Some(id.as_str())
    {
        return false;
    }
    if !compound.classes.is_empty() {
        let class_attr = node.get_attr("class").unwrap_or("");
        for want in &compound.classes {
            if !class_attr.split_whitespace().any(|c| c == want) {
                return false;
            }
        }
    }
    true
}

// The rightmost compound must match `node`; each earlier compound must match some
// ancestor, in order but not necessarily contiguous (the descendant combinator).
fn matches_selector(nodes: &[NodeData], node_id: usize, compounds: &[Compound]) -> bool {
    let Some((last, rest)) = compounds.split_last() else {
        return false;
    };
    if !matches_compound(&nodes[node_id], last) {
        return false;
    }
    let mut remaining = rest.iter().rev();
    let Some(mut want) = remaining.next() else {
        return true;
    };
    let mut ancestor = nodes[node_id].parent;
    while let Some(a) = ancestor {
        ancestor = nodes[a].parent;
        if matches_compound(&nodes[a], want) {
            match remaining.next() {
                Some(next) => want = next,
                None => return true,
            }
        }
    }
    false
}

// Preorder (document-order) walk of `root`'s descendants, collecting matches.
fn collect_matches(
    nodes: &[NodeData],
    root: usize,
    compounds: &[Compound],
    out: &mut Vec<usize>,
    first_only: bool,
) {
    for &child in &nodes[root].children {
        if matches_selector(nodes, child, compounds) {
            out.push(child);
            if first_only {
                return;
            }
        }
        collect_matches(nodes, child, compounds, out, first_only);
        if first_only && !out.is_empty() {
            return;
        }
    }
}

// ── NodeId plumbing ─────────────────────────────────────────────────────────

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
