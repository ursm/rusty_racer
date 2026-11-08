# Changelog

## 0.2.8

### Added

- A **host object** per realm: an object with no prototype that no global
  holds (its global keeps it under a V8 private key), for the host and the
  realm's own bindings to share where a page's script can neither read, call nor
  replace what they put there. Every path the host names starts there when it
  begins with `@`: `Context#attach('@fetch', proc)` / `#attach_many` put a host
  function there (intermediates with no prototype, every property defined, not
  assigned), `#function('@steps.query')` / `#call` / `#call_void` find one
  there, `Module#evaluate(report: '@reportError')` reports to one there, and
  `Isolate.new(host_namespace: '@RustyRacer')` installs rusty_racer's own
  functions (`contextGlobal`, `contextOf`, `drainMicrotasks`, …) there instead
  of on the global. `RustyRacer::HOST_OBJECT`, as an argument (or a host
  function's result) crossing into JS, is the realm's host object — how the
  bindings are handed it, and the realm's host object crosses back into Ruby as
  `HOST_OBJECT`; an embedder linking rusty_racer as a library reaches it
  natively with `rusty_racer::host_object(scope)`, from its realm init hook.
  An `@` path is walked through own properties alone, so nothing a page put on
  a prototype sees or answers the walk. A namespace on the host object has
  `hostObjectOf(id)` besides: another realm's host object, for one realm's
  bindings to reach another's. Made as a realm first uses it, made anew with the
  realm by `reset`.

### Changed

- A path beginning with `@` no longer names a property of the global whose name
  begins with `@` (`globalThis["@x"]`): it names the host object's.
- `Module#evaluate(report:)` looks the reporter up as before — one property,
  dots and all — on the global, or, for an `@` path, as `Context#function` finds
  it on the host object.

## 0.2.7

### Changed

- An operation on a Ruby Fiber looks its stack's bounds up once per stack
  rather than once per operation that a 16-entry cache had dropped. Capybara
  filters each result set in an Enumerator left suspended until GC frees it, so
  a page of finds runs on hundreds of distinct fiber stacks, and each lookup read
  all of `/proc/self/maps` — a fifth of a find loop, and slower the larger
  Ruby's fiber pool grew. Every stack an operation has run on is now kept (Ruby
  hands its pooled stacks out again and never unmaps them), and on Linux 6.11+ a
  lookup asks the kernel for the one mapping (`PROCMAP_QUERY`) instead of
  reading the whole map. A call into V8 from a fresh fiber, steady state: 160 µs
  → 1.0 µs beside a 2,200-line map, 521 µs → 1.0 µs beside 8,300 lines.

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
