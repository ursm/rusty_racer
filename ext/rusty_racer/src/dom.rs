// Native DOM (Stage 2, slice 1): the element arena + a real per-instance V8 binding.
//
// This is the first load-bearing piece of the DOM-in-Rust rewrite. The document
// tree lives in THIS Rust structure, and app JS touches it through native-backed
// V8 objects: an ObjectTemplate whose accessors read the arena directly in a C
// callback — no GVL, no host-fn marshalling, the Blink/Deno binding shape. Stage 1
// proved the mechanism costs ~21 ns/read with the value hardcoded on the accessor's
// data slot; this slice makes it a REAL per-instance binding — each element object
// carries its NodeId in internal field 0, and the accessor indexes the shared arena
// by that id, so N elements each return their OWN values.
//
// Scope of slice 1 (deliberately narrow): three read-only reflected string
// accessors (className / id / tagName) plus a create function. Enough to prove
// per-instance correctness and re-measure the overhead with the arena indirection
// and a per-read V8 string build. The node graph (parent/child/sibling), mutation,
// and the wider IDL surface come in later slices.

use crate::{IsolateState, istate};

// One element's data. Slice 1 is flat — no tree links yet, just the three
// reflected string attributes the accessors expose.
pub(crate) struct NodeData {
    pub(crate) tag_name: String,
    pub(crate) id: String,
    pub(crate) class_name: String,
}

// The per-isolate DOM: the node arena plus the cached instance template every
// element object is stamped from. Lives in IsolateState, so any accessor callback
// reaches it through istate!(scope). The template is isolate-scoped (one
// ObjectTemplate serves every realm of the isolate), built lazily on first install
// and kept as a Global so later realms reuse it.
#[derive(Default)]
pub(crate) struct Dom {
    pub(crate) nodes: Vec<NodeData>,
    element_template: Option<v8::Global<v8::ObjectTemplate>>,
}

// Install globalThis.__dom = { createElement } into |ctx|, building the element
// template on first call. Mirrors install_host_namespace's shape (its own
// HandleScope + ContextScope, safe to re-run per realm). Slice 1 keeps this under a
// dedicated __dom global so it is self-contained and trivially removable; a later
// slice folds element creation into the real document / Node surface.
pub(crate) fn install(scope: &mut v8::PinScope<'_, '_, ()>, ctx: &v8::Global<v8::Context>) {
    v8::scope!(let scope, &mut *scope);
    let context = v8::Local::new(scope, ctx);
    let scope = &mut v8::ContextScope::new(scope, context);

    ensure_template(scope);

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

// Build the element instance template once per isolate and cache it. Internal
// field 0 holds the NodeId (as a V8 integer); the three accessors read the arena.
fn ensure_template(scope: &mut v8::PinScope<'_, '_>) {
    if istate!(scope).dom.element_template.is_some() {
        return;
    }
    let tmpl = v8::ObjectTemplate::new(scope);
    tmpl.set_internal_field_count(1);
    set_reader(scope, tmpl, "className", class_name_getter);
    set_reader(scope, tmpl, "id", id_getter);
    set_reader(scope, tmpl, "tagName", tag_name_getter);
    let global = v8::Global::new(scope, tmpl);
    istate!(scope).dom.element_template = Some(global);
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

// __dom.createElement(tagName, id, className) -> a native-backed element object.
// Pushes the data into the arena and stamps a fresh template instance carrying its
// NodeId in internal field 0.
fn create_element(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
) {
    let tag_name = args.get(0).to_rust_string_lossy(scope);
    let id = args.get(1).to_rust_string_lossy(scope);
    let class_name = args.get(2).to_rust_string_lossy(scope);
    let node_id = {
        let st = istate!(scope);
        st.dom.nodes.push(NodeData {
            tag_name,
            id,
            class_name,
        });
        st.dom.nodes.len() - 1
    };
    let template = match istate!(scope).dom.element_template.clone() {
        Some(t) => t,
        None => return,
    };
    let template = v8::Local::new(scope, &template);
    let Some(obj) = template.new_instance(scope) else {
        return;
    };
    let id_value: v8::Local<v8::Value> = v8::Integer::new(scope, node_id as i32).into();
    obj.set_internal_field(0, id_value.into());
    rv.set(obj.into());
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

// Shared getter body: read the holder's NodeId, pick a string out of the arena
// (cloned so the istate borrow ends before the V8 string alloc), and set it as the
// return. A missing node / out-of-range id leaves the result unset (reads as
// undefined) rather than panicking.
fn read_string(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::PropertyCallbackArguments<'_>,
    mut rv: v8::ReturnValue<'_, v8::Value>,
    pick: impl Fn(&NodeData) -> String,
) {
    let Some(id) = node_id(scope, args) else {
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

// The NodeId stamped into the holder's internal field 0, or None if the slot is
// empty / not an integer (a stray object created off-template).
fn node_id(scope: &mut v8::PinScope<'_, '_>, args: &v8::PropertyCallbackArguments<'_>) -> Option<usize> {
    let data = args.holder().get_internal_field(scope, 0)?;
    let value = v8::Local::<v8::Value>::try_from(data).ok()?;
    Some(value.uint32_value(scope)? as usize)
}
