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
  than none; now ~0.07 µs). Subclasses and `to_str` / `to_ary` / `to_hash`
  conversions marshal as before.
- A getter or `Proxy` trap that throws while `Context#call` resolves its path
  now raises a `RuntimeError` carrying the JS stack, and the thrown value
  propagates through a host function like any other JS throw.

### Documented

- What reading a JS value into Ruby runs (getters, `Proxy` traps, holes read
  through the prototype chain, `toString`) and the binary-String ↔ `Uint8Array`
  rule — README, "Marshalling and the page's JavaScript".
