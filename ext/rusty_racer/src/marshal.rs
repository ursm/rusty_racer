// Value marshalling: the conversion layer between V8 handles, the thread-crossing
// plain-data JsVal, and Ruby objects. Extracted from lib.rs verbatim. JsVal and
// the four conversion entry points (js_to_jsval, jsval_to_js, jsval_to_ruby,
// ruby_to_jsval) are pub(crate); the depth-recursion helpers, the seen-tables,
// and the hex<->words BigInt codec stay private to this module.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use magnus::rb_sys::{AsRawValue, FromRawValue};
use magnus::value::ReprValue;
use magnus::{
    Error, ExceptionClass, IntoValue, RArray, RHash, RString, Ruby, TryConvert, Value, prelude::*,
};

// ---------------------------------------------------------------------------
// Values crossing threads: plain Rust data. No Ruby allocation off the Ruby
// thread, no V8 handles off the V8 thread, no wire format. Replaces serde.c.
// ---------------------------------------------------------------------------
#[derive(Debug, Clone)]
pub(crate) enum JsVal {
    Undefined,
    Null,
    Bool(bool),
    Int(i64),
    Num(f64),
    Str(String),
    // Binary bytes: a JS Uint8Array / ArrayBuffer (view) <-> a Ruby ASCII-8BIT
    // (binary-tagged) String. The encoding tag IS the type declaration, so the
    // round-trip is symmetric and faithful (Uint8Array -> binary String ->
    // Uint8Array), like BigInt/Date/Map/Set — no lossy text coercion. |id| (when
    // Some) registers it in the Ref table so a binary blob aliased in a graph
    // keeps ONE identity instead of being duplicated; None = not identity-tracked
    // (e.g. a to_str result).
    Bytes {
        id: Option<u32>,
        bytes: Vec<u8>,
    },
    // Arbitrary-precision integer (JS BigInt <-> Ruby Integer). Carried as V8's
    // word representation: sign + little-endian u64 limbs. Both ends speak this
    // natively (V8 BigInt words; Ruby Integer via a hex string), so no value is
    // truncated — unlike routing a big int through f64.
    BigInt {
        negative: bool,
        words: Vec<u64>,
    },
    // JS Date <-> Ruby Time, carried as milliseconds since the Unix epoch
    // (v8::Date::value_of's unit). mini_racer marshals Date to Time.
    Date(f64),
    // Containers carry a serialization id so shared/cyclic graphs survive the
    // round-trip: the first time an object is seen it is emitted with its id,
    // and any later occurrence (a sibling sharing it, or a cycle back to an
    // ancestor) is emitted as Ref(id) instead of being re-expanded.
    Array {
        id: u32,
        items: Vec<JsVal>,
    },
    // JS object / Ruby Hash with string keys. Insertion order preserved.
    Obj {
        id: u32,
        entries: Vec<(String, JsVal)>,
    },
    // JS Map <-> Ruby RustyRacer::JSMap (a Hash subclass). Keys are arbitrary
    // values (not just strings), so this is distinct from Obj. Insertion order
    // preserved. The JSMap subclass is what lets a Map round-trip: a plain Ruby
    // Hash marshals to Obj (a JS Object), a JSMap to Map — the return leg tells
    // them apart by class (Ruby has no native Map type).
    Map {
        id: u32,
        pairs: Vec<(JsVal, JsVal)>,
    },
    // JS Set <-> Ruby Set (stdlib).
    Set {
        id: u32,
        items: Vec<JsVal>,
    },
    // Back-reference to an already-emitted container (preserves identity; makes
    // cycles representable instead of truncating at a depth cap).
    Ref(u32),
}

// Cycles and sharing are handled by the Ref table (see JsVal::Ref), so this is
// purely a native-stack backstop against a pathologically deep (but acyclic)
// graph — set well above any realistic nesting.
const MAX_MARSHAL_DEPTH: u32 = 256;

// Little-endian u64 limbs -> big-endian hex magnitude (no sign, no "0x"). The
// shared currency between V8 BigInt words and Ruby Integer(str, 16).
fn words_to_hex(words: &[u64]) -> String {
    let mut hex = String::new();
    for w in words.iter().rev() {
        if hex.is_empty() {
            hex.push_str(&format!("{w:x}")); // top limb: no leading zeros
        } else {
            hex.push_str(&format!("{w:016x}")); // lower limbs: full width
        }
    }
    if hex.is_empty() {
        hex.push('0');
    }
    hex
}

// Big-endian hex magnitude -> little-endian u64 limbs (inverse of words_to_hex).
fn hex_to_words(hex: &str) -> Vec<u64> {
    let mut words = Vec::new();
    let mut end = hex.len();
    while end > 0 {
        let start = end.saturating_sub(16);
        words.push(u64::from_str_radix(&hex[start..end], 16).unwrap_or(0));
        end = start;
    }
    if words.is_empty() {
        words.push(0);
    }
    words
}

// Tracks objects already emitted this marshal so a re-encounter becomes a
// Ref instead of re-expansion. Buckets by V8 identity hash (which can collide),
// disambiguated by Local equality — the same trick the module registry uses.
#[derive(Default)]
struct JsSeen {
    next_id: u32,
    map: HashMap<i32, Vec<(v8::Global<v8::Object>, u32)>>,
}

// Decide how to emit a container object: Ok(id) = first sighting, register it
// and recurse; Err(jsval) = emit this directly and stop (a Ref to an already-
// seen object, or a truncated Str at the depth backstop). Centralising this in
// one place keeps the four container arms (array/object/map/set) in lockstep —
// and crucially orders the checks so a depth-truncated object is NEVER assigned
// an id (which would leave a sibling Ref dangling).
fn js_container_id(
    scope: &mut v8::PinScope<'_, '_>,
    seen: &mut JsSeen,
    value: v8::Local<v8::Value>,
    obj: v8::Local<v8::Object>,
    depth: u32,
) -> Result<u32, JsVal> {
    let hash = obj.get_identity_hash().get();
    if let Some(bucket) = seen.map.get(&hash) {
        for (g, id) in bucket {
            if v8::Local::new(scope, g) == obj {
                return Err(JsVal::Ref(*id));
            }
        }
    }
    // First sighting but too deep: truncate WITHOUT registering, so no later
    // Ref can target a container that was never emitted.
    if depth >= MAX_MARSHAL_DEPTH {
        return Err(JsVal::Str(stringify(scope, value)));
    }
    let id = seen.next_id;
    seen.next_id += 1;
    let g = v8::Global::new(scope, obj);
    seen.map.entry(hash).or_default().push((g, id));
    Ok(id)
}

// Copy |len| bytes from a V8 (Shared)ArrayBuffer backing pointer into an owned
// Vec, with one allocation and no zero-fill (data is fully overwritten). |data|
// is None only for a zero-length buffer, where the empty Vec is already right.
fn copy_buffer_bytes(data: Option<std::ptr::NonNull<c_void>>, len: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(len);
    if let Some(p) = data {
        unsafe {
            std::ptr::copy_nonoverlapping(p.as_ptr() as *const u8, buf.as_mut_ptr(), len);
            buf.set_len(len);
        }
    }
    buf
}

// JS -> JsVal reads a value the way a script would, so it can run the page's
// JavaScript: an object's own enumerable string keys are listed and each is read
// with [[Get]] (a getter runs), a Proxy answers through its ownKeys /
// getOwnPropertyDescriptor / get traps, an array's holes read through its
// prototype chain (`get_index` is `arr[i]`), and a value with no structural form
// is its ToString (`stringify`). A throw there is the op's (`marshalled!`). Map and
// Set contents (`as_array`), typed arrays and buffers are read without any.
pub(crate) fn js_to_jsval(scope: &mut v8::PinScope<'_, '_>, value: v8::Local<v8::Value>) -> JsVal {
    let mut seen = JsSeen::default();
    js_to_jsval_d(scope, value, &mut seen, 0)
}

fn js_to_jsval_d(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<v8::Value>,
    seen: &mut JsSeen,
    depth: u32,
) -> JsVal {
    if value.is_undefined() {
        return JsVal::Undefined;
    }
    if value.is_null() {
        return JsVal::Null;
    }
    if value.is_boolean() {
        return JsVal::Bool(value.boolean_value(scope));
    }
    if value.is_int32() {
        return JsVal::Int(value.integer_value(scope).unwrap_or(0));
    }
    if value.is_number() {
        return JsVal::Num(value.number_value(scope).unwrap_or(f64::NAN));
    }
    if value.is_big_int()
        && let Ok(bi) = v8::Local::<v8::BigInt>::try_from(value)
    {
        let mut words = vec![0u64; bi.word_count()];
        let (negative, _) = bi.to_words_array(&mut words);
        return JsVal::BigInt { negative, words };
    }
    // Date before the generic object branch (a Date *is* an object).
    if value.is_date()
        && let Ok(date) = v8::Local::<v8::Date>::try_from(value)
    {
        return JsVal::Date(date.value_of());
    }
    // Binary buffers before the generic object branch (they are objects too).
    // A TypedArray/DataView copies its VIEWED window; a bare ArrayBuffer or
    // SharedArrayBuffer copies the whole buffer. All become a Ruby binary
    // String. (Without the SharedArrayBuffer arm a bare SAB would fall through
    // to the plain-object branch and marshal as an empty Hash — silent loss.)
    if value.is_array_buffer_view()
        && let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(value)
    {
        let obj = v8::Local::<v8::Object>::try_from(value).unwrap();
        // depth 0: a buffer is a leaf (no recursion into children), so it
        // never risks native-stack overflow and must stay faithful bytes
        // even when deeply nested — only the identity (Ref) check applies,
        // never the depth-truncation-to-lossy-string the generic path uses.
        let id = match js_container_id(scope, seen, value, obj, 0) {
            Ok(id) => id,
            Err(jsval) => return jsval, // a Ref to the same buffer
        };
        let len = view.byte_length();
        let mut buf: Vec<u8> = Vec::with_capacity(len);
        // copy_contents_uninit writes into the UNINITIALIZED spare capacity
        // (a &mut [MaybeUninit<u8>]) — never forming a &mut [u8] over uninit
        // memory the way copy_contents would (that's UB). set_len to exactly
        // what it wrote so a detached/short view never exposes uninit bytes.
        let n = view.copy_contents_uninit(&mut buf.spare_capacity_mut()[..len]);
        unsafe { buf.set_len(n) };
        return JsVal::Bytes {
            id: Some(id),
            bytes: buf,
        };
    }
    if value.is_array_buffer()
        && let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(value)
    {
        let obj = v8::Local::<v8::Object>::try_from(value).unwrap();
        // depth 0 — a buffer is a leaf; see the view arm above.
        let id = match js_container_id(scope, seen, value, obj, 0) {
            Ok(id) => id,
            Err(jsval) => return jsval,
        };
        return JsVal::Bytes {
            id: Some(id),
            bytes: copy_buffer_bytes(ab.data(), ab.byte_length()),
        };
    }
    if value.is_shared_array_buffer()
        && let Ok(sab) = v8::Local::<v8::SharedArrayBuffer>::try_from(value)
    {
        let obj = v8::Local::<v8::Object>::try_from(value).unwrap();
        // depth 0 — a buffer is a leaf; see the view arm above.
        let id = match js_container_id(scope, seen, value, obj, 0) {
            Ok(id) => id,
            Err(jsval) => return jsval,
        };
        let store = sab.get_backing_store();
        return JsVal::Bytes {
            id: Some(id),
            bytes: copy_buffer_bytes(store.data(), sab.byte_length()),
        };
    }
    // Map/Set before the generic object branch (both are objects).
    if value.is_map() {
        let obj = v8::Local::<v8::Object>::try_from(value).unwrap();
        let id = match js_container_id(scope, seen, value, obj, depth) {
            Ok(id) => id,
            Err(jsval) => return jsval,
        };
        let map = v8::Local::<v8::Map>::try_from(value).unwrap();
        let arr = map.as_array(scope); // [k0, v0, k1, v1, ...]
        let mut pairs = Vec::with_capacity((arr.length() / 2) as usize);
        let mut i = 0;
        while i + 1 < arr.length() {
            let k = arr
                .get_index(scope, i)
                .unwrap_or_else(|| v8::undefined(scope).into());
            let v = arr
                .get_index(scope, i + 1)
                .unwrap_or_else(|| v8::undefined(scope).into());
            let kj = js_to_jsval_d(scope, k, seen, depth + 1);
            let vj = js_to_jsval_d(scope, v, seen, depth + 1);
            pairs.push((kj, vj));
            i += 2;
        }
        return JsVal::Map { id, pairs };
    }
    if value.is_set() {
        let obj = v8::Local::<v8::Object>::try_from(value).unwrap();
        let id = match js_container_id(scope, seen, value, obj, depth) {
            Ok(id) => id,
            Err(jsval) => return jsval,
        };
        let set = v8::Local::<v8::Set>::try_from(value).unwrap();
        let arr = set.as_array(scope);
        let mut items = Vec::with_capacity(arr.length() as usize);
        for i in 0..arr.length() {
            let el = arr
                .get_index(scope, i)
                .unwrap_or_else(|| v8::undefined(scope).into());
            items.push(js_to_jsval_d(scope, el, seen, depth + 1));
        }
        return JsVal::Set { id, items };
    }
    if value.is_array() {
        let obj = v8::Local::<v8::Object>::try_from(value).unwrap();
        let id = match js_container_id(scope, seen, value, obj, depth) {
            Ok(id) => id,
            Err(jsval) => return jsval,
        };
        let arr = v8::Local::<v8::Array>::try_from(value).unwrap();
        let mut items = Vec::with_capacity(arr.length() as usize);
        for i in 0..arr.length() {
            let el = arr
                .get_index(scope, i)
                .unwrap_or_else(|| v8::undefined(scope).into());
            items.push(js_to_jsval_d(scope, el, seen, depth + 1));
        }
        return JsVal::Array { id, items };
    }
    // Plain object -> string-keyed Obj. Functions/Date/etc. fall through to
    // their toString (the spike's primitive escape hatch).
    if value.is_object() && !value.is_function() {
        let obj = v8::Local::<v8::Object>::try_from(value).unwrap();
        let id = match js_container_id(scope, seen, value, obj, depth) {
            Ok(id) => id,
            Err(jsval) => return jsval,
        };
        if let Some(names) = obj.get_own_property_names(scope, Default::default()) {
            let mut entries = Vec::with_capacity(names.length() as usize);
            for i in 0..names.length() {
                let Some(key) = names.get_index(scope, i) else {
                    continue;
                };
                let key_str = stringify(scope, key);
                let val = obj
                    .get(scope, key)
                    .unwrap_or_else(|| v8::undefined(scope).into());
                entries.push((key_str, js_to_jsval_d(scope, val, seen, depth + 1)));
            }
            return JsVal::Obj { id, entries };
        }
        // Enumeration THREW — a module namespace whose bindings aren't
        // initialized yet, a Proxy with a hostile ownKeys trap. Falling through
        // to the stringify below would hand the caller a plausible-looking
        // "[object Object]" for an object we never actually read. Leave the
        // pending exception alone instead and let the op's TryCatch report it —
        // and give back the id, or a sibling Ref to this same object would point
        // at a container that is never emitted.
        unregister_container(seen, obj, id);
        return JsVal::Undefined;
    }
    JsVal::Str(stringify(scope, value))
}

// The escape hatch for values with no structural representation — Function,
// Date, RegExp, Symbol. to_rust_string_lossy is Value::ToString, which THROWS on
// a Symbol, and a Symbol sitting anywhere in a graph (every React element has
// one) would then leave a pending exception behind and fail the whole op over a
// value that marshalled fine. to_detail_string is V8's no-side-effects renderer
// and cannot throw, so Symbols go through it.
fn stringify(scope: &mut v8::PinScope<'_, '_>, value: v8::Local<v8::Value>) -> String {
    if value.is_symbol() {
        return value
            .to_detail_string(scope)
            .map(|s| s.to_rust_string_lossy(scope))
            .unwrap_or_default();
    }
    value.to_rust_string_lossy(scope)
}

// Undo js_container_id's registration for a container we end up NOT emitting.
// The id is always the last one pushed to its bucket, since nothing else can
// register while we are inside this object.
fn unregister_container(seen: &mut JsSeen, obj: v8::Local<v8::Object>, id: u32) {
    let hash = obj.get_identity_hash().get();
    if let Some(bucket) = seen.map.get_mut(&hash) {
        bucket.retain(|(_, existing)| *existing != id);
    }
}

// Owned-by-value (not &JsVal): a JsVal::Bytes hands its Vec straight to V8's
// backing store with no copy of the payload, so a large binary blob crosses
// Ruby->JS with zero extra allocation.
//
// Nothing here runs the page's JavaScript. An array's elements and an object's
// entries are DEFINED (CreateDataProperty), never assigned: an assignment is
// [[Set]], which walks the prototype chain, so an accessor a page put on
// `Array.prototype[0]` or `Object.prototype.x` would take the value — or swallow
// it — before it reached the new container. The rest makes nothing a page can
// reach either: a Map and a Set are filled by V8's own builtins (Map::Set calls
// the realm's original Map.prototype.set, not whatever the page left there), and
// a Uint8Array, a Date and a BigInt are made outright. A "__proto__" key is an
// own property like any other (as JSON.parse makes it), not the prototype.
pub(crate) fn jsval_to_js<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    val: JsVal,
) -> v8::Local<'s, v8::Value> {
    jsval_to_js_d(scope, val, &mut Built::default())
}

// The containers made so far in one jsval_to_js, by id, for a Ref to resolve to.
#[derive(Default)]
struct Built<'s> {
    values: HashMap<u32, v8::Local<'s, v8::Value>>,
    // Arrays whose elements are still being made — id -> length. An array is
    // made from its finished elements in one step (Array::new_with_elements), so
    // until then it has no object for a Ref to name; a Ref from inside it (a
    // cycle) makes it early instead (`Built::get`).
    open_arrays: HashMap<u32, usize>,
}

impl<'s> Built<'s> {
    fn get(
        &mut self,
        scope: &mut v8::PinScope<'s, '_>,
        id: u32,
    ) -> Option<v8::Local<'s, v8::Value>> {
        if let Some(value) = self.values.get(&id) {
            return Some(*value);
        }
        let length = *self.open_arrays.get(&id)?;
        let array: v8::Local<v8::Value> = v8::Array::new(scope, length as i32).into();
        self.values.insert(id, array);
        Some(array)
    }
}

fn jsval_to_js_d<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    val: JsVal,
    built: &mut Built<'s>,
) -> v8::Local<'s, v8::Value> {
    match val {
        JsVal::Undefined => v8::undefined(scope).into(),
        JsVal::Null => v8::null(scope).into(),
        JsVal::Bool(b) => v8::Boolean::new(scope, b).into(),
        JsVal::Int(i) => v8::Number::new(scope, i as f64).into(),
        JsVal::Num(n) => v8::Number::new(scope, n).into(),
        JsVal::Str(s) => v8::String::new(scope, &s)
            .map(|s| s.into())
            .unwrap_or_else(|| v8::undefined(scope).into()),
        // Bytes -> Uint8Array, moving the Vec into V8's backing store (no copy
        // of the payload). Registered under |id| so an aliased blob resolves to
        // the same Uint8Array via Ref.
        JsVal::Bytes { id, bytes } => {
            let len = bytes.len();
            let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
            let ab = v8::ArrayBuffer::with_backing_store(scope, &store);
            let arr: v8::Local<v8::Value> = v8::Uint8Array::new(scope, ab, 0, len)
                .map(|a| a.into())
                .unwrap_or_else(|| v8::undefined(scope).into());
            if let Some(id) = id {
                built.values.insert(id, arr);
            }
            arr
        }
        JsVal::BigInt { negative, words } => v8::BigInt::new_from_words(scope, negative, &words)
            .map(|b| b.into())
            .unwrap_or_else(|| v8::undefined(scope).into()),
        JsVal::Date(ms) => v8::Date::new(scope, ms)
            .map(|d| d.into())
            .unwrap_or_else(|| v8::undefined(scope).into()),
        // The elements first, then the array made of them — unless one of them
        // referred back to it, which made it early (`Built::get`): then they are
        // defined on it one by one.
        JsVal::Array { id, items } => {
            built.open_arrays.insert(id, items.len());
            let elements: Vec<v8::Local<v8::Value>> = items
                .into_iter()
                .map(|it| jsval_to_js_d(scope, it, built))
                .collect();
            built.open_arrays.remove(&id);
            if let Some(early) = built.values.get(&id).copied() {
                let array = v8::Local::<v8::Object>::try_from(early).unwrap();
                for (i, element) in elements.into_iter().enumerate() {
                    if let Some(key) = v8::String::new(scope, &i.to_string()) {
                        array.create_data_property(scope, key.into(), element);
                    }
                }
                early
            } else {
                let array = v8::Array::new_with_elements(scope, &elements).into();
                built.values.insert(id, array);
                array
            }
        }
        // Register the container under its id BEFORE filling it, so a Ref from
        // a descendant (a cycle back to here) resolves to this same object.
        JsVal::Obj { id, entries } => {
            let obj = v8::Object::new(scope);
            built.values.insert(id, obj.into());
            for (k, it) in entries {
                let Some(key) = v8::String::new(scope, &k) else {
                    continue;
                };
                let v = jsval_to_js_d(scope, it, built);
                obj.create_data_property(scope, key.into(), v);
            }
            obj.into()
        }
        JsVal::Map { id, pairs } => {
            let map = v8::Map::new(scope);
            built.values.insert(id, map.into());
            for (k, v) in pairs {
                let kk = jsval_to_js_d(scope, k, built);
                let vv = jsval_to_js_d(scope, v, built);
                map.set(scope, kk, vv);
            }
            map.into()
        }
        JsVal::Set { id, items } => {
            let set = v8::Set::new(scope);
            built.values.insert(id, set.into());
            for it in items {
                let v = jsval_to_js_d(scope, it, built);
                set.add(scope, v);
            }
            set.into()
        }
        JsVal::Ref(id) => built
            .get(scope, id)
            .unwrap_or_else(|| v8::undefined(scope).into()),
    }
}

pub(crate) fn jsval_to_ruby(ruby: &Ruby, val: &JsVal) -> Result<Value, Error> {
    let mut built: HashMap<u32, Value> = HashMap::new();
    jsval_to_ruby_d(ruby, val, &mut built)
}

// `built` is a HashMap<u32, Value> — the same "bare Values in a heap container,
// hidden from the GC mark phase" shape that's a use-after-free in call_proc. It
// is safe HERE only because every entry is, at every allocating safepoint, ALSO
// reachable from a live stack local: each container arm (Array/Obj/Map/Set)
// keeps its arr/h/set as a live local while its children recurse and grafts each
// child into it (push/aset), so the child is marked transitively; Bytes inserts
// then immediately returns its live local `s`. So `built` never holds the sole
// reference. This invariant is load-bearing: do NOT refactor an arm to stash a
// value in `built` without keeping it rooted by a live local until it's grafted.

// The RustyRacer::JSMap class (a Hash subclass), defined in init. A JS Map
// marshals to an instance of it so the reverse direction can distinguish it from
// a plain Hash (a JS Object) and rebuild a JS Map. Errors only if init didn't run.
fn js_map_class(ruby: &Ruby) -> Result<magnus::RClass, Error> {
    ruby.class_object()
        .const_get::<_, magnus::RModule>("RustyRacer")?
        .const_get::<_, magnus::RClass>("JSMap")
}

fn jsval_to_ruby_d(
    ruby: &Ruby,
    val: &JsVal,
    built: &mut HashMap<u32, Value>,
) -> Result<Value, Error> {
    Ok(match val {
        JsVal::Undefined | JsVal::Null => ruby.qnil().as_value(),
        JsVal::Bool(b) => (*b).into_value_with(ruby),
        JsVal::Int(i) => (*i).into_value_with(ruby),
        JsVal::Num(n) => (*n).into_value_with(ruby),
        JsVal::Str(s) => s.clone().into_value_with(ruby),
        // Bytes -> a binary (ASCII-8BIT) String: str_from_slice uses rb_str_new,
        // which tags the result ASCII-8BIT — so it round-trips back to bytes.
        // Registered under |id| so an aliased blob stays one String via Ref.
        JsVal::Bytes { id, bytes } => {
            let s = ruby.str_from_slice(bytes).as_value();
            if let Some(id) = id {
                built.insert(*id, s);
            }
            s
        }
        // Reconstruct the Ruby Integer from the hex magnitude (arbitrary
        // precision); negate via Ruby so bignums stay exact.
        JsVal::BigInt { negative, words } => {
            let mag: Value = ruby
                .str_new(&words_to_hex(words))
                .funcall("to_i", (16i64,))?;
            if *negative {
                mag.funcall("-@", ())?
            } else {
                mag
            }
        }
        // Time.at takes seconds; carry sub-second precision as the Float. An
        // invalid Date (value_of NaN) raises RangeError, matching csim's
        // des_date — never a silent nil.
        JsVal::Date(ms) => {
            if !ms.is_finite() {
                return Err(Error::new(ruby.exception_range_error(), "invalid Date"));
            }
            ruby.class_object()
                .const_get::<_, magnus::RClass>("Time")?
                .funcall::<_, _, Value>("at", (*ms / 1000.0,))?
        }
        // Register before filling so a Ref from a descendant resolves to the
        // same Ruby object (shared/cyclic graphs keep their identity).
        JsVal::Array { id, items } => {
            let arr = ruby.ary_new();
            built.insert(*id, arr.as_value());
            for it in items {
                let _ = arr.push(jsval_to_ruby_d(ruby, it, built)?);
            }
            arr.as_value()
        }
        // JS objects -> string-keyed Hashes.
        JsVal::Obj { id, entries } => {
            let h = ruby.hash_new();
            built.insert(*id, h.as_value());
            for (k, it) in entries {
                let _ = h.aset(k.as_str(), jsval_to_ruby_d(ruby, it, built)?);
            }
            h.as_value()
        }
        // JS Map -> RustyRacer::JSMap, a Hash subclass: arbitrary marshalled keys
        // (not just strings) AND distinguishable from a JS Object (a plain Hash),
        // so ruby_to_jsval can rebuild a JS Map rather than a plain object. Build
        // empty then aset so a cyclic Map (a value referring back to the Map)
        // resolves through the Ref table.
        //
        // LIMITATION: keys collapse by Ruby Hash equality (eql?/hash), but a JS
        // Map keys by identity (SameValueZero). Primitive keys (string/number/
        // bool/bigint) round-trip exactly; two distinct-but-structurally-equal
        // OBJECT keys (e.g. `{}` and `{}`) become ONE entry — inherent to a Hash
        // representation, and object identity can't survive copy-marshalling anyway.
        JsVal::Map { id, pairs } => {
            let map_obj: Value = js_map_class(ruby)?.funcall("new", ())?;
            built.insert(*id, map_obj);
            let h = RHash::from_value(map_obj).expect("JSMap is a Hash");
            for (k, v) in pairs {
                let kk = jsval_to_ruby_d(ruby, k, built)?;
                let vv = jsval_to_ruby_d(ruby, v, built)?;
                let _ = h.aset(kk, vv);
            }
            map_obj
        }
        // JS Set -> Ruby Set (stdlib); build empty then add so a cyclic Set
        // (a Set containing itself) resolves through the Ref table.
        JsVal::Set { id, items } => {
            let set: Value = ruby
                .class_object()
                .const_get::<_, magnus::RClass>("Set")?
                .funcall("new", ())?;
            built.insert(*id, set);
            for it in items {
                let v = jsval_to_ruby_d(ruby, it, built)?;
                let _: Value = set.funcall("add", (v,))?;
            }
            set
        }
        JsVal::Ref(id) => built
            .get(id)
            .copied()
            .unwrap_or_else(|| ruby.qnil().as_value()),
    })
}

// A Ruby String marshalled by its encoding TAG (the tag is the type):
//   - ASCII-8BIT (binary) -> JsVal::Bytes (a JS Uint8Array);
//   - any text encoding   -> JsVal::Str (UTF-8). Already-UTF-8 text is taken
//     as-is; other text encodings transcode (Ruby raises on unmappable bytes).
//     Either way the bytes must be VALID UTF-8 — invalid bytes RAISE, never
//     silently degrade to U+FFFD (loud failure beats silent corruption). A
//     text String mis-tagged binary surfaces loudly too (it becomes a Uint8Array).
fn string_to_jsval(ruby: &Ruby, s: RString) -> Result<JsVal, Error> {
    use magnus::encoding::EncodingCapable;
    if s.enc_get() == ruby.ascii8bit_encindex() {
        // Binary: the bytes ARE the value (O(n) copy, no inflation). id: None —
        // the identity-tracked path is the direct-String branch in
        // ruby_to_jsval_d; a to_str result reaching here is transient.
        return Ok(JsVal::Bytes {
            id: None,
            bytes: unsafe { s.as_slice() }.to_vec(),
        });
    }
    // Text. encode('UTF-8') on an already-UTF-8 source is a no-op that does NOT
    // validate, so skip it (one fewer copy) and let the from_utf8 check below
    // catch invalid bytes; other encodings transcode (raising on unmappable).
    let utf8: RString = if s.enc_get() == ruby.utf8_encindex() {
        s
    } else {
        s.funcall("encode", ("UTF-8",))?
    };
    // Build the Rust String with a real UTF-8 check (not lossy): invalid bytes
    // in a text-tagged String are an error, not silent U+FFFD substitution.
    match String::from_utf8(unsafe { utf8.as_slice() }.to_vec()) {
        Ok(s) => Ok(JsVal::Str(s)),
        Err(_) => Err(Error::new(
            ruby.class_object()
                .const_get::<_, ExceptionClass>("EncodingError")
                .unwrap_or_else(|_| ruby.exception_runtime_error()),
            "text-tagged String contains invalid UTF-8 bytes",
        )),
    }
}

// A JS object key must be a string. A Ruby String key crosses by its bytes as
// UTF-8 — but unlike a binary VALUE (which becomes a Uint8Array), a key has
// nowhere to put raw bytes, so invalid UTF-8 RAISES rather than silently
// degrading to U+FFFD. None for a non-String (the caller then tries to_s).
fn string_key(ruby: &Ruby, val: Value) -> Option<Result<String, Error>> {
    let s = RString::from_value(val)?;
    let bytes = unsafe { s.as_slice() }.to_vec();
    Some(String::from_utf8(bytes).map_err(|_| {
        Error::new(
            ruby.class_object()
                .const_get::<_, ExceptionClass>("EncodingError")
                .unwrap_or_else(|_| ruby.exception_runtime_error()),
            "hash key is not valid UTF-8",
        )
    }))
}

// A Ruby String's bytes interpreted as UTF-8 (invalid sequences become U+FFFD),
// regardless of the encoding tag. Used for the depth-truncation to_s fallback,
// where the value is already being lossily summarised.
fn lossy_string(val: Value) -> Option<String> {
    let s = RString::from_value(val)?;
    // Copy the bytes out before any further Ruby call can move/free them.
    let bytes = unsafe { s.as_slice() }.to_vec();
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

// Tracks Ruby containers already emitted this marshal (by object_id, which is
// exact — no collision handling needed) so shared/cyclic structures become Refs.
#[derive(Default)]
struct RbSeen {
    next_id: u32,
    map: HashMap<usize, u32>,
}

pub(crate) fn ruby_to_jsval(val: Value) -> Result<JsVal, Error> {
    let mut seen = RbSeen::default();
    ruby_to_jsval_d(val, &mut seen, 0)
}

fn ruby_to_jsval_d(val: Value, seen: &mut RbSeen, depth: u32) -> Result<JsVal, Error> {
    let ruby = Ruby::get_with(val);
    // The built-in types first, each by its type tag alone: no class looked up,
    // no method called, nothing raised and rescued — a String asked whether it
    // converts to a Float raises a TypeError to say no, which cost a String
    // argument over a microsecond. A subclass of a built-in has the same tag, and
    // goes the same way, as it always did.
    if val.is_nil() {
        return Ok(JsVal::Null);
    }
    if val.as_raw() == ruby.qtrue().as_raw() {
        return Ok(JsVal::Bool(true));
    }
    if val.as_raw() == ruby.qfalse().as_raw() {
        return Ok(JsVal::Bool(false));
    }
    // Integer. A JS Number is an f64, so only integers exactly representable
    // there (|n| <= 2^53) become Int/Number; anything larger (the rest of the
    // i64 range AND true bignums) becomes a BigInt so no precision is lost.
    if let Some(int) = magnus::Integer::from_value(val) {
        if let Ok(i) = int.to_i64()
            && i.unsigned_abs() <= (1u64 << 53)
        {
            return Ok(JsVal::Int(i));
        }
        let abs: Value = val.funcall("abs", ())?;
        let hex: String = abs.funcall("to_s", (16i64,))?;
        let negative = val.funcall::<_, _, bool>("negative?", ())?;
        return Ok(JsVal::BigInt {
            negative,
            words: hex_to_words(&hex),
        });
    }
    if let Some(float) = magnus::Float::from_value(val) {
        return Ok(JsVal::Num(float.to_f64()));
    }
    // Bare Symbol -> JS string (one-way: it comes back as a Ruby String). A
    // binary-encoded symbol surfaces the same curated EncodingError as a text
    // String with invalid UTF-8, not magnus's raw "expected utf-8" message.
    if let Some(sym) = magnus::Symbol::from_value(val) {
        let name = sym.name().map_err(|_| {
            Error::new(
                ruby.class_object()
                    .const_get::<_, ExceptionClass>("EncodingError")
                    .unwrap_or_else(|_| ruby.exception_runtime_error()),
                "symbol name is not valid UTF-8",
            )
        })?;
        return Ok(JsVal::Str(name.into_owned()));
    }
    // Real Strings: the encoding tag is the type declaration. A binary
    // (ASCII-8BIT) String -> bytes (JS Uint8Array), identity-tracked so an
    // aliased blob stays one Uint8Array; any text encoding -> a JS string.
    if let Some(rstr) = RString::from_value(val) {
        use magnus::encoding::EncodingCapable;
        if rstr.enc_get() == ruby.ascii8bit_encindex() {
            // depth 0 — a binary blob is a leaf, so it stays faithful bytes even
            // when deeply nested (never the depth-truncation-to-lossy-string);
            // only the identity (Ref) check applies. Frozen/interned binary
            // Strings share an object_id, so two `-"x".b` literals deliberately
            // collapse to ONE Uint8Array (they ARE the same Ruby object).
            let id = match rb_container_id(seen, val, 0)? {
                RbId::New(id) => id,
                RbId::Reuse(jv) => return Ok(jv),
            };
            return Ok(JsVal::Bytes {
                id: Some(id),
                bytes: unsafe { rstr.as_slice() }.to_vec(),
            });
        }
        return string_to_jsval(&ruby, rstr);
    }
    if let Some(arr) = RArray::from_value(val) {
        return array_to_jsval(arr, val, seen, depth);
    }
    if let Some(hash) = RHash::from_value(val) {
        // A plain Hash is a JS object. RustyRacer::JSMap, a Hash subclass, is a JS
        // Map, its arbitrary keys kept as real JS values (NOT string-coerced) — it
        // is what makes a JS Map round-trip (JS Map -> JSMap -> JS Map) instead of
        // degrading to an object. Only a Hash of another class asks which it is.
        let plain = val.class().as_raw() == ruby.class_hash().as_raw();
        if !plain
            && let Ok(js_map) = js_map_class(&ruby)
            && val.is_kind_of(js_map)
        {
            return map_to_jsval(hash, val, seen, depth);
        }
        return hash_to_jsval(&ruby, hash, val, seen, depth);
    }
    // What only a class, or a conversion method, can say.
    //
    // Ruby Time -> JS Date. Before the Numeric fallback: magnus's f64 TryConvert
    // coerces a Time via to_f, so it would otherwise marshal as a bare epoch
    // number. Time#to_f is epoch seconds; Date wants ms.
    if let Ok(time_class) = ruby.class_object().const_get::<_, magnus::RClass>("Time")
        && val.is_kind_of(time_class)
    {
        let sec = val.funcall::<_, _, f64>("to_f", ())?;
        return Ok(JsVal::Date(sec * 1000.0));
    }
    // Any other Float-convertible value (a Rational, a BigDecimal, an
    // ActiveSupport::Duration — whatever rb_num2dbl takes, to_f included) -> its
    // Float. A probe that raises to say no, but the built-ins are past it by now.
    if let Ok(n) = f64::try_convert(val) {
        return Ok(JsVal::Num(n));
    }
    // A String-like (to_str) gets the same tag-driven treatment, but its result
    // is transient so it is not identity-tracked.
    if val.respond_to("to_str", false).unwrap_or(false) {
        let s: Value = val.funcall("to_str", ())?;
        if let Some(rstr) = RString::from_value(s) {
            return string_to_jsval(&ruby, rstr);
        }
    }
    // Ruby Set -> JS Set. Before the to_ary/to_hash conversions (a Set is neither).
    if let Ok(set_class) = ruby.class_object().const_get::<_, magnus::RClass>("Set")
        && val.is_kind_of(set_class)
    {
        let id = match rb_container_id(seen, val, depth)? {
            RbId::New(id) => id,
            RbId::Reuse(jv) => return Ok(jv),
        };
        let arr: RArray = val.funcall("to_a", ())?;
        let mut items = Vec::with_capacity(arr.len());
        for i in 0..arr.len() {
            let el: Value = arr.entry::<Value>(i as isize)?;
            items.push(ruby_to_jsval_d(el, seen, depth + 1)?);
        }
        return Ok(JsVal::Set { id, items });
    }
    if let Ok(arr) = RArray::try_convert(val) {
        return array_to_jsval(arr, val, seen, depth);
    }
    if let Ok(hash) = RHash::try_convert(val) {
        return hash_to_jsval(&ruby, hash, val, seen, depth);
    }
    Err(Error::new(
        ruby.exception_type_error(),
        "unsupported type crossing into JS",
    ))
}

// An Array (or what to_ary made of |val|) -> a JS array; |val| is the identity.
fn array_to_jsval(arr: RArray, val: Value, seen: &mut RbSeen, depth: u32) -> Result<JsVal, Error> {
    let id = match rb_container_id(seen, val, depth)? {
        RbId::New(id) => id,
        RbId::Reuse(jv) => return Ok(jv),
    };
    let mut items = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        let el: Value = arr.entry::<Value>(i as isize)?;
        items.push(ruby_to_jsval_d(el, seen, depth + 1)?);
    }
    Ok(JsVal::Array { id, items })
}

// A RustyRacer::JSMap -> a JS Map.
fn map_to_jsval(hash: RHash, val: Value, seen: &mut RbSeen, depth: u32) -> Result<JsVal, Error> {
    let id = match rb_container_id(seen, val, depth)? {
        RbId::New(id) => id,
        RbId::Reuse(jv) => return Ok(jv),
    };
    let pairs = RefCell::new(Vec::new());
    hash.foreach(|k: Value, v: Value| {
        pairs.borrow_mut().push((
            ruby_to_jsval_d(k, seen, depth + 1)?,
            ruby_to_jsval_d(v, seen, depth + 1)?,
        ));
        Ok(magnus::r_hash::ForEach::Continue)
    })?;
    Ok(JsVal::Map {
        id,
        pairs: pairs.into_inner(),
    })
}

// A Hash (or what to_hash made of |val|) -> a JS object with string keys.
fn hash_to_jsval(
    ruby: &Ruby,
    hash: RHash,
    val: Value,
    seen: &mut RbSeen,
    depth: u32,
) -> Result<JsVal, Error> {
    let id = match rb_container_id(seen, val, depth)? {
        RbId::New(id) => id,
        RbId::Reuse(jv) => return Ok(jv),
    };
    let entries = RefCell::new(Vec::new());
    hash.foreach(|k: Value, v: Value| {
        // String/Symbol keys -> a UTF-8 String; anything else via to_s. A JS
        // object key has nowhere to put raw bytes, so unlike a binary VALUE
        // (-> Uint8Array) a binary KEY with invalid UTF-8 RAISES (string_key),
        // and a to_s returning a non-String is a loud error, not a silent "".
        let key = match string_key(ruby, k) {
            Some(r) => r?,
            None => {
                // A non-String key (Symbol, Integer, ...) -> to_s, then the
                // same UTF-8 rule.
                let s: Value = k.funcall("to_s", ())?;
                match string_key(ruby, s) {
                    Some(r) => r?,
                    None => {
                        return Err(Error::new(
                            ruby.exception_type_error(),
                            "hash key's to_s did not return a String",
                        ));
                    }
                }
            }
        };
        entries
            .borrow_mut()
            .push((key, ruby_to_jsval_d(v, seen, depth + 1)?));
        Ok(magnus::r_hash::ForEach::Continue)
    })?;
    Ok(JsVal::Obj {
        id,
        entries: entries.into_inner(),
    })
}

enum RbId {
    New(u32),
    Reuse(JsVal),
}

// Ruby-side mirror of js_container_id: New(id) to register and recurse, or
// Reuse(jsval) to emit directly (a Ref to an already-seen object, or a
// depth-truncated Str). Computes object_id once.
fn rb_container_id(seen: &mut RbSeen, val: Value, depth: u32) -> Result<RbId, Error> {
    // rb_obj_id itself rather than a call to #object_id: the same id, without a
    // method dispatch per container.
    let oid = usize::try_convert(unsafe { Value::from_raw(rb_sys::rb_obj_id(val.as_raw())) })?;
    if let Some(id) = seen.map.get(&oid) {
        return Ok(RbId::Reuse(JsVal::Ref(*id)));
    }
    if depth >= MAX_MARSHAL_DEPTH {
        let ruby = Ruby::get().unwrap();
        let s: Value = val.funcall("to_s", ())?;
        let s = lossy_string(s).ok_or_else(|| {
            Error::new(ruby.exception_type_error(), "to_s did not return a String")
        })?;
        return Ok(RbId::Reuse(JsVal::Str(s)));
    }
    let id = seen.next_id;
    seen.next_id += 1;
    seen.map.insert(oid, id);
    Ok(RbId::New(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Run |check| in a fresh context of a fresh isolate, after |setup|.
    fn in_context(setup: &str, check: impl FnOnce(&mut v8::PinScope<'_, '_>)) {
        crate::init_v8();
        let isolate = &mut v8::Isolate::new(Default::default());
        v8::scope!(let scope, isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        eval(scope, setup);
        check(scope);
    }

    fn eval<'s>(scope: &mut v8::PinScope<'s, '_>, source: &str) -> v8::Local<'s, v8::Value> {
        let code = v8::String::new(scope, source).unwrap();
        v8::Script::compile(scope, code, None)
            .unwrap()
            .run(scope)
            .unwrap()
    }

    fn global_set(scope: &mut v8::PinScope<'_, '_>, name: &str, value: v8::Local<v8::Value>) {
        let key = v8::String::new(scope, name).unwrap();
        let global = scope.get_current_context().global(scope);
        global.create_data_property(scope, key.into(), value);
    }

    fn as_string(scope: &mut v8::PinScope<'_, '_>, source: &str) -> String {
        let value = eval(scope, source);
        value.to_rust_string_lossy(scope)
    }

    const TRAPS: &str = r#"
        globalThis.hits = [];
        Object.defineProperty(Array.prototype, '0', {set(v) { hits.unshift(v) }, get() { return 'proto' }});
        Object.defineProperty(Object.prototype, 'k', {set(v) { hits.unshift(v) }, get() { return 'proto' }});
    "#;

    #[test]
    fn arrays_and_objects_are_defined_past_prototype_setters() {
        in_context(TRAPS, |scope| {
            let value = jsval_to_js(
                scope,
                JsVal::Array {
                    id: 0,
                    items: vec![
                        JsVal::Int(41),
                        JsVal::Obj {
                            id: 1,
                            entries: vec![("k".into(), JsVal::Str("own".into()))],
                        },
                    ],
                },
            );
            global_set(scope, "v", value);
            assert_eq!(
                as_string(
                    scope,
                    "[v[0], v[1].k, Object.hasOwn(v[1], 'k'), hits.length].join()"
                ),
                "41,own,true,0"
            );
        });
    }

    #[test]
    fn a_proto_key_is_an_own_property() {
        in_context("", |scope| {
            let value = jsval_to_js(
                scope,
                JsVal::Obj {
                    id: 0,
                    entries: vec![("__proto__".into(), JsVal::Null)],
                },
            );
            global_set(scope, "v", value);
            assert_eq!(
                as_string(
                    scope,
                    "[Object.getPrototypeOf(v) === Object.prototype, Object.hasOwn(v, '__proto__')].join()"
                ),
                "true,true"
            );
        });
    }

    #[test]
    fn an_array_referring_to_itself_is_one_array() {
        in_context(TRAPS, |scope| {
            // [1, <itself>, [<itself>]]
            let value = jsval_to_js(
                scope,
                JsVal::Array {
                    id: 0,
                    items: vec![
                        JsVal::Int(1),
                        JsVal::Ref(0),
                        JsVal::Array {
                            id: 1,
                            items: vec![JsVal::Ref(0)],
                        },
                    ],
                },
            );
            global_set(scope, "v", value);
            assert_eq!(
                as_string(
                    scope,
                    "[v.length, v[0], v[1] === v, v[2][0] === v, Array.isArray(v), hits.length].join()"
                ),
                "3,1,true,true,true,0"
            );
        });
    }

    #[test]
    fn maps_and_sets_are_filled_by_the_original_builtins() {
        in_context(
            "Map.prototype.set = () => { throw new Error('patched') }; Set.prototype.add = Map.prototype.set;",
            |scope| {
                let value = jsval_to_js(
                    scope,
                    JsVal::Array {
                        id: 0,
                        items: vec![
                            JsVal::Map {
                                id: 1,
                                pairs: vec![(JsVal::Str("a".into()), JsVal::Int(1))],
                            },
                            JsVal::Set {
                                id: 2,
                                items: vec![JsVal::Int(2)],
                            },
                        ],
                    },
                );
                global_set(scope, "v", value);
                assert_eq!(
                    as_string(scope, "[v[0].get('a'), v[1].has(2)].join()"),
                    "1,true"
                );
            },
        );
    }
}
