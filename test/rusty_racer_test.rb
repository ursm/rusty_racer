# frozen_string_literal: true

require "minitest/autorun"
require "rusty_racer"

# The probe suite cibuildgem runs natively on each platform — proving the
# from-source V8 build links and runs, not just compiles. Mapped to the
# mini_racer-csim audit's hang classes where relevant.
class RustyRacerTest < Minitest::Test
  def setup
    @iso = RustyRacer::Isolate.new
    @ctx = @iso.context
  end

  def test_classic_script_compile_run
    s = @ctx.compile("globalThis.X = 40; X + 2", filename: "/inline.js")
    assert_equal 42, s.run
    assert_equal 40, @ctx.eval("globalThis.X")
    assert_equal 42, s.run # re-runnable
  end

  def test_classic_script_top_level_lexical_visible_to_later_eval
    # const/let/class at script top level must persist for later evals/scripts
    # (shared global lexical environment) — load-bearing for csim.
    @ctx.compile("const SHARED = 7;").run
    assert_equal 7, @ctx.eval("SHARED")
  end

  def test_classic_script_parse_error_is_parse_error
    assert_raises(RustyRacer::ParseError) { @ctx.compile("function (", filename: "/bad.js") }
  end

  def test_classic_script_runtime_error_is_runtime_error
    s = @ctx.compile('throw new Error("scriptboom")', filename: "/t.js")
    e = assert_raises(RustyRacer::RuntimeError) { s.run }
    assert_includes e.message, "scriptboom"
  end

  def test_classic_script_bytecode_cache_round_trip
    src = "(() => 1 + 2)()"
    blob = @ctx.compile(src, filename: "/c.js", produce_cache: true).cached_data
    refute_nil blob
    assert_equal Encoding::ASCII_8BIT, blob.encoding

    iso2 = RustyRacer::Isolate.new
    s = iso2.context.compile(src, filename: "/c.js", cached_data: blob)
    assert_equal false, s.cache_rejected?
    assert_equal 3, s.run
  end

  def test_script_create_code_cache_captures_inner_functions_after_run
    # compile-time produce_cache only sees the lazily-compiled top level. Running
    # the script compiles the inner functions that execute; create_code_cache
    # after run serializes them too -- a warm cache, like a browser keeps.
    src = <<~JS
      function add(a, b) { return a + b }
      function mul(a, b) { return a * b }
      globalThis.RESULT = (() => { let s = 0; for (let i = 0; i < 5; i++) s = add(s, mul(i, 2)); return s })()
    JS
    s = @ctx.compile(src, filename: '/c.js', produce_cache: true)
    cold = s.create_code_cache
    assert_equal Encoding::ASCII_8BIT, cold.encoding
    assert_equal s.cached_data.bytesize, cold.bytesize # both top-level only

    assert_equal 20, s.run
    warm = s.create_code_cache
    assert_operator warm.bytesize, :>, cold.bytesize # inner functions now included

    # the warm cache is a valid input cache: accepted (not rejected) and runs.
    s2 = RustyRacer::Isolate.new.context.compile(src, filename: '/c.js', cached_data: warm)
    assert_equal false, s2.cache_rejected?
    assert_equal 20, s2.run
  end

  def test_script_create_code_cache_raises_after_dispose
    s = @ctx.compile('1')
    s.dispose
    assert_raises(::RuntimeError) { s.create_code_cache }
  end

  def test_create_code_cache_is_nil_when_the_realm_is_gone
    # reset clears the script's compiled handle from the realm's registry. The
    # Ruby Script is still live (not disposed), but there's nothing to serialize,
    # so create_code_cache reports nil rather than raising.
    s = @ctx.compile('function f(){ return 1 }; f()')
    s.run
    @ctx.reset
    assert_nil s.create_code_cache
  end

  def test_eager_compile_runs_and_is_ignored_when_consuming_a_cache
    # eager compiles every function up front; it still runs identically.
    assert_equal 1, @ctx.compile('function f(){ return 1 }; f()', eager: true).run
    assert_instance_of RustyRacer::Module, @ctx.compile_module('export const x = 1', eager: true)

    # eager is incompatible with consuming a cache (V8 forbids the combo); when
    # cached_data: is given, eager is ignored rather than erroring or rejecting.
    blob = @ctx.compile('1 + 2', produce_cache: true).cached_data
    s = @ctx.compile('1 + 2', cached_data: blob, eager: true)
    assert_equal false, s.cache_rejected?
    assert_equal 3, s.run
  end

  def test_eager_does_not_change_the_compile_time_cache_on_v8_150
    # Pins the documented V8-150 behaviour: create_code_cache at COMPILE time
    # doesn't serialize eager-compiled inner functions, so eager: produces a
    # byte-identical compile-time cache. This is expected to start FAILING when a
    # future V8 does serialize them — at which point eager: stops being a no-op
    # for produce_cache: and the docs/comments need revisiting.
    src = 'function a(){ return 1 }; function b(){ return 2 }; a() + b()'
    plain = @ctx.compile(src, filename: '/e.js', produce_cache: true).cached_data
    eager = @ctx.compile(src, filename: '/e.js', produce_cache: true, eager: true).cached_data
    assert_equal plain.bytesize, eager.bytesize
  end

  def test_classic_script_run_honours_timeout
    iso = RustyRacer::Isolate.new(timeout_ms: 50)
    s = iso.context.compile("for(;;){}", filename: "/spin.js")
    assert_raises(RustyRacer::ScriptTerminatedError) { s.run }
    assert_equal 2, iso.context.eval("1 + 1") # isolate still usable
  end

  # Bounded so a MISS fails fast instead of hanging or OS-OOM-killing: ~30 * 8MB
  # = ~240MB worst case, well over the 50MB limit (OOMs in ~7 iterations) yet
  # capped if the limit somehow doesn't bite. Numeric .fill keeps the backing
  # store on V8's GC heap (what memory_limit counts).
  RUNAWAY_JS = "var a = []; for (var i = 0; i < 30; i++) { a.push(new Array(1000000).fill(7)); }"

  # Space-axis twin of the timeout test: a runaway allocation must fail its own
  # eval (catchable) instead of aborting the process, and the isolate must
  # recover (forced GC + ceiling reset) so it stays usable for the next eval.
  def test_memory_limit_raises_v8_out_of_memory_and_recovers
    iso = RustyRacer::Isolate.new(memory_limit: 50 * 1024 * 1024)
    ctx = iso.context
    assert_raises(RustyRacer::V8OutOfMemoryError) { ctx.eval(RUNAWAY_JS) }
    # The process is still alive (no abort) and the isolate reclaimed the heap.
    assert_equal 2, ctx.eval("1 + 1")
    # And the limit still bites a second time — recovery re-armed the callback.
    assert_raises(RustyRacer::V8OutOfMemoryError) { ctx.eval(RUNAWAY_JS) }
    assert_equal 4, ctx.eval("2 + 2")
  end

  # An OOM that fires while draining microtasks (not during the synchronous eval)
  # must still surface as V8OutOfMemoryError, not a bogus success: the drain stops
  # at the terminate and leaves the eval's value (here 'queued') behind, so the
  # bracket has to force the terminated outcome. Guards the silent-success path.
  def test_memory_limit_during_microtask_drain_still_raises
    iso = RustyRacer::Isolate.new(memory_limit: 50 * 1024 * 1024)
    ctx = iso.context
    src = "Promise.resolve().then(() => { #{RUNAWAY_JS} }); 'queued'"
    assert_raises(RustyRacer::V8OutOfMemoryError) { ctx.eval(src) }
    assert_equal 2, ctx.eval("1 + 1") # still usable
  end

  # Both axes armed at once: the runaway trips memory before the (generous)
  # timeout, so it surfaces as an error and the isolate recovers. Exercises the
  # watchdog/OOM interaction (the canary must not misfire — see watchdog.rs).
  def test_memory_limit_and_timeout_together
    iso = RustyRacer::Isolate.new(memory_limit: 50 * 1024 * 1024, timeout_ms: 10_000)
    ctx = iso.context
    assert_raises(RustyRacer::EvalError) { ctx.eval(RUNAWAY_JS) }
    assert_equal 2, ctx.eval("1 + 1")
  end

  def test_v8_out_of_memory_is_an_eval_error
    assert_operator RustyRacer::V8OutOfMemoryError, :<, RustyRacer::EvalError
  end

  def test_no_explicit_memory_limit_allows_moderate_allocation
    # With no explicit limit the callback still guards V8's (large, ~2GB) default
    # ceiling, so a moderately large allocation well under it just succeeds — the
    # default protection only bites a genuine multi-GB runaway (not unit-tested:
    # driving the real default ceiling needs ~2GB and shares the mechanism the
    # explicit-limit tests above already cover).
    assert_equal 500000, @ctx.eval("new Array(500000).fill(0).length")
  end

  def test_classic_script_dispose
    s = @ctx.compile("1")
    assert_equal false, s.disposed?
    s.dispose
    assert_equal true, s.disposed?
    assert_raises(::RuntimeError) { s.run }
  end

  def test_cached_data_version_tag
    tag = RustyRacer.cached_data_version_tag
    assert_kind_of Integer, tag
    assert_operator tag, :!=, 0
  end

  def test_context_has_stable_id
    assert_equal 0, @ctx.id # the default context
    a = @iso.create_context
    b = @iso.create_context
    assert_operator a.id, :>, 0
    refute_equal a.id, b.id
  end

  def test_eval_roundtrip
    assert_equal 2, @ctx.eval("1 + 1")
    assert_equal 3.0, @ctx.eval("1.5 * 2")
    assert_equal "hello", @ctx.eval('"he" + "llo"')
    assert_equal true, @ctx.eval("1 < 2")
    assert_nil @ctx.eval("null")
  end

  def test_js_exception_becomes_ruby_exception
    e = assert_raises(RustyRacer::RuntimeError) { @ctx.eval('throw new Error("boom")') }
    assert_includes e.message, "boom"
  end

  def test_syntax_error_is_parse_error
    assert_raises(RustyRacer::ParseError) { @ctx.eval("this is not valid js ===") }
  end

  def test_parse_error_includes_location
    e = assert_raises(RustyRacer::ParseError) do
      @ctx.eval("let x = ;", filename: "boot.js")
    end
    assert_includes e.message, "boot.js"
  end

  def test_runtime_error_carries_js_stack_as_backtrace
    src = <<~JS
      function inner() { throw new Error("kaboom") }
      function outer() { inner() }
      outer();
    JS
    e = assert_raises(RustyRacer::RuntimeError) { @ctx.eval(src, filename: "app.js") }
    assert_includes e.message, "kaboom"
    refute_nil e.backtrace
    joined = e.backtrace.join("\n")
    # the JS frames are reconstructed into the Ruby backtrace, with our filename
    assert_includes joined, "app.js"
    assert_includes joined, "inner"
  end

  def test_multiline_error_message_does_not_leak_into_backtrace
    e = assert_raises(RustyRacer::RuntimeError) { @ctx.eval('throw new Error("line1\nline2")') }
    # every backtrace frame must look like a frame (carry a location), not a
    # stray fragment of the multi-line message
    e.backtrace.each { |f| refute_equal "line2", f }
  end

  def test_stackless_throw_has_no_host_backtrace
    # throwing a non-Error has no JS stack; the backtrace must not be backfilled
    # with host-side (Rust/pump) frames.
    e = assert_raises(RustyRacer::RuntimeError) { @ctx.eval("throw 42") }
    assert_equal [], e.backtrace
  end

  def test_eval_filename_appears_in_thrown_stack
    e = assert_raises(RustyRacer::RuntimeError) do
      @ctx.eval('throw new Error("boom")', filename: "widget.js")
    end
    assert(e.backtrace.any? { |line| line.include?("widget.js") }, "filename missing from backtrace")
  end

  def test_other_ruby_threads_progress_during_eval
    counter = 0
    t = Thread.new { loop { counter += 1; Thread.pass } }
    @ctx.eval("const until = Date.now() + 200; while (Date.now() < until) {}")
    t.kill
    t.join
    assert_operator counter, :>, 1000, "GVL not released during eval"
  end

  def test_host_namespace_injects_drain_microtasks
    ctx = RustyRacer::Isolate.new(host_namespace: "MiniRacer").context
    assert_equal "object", ctx.eval("typeof MiniRacer")
    assert_equal "function", ctx.eval("typeof MiniRacer.drainMicrotasks")
    order = ctx.eval(<<~JS)
      const seen = [];
      Promise.resolve().then(() => seen.push("microtask"));
      seen.push("before");
      MiniRacer.drainMicrotasks();
      seen.push("after");
      seen;
    JS
    assert_equal %w[before microtask after], order
  end

  def test_host_namespace_survives_reset
    ctx = RustyRacer::Isolate.new(host_namespace: "MiniRacer").context
    ctx.reset
    assert_equal "object", ctx.eval("typeof MiniRacer")
  end

  def test_no_host_namespace_by_default
    assert_equal "undefined", @ctx.eval("typeof MiniRacer")
  end

  def test_set_flags_after_init_raises
    # V8 is already initialized by setup's Context.new, so set_flags! must
    # refuse (a successful set_flags! needs a fresh process — see csim's
    # subprocess single-threaded tests).
    assert_raises(RustyRacer::PlatformAlreadyInitialized) do
      RustyRacer::Platform.set_flags!(:use_strict)
    end
  end

  def test_marshals_arrays_and_hashes
    # JS -> Ruby
    assert_equal [1, 2, 3], @ctx.eval("[1, 2, 3]")
    assert_equal({ "a" => 1, "b" => [true, "x"] }, @ctx.eval('({a: 1, b: [true, "x"]})'))
    # Ruby -> JS -> Ruby through call args + return
    @ctx.eval("function echo(x) { return x }")
    assert_equal({ "k" => [1, 2] }, @ctx.call("echo", { "k" => [1, 2] }))
  end

  def test_strict_bool_marshalling
    # regression: an Integer arg must NOT become `true` (bool::try_convert is
    # truthy; ruby_to_jsval checks the actual true/false singletons instead).
    @ctx.eval("function kind(x) { return typeof x }")
    assert_equal "number", @ctx.call("kind", 42)
    assert_equal "boolean", @ctx.call("kind", true)
    assert_equal "string", @ctx.call("kind", "hi")
  end

  def test_date_marshals_to_time
    # JS Date -> Ruby Time
    t = @ctx.eval('new Date("2021-01-02T03:04:05.000Z")')
    assert_kind_of Time, t
    assert_equal Time.utc(2021, 1, 2, 3, 4, 5).to_i, t.to_i
    # Ruby Time -> JS Date -> back, through call args
    @ctx.eval("function year(d) { return d.getUTCFullYear() }")
    assert_equal 2021, @ctx.call("year", Time.utc(2021, 6, 1))
    # round-trip identity (to the second)
    now = Time.utc(2022, 3, 4, 5, 6, 7)
    @ctx.eval("function echo(x) { return x }")
    assert_equal now.to_i, @ctx.call("echo", now).to_i
  end

  def test_bigint_marshals_to_integer_without_precision_loss
    # JS BigInt -> Ruby Integer (well beyond Float's 2**53 exact range)
    assert_equal 2**53 + 1, @ctx.eval("BigInt(2)**53n + 1n")
    assert_equal(-(2**70), @ctx.eval("-(2n**70n)"))
    big = 123456789012345678901234567890
    assert_equal big, @ctx.eval("123456789012345678901234567890n")

    # Ruby Integer -> JS: a bignum becomes a BigInt, not a lossy Number
    @ctx.eval("function isBig(x) { return typeof x === 'bigint' }")
    assert_equal true, @ctx.call("isBig", 2**80)
    @ctx.eval("function echo(x) { return x }")
    assert_equal big, @ctx.call("echo", big)
    assert_equal(-big, @ctx.call("echo", -big))

    # small ints stay JS numbers (not bigint)
    assert_equal false, @ctx.call("isBig", 42)
    # integers beyond Number's exact range (2**53) become BigInt even within
    # i64, so precision is never lost (regression guard)
    assert_equal true, @ctx.call("isBig", 2**60)
    assert_equal 2**60 + 1, @ctx.call("echo", 2**60 + 1)
    # 2**53 itself is still exactly representable -> stays a Number
    assert_equal false, @ctx.call("isBig", 2**53)
  end

  def test_large_float_stays_number_not_bigint
    # a Float must not be coerced to Integer/BigInt (strict Integer typing)
    @ctx.eval("function kind(x) { return typeof x }")
    assert_equal "number", @ctx.call("kind", 1e300)
    assert_equal 1e300, @ctx.call("echo", 1e300) if @ctx.eval("typeof echo") == "function"
    @ctx.eval("function echo2(x) { return x }")
    assert_in_delta 1e300, @ctx.call("echo2", 1e300), 0.0
  end

  def test_shared_acyclic_call_arg_not_lost
    # a shared (acyclic) substructure in a call arg must survive, not drop to null
    shared = {"v" => 1}
    @ctx.eval("function bv(x) { return x.b && x.b.v }")
    assert_equal 1, @ctx.call("bv", {"a" => shared, "b" => shared})
  end

  def test_call_preserves_arg_identity_within_one_arg
    # Function::call marshals args faithfully, so within a single arg a shared
    # object stays one object (===), not two copies.
    shared = {"v" => 1}
    @ctx.eval("function sameRef(x) { return x.a === x.b }")
    assert_equal true, @ctx.call("sameRef", {"a" => shared, "b" => shared})
  end

  def test_call_resolves_dotted_path_with_receiver
    @ctx.eval("globalThis.math = { base: 100, addBase(x) { return this.base + x } }")
    # dotted path resolves math.addBase and uses `math` as `this`
    assert_equal 105, @ctx.call("math.addBase", 5)
  end

  def test_call_passes_bigint_arg_without_loss
    @ctx.eval("function inc(x) { return x + 1n }")
    assert_equal 2**70 + 1, @ctx.call("inc", 2**70)
  end

  def test_call_void_runs_without_marshalling_return
    # call_void runs the fn for its side effect but never walks the return,
    # so a huge/cyclic result is fine and the Ruby return is nil.
    @ctx.eval("function makeCyclic() { const a = {}; a.self = a; globalThis.RAN = true; return a }")
    assert_nil @ctx.call_void("makeCyclic")
    assert_equal true, @ctx.eval("globalThis.RAN")
  end

  def test_attach_under_host_namespace
    ctx = RustyRacer::Isolate.new(host_namespace: "MiniRacer").context
    ctx.attach("MiniRacer.rubyAdd", proc { |a, b| a + b })
    assert_equal 7, ctx.eval("MiniRacer.rubyAdd(3, 4)")
    # creates intermediate objects even without a pre-existing namespace
    ctx.attach("Helpers.greet", proc { |who| "hi #{who}" })
    assert_equal "hi bob", ctx.eval('Helpers.greet("bob")')
  end

  def test_attach_many_installs_all_in_one_call
    @ctx.attach_many(
      'add'      => proc {|a, b| a + b },
      'greet'    => proc {|who| "hi #{who}" },
      'Ns.const' => proc { 42 }
    )
    assert_equal 7, @ctx.eval('add(3, 4)')
    assert_equal 'hi bob', @ctx.eval('greet("bob")')
    assert_equal 42, @ctx.eval('Ns.const()') # dotted path creates the namespace
  end

  def test_host_callback_args_survive_gc_during_marshalling
    # When JS calls an attached proc, each arg is marshalled into fresh Ruby
    # objects one after another. An earlier arg must stay GC-rooted while a later
    # arg is being built (marshalling allocates Strings/Arrays/Hashes, which can
    # trigger a GC) — a bare Vec<Value> would hide it from the mark phase and the
    # GC would sweep it, handing the proc a dangling VALUE that corrupts the heap.
    # GC.stress forces a GC at every allocation, so the inter-arg window is hit
    # deterministically (without it the crash is ~2 in several hundred runs).
    @ctx.attach('sink', proc {|a, b, c, d|
      "#{a['k'].join(',')}|#{b.length}|#{c}|#{d.length}"
    })
    @ctx.eval(<<~JS)
      function feed() {
        return sink(
          {k: [1, 2, 3]},
          ['x', 'y', 'z', 'w'],
          'hello world',
          new Array(64).fill('z').join('')
        )
      }
    JS

    GC.stress = true
    begin
      20.times do
        assert_equal '1,2,3|4|hello world|64', @ctx.eval('feed()')
      end
    ensure
      GC.stress = false
    end
  end

  def test_reattach_after_realm_dispose_binds_the_new_proc
    # dispose returns a realm's proc slots to the free list, so a later attach
    # recycles a slot id. Guard that a recycled id always binds the NEW proc,
    # never a stale binding from the dead realm. (Reuse itself is an internal
    # optimisation, not directly observable; this pins its user-visible contract
    # across several recycle cycles.)
    3.times do |i|
      realm = @iso.create_context
      realm.attach('f', proc { "gen#{i}" })
      assert_equal "gen#{i}", realm.eval('f()')
      realm.dispose
    end
  end

  def test_heap_statistics_reports_v8_heap
    iso = RustyRacer::Isolate.new
    ctx = iso.context
    ctx.eval('var a = []; for (let i = 0; i < 10000; i++) a.push({x: i}); a.length')
    s = iso.heap_statistics
    assert_equal(
      %i[
        external_memory
        heap_size_limit
        malloced_memory
        number_of_detached_contexts
        number_of_native_contexts
        peak_malloced_memory
        total_heap_size
        used_heap_size
      ],
      s.keys.sort
    )
    s.each_value {|v| assert_kind_of Integer, v }
    assert_operator s[:used_heap_size], :>, 0
    assert_operator s[:heap_size_limit], :>, s[:used_heap_size]
    assert_operator s[:number_of_native_contexts], :>=, 1
  end

  def test_low_memory_notification_runs_and_isolate_stays_usable
    iso = RustyRacer::Isolate.new
    ctx = iso.context
    ctx.eval('var junk = []; for (let i = 0; i < 50000; i++) junk.push({x: i}); junk = null;')
    assert_nil iso.low_memory_notification
    # The isolate is still fully usable after a forced GC.
    assert_equal 3, ctx.eval('1 + 2')
  end

  def test_context_reset_does_not_leak_native_contexts
    # A quiescent Context#reset swaps in a fresh realm and rusty drops the old
    # context's handle, so after a full GC the detached contexts are reclaimed
    # and the LIVE native-context count stays bounded — it must NOT grow with the
    # number of resets (a realm leak would make it climb ~1 per reset).
    iso = RustyRacer::Isolate.new
    ctx = iso.context
    ctx.eval('1 + 1')
    iso.low_memory_notification
    baseline = iso.heap_statistics[:number_of_native_contexts]

    50.times do
      ctx.eval('var a = []; for (let i = 0; i < 200; i++) a.push({x: i}); a.length')
      ctx.reset
    end
    iso.low_memory_notification
    after = iso.heap_statistics[:number_of_native_contexts]

    assert_operator(
      after, :<=, baseline + 3,
      "live native contexts grew #{baseline} -> #{after} across 50 resets (realm leak?)"
    )
  end

  def test_context_reset_with_pending_microtasks_does_not_leak_the_realm
    # THE leak this whole change targets: a reset with an UNDRAINED microtask
    # (a Promise reaction) used to leak the entire old realm — the queued callback
    # captures its creation realm and sat in the isolate-wide microtask queue
    # forever, pinning the old v8::Context (V8 counted it live; even a full GC
    # couldn't reclaim it), so a warm isolate that resets per visit climbed ~1
    # native context (~1 MB) per reset until it pinned the heap cap and GC
    # thrashed. With a per-realm microtask queue, reset drops the old realm's
    # queue (discarding its pending microtasks), so the count stays bounded.
    iso = RustyRacer::Isolate.new(microtasks: :explicit)
    ctx = iso.context
    iso.low_memory_notification
    baseline = iso.heap_statistics[:number_of_native_contexts]

    100.times do
      # Schedule a microtask capturing a big array, then RESET without draining.
      ctx.eval(<<~JS)
        (function () {
          const big = new Array(20000).fill(0).map((_, j) => ({x: j}));
          Promise.resolve().then(() => { big.length; });
        })();
      JS
      ctx.reset
    end
    iso.low_memory_notification
    after = iso.heap_statistics[:number_of_native_contexts]

    # Before the fix this was baseline + 100. A few realms of slack covers V8's
    # own bookkeeping and GC timing.
    assert_operator(
      after, :<=, baseline + 3,
      "live native contexts grew #{baseline} -> #{after} across 100 resets with pending microtasks (realm leak)"
    )
  end

  def test_cross_realm_promise_resolved_after_dispose_does_not_use_freed_queue
    # V8 enqueues a promise reaction into the HANDLER's realm's microtask queue.
    # rusty's realms are mutually accessible, so a live realm can hold — and later
    # resolve — a promise whose handler lives in a realm that was disposed/reset.
    # Dropping that realm's queue frees it, but a cross-realm handle keeps the
    # realm's context alive, so the late resolution would enqueue into freed memory
    # — unless retire_realm_to_graveyard repointed the context first. Drive exactly
    # that under GC.stress; it must not corrupt the heap.
    GC.stress = true
    begin
      iso = RustyRacer::Isolate.new(host_namespace: 'NS', microtasks: :explicit)
      main = iso.context
      10.times do
        frame = iso.create_context
        frame.eval(<<~JS)
          globalThis.fire = null;
          globalThis.p = new Promise(r => { globalThis.fire = r }).then(() => 1);
        JS
        # main grabs the frame's global BEFORE dispose, keeping the frame context
        # alive across the dispose that frees the frame's queue.
        main.eval("globalThis.F = NS.contextGlobal(#{frame.id});")
        frame.dispose
        # Resolve the disposed frame's promise from main: the reaction's handler
        # realm is the freed frame, so V8 enqueues into the frame context's queue —
        # which must be the graveyard, not freed memory.
        main.eval('globalThis.F.fire(); globalThis.F = null;')
        iso.perform_microtask_checkpoint
      end
      iso.dispose
    ensure
      GC.stress = false
    end
    pass
  end

  def test_reset_and_dispose_drop_realm_queues_under_gc_stress
    # reset/dispose now DROP the old realm's v8::MicrotaskQueue (the leak fix).
    # Dropping it DESTRUCTs the queue (it leaves V8's per-isolate ring) and frees
    # its pending microtasks. Churn that path hard under GC.stress — reset and
    # dispose realms that have undrained microtasks and live cross-realm handles,
    # then tear the isolate down — to catch any use-after-free in the queue drop.
    GC.stress = true
    begin
      5.times do
        iso = RustyRacer::Isolate.new(host_namespace: 'NS')
        main = iso.context
        10.times do
          main.eval('Promise.resolve().then(() => { const a = [1, 2, 3]; a.length; });')
          frame = iso.create_context
          frame.eval('Promise.resolve().then(() => 1); var x = {y: 1};')
          main.reset
          frame.dispose
        end
        iso.dispose
      end
    ensure
      GC.stress = false
    end
    pass
  end

  def test_cross_isolate_reentrancy_keeps_the_entered_isolate_straight
    # A host callback on isolate A can run Ruby that evals isolate B, whose own
    # callback re-enters A. A is then on the V8 stack (a re-entrant op) while B
    # is the isolate V8 currently has ENTERED on this thread. The re-entrant
    # path used to assume "depth > 0 => my isolate is still the entered one" and
    # bootstrap a scope on A regardless; opening a context scope on A while B was
    # entered tripped V8's "scope and Context do not belong to the same Isolate"
    # panic, which poisoned A (every later op, including reset, then failed with
    # "disposed context"). Drive exactly that A -> B -> A interleave; the
    # innermost A eval must return normally and A must stay usable afterwards.
    a = RustyRacer::Isolate.new.context
    b = RustyRacer::Isolate.new.context

    # Innermost hop: B's callback re-enters A.
    b.attach('reenterA', proc { a.eval('1 + 2') })
    b.eval('function callA() { return reenterA() }')

    # A's callback hops into B (which hops back into A).
    a.attach('intoB', proc { b.eval('callA()') })

    assert_equal 3, a.eval('intoB()')

    # A wasn't poisoned: it still evals and resets.
    assert_equal 4, a.eval('2 + 2')
    a.reset
    assert_equal 5, a.eval('2 + 3')
  end

  def test_realm_churn_and_teardown_survive_gc
    # Realms used to stamp their id into the V8 context's embedder data, which
    # makes V8 attach a ContextAnnex carrying a guaranteed-finalizer weak handle
    # to every realm. At isolate teardown a first-pass weak callback could re-fire
    # during disposal and abort the process. id_of_context now scans the realm
    # table instead, so realms carry no context slots. Churn realms (create, use,
    # dispose) and tear the isolate down under GC.stress: the path that built and
    # finalized those weak handles must complete without aborting.
    GC.stress = true
    begin
      10.times do
        iso = RustyRacer::Isolate.new
        5.times do
          realm = iso.create_context
          realm.eval("var a = []; for (let i = 0; i < 500; i++) a.push({x: i}); a.length")
          realm.dispose
          iso.context.eval("({ ok: true })")
        end
        iso.dispose
      end
    ensure
      GC.stress = false
    end
    pass
  end

  def test_context_default_timeout
    ctx = RustyRacer::Isolate.new(timeout_ms: 50).context
    assert_raises(RustyRacer::ScriptTerminatedError) { ctx.eval("for(;;){}") }
    # context survives and a normal eval still works
    assert_equal 3, ctx.eval("1 + 2")
    # the default also applies to call
    ctx.eval("function spin() { for(;;){} }")
    assert_raises(RustyRacer::ScriptTerminatedError) { ctx.call("spin") }
  end

  def test_timeout_error_carries_the_js_backtrace
    ctx = RustyRacer::Isolate.new(timeout_ms: 50).context
    err = assert_raises(RustyRacer::ScriptTerminatedError) do
      ctx.eval(<<~JS, filename: 'app.js')
        function spin() { while (true) {} }
        function outer() { spin() }
        outer();
      JS
    end
    bt = err.js_backtrace
    assert_kind_of Array, bt
    refute_empty bt, 'expected the JS stack captured at the timeout'
    # Top frame names the running function, formatted "func (script:line:col)".
    assert_match(/spin/, bt.first)
    assert_match(/app\.js:\d+:\d+/, bt.first)
    # The message names the culprit, and the JS stack is the exception backtrace.
    assert_match(/spin/, err.message)
    assert_equal bt, err.backtrace
    # No leaked terminate: the isolate is still usable.
    assert_equal 3, ctx.eval('1 + 2')
  end

  def test_nested_timeout_culprit_surfaces_even_when_re_thrown
    # A host fn issues a nested eval that runs away and times out. The nested
    # ScriptTerminatedError propagates through the host fn (re-thrown into JS), so
    # the outer eval surfaces it — possibly relabelled as a RuntimeError. Either
    # way the culprit must still be named (the top frame is in the message).
    iso = RustyRacer::Isolate.new
    ctx = iso.context
    ctx.attach('nested', proc { ctx.eval('function hot() { while (true) {} } hot()', timeout_ms: 50, filename: 'nested.js') })
    err = assert_raises(RustyRacer::EvalError) do # RuntimeError or ScriptTerminatedError
      ctx.eval('nested()', filename: 'outer.js')
    end
    assert_match(/hot/, err.message)
    assert_equal 3, ctx.eval('1 + 2')
  end

  def test_escalated_outer_timeout_error_still_carries_the_backtrace
    # A nested eval times out; its terminate is isolate-global, so even though the
    # host fn rescues the nested ScriptTerminatedError, the leftover terminate
    # escalates and the OUTER eval also surfaces a ScriptTerminatedError. That
    # outer error must NOT be empty — reply_value clones the capture rather than
    # consuming it at the nested frame, so the escalated outer error still names
    # the runaway. (With a consuming take it would have been [].)
    iso = RustyRacer::Isolate.new
    ctx = iso.context
    ctx.attach('nested', proc {
      ctx.eval('function inner() { while (true) {} } inner()', timeout_ms: 50, filename: 'nested.js') rescue nil
      1
    })
    err = assert_raises(RustyRacer::ScriptTerminatedError) do
      ctx.eval('function f() { nested() } f()', filename: 'outer.js')
    end
    refute_empty err.js_backtrace
    assert_match(/inner/, err.js_backtrace.join("\n"), err.js_backtrace.inspect)
    assert_equal 3, ctx.eval('1 + 2')
  end

  def test_repeated_timeouts_do_not_leak_terminate_or_backtrace
    # The watchdog now fires via RequestInterrupt (to capture the stack), which
    # can't be cancelled — so a guard must keep a leftover interrupt from
    # terminating the NEXT op or attaching a stale backtrace. Hammer it.
    ctx = RustyRacer::Isolate.new(timeout_ms: 50).context
    5.times do
      assert_raises(RustyRacer::ScriptTerminatedError) { ctx.eval('while (true) {}') }
      # The very next op must run normally and cleanly.
      assert_equal 4, ctx.eval('2 + 2')
    end
  end

  def test_bare_terminate_has_empty_js_backtrace
    # A non-timeout stop (Isolate#terminate) goes through a direct terminate, not
    # the watchdog interrupt, so no JS stack is captured: #js_backtrace is [] and
    # the exception keeps its ordinary Ruby backtrace.
    iso = RustyRacer::Isolate.new # no timeout
    ctx = iso.context
    stopper = Thread.new { sleep 0.1; iso.terminate }
    err = assert_raises(RustyRacer::ScriptTerminatedError) { ctx.eval('for(;;){}') }
    stopper.join
    assert_equal [], err.js_backtrace
    ctx.eval('1') # cancel the leftover terminate
  end

  def test_host_fn_invoked_from_microtask_during_checkpoint
    # csim's settle model: a Promise resolved via a host callback. The host fn
    # fires from a microtask during the checkpoint and must still route to Ruby.
    iso = RustyRacer::Isolate.new(microtasks: :explicit)
    ctx = iso.context
    ctx.attach("rubyVal", proc { 99 })
    ctx.eval('globalThis.out = null; Promise.resolve().then(() => { globalThis.out = rubyVal() });')
    assert_nil ctx.eval("globalThis.out") # not run yet (explicit policy)
    iso.perform_microtask_checkpoint
    assert_equal 99, ctx.eval("globalThis.out")
  end

  def test_auto_microtasks_drain_at_end_of_outermost_eval
    # the default mirrors V8's kAuto (the standard embedder contract): the
    # queue drains when the outermost eval/call completes, so promise
    # continuations are visible to the next eval without a manual checkpoint
    @ctx.eval('globalThis.x = 0; Promise.resolve().then(() => { globalThis.x = 1 });')
    assert_equal 1, @ctx.eval("globalThis.x")
  end

  def test_auto_microtasks_do_not_drain_after_nested_ops
    # a nested eval completes at call depth > 0, so it must NOT drain (same as
    # nested script entry in a browser); the queue drains when the OUTER call
    # finishes
    @ctx.attach("f", proc {
      @ctx.eval('Promise.resolve().then(() => { globalThis.n = 1 });')
      @ctx.eval("typeof globalThis.n") # still pending inside the nested window
    })
    assert_equal "undefined", @ctx.call("f")
    assert_equal 1, @ctx.eval("globalThis.n") # drained when the call returned
  end

  def test_explicit_microtasks_option_validation
    assert_raises(ArgumentError) { RustyRacer::Isolate.new(microtasks: :bogus) }
  end

  def test_auto_drain_is_covered_by_the_watchdog
    # the kAuto end-of-script drain runs inside the request's watchdog bracket,
    # so a runaway microtask continuation times out instead of running unbounded
    c = RustyRacer::Isolate.new.context
    assert_raises(RustyRacer::ScriptTerminatedError) do
      c.eval('Promise.resolve().then(() => { for(;;){} }); 42', timeout_ms: 200)
    end
  end

  def test_auto_drain_self_requeueing_microtask_terminates_not_hangs
    # a microtask that re-queues itself would spin the drain forever; the
    # explicit-checkpoint drain (not V8's kAuto, which ignores termination
    # inside Function::Call) must let the watchdog stop it
    iso = RustyRacer::Isolate.new(timeout_ms: 200)
    c = iso.context
    c.eval('function f(){ Promise.resolve().then(function spin(){ Promise.resolve().then(spin) }); return 7 }')
    assert_raises(RustyRacer::ScriptTerminatedError) { c.call('f') }
    assert_equal 2, iso.context.eval('1 + 1') # isolate still usable
  end

  def test_auto_drain_watchdog_timeout_is_not_masked_by_completion_value
    # the watchdog fires DURING the drain, after the script's completion value
    # is computed — the value must not mask the timeout
    c = RustyRacer::Isolate.new.context
    assert_raises(RustyRacer::ScriptTerminatedError) do
      c.eval('Promise.resolve().then(() => { while(true){} }); 99', timeout_ms: 200)
    end
  end

  def test_evaluate_module_honours_timeout
    # Module#evaluate (and the kAuto drain of its TLA continuation) is watchdog-
    # covered like eval/call
    iso = RustyRacer::Isolate.new(timeout_ms: 200)
    m = iso.context.compile_module('await Promise.resolve(); for(;;){}')
    m.instantiate {|_s, _r| nil }
    assert_raises(RustyRacer::ScriptTerminatedError) { m.evaluate }
    assert_equal 2, iso.context.eval('1 + 1')
  end

  def test_attach_through_runaway_setter_times_out
    # attaching writes onto globalThis, which can fire a user setter running
    # arbitrary JS; an infinite loop there must time out, not hang the thread
    iso = RustyRacer::Isolate.new(timeout_ms: 200)
    ctx = iso.context
    ctx.eval("Object.defineProperty(globalThis, 'victim', { set() { for(;;){} }, configurable: true })")
    assert_raises(RustyRacer::ScriptTerminatedError) { ctx.attach("victim", proc { 1 }) }
    assert_equal 2, ctx.eval("1 + 1") # isolate still usable
  end

  def test_attach_host_fn_called_from_setter_routes_to_ruby
    # a host fn invoked by the setter JS that runs during attach must reach
    # Ruby (REPLY_STACK pushed), not silently return undefined
    @ctx.attach("probe", proc { "ruby-saw-it" })
    @ctx.eval("Object.defineProperty(globalThis, 'victim', { set() { globalThis.captured = probe() }, configurable: true })")
    @ctx.attach("victim", proc { 1 })
    assert_equal "ruby-saw-it", @ctx.eval("globalThis.captured")
  end

  def test_attach_does_not_clobber_primitive_global
    @ctx.eval("globalThis.x = 42")
    assert_raises(RustyRacer::RuntimeError) { @ctx.attach("x.y", proc { 1 }) }
    assert_equal 42, @ctx.eval("globalThis.x") # untouched
  end

  def test_perform_microtask_checkpoint_drains_queue
    # the :explicit opt-out (V8's kExplicit): nothing drains until the
    # embedder says so
    iso = RustyRacer::Isolate.new(microtasks: :explicit)
    ctx = iso.context
    order = ctx.eval(<<~JS)
      globalThis.seen = [];
      Promise.resolve().then(() => seen.push("micro"));
      seen.push("before");
      seen;
    JS
    assert_equal ["before"], order
    assert_equal ["before"], ctx.eval("globalThis.seen") # still pending
    iso.perform_microtask_checkpoint
    assert_equal %w[before micro], ctx.eval("globalThis.seen")
  end

  def test_call_unknown_name_raises_not_injects
    # name is resolved as a property path, never eval'd, so a bogus/injection-y
    # name cannot execute code — it just fails to resolve to a function.
    assert_raises(RustyRacer::RuntimeError) { @ctx.call("no.such.fn") }
    assert_raises(RustyRacer::RuntimeError) { @ctx.call("(()=>42)") }
  end

  def test_js_map_marshals_to_ruby_jsmap
    h = @ctx.eval('new Map([["a", 1], [2, "two"], ["nested", {x: 9}]])')
    # surfaces as RustyRacer::JSMap, a Hash subclass — reads like a Hash...
    assert_instance_of RustyRacer::JSMap, h
    assert_kind_of Hash, h
    assert_equal 1, h["a"]
    assert_equal "two", h[2]            # non-string key preserved
    assert_equal({"x" => 9}, h["nested"])
  end

  def test_js_map_round_trips_back_to_a_js_map
    # JS Map -> Ruby JSMap -> JS Map: the type is preserved (not degraded to a
    # plain object), and non-string / object keys survive the round-trip. This is
    # what lets csim postMessage a Map window<->worker without losing it.
    src = RustyRacer::Isolate.new.context
    dst = RustyRacer::Isolate.new.context
    m = src.eval('new Map([["a", 1], [2, "two"], [{k: 9}, [3, 4]]])')
    dst.eval(<<~JS)
      function probe(x) {
        return [
          x instanceof Map,
          x.get("a"),
          x.get(2),                                   // numeric key
          Array.from(x.keys()).map((k) => typeof k),  // key types preserved
          x.size,
        ];
      }
    JS
    assert_equal [true, 1, "two", %w[string number object], 3], dst.call("probe", m)
  end

  def test_ruby_jsmap_marshals_to_js_map
    # a Ruby-constructed RustyRacer::JSMap becomes a JS Map (a plain Hash stays a
    # JS object — see the next test), so the embedder can hand a Map to JS.
    jm = RustyRacer::JSMap.new
    jm["a"] = 1
    jm[2] = "two"
    @ctx.eval('function probe(x) { return [x instanceof Map, x.get(2), x.size] }')
    assert_equal [true, "two", 2], @ctx.call("probe", jm)
  end

  def test_plain_hash_still_marshals_to_js_object
    # regression: only a JSMap becomes a Map; an ordinary Hash (incl. one with
    # non-string keys) must still marshal to a plain JS object as before.
    @ctx.eval('function kindOf(x) { return x instanceof Map ? "Map" : x.constructor.name }')
    assert_equal "Object", @ctx.call("kindOf", {"x" => 1})
    assert_equal "Object", @ctx.call("kindOf", {1 => "a"})
  end

  def test_js_set_marshals_to_ruby_set
    s = @ctx.eval('new Set([1, 2, 2, 3])')
    assert_kind_of Set, s
    assert_equal Set[1, 2, 3], s
  end

  def test_ruby_set_marshals_to_js_set
    @ctx.attach("getSet", proc { Set[1, 2, 3] })
    assert_equal "object", @ctx.eval("typeof getSet()")
    assert_equal true, @ctx.eval("getSet() instanceof Set")
    assert_equal 3, @ctx.eval("getSet().size")
    assert_equal true, @ctx.eval("getSet().has(2)")
  end

  def test_ruby_set_passes_through_call_as_js_set
    # a Ruby Set passed via Context#call arrives as a real JS Set
    @ctx.eval("function hasIt(s, x) { return s instanceof Set && s.has(x) }")
    assert_equal true, @ctx.call("hasIt", Set[1, 2, 3], 2)
  end

  def test_shared_reference_preserved_js_to_ruby
    # one object referenced twice stays one object on the Ruby side
    h = @ctx.eval('const x = {v: 1}; ({a: x, b: x})')
    assert_same h["a"], h["b"]
    h["a"]["v"] = 99
    assert_equal 99, h["b"]["v"]
  end

  def test_cycle_preserved_js_to_ruby
    # a self-referential object round-trips as a Ruby cycle, not a crash/truncation
    h = @ctx.eval('const a = {name: "root"}; a.self = a; a')
    assert_equal "root", h["name"]
    assert_same h, h["self"]
    assert_same h, h["self"]["self"]["self"]
  end

  def test_cycle_preserved_ruby_to_js
    # build a cyclic Ruby Hash, hand it to JS via a host fn return, prove JS
    # sees the cycle (the ref table reconstructs identity on the V8 side).
    cyclic = {}
    cyclic["self"] = cyclic
    @ctx.attach("getCyclic", proc { cyclic })
    assert_equal true, @ctx.eval("(() => { const x = getCyclic(); return x.self === x })()")
  end

  def test_shared_reference_preserved_ruby_to_js
    shared = {"v" => 1}
    pair = {"a" => shared, "b" => shared}
    @ctx.attach("getPair", proc { pair })
    assert_equal true, @ctx.eval("(() => { const x = getPair(); return x.a === x.b })()")
  end

  def test_invalid_date_raises_not_silent_nil
    # parity with csim's des_date: a non-finite Date is a RangeError, not nil.
    assert_raises(RangeError) { @ctx.eval('new Date("not a date")') }
  end

  def test_reset_clears_globals
    @ctx.eval("globalThis.x = 41")
    assert_equal 41, @ctx.eval("globalThis.x")
    @ctx.reset
    assert_equal "undefined", @ctx.eval("typeof globalThis.x")
    assert_equal 2, @ctx.eval("1 + 1") # realm usable after reset
  end

  def test_reset_replays_the_snapshot
    # reset gives a FRESH context, and on a snapshotted isolate that context is
    # re-deserialized from the snapshot — so the snapshot's baked-in state (and
    # its precompiled code cache) returns, while runtime mutations are dropped.
    # This is the contract warm-compile relies on: reset == back to the snapshot.
    iso = RustyRacer::Isolate.new(snapshot: RustyRacer::Snapshot.new('globalThis.X = 42'))
    ctx = iso.context
    assert_equal 42, ctx.eval('globalThis.X')
    ctx.eval('globalThis.X = 99')
    ctx.reset
    assert_equal 42, ctx.eval('globalThis.X') # snapshot value restored, not 99
  end

  def test_snapshot_bakes_globals_into_a_booted_context
    snap = RustyRacer::Snapshot.new(<<~JS)
      globalThis.GREETING = "from snapshot";
      function double(x) { return x * 2 }
    JS
    assert_operator snap.size, :>, 0

    ctx = RustyRacer::Isolate.new(snapshot: snap).context
    assert_equal "from snapshot", ctx.eval("GREETING")
    assert_equal 42, ctx.eval("double(21)")

    # a context with no snapshot does not have those globals
    assert_equal "undefined", @ctx.eval("typeof GREETING")
  end

  def test_snapshot_dump_and_load_round_trip
    snap = RustyRacer::Snapshot.new('globalThis.V = 7')
    blob = snap.dump
    assert_equal Encoding::ASCII_8BIT, blob.encoding
    reloaded = RustyRacer::Snapshot.load(blob)
    assert_equal snap.size, reloaded.size
    ctx = RustyRacer::Isolate.new(snapshot: reloaded).context
    assert_equal 7, ctx.eval("V")
  end

  def test_snapshot_load_rejects_an_invalid_blob
    # a garbage/empty blob would FATAL-CHECK-abort the process at Isolate.new;
    # load runs V8's is_valid so it raises a rescuable SnapshotError instead.
    assert_raises(RustyRacer::SnapshotError) { RustyRacer::Snapshot.load('') }
    assert_raises(RustyRacer::SnapshotError) { RustyRacer::Snapshot.load("not a v8 snapshot \x00\xff" * 64) }
    # a genuine blob still loads and boots
    blob = RustyRacer::Snapshot.new('globalThis.OK = 1').dump
    assert_equal 1, RustyRacer::Isolate.new(snapshot: RustyRacer::Snapshot.load(blob)).context.eval('OK')
  end

  def test_snapshot_warmup_keeps_code_cache_but_not_heap_state
    # V8's WarmUpSnapshotDataBlob contract: the warmup code runs in a
    # THROWAWAY context (pre-compiling functions into the blob's code cache);
    # its heap mutations do NOT bake into the blob — the cold state does.
    snap = RustyRacer::Snapshot.new('globalThis.A = 1; function double(x) { return x * 2 }')
    snap.warmup!('globalThis.W = 9; double(21);')
    ctx = RustyRacer::Isolate.new(snapshot: snap).context
    assert_equal 1, ctx.eval('A')                  # cold state kept
    assert_equal 42, ctx.eval('double(21)')        # cold functions kept
    assert_equal 'undefined', ctx.eval('typeof W') # warmup heap state NOT baked
  end

  def test_snapshot_with_broken_code_raises
    assert_raises(RustyRacer::SnapshotError) do
      RustyRacer::Snapshot.new("this is not valid js ===")
    end
  end

  def test_create_realm_is_isolated_from_main_and_siblings
    a = @iso.create_context
    b = @iso.create_context
    @ctx.eval("globalThis.x = 'main'")
    a.eval("globalThis.x = 'a'")
    b.eval("globalThis.x = 'b'")
    # each realm has its own globalThis
    assert_equal "main", @ctx.eval("globalThis.x")
    assert_equal "a", a.eval("globalThis.x")
    assert_equal "b", b.eval("globalThis.x")
    # the main realm never saw the realms' globals
    assert_equal "undefined", a.eval("typeof globalThis.notThere")
  end

  def test_module_compiled_per_context_and_evaluates_in_it
    other = @iso.create_context
    m = other.compile_module("globalThis.WHERE = 'other'; export const x = 1;")
    m.instantiate { |_s, _r| nil }
    m.evaluate
    # the module ran in `other`, not the default context
    assert_equal "other", other.eval("globalThis.WHERE")
    assert_equal "undefined", @ctx.eval("typeof globalThis.WHERE")
  end

  def test_cross_context_import_is_rejected_not_aborted
    # a resolve block returning a module from a *different* context must fail
    # cleanly (V8 would CHECK-abort the process otherwise).
    other = @iso.create_context
    dep_elsewhere = other.compile_module("export const x = 1;", filename: "/dep.js")
    app = @ctx.compile_module('import {x} from "./dep.js";', filename: "/app.js")
    assert_raises(RustyRacer::RuntimeError) do
      app.instantiate { |_s, _r| dep_elsewhere } # foreign-context dep
    end
    # the isolate is still usable (no crash)
    assert_equal 2, @ctx.eval("1 + 1")
  end

  def test_realm_call_and_attach
    r = @iso.create_context
    r.eval("function mul(a, b) { return a * b }")
    assert_equal 12, r.call("mul", 3, 4)
    r.attach("rubyAdd", proc { |a, b| a + b })
    assert_equal 30, r.eval("rubyAdd(10, 20)")
    # the host fn lives only in that realm, not the main one
    assert_equal "undefined", @ctx.eval("typeof rubyAdd")
  end

  def test_context_global_reaches_another_realm
    # the embedder's iframe.contentWindow: the frame realm's globalThis,
    # reachable from the parent realm (and vice versa — plain V8
    # cross-context access, no security tokens)
    iso = RustyRacer::Isolate.new(host_namespace: 'NS')
    ctx = iso.context
    frame = iso.create_context
    frame.eval('globalThis.WHO = "frame"')
    assert_equal 'frame', ctx.eval("NS.contextGlobal(#{frame.id}).WHO")
    ctx.eval('globalThis.WHO = "main"')
    assert_equal 'main', frame.eval('NS.contextGlobal(0).WHO')
    ctx.eval("NS.contextGlobal(#{frame.id}).fromParent = 42")
    assert_equal 42, frame.eval('globalThis.fromParent')
  end

  def test_context_global_unknown_id_throws
    iso = RustyRacer::Isolate.new(host_namespace: 'NS')
    e = assert_raises(RustyRacer::RuntimeError) { iso.context.eval('NS.contextGlobal(999)') }
    assert_includes e.message, 'unknown context'
  end

  def test_context_of_attributes_values_to_their_context
    iso = RustyRacer::Isolate.new(host_namespace: 'NS')
    ctx = iso.context
    frame = iso.create_context
    assert_equal 0, ctx.eval('NS.contextOf(function f() {})')
    assert_equal frame.id, frame.eval('NS.contextOf(() => 1)')
    # cross-realm: a function created in the frame, inspected from the parent
    frame.eval('globalThis.frameFn = () => 1')
    assert_equal frame.id, ctx.eval("NS.contextOf(NS.contextGlobal(#{frame.id}).frameFn)")
    # primitives have no creation context
    assert_nil ctx.eval('NS.contextOf(42)')
  end

  def test_context_of_a_reset_away_realm_is_undefined
    # a function captured before reset still carries its old realm-id stamp;
    # contextOf must report it as gone (the id no longer maps back to it), not
    # mis-attribute it to the fresh realm now holding that id
    iso = RustyRacer::Isolate.new(host_namespace: 'NS')
    ctx = iso.context
    frame = iso.create_context
    frame.eval('globalThis.oldFn = () => 1')
    ctx.eval("globalThis.captured = NS.contextGlobal(#{frame.id}).oldFn")
    assert_equal frame.id, ctx.eval('NS.contextOf(captured)')
    frame.reset
    assert_nil ctx.eval('NS.contextOf(captured)') # its realm was reset away
  end

  def test_promise_reject_handler_attributes_rejections_to_context
    iso = RustyRacer::Isolate.new(host_namespace: 'NS')
    ctx = iso.context
    frame = iso.create_context
    ctx.eval(<<~JS)
      globalThis.SEEN = [];
      NS.setPromiseRejectHandler((event, contextId, promise, reason) => {
        SEEN.push([event, contextId, String(reason)]);
      });
    JS
    ctx.eval('Promise.reject(new Error("main boom"))')
    frame.eval('Promise.reject(new Error("frame boom"))')
    seen = ctx.eval('globalThis.SEEN')
    assert_includes seen, [0, 0, 'Error: main boom']
    assert_includes seen, [0, frame.id, 'Error: frame boom']
  end

  def test_promise_reject_handler_reports_late_handler_addition
    # HTML's bookkeeping needs the revocation too: event 1 = a handler was
    # added after the reject (the promise identity links the pair)
    iso = RustyRacer::Isolate.new(host_namespace: 'NS')
    ctx = iso.context
    ctx.eval(<<~JS)
      globalThis.SEEN = [];
      NS.setPromiseRejectHandler((event, contextId, promise) => {
        globalThis.P ??= promise;
        SEEN.push([event, promise === globalThis.P]);
      });
      globalThis.p = Promise.reject(1);
    JS
    ctx.eval('globalThis.p.catch(() => {})')
    assert_equal [[0, true], [1, true]], ctx.eval('globalThis.SEEN')
  end

  def test_promise_reject_handler_cleared_when_its_context_dies
    iso = RustyRacer::Isolate.new(host_namespace: 'NS')
    ctx = iso.context
    ctx.eval('NS.setPromiseRejectHandler(() => { globalThis.BOOM = 1 })')
    ctx.reset
    # the recorder's context is gone; a rejection must simply not notify
    # (and must not crash)
    assert_equal 2, ctx.eval('Promise.reject(1); 1 + 1')
  end

  def test_promise_reject_handler_does_not_swallow_termination
    # the handler fires synchronously mid-script; a watchdog/terminate aimed at
    # the surrounding script must survive the handler's TryCatch (which only
    # exists to swallow the handler's own throws), not be absorbed by it
    iso = RustyRacer::Isolate.new(host_namespace: 'NS')
    ctx = iso.context
    ctx.eval('NS.setPromiseRejectHandler(() => { for(;;){} })')
    assert_raises(RustyRacer::ScriptTerminatedError) do
      ctx.eval('Promise.reject(1); for(;;){}', timeout_ms: 200)
    end
    assert_equal 2, ctx.eval('1 + 1') # isolate still usable
  end

  def test_realm_dispose
    r = @iso.create_context
    assert_equal false, r.disposed?
    assert_equal 5, r.eval("2 + 3")
    r.dispose
    assert_equal true, r.disposed?
    assert_raises(::RuntimeError) { r.eval("1") }
    r.dispose # idempotent
    # the parent context still works after a realm is disposed
    assert_equal 2, @ctx.eval("1 + 1")
  end

  def test_import_meta_url_is_the_module_filename
    m = @ctx.compile_module('globalThis.U = import.meta.url;', filename: '/app.js')
    m.instantiate {|_s, _r| nil }
    m.evaluate
    assert_equal '/app.js', @ctx.eval('globalThis.U')
  end

  def test_import_meta_url_on_dynamically_imported_module
    @iso.dynamic_import_resolver = ->(spec, _ref, _ctx) {
      @ctx.compile_module('globalThis.DU = import.meta.url;', filename: spec)
    }
    t = deadline_thread {
      @ctx.eval('import("/lazy.js");')
    }
    flunk 'deadlocked' unless t.join(10)
    t.value
    @iso.perform_microtask_checkpoint
    assert_equal '/lazy.js', @ctx.eval('globalThis.DU')
  end

  def test_import_meta_url_distinct_per_module
    a = @ctx.compile_module('export const u = import.meta.url;', filename: '/a.js')
    b = @ctx.compile_module('export const u = import.meta.url;', filename: '/b.js')
    [a, b].each {|m| m.instantiate {|_s, _r| nil }; m.evaluate }
    assert_equal '/a.js', a.namespace['u']
    assert_equal '/b.js', b.namespace['u']
  end

  def test_es_module_compile_instantiate_evaluate
    dep = @ctx.compile_module("export const x = 21;", filename: "/dep.js")
    app = @ctx.compile_module(
      'import {x} from "./dep.js"; export const result = x * 2; globalThis.RAN = result;',
      filename: "/app.js"
    )
    # the resolve block maps each import to an already-compiled Module
    app.instantiate do |specifier, referrer_url|
      assert_equal "/app.js", referrer_url
      specifier == "./dep.js" ? dep : nil
    end
    app.evaluate
    assert_equal 42, @ctx.eval("globalThis.RAN")
    # module namespaces expose exports
    assert_equal 42, app.namespace["result"]
    assert_equal 21, dep.namespace["x"]
  end

  def test_es_module_unresolved_import_raises
    app = @ctx.compile_module('import {x} from "./missing.js";', filename: "/app.js")
    assert_raises(RustyRacer::RuntimeError) { app.instantiate { |_spec, _ref| nil } }
  end

  def test_es_module_syntax_error_is_parse_error
    assert_raises(RustyRacer::ParseError) { @ctx.compile_module("import from", filename: "/bad.js") }
  end

  def test_es_module_namespace_before_instantiate_raises_not_aborts
    # guard against V8 CHECK-aborting the process on an un-instantiated module
    m = @ctx.compile_module("export const a = 1;")
    assert_raises(RustyRacer::RuntimeError) { m.namespace }
    assert_raises(RustyRacer::RuntimeError) { m.evaluate }
  end

  def test_es_module_top_level_throw_surfaces
    m = @ctx.compile_module('throw new Error("boom in module");', filename: "/t.js")
    m.instantiate { |_s, _r| nil }
    e = assert_raises(RustyRacer::RuntimeError) { m.evaluate }
    assert_includes e.message, "boom in module"
  end

  def test_es_module_resolver_raise_propagates
    app = @ctx.compile_module('import {x} from "./dep.js";', filename: "/app.js")
    e = assert_raises(ArgumentError) { app.instantiate { |_s, _r| raise ArgumentError, "resolver boom" } }
    assert_includes e.message, "resolver boom"
  end

  def test_es_module_resolver_wrong_type_raises
    app = @ctx.compile_module('import {x} from "./dep.js";', filename: "/app.js")
    assert_raises(TypeError) { app.instantiate { |_s, _r| 42 } }
  end

  def test_instantiate_after_isolate_dispose_raises_not_crashes
    # instantiate reaches the isolate slot via iso_ptr BEFORE the run() guard; a
    # post-dispose instantiate must raise (the module's own flag stays live), not
    # use-after-free the dropped isolate.
    iso = RustyRacer::Isolate.new
    m = iso.context.compile_module('export const x = 1;', filename: '/m.js')
    iso.dispose
    e = assert_raises(::RuntimeError) { m.instantiate { |_s, _r| nil } }
    assert_includes e.message, 'disposed'
  end

  def test_reentrant_instantiate_does_not_clobber_outer_resolver
    # A resolve block that itself issues a (refused) nested instantiate must not
    # wipe the OUTER instantiate's parked resolver, or the outer's remaining
    # import edges would silently fail to resolve.
    dep_a = @ctx.compile_module('export const a = 1;', filename: '/a.js')
    dep_b = @ctx.compile_module('export const b = 2;', filename: '/b.js')
    extra = @ctx.compile_module('export const e = 9;', filename: '/e.js')
    app = @ctx.compile_module(
      'import {a} from "./a.js"; import {b} from "./b.js"; export const r = a + b;',
      filename: '/app.js'
    )
    app.instantiate do |spec, _ref|
      if spec == './a.js'
        # nested instantiate is refused (not re-entrant); rescued, must not
        # clobber the outer op's parked resolver.
        begin
          extra.instantiate { |_s, _r| nil }
        rescue RustyRacer::RuntimeError
          nil
        end
        dep_a
      else
        dep_b
      end
    end
    app.evaluate
    assert_equal 3, app.namespace['r']
  end

  def test_dynamic_import_resolves_to_a_loaded_module
    # explicit mode keeps the import() continuation pending until drained
    iso = RustyRacer::Isolate.new(microtasks: :explicit)
    ctx = iso.context
    dep = ctx.compile_module("export const v = 7;", filename: "/dep.js")
    dep.instantiate { |_s, _r| nil }
    dep.evaluate
    iso.dynamic_import_resolver = ->(specifier, _referrer, _ctx) { specifier == "/dep.js" ? dep : nil }

    ctx.eval(<<~JS, filename: "/main.js")
      globalThis.OUT = null;
      import("/dep.js").then(m => { globalThis.OUT = m.v });
    JS
    assert_nil ctx.eval("globalThis.OUT") # pending until drained (explicit policy)
    iso.perform_microtask_checkpoint
    assert_equal 7, ctx.eval("globalThis.OUT")
  end

  def test_dynamic_import_auto_links_and_evaluates_a_compiled_module
    # V8's host contract: the resolver may return a merely COMPILED module —
    # linking and evaluating are the binding's job
    @iso.dynamic_import_resolver = ->(spec, _ref, _ctx) { @ctx.compile_module('export const v = 7;', filename: spec) }
    t = deadline_thread {
      @ctx.eval('globalThis.OUT = null; import("/m.js").then(m => { globalThis.OUT = m.v }, e => { globalThis.OUT = String(e) });')
    }
    flunk 'dynamic-import auto-link deadlocked' unless t.join(10)
    t.value
    assert_equal 7, @ctx.eval('globalThis.OUT')
  end

  def test_dynamic_import_auto_link_resolves_static_imports_via_the_resolver
    sources = {
      '/app.js' => 'import {x} from "/dep.js"; export const v = x + 1;',
      '/dep.js' => 'export const x = 41;'
    }
    @iso.dynamic_import_resolver = ->(spec, _ref, _ctx) {
      sources[spec] && @ctx.compile_module(sources[spec], filename: spec)
    }
    t = deadline_thread {
      @ctx.eval('globalThis.OUT = null; import("/app.js").then(m => { globalThis.OUT = m.v }, e => { globalThis.OUT = String(e) });')
    }
    flunk 'static-dep auto-link deadlocked' unless t.join(10)
    t.value
    assert_equal 42, @ctx.eval('globalThis.OUT')
  end

  def test_dynamic_import_resolver_receives_the_initiating_realm
    # import() from an extra realm hands the resolver THAT realm's Context (the
    # 3rd arg), so iframe-style imports resolve/compile in their own realm
    # instead of the main one. .id identifies the realm.
    realm = @iso.create_context
    seen = []
    @iso.dynamic_import_resolver = ->(spec, _ref, ctx) {
      seen << ctx.id
      ctx.compile_module('export const v = 7;', filename: spec)
    }
    t = deadline_thread {
      realm.eval('globalThis.OUT = null; import("/m.js").then(m => { globalThis.OUT = m.v }, e => { globalThis.OUT = String(e) });')
    }
    flunk 'dynamic import from an extra realm deadlocked' unless t.join(10)
    t.value
    assert_equal 7, realm.eval('globalThis.OUT')
    assert_equal [realm.id], seen, 'resolver did not receive the initiating realm'
    refute_equal 0, realm.id, 'the extra realm must not be the main realm'
  end

  def test_dynamic_import_evaluation_error_rejects_the_promise
    @iso.dynamic_import_resolver = ->(spec, _ref, _ctx) {
      @ctx.compile_module('throw new Error("module boom");', filename: spec)
    }
    t = deadline_thread {
      @ctx.eval('globalThis.OUT = null; import("/x.js").then(() => { globalThis.OUT = "ok" }, e => { globalThis.OUT = String(e) });')
    }
    flunk 'deadlocked' unless t.join(10)
    t.value
    assert_includes @ctx.eval('globalThis.OUT'), 'module boom'
  end

  def test_dynamic_import_top_level_await_completes
    # the import() promise is settled FROM the evaluation promise, so a
    # top-level await module hands out its namespace only once it finished
    @iso.dynamic_import_resolver = ->(spec, _ref, _ctx) {
      @ctx.compile_module('await Promise.resolve(); export const v = 5;', filename: spec)
    }
    t = deadline_thread {
      @ctx.eval('globalThis.OUT = null; import("/tla.js").then(m => { globalThis.OUT = m.v }, e => { globalThis.OUT = String(e) });')
    }
    flunk 'TLA import deadlocked' unless t.join(10)
    t.value
    assert_equal 5, @ctx.eval('globalThis.OUT')
  end

  def test_dynamic_import_settle_is_immune_to_patched_promise_then
    # the binding settles import() via the native Promise::then builtin, so a
    # user-patched Promise.prototype.then cannot break the link/evaluate of an
    # imported module (the module's own side effects still happen)
    @iso.dynamic_import_resolver = ->(spec, _ref, _ctx) {
      @ctx.compile_module('globalThis.SIDE = "ran"; export const v = 7;', filename: spec)
    }
    @ctx.eval('Promise.prototype.then = function(){ throw new Error("patched") }')
    @ctx.eval('globalThis.SIDE = null; import("/m.js");')
    @iso.perform_microtask_checkpoint
    assert_equal 'ran', @ctx.eval('globalThis.SIDE')
  end

  def test_dynamic_import_evaluation_timeout_terminates_not_swallowed
    # a watchdog/terminate during the imported module's evaluation must escalate
    # to the outer eval (the import callback must not absorb it), not vanish
    iso = RustyRacer::Isolate.new(timeout_ms: 200)
    ctx = iso.context
    iso.dynamic_import_resolver = ->(spec, _ref, _ctx) { ctx.compile_module('for(;;){}', filename: spec) }
    assert_raises(RustyRacer::ScriptTerminatedError) { ctx.eval('import("/spin.js"); 1') }
    assert_equal 2, ctx.eval('1 + 1')
  end

  def test_dynamic_import_resolves_in_the_realm_it_actually_fired_in
    # under kAuto a microtask queued by a frame realm can run import() during
    # the drain at the end of the MAIN realm's eval — the import must resolve
    # against the realm it actually executes in (the frame), not CURRENT_CTX
    # (which still names the main realm)
    iso = RustyRacer::Isolate.new(host_namespace: 'NS')
    ctx = iso.context
    frame = iso.create_context
    iso.dynamic_import_resolver = ->(spec, _ref, _ctx) {
      frame.compile_module('export const v = 99;', filename: spec)
    }
    # queue the frame's import behind a resolver the main realm will trigger
    frame.eval(<<~JS)
      globalThis.GOT = null;
      globalThis.fire = null;
      new Promise(r => { globalThis.fire = r })
        .then(() => import("/m.js"))
        .then(m => { globalThis.GOT = m.v });
    JS
    # fire it from the MAIN realm; the import() then runs in the frame during
    # the main eval's end-of-script drain, with CURRENT_CTX == 0
    ctx.eval("NS.contextGlobal(#{frame.id}).fire()")
    5.times { iso.perform_microtask_checkpoint }
    assert_equal 99, frame.eval('globalThis.GOT')
  end

  def test_dynamic_import_unresolved_static_dep_rejects
    @iso.dynamic_import_resolver = ->(spec, _ref, _ctx) {
      spec == '/app.js' ? @ctx.compile_module('import {x} from "/missing.js";', filename: spec) : nil
    }
    t = deadline_thread {
      @ctx.eval('globalThis.OUT = null; import("/app.js").then(() => { globalThis.OUT = "ok" }, e => { globalThis.OUT = String(e) });')
    }
    flunk 'deadlocked' unless t.join(10)
    t.value
    refute_equal 'ok', @ctx.eval('globalThis.OUT')
    assert_kind_of String, @ctx.eval('globalThis.OUT')
  end

  def test_dynamic_import_without_resolver_rejects
    @ctx.eval('globalThis.ERR = null; import("/x.js").catch(e => { globalThis.ERR = String(e) });')
    @iso.perform_microtask_checkpoint
    assert_match(/import|not|resolved/i, @ctx.eval("globalThis.ERR"))
  end

  def test_module_cached_data_round_trip
    src = "export const x = 1 + 2;"
    # produce a bytecode cache
    m1 = @ctx.compile_module(src, filename: "/m.js", produce_cache: true)
    blob = m1.cached_data
    refute_nil blob
    assert_operator blob.bytesize, :>, 0
    assert_equal Encoding::ASCII_8BIT, blob.encoding

    # consume it in a fresh context: accepted (not rejected), same result
    ctx2 = RustyRacer::Isolate.new.context
    m2 = ctx2.compile_module(src, filename: "/m.js", cached_data: blob)
    assert_equal false, m2.cache_rejected?
    m2.instantiate { |_s, _r| nil }
    m2.evaluate
    assert_equal 3, m2.namespace["x"]
  end

  def test_module_cache_rejected_on_source_mismatch
    blob = @ctx.compile_module("export const x = 1;", produce_cache: true).cached_data
    # a different source invalidates the cache; V8 recompiles and flags rejected
    m = @ctx.compile_module("export const x = 999;", cached_data: blob)
    assert_equal true, m.cache_rejected?
    m.instantiate { |_s, _r| nil }
    m.evaluate
    assert_equal 999, m.namespace["x"] # still correct (recompiled from source)
  end

  def test_module_non_binary_cached_data_raises
    # a cache string that isn't ASCII-8BIT (e.g. read without 'rb') is refused
    assert_raises(EncodingError) do
      @ctx.compile_module("export const x = 1;", cached_data: "not binary".encode("UTF-8"))
    end
  end

  def test_module_without_produce_cache_has_nil_cached_data
    m = @ctx.compile_module("export const x = 1;")
    assert_nil m.cached_data
    assert_equal false, m.cache_rejected?
  end

  def test_module_create_code_cache_captures_inner_functions_after_evaluate
    # As with Script#create_code_cache: evaluate compiles the inner functions
    # that run, so create_code_cache after evaluate carries more than the
    # compile-time top-level-only cache.
    src = <<~JS
      function add(a, b) { return a + b }
      function mul(a, b) { return a * b }
      export const result = (() => { let s = 0; for (let i = 0; i < 5; i++) s = add(s, mul(i, 2)); return s })()
    JS
    m = @ctx.compile_module(src, filename: '/m.js', produce_cache: true)
    m.instantiate {|_s, _r| nil }
    cold = m.create_code_cache
    assert_equal Encoding::ASCII_8BIT, cold.encoding

    m.evaluate
    warm = m.create_code_cache
    assert_operator warm.bytesize, :>, cold.bytesize

    ctx2 = RustyRacer::Isolate.new.context
    m2 = ctx2.compile_module(src, filename: '/m.js', cached_data: warm)
    assert_equal false, m2.cache_rejected?
    m2.instantiate {|_s, _r| nil }
    m2.evaluate
    assert_equal 20, m2.namespace['result']
  end

  def test_module_create_code_cache_raises_after_dispose
    m = @ctx.compile_module('export const x = 1;')
    m.dispose
    assert_raises(::RuntimeError) { m.create_code_cache }
  end

  def test_create_code_cache_on_an_errored_module_is_safe
    # A module that threw at evaluate is :errored, but its compiled top-level
    # script still exists — create_code_cache serializes that rather than
    # aborting the process (get_unbound_module_script is status-independent).
    m = @ctx.compile_module('throw new Error("boom"); export const x = 1;', filename: '/err.js')
    m.instantiate {|_s, _r| nil }
    assert_raises(RustyRacer::RuntimeError) { m.evaluate }
    assert_equal :errored, m.status
    refute_nil m.create_code_cache
  end

  def test_module_status_follows_the_lifecycle
    m = @ctx.compile_module('export const x = 1;')
    assert_equal :uninstantiated, m.status
    m.instantiate {|_s, _r| nil }
    assert_equal :instantiated, m.status
    m.evaluate
    assert_equal :evaluated, m.status
  end

  def test_module_status_errored
    m = @ctx.compile_module('throw new Error("boom");')
    m.instantiate {|_s, _r| nil }
    assert_raises(RustyRacer::RuntimeError) { m.evaluate }
    assert_equal :errored, m.status
  end

  def test_top_level_await_rejection_surfaces_under_auto_drain
    # a TLA module that rejects only after the drain runs its continuation must
    # raise, not silently return nil (the evaluate() promise is pending at the
    # status check and only settles during auto_drain)
    m = @ctx.compile_module('await Promise.resolve(); throw new Error("late TLA failure");')
    m.instantiate {|_s, _r| nil }
    e = assert_raises(RustyRacer::RuntimeError) { m.evaluate }
    assert_includes e.message, 'late TLA failure'
    assert_equal :errored, m.status
  end

  def test_es_module_dispose
    m = @ctx.compile_module("export const a = 1;")
    assert_equal false, m.disposed?
    m.dispose
    assert_equal true, m.disposed?
    assert_raises(::RuntimeError) { m.evaluate }
  end

  def test_call_invokes_global_function
    @ctx.eval("function mul(a, b) { return a * b }")
    assert_equal 6, @ctx.call("mul", 2, 3)
    @ctx.eval('globalThis.greet = (who) => "hi " + who')
    assert_equal "hi bob", @ctx.call("greet", "bob")
  end

  def test_host_function_roundtrip
    @ctx.attach("rubyAdd", proc { |a, b| a + b })
    assert_equal 42, @ctx.eval("rubyAdd(20, 22)")
    assert_equal "ab", @ctx.eval('rubyAdd("a", "b")')
  end

  def test_ruby_exception_in_host_fn_surfaces_and_context_survives
    @ctx.attach("rubyBoom", proc { raise ArgumentError, "no thanks" })
    out = @ctx.eval('(() => { try { rubyBoom(); return "uncaught"; } catch (e) { return "caught:" + String(e).includes("no thanks"); } })()')
    assert_equal "caught:true", out
    # audit #24: the context must not be wedged afterwards
    assert_equal 2, @ctx.eval("1 + 1")
  end

  def test_nested_ruby_js_ruby_js
    @ctx.attach("reenter", proc { @ctx.eval("6 * 7") })
    assert_equal 42, @ctx.eval("reenter()")
  end

  def test_text_string_marshals_to_js_string
    @ctx.eval('function kind(x) { return typeof x }')
    @ctx.eval('function id(x) { return x }')
    # text-tagged Strings are JS strings, and round-trip back as UTF-8 Strings
    assert_equal 'string', @ctx.call('kind', 'café')
    out = @ctx.call('id', 'café')
    assert_equal Encoding::UTF_8, out.encoding
    assert_equal 'café', out
  end

  def test_binary_string_marshals_to_uint8array_and_back
    # the encoding tag is the type: a binary (ASCII-8BIT) String crosses as a
    # JS Uint8Array, and a Uint8Array comes back as a binary String — symmetric
    @ctx.eval('function kind(x) { return x instanceof Uint8Array }')
    @ctx.eval('function len(x) { return x.length }')
    @ctx.eval('function id(x) { return x }')
    bytes = 'café'.b # 5 bytes, high bytes
    assert_equal true, @ctx.call('kind', bytes)
    assert_equal 5, @ctx.call('len', bytes) # JS sees the bytes
    out = @ctx.call('id', bytes)
    assert_equal Encoding::ASCII_8BIT, out.encoding
    assert_equal bytes, out # full round-trip, byte-for-byte
    # arbitrary bytes (not valid UTF-8) survive intact — no U+FFFD, no error
    raw = (0..255).to_a.pack('C*')
    assert_equal raw, @ctx.call('id', raw)
  end

  def test_js_array_buffer_and_views_marshal_to_binary_string
    # a bare ArrayBuffer and any typed-array/DataView view become binary Strings
    assert_equal "\x01\x02\x03\x04".b, @ctx.eval('new Uint8Array([1,2,3,4])')
    assert_equal Encoding::ASCII_8BIT, @ctx.eval('new Uint8Array([1]).buffer').encoding
    # a view copies only its window, not the whole buffer
    assert_equal "\x02\x03".b, @ctx.eval('new Uint8Array([0,1,2,3,4,5]).subarray(2,4)')
    assert_equal "\x00\x00\x80\x3f".b, @ctx.eval('new Uint8Array(new Float32Array([1.0]).buffer)')
    # a bare SharedArrayBuffer too (not just views over it) — must not silently
    # marshal as an empty Hash
    sab = @ctx.eval('const s = new SharedArrayBuffer(4); new Uint8Array(s).set([1,2,3,4]); s')
    assert_equal "\x01\x02\x03\x04".b, sab
    assert_equal Encoding::ASCII_8BIT, sab.encoding
  end

  def test_transfer_out_in_round_trips_a_buffer_across_isolates
    # NS.transferOut hands a buffer's backing store to a global registry and
    # returns a token; NS.transferIn rebuilds an ArrayBuffer over the SAME memory
    # in another isolate (zero copy) — the cross-isolate half of postMessage
    # transferables. The bytes must survive the trip intact.
    src = RustyRacer::Isolate.new(host_namespace: "NS").context
    dst = RustyRacer::Isolate.new(host_namespace: "NS").context
    token = src.eval("NS.transferOut(new Uint8Array([10, 20, 30, 40]).buffer)")
    assert_operator token, :>, 0
    dst.eval("globalThis.__t = #{token.to_i}")
    bytes = dst.eval("Array.from(new Uint8Array(NS.transferIn(__t)))")
    assert_equal [10, 20, 30, 40], bytes
  end

  def test_transfer_out_detaches_the_source
    # transfer semantics: after exporting, the source buffer is neutered
    # (byteLength 0, detached true) — the memory now belongs to the recipient.
    ctx = RustyRacer::Isolate.new(host_namespace: "NS").context
    detached = ctx.eval(<<~JS)
      const buf = new Uint8Array([1, 2, 3, 4]).buffer;
      NS.transferOut(buf);
      buf.byteLength === 0 && buf.detached === true;
    JS
    assert_equal true, detached
  end

  def test_transferred_buffer_survives_source_isolate_dispose
    # the backing store is V8's heap-external, atomic-refcounted allocation, so a
    # token stays importable even after the EXPORTING isolate is fully disposed —
    # the registry's reference keeps the memory alive on its own.
    src = RustyRacer::Isolate.new(host_namespace: "NS")
    dst = RustyRacer::Isolate.new(host_namespace: "NS").context
    token = src.context.eval("NS.transferOut(new Uint8Array([7, 8, 9]).buffer)")
    src.dispose
    GC.start
    dst.eval("globalThis.__t = #{token.to_i}")
    assert_equal [7, 8, 9], dst.eval("Array.from(new Uint8Array(NS.transferIn(__t)))")
  end

  def test_transfer_in_works_from_another_thread
    # csim's window and worker isolates live on different threads, so a token
    # exported on one thread must import on another — exercises the manual Send on
    # the backing-store handle (the registry moves it between owner threads).
    src = RustyRacer::Isolate.new(host_namespace: "NS").context
    token = src.eval("NS.transferOut(new Uint8Array([100, 101]).buffer)")
    bytes = Thread.new {
      dst = RustyRacer::Isolate.new(host_namespace: "NS").context
      dst.eval("globalThis.__t = #{token.to_i}")
      dst.eval("Array.from(new Uint8Array(NS.transferIn(__t)))")
    }.value
    assert_equal [100, 101], bytes
  end

  def test_transfer_out_returns_zero_for_non_transferable
    # a non-ArrayBuffer argument (or a non-detachable buffer) can't be zero-copy
    # transferred; transferOut returns 0 so the caller falls back to a copy.
    ctx = RustyRacer::Isolate.new(host_namespace: "NS").context
    assert_equal true, ctx.eval("NS.transferOut(123) === 0")
    assert_equal true, ctx.eval("NS.transferOut('not a buffer') === 0")
  end

  def test_transfer_in_unknown_token_is_undefined
    # an unknown token (already imported, or a dropped message) yields undefined
    # rather than throwing, so the recipient can detect a lost transfer.
    ctx = RustyRacer::Isolate.new(host_namespace: "NS").context
    assert_equal true, ctx.eval("NS.transferIn(999999) === undefined")
  end

  def test_transfer_in_rejects_non_integer_tokens_instead_of_truncating
    # a fractional/NaN/negative token must NOT truncate-and-collide with a live
    # token (1.9 -> 1 would steal token 1's buffer); it reads as unknown.
    src = RustyRacer::Isolate.new(host_namespace: "NS").context
    dst = RustyRacer::Isolate.new(host_namespace: "NS").context
    token = src.eval("NS.transferOut(new Uint8Array([1, 2, 3]).buffer)")
    dst.eval("globalThis.__t = #{token.to_i}")
    # a fractional neighbour of the real token imports nothing and leaves the
    # real token intact
    assert_equal true, dst.eval("NS.transferIn(__t + 0.5) === undefined")
    assert_equal true, dst.eval("NS.transferIn(NaN) === undefined")
    assert_equal true, dst.eval("NS.transferIn(-1) === undefined")
    # the genuine integer token still works afterward
    assert_equal [1, 2, 3], dst.eval("Array.from(new Uint8Array(NS.transferIn(__t)))")
  end

  def test_transfer_out_returns_zero_for_shared_array_buffer
    # a SharedArrayBuffer is shared, not transferred — transferOut declines it
    # (returns 0) rather than detaching it.
    ctx = RustyRacer::Isolate.new(host_namespace: "NS").context
    assert_equal true, ctx.eval("NS.transferOut(new SharedArrayBuffer(8)) === 0")
    assert_equal true, ctx.eval("NS.transferOut(new Uint8Array(new SharedArrayBuffer(8))) === 0")
  end

  def test_transfer_drop_and_pending_transfer_count_release_memory
    # an exported-but-never-imported buffer pins its memory; transferDrop releases
    # it without importing, and pending_transfer_count makes the leak observable.
    ctx = RustyRacer::Isolate.new(host_namespace: "NS").context
    before = RustyRacer.pending_transfer_count
    ctx.eval("globalThis.__tok = NS.transferOut(new Uint8Array([1, 2]).buffer)")
    assert_equal before + 1, RustyRacer.pending_transfer_count
    ctx.eval("NS.transferDrop(__tok)")
    assert_equal before, RustyRacer.pending_transfer_count
  end

  def test_transfer_round_trip_survives_gc_stress_with_a_large_buffer
    # a multi-megabyte buffer round-trips byte-for-byte even under GC.stress —
    # guards the backing-store lifetime across the export/dispose-pressure/import
    # window.
    src = RustyRacer::Isolate.new(host_namespace: "NS").context
    dst = RustyRacer::Isolate.new(host_namespace: "NS").context
    GC.stress = true
    begin
      token = src.eval("const u = new Uint8Array(4 * 1024 * 1024); u[0] = 1; u[u.length - 1] = 255; NS.transferOut(u.buffer)")
      dst.eval("globalThis.__t = #{token.to_i}")
      ok = dst.eval(<<~JS)
        const v = new Uint8Array(NS.transferIn(__t));
        v.length === 4 * 1024 * 1024 && v[0] === 1 && v[v.length - 1] === 255;
      JS
      assert_equal true, ok
    ensure
      GC.stress = false
    end
  end

  def test_share_out_in_shares_memory_across_isolates
    # a SharedArrayBuffer is *shared*, not transferred: shareOut keeps the source
    # live and shareIn rebuilds a SharedArrayBuffer over the SAME memory, so a
    # write in one isolate is visible in the other (and Atomics work across them).
    src = RustyRacer::Isolate.new(host_namespace: "NS").context
    dst = RustyRacer::Isolate.new(host_namespace: "NS").context
    token = src.eval("globalThis.s = new SharedArrayBuffer(8); new Uint8Array(s).set([1, 2, 3, 4]); NS.shareOut(s)")
    assert_operator token, :>, 0
    # source is NOT detached (unlike transferOut)
    assert_equal 8, src.eval("s.byteLength")
    dst.eval("globalThis.shared = NS.shareIn(#{token.to_i})")
    assert_equal true, dst.eval("shared instanceof SharedArrayBuffer")
    assert_equal [1, 2, 3, 4], dst.eval("Array.from(new Uint8Array(shared, 0, 4))")
    # write in dst, read in src — proves it is the SAME backing memory
    dst.eval("new Uint8Array(shared)[0] = 99; Atomics.store(new Int32Array(shared), 1, 7)")
    assert_equal 99, src.eval("new Uint8Array(s)[0]")
    assert_equal 7, src.eval("Atomics.load(new Int32Array(s), 1)")
  end

  def test_share_works_across_threads_with_atomics
    # csim's window and worker isolates share a SAB across threads; a worker-thread
    # Atomics write must be visible to the main thread through the shared memory.
    main = RustyRacer::Isolate.new(host_namespace: "NS").context
    token = main.eval("globalThis.s = new SharedArrayBuffer(16); NS.shareOut(s)")
    Thread.new {
      w = RustyRacer::Isolate.new(host_namespace: "NS").context
      w.eval("const ia = new Int32Array(NS.shareIn(#{token.to_i})); Atomics.store(ia, 0, 42); Atomics.store(ia, 1, 1234)")
    }.join
    assert_equal [42, 1234], main.eval("[Atomics.load(new Int32Array(s), 0), Atomics.load(new Int32Array(s), 1)]")
  end

  def test_share_out_returns_zero_for_non_shared_array_buffer
    # shareOut only accepts a SharedArrayBuffer; a plain ArrayBuffer (which would
    # go through transferOut) or a non-buffer returns 0.
    ctx = RustyRacer::Isolate.new(host_namespace: "NS").context
    assert_equal true, ctx.eval("NS.shareOut(new ArrayBuffer(4)) === 0")
    assert_equal true, ctx.eval("NS.shareOut(42) === 0")
  end

  def test_transfer_and_share_tokens_do_not_cross
    # a token's kind is enforced: transferIn refuses a share token and shareIn
    # refuses a transfer token (rebuilding the wrong buffer type over the memory),
    # and the refused token stays parked (not consumed).
    src = RustyRacer::Isolate.new(host_namespace: "NS").context
    dst = RustyRacer::Isolate.new(host_namespace: "NS").context
    share_tok = src.eval("NS.shareOut(new SharedArrayBuffer(8))")
    xfer_tok  = src.eval("NS.transferOut(new Uint8Array([1, 2]).buffer)")
    assert_equal true, dst.eval("NS.transferIn(#{share_tok.to_i}) === undefined")
    assert_equal true, dst.eval("NS.shareIn(#{xfer_tok.to_i}) === undefined")
    # the correctly-typed imports still work afterward (tokens were not consumed)
    assert_equal true, dst.eval("NS.shareIn(#{share_tok.to_i}) instanceof SharedArrayBuffer")
    assert_equal [1, 2], dst.eval("Array.from(new Uint8Array(NS.transferIn(#{xfer_tok.to_i})))")
  end

  def test_binary_symbol_value_raises_curated_encoding_error
    # a binary-encoded Symbol value can't become a JS string; the error is the
    # binding's curated EncodingError, not a raw magnus "expected utf-8" message
    @ctx.eval('function id(x) { return x }')
    sym = "\xFF\xFE".b.force_encoding('ASCII-8BIT').to_sym
    e = assert_raises(EncodingError) { @ctx.call('id', sym) }
    assert_includes e.message, 'not valid UTF-8'
  end

  def test_reset_during_microtask_checkpoint_is_refused
    # a microtask (any realm) may be live on the V8 stack during a checkpoint;
    # resetting/disposing a realm then would corrupt it, so it's refused
    @ctx.attach('killer', proc {
      begin
        @ctx.reset
        'reset-succeeded'
      rescue RustyRacer::RuntimeError => e
        "refused:#{e.message.include?('checkpoint')}"
      end
    })
    @ctx.eval('globalThis.OUT = null; Promise.resolve().then(() => { globalThis.OUT = killer() });')
    @iso.perform_microtask_checkpoint
    assert_equal 'refused:true', @ctx.eval('globalThis.OUT')
    assert_equal 2, @ctx.eval('1 + 1') # isolate still usable
  end

  def test_evaluate_already_errored_module_reports_its_error_under_tight_timeout
    # an already-errored module's re-evaluate runs no JS; even with a tiny
    # isolate timeout it must report the module's real error, not a spurious
    # ScriptTerminatedError (ran_js is false for the no-JS status arms)
    iso = RustyRacer::Isolate.new(timeout_ms: 1)
    ctx = iso.context
    m = ctx.compile_module('throw new Error("real module error");')
    m.instantiate {|_s, _r| nil }
    assert_raises(RustyRacer::RuntimeError) { m.evaluate } # first evaluate errors it
    e = assert_raises(RustyRacer::RuntimeError) { m.evaluate } # re-evaluate: no JS
    refute_kind_of RustyRacer::ScriptTerminatedError, e
    assert_includes e.message, 'real module error'
  end

  def test_shared_binary_keeps_one_identity
    # an aliased binary blob must stay ONE object across the boundary, not be
    # duplicated (like shared Arrays/Hashes) — both directions
    @ctx.eval('function sameRef(a) { return a[0] === a[1] }')
    bin = 'payload'.b
    assert_equal true, @ctx.call('sameRef', [bin, bin]) # Ruby -> JS: one Uint8Array
    # JS -> Ruby: one binary String for an aliased Uint8Array
    pair = @ctx.eval('const u = new Uint8Array([1,2,3]); [u, u]')
    assert_same pair[0], pair[1]
    pair[0] << 9 # mutating one is visible through the other (same object)
    assert_equal pair[0], pair[1]
  end

  def test_mistagged_text_as_binary_surfaces_as_uint8array
    # the loud-failure property: a text string mis-tagged binary becomes a
    # Uint8Array (so the mis-tag is detectable, not silently coerced)
    @ctx.eval('function kind(x) { return x instanceof Uint8Array }')
    assert_equal true, @ctx.call('kind', 'plain text'.b)
  end

  def test_binary_tagged_hash_key_marshals
    @ctx.eval('function keys(h) { return Object.keys(h) }')
    assert_equal ['café'], @ctx.call('keys', {'café'.b => 1})
  end

  def test_bare_symbol_marshals_to_js_string
    # symbols already crossed as hash KEYS; bare values (args, array elements,
    # hash values) get the same one-way Symbol -> String treatment
    @ctx.eval('function id(x) { return x }')
    assert_equal 'click', @ctx.call('id', :click)
    assert_equal %w[a b], @ctx.call('id', [:a, :b])
    assert_equal({'k' => 'v'}, @ctx.call('id', {k: :v}))
    @ctx.eval('function kind(x) { return typeof x }')
    assert_equal 'string', @ctx.call('kind', :sym)
  end

  def test_to_str_string_like_marshals_by_tag
    # an object delegating via to_str gets the same tag-driven treatment as
    # the String it wraps
    stringlike = Class.new {
      def initialize(s) = @s = s
      def to_str = @s
    }
    @ctx.eval('function id(x) { return x }')
    @ctx.eval('function kind(x) { return x instanceof Uint8Array }')
    # text-tagged -> JS string
    assert_equal 'café', @ctx.call('id', stringlike.new('café'))
    # binary-tagged -> Uint8Array -> binary String
    assert_equal true, @ctx.call('kind', stringlike.new('café'.b))
    assert_equal 'xy'.b, @ctx.call('id', stringlike.new('xy'.b))
  end

  def test_non_utf8_text_transcodes_or_raises
    @ctx.eval('function id(x) { return x }')
    # a non-UTF-8 TEXT encoding is transcoded to UTF-8 on the way to JS
    assert_equal 'あ', @ctx.call('id', 'あ'.encode('Shift_JIS'))
    # bytes unmappable in their declared text encoding raise loudly
    assert_raises(EncodingError) do
      @ctx.call('id', "\x82".dup.force_encoding('Shift_JIS')) # lone SJIS lead byte
    end
    # invalid bytes in a UTF-8-TAGGED String also raise, not silently U+FFFD
    assert_raises(EncodingError) do
      @ctx.call('id', "\xC3\x28".dup.force_encoding('UTF-8')) # invalid UTF-8
    end
  end

  def test_hash_key_with_broken_to_s_raises_not_empty_string
    # a to_s returning a non-String must stay a loud error: silently mapping
    # the key to "" would collide/clobber other keys
    weird = Class.new { def to_s = 42 }
    @ctx.eval('function id(x) { return x }')
    assert_raises(TypeError) { @ctx.call('id', {weird.new => 1}) }
  end

  def test_reset_releases_attached_proc_roots
    ref = capture_released_by { |_iso, ctx| ctx.reset }
    GC.start
    GC.start
    refute ref.weakref_alive?, 'reset left the attached proc (and its captures) GC-rooted'
  end

  def test_dispose_releases_attached_proc_roots
    ref = capture_released_by { |iso, _ctx| iso.dispose }
    GC.start
    GC.start
    refute ref.weakref_alive?, 'dispose left the attached proc (and its captures) GC-rooted'
  end

  def test_deep_caller_js_overflow_throws_not_fatal
    # In-thread V8 runs on the Ruby thread's stack, so the V8 stack limit must be
    # reset to the current native stack each op. If it stayed fixed at a shallow
    # first entry, entering from a deep Ruby caller false-overflows, and that bad
    # throw trips V8's IsOnCentralStack CHECK -> a FATAL abort (the whole process
    # dies). With the per-op reset, a JS overflow from a deep caller is a clean
    # RangeError and the isolate keeps working.
    @ctx.eval('globalThis.rec = function (n) { return n <= 0 ? 0 : 1 + rec(n - 1) }')
    @ctx.eval('1 + 1') # establish a limit at a shallow frame first
    deep = ->(n, &b) { n.zero? ? b.call : deep.call(n - 1, &b) }
    e = deep.call(1500) { assert_raises(RustyRacer::RuntimeError) { @ctx.eval('rec(10_000_000)') } }
    assert_includes e.message, 'call stack'
    assert_equal 2, @ctx.eval('1 + 1')
  end

  def test_eval_inside_a_fiber_does_not_crash
    # A Ruby Fiber runs on the same native thread but a SEPARATE (mmap'd) stack,
    # which pthread can't see. If the V8 limit were anchored to the native stack
    # bottom (above the fiber stack), V8 would false-overflow and the throw would
    # trip a FATAL IsOnCentralStack CHECK. Capybara::Result is an Enumerator (=
    # Fiber), so `find` always evals inside one. A trivial eval and a deep JS
    # overflow must both behave cleanly on a fiber.
    @ctx.eval('globalThis.rec = function (n) { return n <= 0 ? 0 : 1 + rec(n - 1) }')
    assert_equal 2, Fiber.new { @ctx.eval('1 + 1') }.resume
    assert_equal 6, Fiber.new { @ctx.eval('[1,2,3].reduce((a, b) => a + b, 0)') }.resume
    err = Fiber.new do
      @ctx.eval('rec(10_000_000)')
      nil
    rescue RustyRacer::RuntimeError => e
      e
    end.resume
    assert_includes err.message, 'call stack'
    # an Enumerator (the Capybara::Result shape) also evaluates lazily in a fiber
    enum = Enumerator.new { |y| y << @ctx.eval('40 + 2') }
    assert_equal 42, enum.next
    assert_equal 2, @ctx.eval('1 + 1') # isolate still works on the main stack
  end

  def test_fiber_eval_survives_garbage_collection
    # V8's conservative GC walks the C stack from a marker up to its recorded
    # stack_start; on a fiber that start is still the NATIVE thread top (a
    # different mmap region), so a full GC during a fiber op would run the scan
    # off the fiber's mapped stack and SEGV. The runner re-points the scan start
    # to a live address above the op's V8 frames; allocate hard inside a fiber to
    # trigger the scanning GC and prove it stays mapped. (Avo's Capybara filter
    # chain hit this.)
    out = Fiber.new do
      last = nil
      2_000.times do
        last = @ctx.eval('(function () { let a = []; for (let i = 0; i < 1000; i++) a.push({ k: i, v: [i, i + 1] }); return a.length })()')
      end
      last
    end.resume
    assert_equal 1000, out
    assert_equal 2, @ctx.eval('1 + 1')
  end

  def test_fiber_js_headroom_follows_the_fibers_real_stack_size
    # Proves the limit is derived from the fiber's ACTUAL bounds and not from a
    # fixed budget below the SP — the fallback taken when the platform can't look
    # a region up. Absolute depths differ per platform and architecture, so assert
    # the RELATIONSHIP instead: quadruple the fiber stack and the reachable JS
    # depth has to grow with it. A fixed budget would return the same depth for
    # both. Ruby reads the size at boot, hence the subprocesses.
    small = fiber_js_depth(512 * 1024)
    large = fiber_js_depth(4 * 1024 * 1024)
    assert_operator large, :>, small * 3,
                    "JS headroom on a fiber is not tracking its stack size " \
                    "(512KB -> #{small}, 4MB -> #{large}); the region lookup is " \
                    'not in effect and the fixed fallback budget is being used'
  end

  def test_alternating_fibers_each_get_their_own_stack_bounds
    # Stack bounds are cached per thread, a few regions at a time so that ops
    # alternating between fibers don't look them up every time. Each fiber must
    # still come back with ITS OWN bottom: handing one fiber another's (lower)
    # bottom would tell V8 it has far more headroom than the fiber really has,
    # and the recursion below would run through the guard page instead of
    # throwing. Run more fibers than FIBER_REGION_SLOTS (16, in stack.rs) so
    # entries are evicted and re-queried rather than all sitting resident — raise
    # that constant past this count and the test quietly stops covering eviction.
    #
    # probe() reports how deep it got before overflowing, which catches BOTH ways
    # the bounds can be wrong. Too low a bottom (another fiber's) hands V8 far
    # more headroom than the stack has and the recursion leaves the mapping — a
    # SEGV, so the process dies rather than the assertion failing. Too high a
    # bottom clamps the limit just under the SP and starves the fiber instead,
    # which is silent unless the depth is checked. Compare against a lone fiber
    # measured here rather than an absolute floor: fibers differ in size across
    # platforms, but every fiber in one process should reach the same order of
    # depth as any other.
    @ctx.eval('globalThis.probe = function () { let d = 0; function r() { d++; r() } try { r() } catch (e) { return d } }')
    alone = Fiber.new { @ctx.eval('probe()') }.resume
    fibers = Array.new(20) {
      Fiber.new {
        loop do
          Fiber.yield(@ctx.eval('probe()'))
        end
      }
    }
    2.times do
      fibers.each do |f|
        assert_operator f.resume, :>, alone / 2
      end
    end
    assert_equal 2, @ctx.eval('1 + 1')
  end

  def test_host_callback_that_evals_inside_a_fiber
    # The stack description V8 holds lives in the ISOLATE, not in the op, so a
    # nested op that switches stacks needs its own. Here a host fn resumes a
    # Fiber and evals again from inside it (Avo reaches this through Rails while
    # a rack-fetch host call is on the V8 stack). Under the ENCLOSING op's
    # native-stack limit the nested entry is instantly a false overflow — and
    # since V8 derives its central-stack window from that same limit, the
    # RangeError it then tries to throw fails a release CHECK and V8_Fatals the
    # whole process. With a per-op description the nested throw is an ordinary,
    # catchable error.
    @ctx.attach('hop', proc {
      Fiber.new {
        begin
          @ctx.eval('throw new Error("boom")')
          nil
        rescue RustyRacer::RuntimeError => e
          e.message
        end
      }.resume
    })
    @ctx.eval('globalThis.outer = function () { return globalThis.hop() }')
    assert_includes @ctx.call('outer'), 'boom'
    assert_equal 2, @ctx.eval('1 + 1') # isolate still works on the native stack
  end

  def test_host_callback_fiber_eval_overflows_cleanly
    # Same path, but the nested op overflows for real. A fiber stack is far
    # smaller than the native one, so the limit has to come from the FIBER's own
    # mapping — otherwise V8 either never notices (and grows through the guard
    # page) or false-overflows. A clean RangeError, not a SEGV or an abort.
    @ctx.eval('globalThis.rec = function (n) { return n <= 0 ? 0 : 1 + rec(n - 1) }')
    @ctx.attach('hop', proc {
      Fiber.new {
        begin
          @ctx.eval('rec(10_000_000)')
          nil
        rescue RustyRacer::RuntimeError => e
          e.message
        end
      }.resume
    })
    @ctx.eval('globalThis.outer = function () { return globalThis.hop() }')
    assert_includes @ctx.call('outer'), 'call stack'
    assert_equal 2, @ctx.eval('1 + 1')
  end

  def test_nested_fiber_eval_restores_the_enclosing_ops_stack_limit
    # The nested op installs the FIBER's limit, which sits far below the native
    # stack it was called from. If that stayed installed once the fiber returned,
    # V8 would never see the native stack's bottom coming and would recurse
    # straight through the guard page (SEGV) instead of throwing — so the scope
    # has to put the enclosing op's limit back. The recursion below runs on the
    # native stack, in the SAME op that hopped through the fiber.
    @ctx.eval('globalThis.rec = function (n) { return n <= 0 ? 0 : 1 + rec(n - 1) }')
    @ctx.attach('hop', proc { Fiber.new { @ctx.eval('1 + 1') }.resume })
    @ctx.eval(<<~JS)
      globalThis.outer = function () {
        globalThis.hop();
        try { rec(10_000_000) } catch (e) { return e.constructor.name }
        return 'no overflow';
      }
    JS
    assert_equal 'RangeError', @ctx.call('outer')
    assert_equal 2, @ctx.eval('1 + 1')
  end

  def test_host_callback_fiber_eval_survives_garbage_collection
    # A GC during the nested op walks [marker, scan start), so the scan start has
    # to follow the op onto the fiber or the walk runs off the fiber's mapped top
    # into unmapped memory. Allocate hard inside the nested eval to force one,
    # with the enclosing op's frames still live on the native stack.
    @ctx.attach('hop', proc {
      Fiber.new {
        last = nil
        500.times do
          last = @ctx.eval('(function () { let a = []; for (let i = 0; i < 1000; i++) a.push({ k: i, v: [i, i + 1] }); return a.length })()')
        end
        last
      }.resume
    })
    @ctx.eval('globalThis.outer = function () { return globalThis.hop() }')
    assert_equal 1000, @ctx.call('outer')
    assert_equal 2, @ctx.eval('1 + 1')
  end

  def test_cross_isolate_reentry_from_a_fiber_keeps_the_outer_scan_start
    # A -> B -> A while A's op runs on a Fiber. The hop back into A finds a
    # FOREIGN isolate current, so it enters A — and Isolate::Enter re-points A's
    # conservative-GC-scan start at the NATIVE stack top. The nested op has to
    # remember the start from BEFORE it entered; putting back the one it finds
    # afterwards leaves the outer fiber op describing the wrong stack, and the
    # next scanning GC there walks off the fiber's mapped top:
    #   [BUG] Segmentation fault ... IteratePointersInStack
    # The GC has to land inside the OUTER op (any later op reinstalls the start
    # and hides it), hence deep JS frames and sustained allocation after the hop.
    a = RustyRacer::Isolate.new.context
    b = RustyRacer::Isolate.new.context
    b.attach('reenterA', proc { a.eval('1 + 2') })
    b.eval('function callA() { return reenterA() }')
    a.attach('intoB', proc { b.eval('callA()') })
    a.eval(<<~JS)
      globalThis.deep = function (n) {
        if (n > 0) return deep(n - 1);
        let keep = [];
        for (let i = 0; i < 500; i++) {
          const x = new Array(2000);
          for (let j = 0; j < 2000; j++) x[j] = {j, s: 'value' + j, t: [j, j + 1]};
          keep.push(x);
          if (keep.length > 3) keep.shift();
        }
        return keep.length;
      }
    JS
    12.times { assert_equal 3, Fiber.new { a.eval('intoB(); deep(40)') }.resume }
    assert_equal 4, a.eval('2 + 2') # A wasn't left describing the wrong stack
  end

  def test_nested_op_on_the_same_fiber_runs_under_gc_pressure
    # A host callback that evals again WITHOUT switching stacks stays on the one
    # fiber, so the nested op keeps the enclosing op's scan start rather than
    # narrowing it to its own frame (which would leave the enclosing frames,
    # sitting above it, outside [marker, start) for the duration). Allocate hard
    # in the nested op so the scan runs with both ops live on the same fiber.
    @ctx.attach('inner', proc {
      @ctx.eval('(function () { let s = 0; for (let i = 0; i < 1500; i++) { const x = []; for (let j = 0; j < 500; j++) x.push({j}); s += x.length } return s })()')
    })
    @ctx.eval('globalThis.outer = function () { return globalThis.inner() }')
    assert_equal 750_000, Fiber.new { @ctx.call('outer') }.resume
    assert_equal 2, @ctx.eval('1 + 1')
  end

  def test_isolate_is_thread_confined
    # Every op must run on the isolate's owner thread; a foreign-thread op raises
    # WrongThreadError rather than corrupting V8.
    e = Thread.new { (@ctx.eval('1 + 1') rescue $!) }.value # rubocop:disable Style/RescueModifier
    assert_instance_of RustyRacer::WrongThreadError, e
    assert_kind_of RustyRacer::Error, e
    # The owner thread still works (the failed foreign op didn't wedge it).
    assert_equal 2, @ctx.eval('1 + 1')
  end

  def test_terminate_is_allowed_from_another_thread
    # terminate is the lone thread-safe op: a watchdog-less infinite loop on the
    # owner thread is stopped by another thread calling #terminate.
    stopper = Thread.new { sleep 0.1; @iso.terminate }
    assert_raises(RustyRacer::ScriptTerminatedError) { @ctx.eval('for(;;){}') }
    stopper.join
    @ctx.eval('1') # cancel the leftover terminate
  end

  def test_live_isolate_count_tracks_create_and_dispose
    base = RustyRacer.live_isolate_count
    iso = RustyRacer::Isolate.new
    iso.context.eval('1')
    assert_equal base + 1, RustyRacer.live_isolate_count
    iso.dispose
    assert_equal base, RustyRacer.live_isolate_count
    assert_kind_of Integer, RustyRacer.leaked_isolate_count
  end

  def test_attached_proc_survives_gc
    # the attached lambdas below have NO other Ruby reference: the extension
    # itself must root them (rb_gc_register_address via RootedProc) or GC
    # collects them mid-suite and call_proc SEGVs
    @ctx.attach('churn', ->(i) { GC.start if (i % 25).zero?; 'x' * 100 + i.to_s })
    @ctx.attach('nestedOp', ->(s) {
      scr = @ctx.compile("1+#{s.to_i}", filename: 'n.js')
      v = scr.run
      scr.dispose
      v
    })
    total = @ctx.eval(<<~JS, timeout_ms: 30_000)
      let a = 0;
      for (let i = 0; i < 500; i++) {
        a += churn(i).length;
        if (i % 100 === 0) a += nestedOp(String(i));
      }
      a;
    JS
    assert_kind_of Integer, total
  end

  def test_attached_proc_survives_gc_compact
    skip 'GC.compact unavailable' unless GC.respond_to?(:compact)
    # rooting pins the proc, so compaction cannot move it out from under the
    # raw VALUE copies the extension holds
    @ctx.attach('f', -> { 'alive' })
    assert_equal 'alive', @ctx.eval('f()')
    GC.compact
    assert_equal 'alive', @ctx.eval('f()')
  end

  def test_dynamic_import_resolver_survives_gc_compact
    skip 'GC.compact unavailable' unless GC.respond_to?(:compact)
    dep = @ctx.compile_module('export const v = 7;', filename: '/dep.js')
    dep.instantiate {|_s, _r| nil }
    dep.evaluate
    @iso.dynamic_import_resolver = ->(specifier, _referrer, _ctx) { specifier == '/dep.js' ? dep : nil }
    GC.compact
    @ctx.eval('globalThis.OUT = null; import("/dep.js").then(m => { globalThis.OUT = m.v });')
    @iso.perform_microtask_checkpoint
    assert_equal 7, @ctx.eval('globalThis.OUT')
  end

  # --- re-entrancy matrix: while a host proc runs, the V8 thread is parked in
  # that callback awaiting the answer and is NOT reading the main queue, so
  # EVERY op issued from inside the proc must be serviced by the suspended
  # frame (service_request) — anything less deadlocks the rendezvous. Each op
  # runs on a worker thread with a deadline so a regression fails fast instead
  # of hanging the suite (the wedged isolate is abandoned).

  def test_nested_compile_and_run_inside_host_fn
    @ctx.attach('f', proc { @ctx.compile('6 * 7', filename: '/n.js').run })
    assert_equal 42, call_with_deadline(@ctx, 'f')
  end

  def test_nested_module_pipeline_inside_host_fn
    @ctx.attach('f', proc {
      m = @ctx.compile_module('export const x = 41; globalThis.MX = x + 1;', filename: '/m.js')
      m.instantiate {|_s, _r| nil }
      m.evaluate
      @ctx.eval('globalThis.MX')
    })
    assert_equal 42, call_with_deadline(@ctx, 'f')
  end

  def test_nested_microtask_checkpoint_inside_host_fn
    @ctx.attach('f', proc { @iso.perform_microtask_checkpoint; 1 })
    assert_equal 1, call_with_deadline(@ctx, 'f')
  end

  def test_nested_microtask_checkpoint_drains_queue_mid_script
    # csim's __csim_yield: a host fn checkpoints BETWEEN listeners, with JS
    # still on the stack — the queued microtasks must actually run, not no-op
    @ctx.attach('yieldNow', proc { @iso.perform_microtask_checkpoint; @ctx.eval('globalThis.SEEN') })
    @ctx.eval('globalThis.SEEN = null; function g() { Promise.resolve().then(() => { globalThis.SEEN = "micro" }); return yieldNow(); }')
    assert_equal 'micro', call_with_deadline(@ctx, 'g')
  end

  def test_nested_create_context_inside_host_fn
    @ctx.attach('f', proc { @iso.create_context.eval('20 + 22') })
    assert_equal 42, call_with_deadline(@ctx, 'f')
  end

  def test_nested_attach_inside_host_fn
    @ctx.attach('f', proc {
      @ctx.attach('g', proc { 42 })
      @ctx.eval('g()')
    })
    assert_equal 42, call_with_deadline(@ctx, 'f')
  end

  def test_nested_cross_realm_eval_inside_host_fn
    # a nested op carries its own context id, so a host fn on the main realm
    # can evaluate in ANOTHER realm (previously it silently ran in the
    # suspended frame's realm)
    realm = @iso.create_context
    realm.eval('globalThis.WHO = "realm"')
    @ctx.attach('f', proc { realm.eval('globalThis.WHO') })
    assert_equal 'realm', call_with_deadline(@ctx, 'f')
  end

  def test_nested_eval_timeout_terminates_and_escalates
    # nested ops get the watchdog too: a runaway nested eval times out instead
    # of wedging. V8's terminate flag is isolate-global and a nested frame
    # never cancels it (that could erase an Isolate#terminate aimed at the
    # suspended outer JS), so the termination ESCALATES: the proc sees the
    # nested ScriptTerminatedError, and the outer call is terminated as it
    # resumes. The isolate stays usable afterwards.
    caught = false
    @ctx.attach('f', proc {
      begin
        @ctx.eval('for(;;){}', timeout_ms: 100)
        'no-timeout'
      rescue RustyRacer::ScriptTerminatedError
        caught = true
        'terminated'
      end
    })
    t = deadline_thread { @ctx.call('f') }
    assert_raises(RustyRacer::ScriptTerminatedError) {
      flunk 'nested timeout deadlocked' unless t.join(10)
      t.value
    }
    assert caught, 'the proc did not observe the nested termination'
    assert_equal 2, @ctx.eval('1 + 1')
  end

  def test_outer_timeout_survives_a_nested_op
    # The watchdog tracks every armed op's deadline independently (a LIFO stack),
    # not one shared slot. A nested op (host fn -> nested eval) arms and disarms
    # the watchdog while the OUTER op is suspended; that must NOT clear the
    # outer op's own deadline. With a single slot the nested disarm would leave
    # the outer `for(;;){}` below unwatched and it would run unbounded.
    @ctx.attach('quick', proc { @ctx.eval('1 + 1', timeout_ms: 100) })
    t = deadline_thread { @ctx.eval('quick(); for(;;){}', timeout_ms: 200) }
    assert_raises(RustyRacer::ScriptTerminatedError) {
      flunk 'outer timeout was lost after the nested op returned' unless t.join(10)
      t.value
    }
    assert_equal 2, @ctx.eval('1 + 1')
  end

  def test_terminate_during_host_proc_is_not_erased_by_nested_watchdog
    # an Isolate#terminate aimed at the suspended outer JS must survive a
    # nested watchdog firing in the same window (a nested cancel of the
    # isolate-global flag would let the outer loop run unbounded)
    @ctx.attach('slow', proc { sleep 0.3; 1 })
    @ctx.attach('f', proc {
      begin
        # nested watchdog fires at ~100ms while the nested JS is parked in
        # `slow`; the outer terminate lands at ~200ms, before `slow` returns
        @ctx.eval('slow(); for(;;){}', timeout_ms: 100)
      rescue RustyRacer::ScriptTerminatedError
        # expected
      end
      1
    })
    stopper = Thread.new { sleep 0.2; @iso.terminate }
    t = deadline_thread { @ctx.eval('f(); for(;;){}') }
    assert_raises(RustyRacer::ScriptTerminatedError) {
      flunk 'outer loop ran unbounded (the terminate was erased)' unless t.join(10)
      t.value
    }
    stopper.join
    assert_equal 2, @ctx.eval('1 + 1')
  end

  def test_nested_op_targets_its_own_isolate
    # a proc of isolate A calling into isolate B must reach B's V8 thread (B
    # is idle), not be misrouted into A's suspended frame and run in A's realm
    iso_b = RustyRacer::Isolate.new
    ctx_b = iso_b.context
    @ctx.eval('globalThis.who = "A"')
    ctx_b.eval('globalThis.who = "B"')
    @ctx.attach('f', proc { ctx_b.eval('globalThis.who') })
    assert_equal 'B', call_with_deadline(@ctx, 'f')
  end

  def test_nested_instantiate_during_instantiate_raises_not_crashes
    # V8 module instantiation is not re-entrant: a resolve block instantiating
    # another module mid-instantiate walks the half-built graph and SEGVs, so
    # it must be refused with a clean error (the block may still COMPILE
    # lazily — the outer instantiate links the dep)
    app = @ctx.compile_module('import {x} from "/dep.js"; globalThis.X = x;', filename: '/app.js')
    t = deadline_thread {
      app.instantiate {|spec, _ref|
        dep = @ctx.compile_module('export const x = 1;', filename: spec)
        dep.instantiate {|_s, _r| nil } # re-entrant: must raise, not SEGV
        dep
      }
    }
    e = assert_raises(RustyRacer::RuntimeError) {
      flunk 'nested instantiate deadlocked' unless t.join(10)
      t.value
    }
    assert_includes e.message, 'not re-entrant'
    assert_equal 2, @ctx.eval('1 + 1')
  end

  def test_nested_reset_of_suspended_realm_is_refused
    # resetting the realm whose request is suspended on the V8 stack would
    # drop its in-flight modules and swap the v8::Context behind the same id
    # (defeating the cross-context import guards), so it is refused
    @ctx.attach('f', proc {
      @ctx.reset
      'reset-succeeded'
    })
    t = deadline_thread { @ctx.call('f') }
    e = assert_raises(RustyRacer::RuntimeError) {
      flunk 'nested reset deadlocked' unless t.join(10)
      t.value
    }
    assert_includes e.message, 'suspended'
    assert_equal 2, @ctx.eval('1 + 1')
    # resetting OUTSIDE a suspended frame still works
    @ctx.reset
    assert_equal 2, @ctx.eval('1 + 1')
  end

  def test_nested_reset_of_idle_realm_is_allowed
    # only the realms on the V8 stack are protected; a host fn may freely
    # reset a DIFFERENT, idle realm
    realm = @iso.create_context
    realm.eval('globalThis.x = 1')
    @ctx.attach('f', proc {
      realm.reset
      realm.eval('typeof globalThis.x')
    })
    assert_equal 'undefined', call_with_deadline(@ctx, 'f')
  end

  def test_lazy_compile_inside_instantiate_resolver
    # the instantiate resolve block may compile dependencies lazily — the
    # suspended InstantiateModule frame services the nested compile
    app = @ctx.compile_module('import {x} from "./dep.js"; globalThis.LAZY = x;', filename: '/app.js')
    t = deadline_thread {
      app.instantiate {|specifier, _referrer|
        @ctx.compile_module('export const x = 42;', filename: specifier)
      }
      app.evaluate
    }
    flunk 'lazy compile in instantiate resolver deadlocked' unless t.join(10)
    t.value
    assert_equal 42, @ctx.eval('globalThis.LAZY')
  end

  def test_lazy_load_inside_dynamic_import_resolver
    # the dynamic-import resolver may compile + instantiate + evaluate the
    # module on demand (the suspended import() frame services all three)
    @iso.dynamic_import_resolver = lambda {|specifier, _referrer, _ctx|
      m = @ctx.compile_module('export const v = 7;', filename: specifier)
      m.instantiate {|_s, _r| nil }
      m.evaluate
      m
    }
    t = deadline_thread {
      @ctx.eval('globalThis.OUT = null; import("/lazy.js").then(m => { globalThis.OUT = m.v });')
      @iso.perform_microtask_checkpoint
    }
    flunk 'lazy dynamic import deadlocked' unless t.join(10)
    t.value
    assert_equal 7, @ctx.eval('globalThis.OUT')
  end

  def test_timeout_terminates_and_recovers
    assert_raises(RustyRacer::ScriptTerminatedError) { @ctx.eval("for(;;){}", timeout_ms: 100) }
    assert_equal 4, @ctx.eval("2 + 2")
  end

  def test_late_watchdog_does_not_poison_next_request
    # audit #3: a late TerminateExecution must not leak into the next request.
    100.times do
      begin
        @ctx.eval("{ const u = Date.now() + 1; while (Date.now() < u) {} }", timeout_ms: 1)
      rescue RustyRacer::ScriptTerminatedError
        # terminated this round — fine
      end
      assert_equal 1, @ctx.eval("1")
    end
  end

  def test_stop_from_another_thread_then_usable
    stopper = Thread.new { sleep 0.05; @iso.terminate }
    assert_raises(RustyRacer::ScriptTerminatedError) { @ctx.eval("for(;;){}") }
    stopper.join
    assert_equal 6, @ctx.eval("3 + 3")
  end

  def test_idle_terminate_does_not_poison_the_next_request
    # Isolate#terminate fired while the V8 thread is idle (no JS running) sets
    # the global terminate flag but no watchdog; the next eval must clear it and
    # run normally, not abort spuriously
    @ctx.eval("1") # warm up
    @iso.terminate # nothing is running
    results = (101..105).map {|n| @ctx.eval(n.to_s) rescue :terminated }
    assert_equal [101, 102, 103, 104, 105], results
  end

  def test_realm_disposed_error_is_not_turned_into_terminated
    # a watchdog is armed even for a disposed realm (which runs no JS); a raced
    # firing must not mask the real "disposed" error as ScriptTerminatedError
    iso = RustyRacer::Isolate.new(timeout_ms: 1)
    r = iso.create_context
    r.dispose
    50.times do
      e = assert_raises(::RuntimeError) { r.eval("1 + 1") }
      refute_kind_of RustyRacer::ScriptTerminatedError, e
    end
  end

  def test_dispose_racing_eval_does_not_hang
    # audit #12/#13/#26: dispose racing an in-flight eval must not hang.
    10.times do
      iso = RustyRacer::Isolate.new
      c = iso.context
      worker = Thread.new do
        c.eval("const u = Date.now() + 30; while (Date.now() < u) {}")
      rescue StandardError
        # disposed/terminated mid-run is acceptable (class varies with the
        # race); hanging is not.
      end
      sleep(rand * 0.03)
      iso.dispose
      assert worker.join(5), "worker hung"
      # post-dispose use raises the plain disposed-context guard, not a JS error
      assert_raises(::RuntimeError) { c.eval("1") }
    end
  end

  private

  # An isolate is thread-confined: every op must run on the thread that created
  # it. These re-entrancy/timeout tests therefore run the op on the OWNER (test)
  # thread directly — re-entrancy is a plain Rust call stack now and can't
  # deadlock the way the old dedicated-V8-thread + channel model could, and a
  # runaway loop is bounded by the watchdog (which terminates from its own
  # thread). InlineRun keeps the existing `t = deadline_thread { ... }; t.join;
  # t.value` call sites working without spawning a (forbidden) worker thread.
  InlineRun = Struct.new(:ok, :val) do
    def join(_timeout = nil) = true
    def value = ok ? val : raise(val)
  end

  def call_with_deadline(ctx, name)
    ctx.call(name)
  end

  # How deep JS recursion gets inside a Fiber whose machine stack is
  # `stack_bytes`. Ruby sizes fiber stacks from the environment at boot, so this
  # has to be a fresh process; the child inherits this one's $LOAD_PATH so it
  # loads the extension under test rather than an installed gem.
  def fiber_js_depth(stack_bytes)
    script = <<~'RUBY'
      require 'rusty_racer'
      ctx = RustyRacer::Isolate.new.context
      ctx.eval('globalThis.probe = function () { let d = 0; function r() { d++; r() } try { r() } catch (e) { return d } }')
      print Fiber.new { ctx.eval('probe()') }.resume
    RUBY
    env = {'RUBY_FIBER_MACHINE_STACK_SIZE' => stack_bytes.to_s}
    argv = [RbConfig.ruby, *$LOAD_PATH.flat_map {|d| ['-I', d] }, '-e', script]
    out = IO.popen(env, argv, &:read)
    assert_predicate $?, :success?, "probe subprocess failed: #{out}"
    Integer(out)
  end

  def deadline_thread(&block)
    InlineRun.new(true, block.call)
  rescue Exception => e # rubocop:disable Lint/RescueException
    InlineRun.new(false, e)
  end

  # Run an isolate's whole lifecycle — create, attach a proc capturing a fresh
  # object, then |op| (which must release the proc's GC root) — entirely on ONE
  # throwaway thread (the isolate's owner; the isolate is thread-confined, so the
  # op MUST run on its creating thread). Doing it off the test thread leaves no
  # conservative stack residue here, so a still-alive WeakRef afterwards means a
  # genuine leaked root, not a stale stack slot. Returns the WeakRef.
  def capture_released_by(&op)
    require 'weakref'
    Thread.new {
      iso = RustyRacer::Isolate.new
      ctx = iso.context
      captured = Object.new
      ctx.attach('f', proc { captured.object_id })
      ref = WeakRef.new(captured)
      op.call(iso, ctx)
      ref
    }.value
  end
end
