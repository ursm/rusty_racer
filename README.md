# rusty_racer

Embed [V8](https://v8.dev/) in Ruby, built on [rusty_v8](https://crates.io/crates/v8)
(the `v8` crate) and [Magnus](https://github.com/matsadler/magnus) via
[rb-sys](https://github.com/oxidize-rb/rb-sys).

> Early and experimental — the API still moves. Each `Isolate` runs V8 in-thread
> on the Ruby thread that created it (the GVL is released around the JS run), and
> is **thread-confined**: every operation must happen on that owner thread, or it
> raises. `Isolate#terminate` is the exception — it is safe from any thread.

## Highlights

- **ES modules**, including dynamic `import()` with an embedder-owned resolver —
  not just classic scripts.
- **Faithful value marshalling**: `BigInt`, `Date`, `Map`, `Set`, typed binary
  (`Uint8Array`/`ArrayBuffer` ↔ binary `String`), and shared/cyclic object
  graphs all round-trip — no lossy JSON hop. A JS `Map` surfaces in Ruby as a
  `RustyRacer::JSMap` (a `Hash` subclass — it reads like a `Hash` but keeps its
  non-string keys) and marshals back to a JS `Map`; a plain `Hash` still maps to a
  JS object.
- **In-thread execution** — V8 runs on the calling Ruby thread, with no dedicated
  V8 thread and no per-op thread hop; fast when you run many small ops.
- **Drop-in [ExecJS](#execjs) runtime** — any ExecJS consumer switches with no
  code change.
- **Snapshots, realms (`Context`s), host callbacks, and a bytecode cache.**
- **Resource limits on both axes** — a `timeout_ms` (time) and a `memory_limit`
  (space), each catchable: a runaway script fails just its own `eval`, leaving
  the isolate usable, instead of aborting the process. A heap runaway is caught
  even with no explicit `memory_limit` — V8's default ceiling raises instead of
  aborting.
- **Precompiled gems** bundle V8 for Linux/macOS × Ruby 3.3–4.0 — no V8 build,
  no Rust toolchain.

### Compared to mini_racer

[mini_racer](https://github.com/rubyjs/mini_racer) is the mature, widely-deployed
incumbent — if you want a battle-tested binding or **Windows** support, reach for
it. rusty_racer differs where it counts for some workloads: native **ES modules +
dynamic import** (mini_racer is eval/classic-script oriented); **richer
marshalling** (`BigInt`/`Date`/`Map`/`Set` and shared/cyclic graphs cross as
distinct Ruby types, where mini_racer does a narrower value conversion); and
**in-thread execution** with no per-op thread hop,
which is faster for overhead-dominated workloads (lots of tiny `eval`/`call`) and
at parity once the per-op JS work dominates. It is also younger and
**experimental** — fewer miles, no Windows yet. Parity with
mini_racer is not a goal; the overlap is convergent evolution, not a port.

## What it can do

Names follow V8's: an `Isolate` is the VM; it hands out `Context`s (v8::Context,
a realm) that you run JS in.

```ruby
require "rusty_racer"

iso = RustyRacer::Isolate.new(timeout_ms: 1000)
ctx = iso.context                        # the default context

ctx.eval("1 + 1")                        # => 2
ctx.eval("({a: 1, b: [true, 'x']})")     # => {"a"=>1, "b"=>[true, "x"]}

# Call a JS function with marshalled args (BigInt/Date/Map/Set/shared refs
# all round-trip faithfully; a JS Map surfaces as a RustyRacer::JSMap).
ctx.eval("function add(a, b) { return a + b }")
ctx.call("add", 20, 22)                  # => 42
ctx.call_void("doSideEffect")            # runs it; never marshals the return
ctx.eval_void("globalThis.app = boot()") # ditto for the completion value

# Ruby callbacks into JS; a raised Ruby exception becomes a JS exception.
ctx.attach("rubyUpcase", ->(s) { s.upcase })
ctx.eval("rubyUpcase('hi')")             # => "HI"

# Stack traces: JS errors carry the JS stack as the Ruby backtrace.
begin
  ctx.eval("throw new Error('boom')", filename: "app.js")
rescue RustyRacer::RuntimeError => e
  e.message     # => "Error: boom"
  e.backtrace   # => ["app.js:1:7"]  (named frames read "app.js:1:25:in 'fn'")
end
```

Everything the binding raises descends from `RustyRacer::Error`, so one rescue
covers the library. Under it: `ParseError`, `RuntimeError`,
`ScriptTerminatedError` and `V8OutOfMemoryError` for what the JS did;
`DisposedError` for using an isolate (or anything it handed out) after
disposing it; `WrongThreadError` for reaching an isolate from the wrong thread.

Reading a result is not passive — marshalling runs JS (getters, `Proxy` traps,
`toString`), and a throw there fails the operation even though the code itself
ran to completion. When the value is incidental, say so: `#eval_void`,
`Script#run_void` and `#call_void` run for the effect and never touch the
result. A statement list has a completion value whether you want one or not, so
plumbing an object into a global (`globalThis.win = someProxy`) is enough to
make a plain `#eval` fail after every one of its writes has landed.

ES modules (the embedder owns the URL→module registry):

```ruby
dep = ctx.compile_module("export const x = 21;", filename: "/dep.js")
app = ctx.compile_module('import {x} from "./dep.js"; export const r = x * 2;',
                         filename: "/app.js")
app.instantiate { |specifier, referrer| dep if specifier == "./dep.js" }
app.graph_async?                         # => false
app.evaluate
app.namespace["r"]                       # => 42
```

`graph_async?` asks whether top-level `await` appears anywhere in the *linked*
graph, so an `await` in a dependency counts — which is why it needs an
instantiated module and raises before then. `false` is the answer that carries a
guarantee: `evaluate` ran the whole graph to completion. Since there is no event
loop here (see below), a `true` graph whose `await` never settles returns from
`evaluate` with the module body still suspended, and `status` reports
`:evaluated` either way — so `graph_async?` is what distinguishes the two.

Classic `<script>`s work the same way: `ctx.compile("1 + 1").run` # => 2.

### Resource limits

An isolate can cap untrusted code on both axes. Each limit terminates the
offending op and raises a catchable error — the isolate stays usable afterward,
so one runaway script fails just its own `eval`, not the whole process.

```ruby
# Time: timeout_ms caps each eval/call (per-call override on Context#eval).
iso = RustyRacer::Isolate.new(timeout_ms: 1000)
iso.context.eval("for (;;) {}")          # raises RustyRacer::ScriptTerminatedError

# Space: memory_limit caps the V8 heap in bytes (a soft limit, enforced at GC
# granularity). The isolate forces a GC and resets the ceiling on recovery.
iso = RustyRacer::Isolate.new(memory_limit: 64 * 1024 * 1024)
iso.context.eval("a = []; for (;;) a.push(new Array(1e6))")
                                         # raises RustyRacer::V8OutOfMemoryError
iso.context.eval("1 + 1")                # => 2 (still usable)
```

Even without an explicit `memory_limit`, a heap runaway raises
`V8OutOfMemoryError` against V8's own default ceiling (~2 GB on 64-bit) rather
than aborting the process — pass `memory_limit:` for a tighter bound. (One
caveat: if the process's available memory — e.g. a container cgroup limit — sits
below the active ceiling, the OS may kill the process before V8's callback
fires; set an explicit `memory_limit` under that bound to keep it catchable.)

### Bytecode caching

V8 compiles lazily: the top level up front, each function body on first call.
Caches can be produced two ways, matching that.

```ruby
src = "function double(x) { return x * 2 }; double(21)"

# produce_cache: — a cold cache taken at compile time (top level only). Persist
# it, then pass it back via cached_data: to skip the reparse on the next boot,
# even in another process or isolate.
blob  = ctx.compile(src, produce_cache: true).cached_data
other = RustyRacer::Isolate.new.context.compile(src, cached_data: blob)
other.cache_rejected?            # => false (true if the blob was stale)

# create_code_cache — a warm cache from the current compile state. Run a script
# (or evaluate a module) first, and it also captures the inner functions that
# actually ran — the warm cache a browser keeps; produce_cache can't see them.
s = ctx.compile(src)
s.run
warm = s.create_code_cache       # binary String, or nil if V8 can't serialize

# eager: compiles every function up front instead of lazily (~2× compile time,
# more memory) — worth it only when producing a cache. Ignored with cached_data:.
ctx.compile(src, produce_cache: true, eager: true)
```

Both `compile` (classic scripts → `Script#run`) and `compile_module` (ES modules
→ `#instantiate`/`#evaluate`) take `cached_data:`, `produce_cache:`, and `eager:`,
and expose `#cached_data` / `#cache_rejected?` / `#create_code_cache`.

Also available:

- **`Snapshot`** — startup blobs: boot an isolate from a baked-in heap and code
  cache.
- **`Isolate#create_context`** — an extra realm with its own globals, sharing the
  isolate's heap. All realms are mutually same-origin (with a host namespace,
  `NS.contextGlobal(id)` reaches another realm's `globalThis`, like a same-origin
  `iframe.contentWindow`), so this is **not** an isolation boundary.
- **Zero-copy buffer transfer/share between isolates** — with a host namespace, the
  `NS` object exposes the engine primitive for implementing `postMessage`
  transferables and `SharedArrayBuffer` sharing, both with no byte copy:
  - **Transfer** (the transfer list): `NS.transferOut(arrayBufferOrView)` detaches
    a buffer (its `byteLength` → 0) and returns an integer token; `NS.transferIn(token)`
    rebuilds an `ArrayBuffer` over the **same** memory in another isolate.
  - **Share** (a `SharedArrayBuffer`): `NS.shareOut(sharedArrayBuffer)` returns a
    token *without* detaching — the source stays live; `NS.shareIn(token)` rebuilds
    a `SharedArrayBuffer` over the same memory, so both isolates see each other's
    writes and `Atomics` work across them (including across threads).

  The backing store is heap-external and atomic-refcounted, so a token stays
  importable even after the exporting isolate is disposed. `transferOut`/`shareOut`
  return `0` for an unsuitable argument (so the caller can fall back to a copy);
  `transferIn`/`shareIn` return `undefined` for an unknown or wrong-kind token;
  `NS.transferDrop(token)` releases an exported buffer that is never imported.
  `RustyRacer.pending_transfer_count` reports exported-but-not-yet-imported buffers
  so dropped messages don't leak silently.
- **`Isolate#perform_microtask_checkpoint`** — manual microtask drain. The default
  `microtasks: :auto` also drains at the end of each outermost eval/call/evaluate;
  `microtasks: :explicit` leaves it fully manual. There is no event loop or timers
  either way.
- **`Isolate#terminate`**, **`Isolate#dynamic_import_resolver=`**,
  **`Context#reset`** (below), and **`Platform.set_flags!`**.

### `Context#reset`

`reset` swaps the realm's `globalThis` for a fresh `v8::Context`, reusing the
warm isolate — a per-visit reset that avoids rebuilding the VM. Its contract:

- **The snapshot is replayed.** On a snapshotted isolate the fresh context is
  re-deserialized from the snapshot, so the snapshot's baked-in globals — and
  its precompiled code cache — come back. `reset` means "back to the snapshot"
  (or to an empty realm, with no snapshot).
- **Runtime mutations are dropped.** Anything set on the realm at runtime is gone.
- **Host fns are dropped.** Functions `attach`/`attach_many`'d into the realm are
  released (their GC roots freed); re-attach them after a reset.
- **Modules and classic scripts are dropped.** Handles compiled in the realm die
  with the old context.
- **The realm id and the shared same-origin token are preserved** — the id keeps
  addressing the realm, now backed by the fresh context.
- **`reset` is refused (raises), leaving the realm untouched, when** a microtask
  checkpoint is draining, the realm is unknown/disposed, or a request for it is
  suspended on the V8 stack (e.g. resetting a realm from inside one of its own
  host fns).

## ExecJS

rusty_racer ships an optional [ExecJS](https://github.com/rails/execjs) runtime,
so any ExecJS consumer (asset pipelines, CoffeeScript/Babel/Uglify wrappers, …)
can run on V8-in-Ruby with no code change:

```ruby
require "rusty_racer/execjs"
ExecJS.runtime = RustyRacer::ExecJSRuntime.new

ExecJS.eval("'foo bar'.toUpperCase()")   # => "FOO BAR"
ctx = ExecJS.compile("function add(a, b) { return a + b }")
ctx.call("add", 1, 2)                     # => 3
```

The adapter is **opt-in** — `rusty_racer` never requires `execjs` itself, so it
stays a non-dependency; `require "rusty_racer/execjs"` pulls it in only when you
ask. Values cross with ExecJS's JSON semantics (functions and `undefined` drop
out, Dates become ISO strings), matching what ExecJS's external runtimes give, so
results are identical whatever runtime a library picked. The integration is
verified against ExecJS's own runtime contract suite (`test/execjs_test.rb`).

## Threading

An `Isolate` runs V8 **in-thread** on the Ruby thread that created it, and is
**thread-confined**: every operation on it — and on the `Context`s, `Module`s,
and `Script`s it hands out — must run on that owner thread. A V8 isolate is bound
to one native thread (rusty_v8 exposes no `v8::Locker`), so using it from another
thread raises `RustyRacer::WrongThreadError` rather than corrupting the VM.

- **`Isolate#terminate` is the one exception** — it is safe to call from any
  thread (it stops a runaway script on the owner thread). It stops JavaScript at
  V8's next interrupt check, which a runaway script reaches almost immediately
  and a *short* one may never reach: terminating from inside a host function that
  then returns to a script which simply finishes lets that script finish, and the
  request is dropped rather than carried into the next operation. Use it to stop
  code that is running away, not as a way to fail the call you are inside.
- **Dispose on the owner thread.** `Isolate#dispose` must run on the owner
  thread. If the last reference to an isolate is instead garbage-collected on a
  *different* thread (e.g. its owner thread already exited), it cannot be
  disposed and the V8 isolate **leaks** until the process exits. To avoid this,
  call `iso.dispose` on the owner thread before that thread ends — and watch
  `RustyRacer.leaked_isolate_count` (and `RustyRacer.live_isolate_count`) to
  confirm a long-running, thread-churning workload isn't leaking.

One isolate per thread is the supported model; share work between threads by
giving each thread its own isolate.

### Fibers

In-thread V8 runs on whatever stack the calling Ruby code is on — including a
**Fiber**'s separate stack (a plain `Enumerator` is a Fiber, so this is common:
`Capybara::Result#find`, lazy enumerators, …). This works on the **main thread**,
where the process stack is the highest address and every Fiber sits below it.
That includes a Fiber entered *underneath* a running JS call — a host function
that resumes a Fiber which evals again — since each operation describes its own
stack to V8 and restores the enclosing one when it returns.

On a **non-main thread** it does not. V8 decides "is this the central stack?" by
testing the stack pointer against a window that ends at the thread's native stack
top — a pthread value it caches, with no API to retarget on POSIX. Only the
window's *lower* edge follows the per-operation stack limit, so a Fiber allocated
*above* that top — the usual case off the main thread, whose own stack sits below
later Fiber mmaps — is outside the window whatever we do, and V8 aborts the
process on the next GC or thrown exception. So **don't call into an isolate from
inside a Fiber on a worker thread**; drive isolate ops directly on the thread, or
keep Fiber/Enumerator-mediated JS calls on the main thread.

The other limit is on *where* a Fiber may switch. Operations on one isolate have
to nest: everything V8 keeps per isolate — the frame chain, the handle scopes,
the description of the stack — unwinds in the reverse of the order it was
entered. Resuming a Fiber from inside a host function keeps that (the outer
operation is simply blocked until the inner one returns, which is why
`Capybara::Result#find` inside a callback works), but **yielding out of a host
function** does not: it strands that operation mid-flight. That covers
`Fiber.yield` and `Fiber#transfer`, an `Enumerator::Yielder#<<` from inside a
callback, and any blocking call made under a Fiber scheduler (`async`, `falcon`),
which yields out of the Fiber for you — the last of those without a `Fiber`
keyword anywhere in your code.

What happens next depends on what the stranded operation's Fiber does:

- **Resumed while another operation is still running** — the two can now only
  finish in the order they started, which V8 has no room for. There is no
  exception to raise: raising *is* that unwind. So the binding prints what
  happened, which host function did it, and where — then stops the process with
  `SIGABRT` (exit 134). It is not rescuable and there is no opt-out; on a
  threaded server that takes the whole worker with it.

  ```
  [rusty_racer] fatal: operations on this isolate stopped nesting.

  The host function `hop` handed control to Ruby.
  By the time it came back, the operations in flight on this isolate had
  changed (now 2, was 1) — so they no longer nest. …
  ```

- **Resumed only after the other operation has finished** — memory-safe, and it
  keeps working. But note that *while* an operation is stranded, every other
  operation on that isolate runs as a **nested** one, and only an outermost
  operation drains microtasks. So a promise queued in the meantime does not
  settle until the stranded operation finishes. **This is the shape a Fiber
  scheduler produces**: it resumes the stranded Fiber once the other fiber's
  operation is done, so `async`/`falcon` get the quiet variant, not the abort.

- **Never resumed** — Ruby frees an unresumed Fiber's stack without unwinding it,
  so the operation's frames vanish while the isolate is still entered by them.
  The isolate cannot be disposed after that; it is leaked, with a warning saying
  why. If you still hold the Fiber, **`Fiber#kill` recovers it**: it resumes the
  Fiber to unwind it, so the stranded operation finishes in order and the isolate
  goes back to normal. (The one thing that defeats that is a script which catches
  the error the killed callback throws and carries on — the kill only lands once
  that operation ends, just as it would wait out any Ruby call in progress, and
  until then the script can call further host functions, which run on the
  already-killed Fiber.)

## Installation

Precompiled gems bundle V8 — no V8 build, no Rust toolchain — for Ruby 3.3, 3.4,
and 4.0 on:

- **Linux:** x86_64 and arm64 (aarch64)
- **macOS:** arm64 (Apple silicon) and x86_64 (Intel)

```ruby
gem "rusty_racer"
```

or `gem install rusty_racer`. On any other platform or Ruby, the source gem
builds the extension at install time — see below.

## Building from source

Building the source gem needs only a Rust toolchain — the `v8` crate downloads
Deno's stock prebuilt `rusty_v8` archive and links it statically. Since rusty_v8
150.1.0 that archive is shared-library-safe on Linux too (it no longer emits the
`R_X86_64_TPOFF32`-under-`-shared` relocation that used to block linking V8 into
an extension's shared object), so no `RUSTY_V8_ARCHIVE` override or from-source V8
build is required on any supported platform.

## License

MIT.
