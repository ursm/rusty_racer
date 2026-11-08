# Changelog

## 0.2.6

### Added

- `Context#function(path)` returns a `RustyRacer::Function`: the function a
  dotted path names, resolved once, with the object it was read off kept as its
  `this`. `#call` / `#call_void` invoke it as `Context#call` / `#call_void`
  would — same marshalling, timeout, errors and re-entrancy — without walking
  the path or making a property name per call. It raises `DisposedError` once
  its realm is reset or disposed and never keeps that realm alive; `#dispose`
  releases it early, and garbage collection releases it too.

### Changed

- Ruby → JS marshalling no longer runs the page's JavaScript. Arrays and
  objects built for JS have their elements and entries defined
  (`CreateDataProperty`) rather than assigned, so an accessor on
  `Array.prototype` or `Object.prototype` can no longer intercept or swallow
  call arguments, host function results or the host namespace's members. A
  `"__proto__"` key in a Hash now becomes an own property of the object, as
  `JSON.parse` makes it, instead of setting its prototype.
- Ruby → JS marshalling dispatches on the value's built-in type first: a String,
  Symbol, Array or Hash argument no longer pays for a raised-and-rescued
  conversion probe or a constant lookup (a String argument cost ~1.2 µs more
  than none; now ~0.07 µs). Subclasses, `to_f` (an `ActiveSupport::Duration`),
  `to_str`, `to_ary` and `to_hash` conversions marshal as before — except that
  an Array or Hash subclass is now always an array or an object, where one that
  also defined `to_str` (or a Hash subclass `to_ary`) used to cross as that.
- A getter or `Proxy` trap that throws while `Context#call` resolves its path
  now raises a `RuntimeError` carrying the JS stack, and the thrown value
  propagates through a host function like any other JS throw.
- A `Module` or `Script` whose wrapper is garbage-collected without `#dispose`
  is released too (at the end of the isolate's next operation), where 0.2.5 kept
  it until its realm was reset or disposed. `#dispose` itself now only queues
  the release, as `Function#dispose` does, so it works from any thread — in
  0.2.5 a dispose from another thread marked the wrapper disposed but silently
  kept the module or script for good.
- A module's url (its `import.meta.url`, its url as a referrer) is kept for as
  long as V8 keeps the module, not as long as its wrapper or its realm: a
  dependency the resolver compiled and Ruby dropped or disposed, or a function
  of a disposed realm's module that another realm still holds, still reads it.
  In 0.2.5 either read `undefined`.

### Documented

- What reading a JS value into Ruby runs (getters, `Proxy` traps, holes read
  through the prototype chain, `toString`) and the binary-String ↔ `Uint8Array`
  rule — README, "Marshalling and the page's JavaScript".

### Deliberately unchanged

- JS → Ruby still renders a value with no structural form — a function, a
  RegExp, a depth-truncated object — through its ToString, which a page can
  override (`Function.prototype.toString`) to run code or throw during the read.
  V8's side-effect-free renderer (`Value::ToDetailString`, already used for
  Symbols) would run nothing, but it truncates a function's source past 128
  characters and renders objects as `#<Object>`, so switching is a behaviour
  change left for a release that decides it on purpose.
