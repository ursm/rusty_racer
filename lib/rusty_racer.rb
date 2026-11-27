# frozen_string_literal: true

require "set" # JS Set <-> Ruby Set marshalling needs the stdlib Set constant

require_relative "rusty_racer/version"

# Load the compiled extension (defines RustyRacer::Isolate etc.). A precompiled
# ("fat") gem ships one binary per Ruby minor version under
# rusty_racer/<major.minor>/ (the .so is ABI-specific — a 3.3 build malfunctions
# on 4.0); the source gem and a local `rake compile` build a single flat
# rusty_racer/rusty_racer instead. Pick by existence rather than rescuing
# LoadError, so a binary that IS present but fails to load (a missing transitive
# lib, an init error) surfaces its real error instead of being masked by the
# flat fallback.
versioned = "rusty_racer/#{RUBY_VERSION[/\d+\.\d+/]}/rusty_racer"

# Skip the native load when an embedder has already defined the classes. Linking
# rusty_racer as a library into another extension's cdylib (e.g.
# capybara-simulated, which bundles rusty_racer + its native DOM) calls
# install_classes from that extension's own init; loading this gem's separate .so
# on top would put a SECOND V8 runtime in the process (unsound). This file is then
# required only for the pure-Ruby API wrappers below.
unless defined?(RustyRacer::Isolate)
  if File.exist?(File.join(__dir__, "#{versioned}.#{RbConfig::CONFIG['DLEXT']}"))
    require_relative versioned
  else
    require "rusty_racer/rusty_racer"
  end
end

module RustyRacer
  # JS exceptions map to these (see err_class on the Rust side).
  class Error < StandardError; end
  class EvalError < Error; end
  class ParseError < EvalError; end
  class RuntimeError < EvalError; end
  class ScriptTerminatedError < EvalError
    # The JS stack at the moment the call timed out (watchdog), as
    # ["func (script:line:col)", ...], top frame first — captured on the isolate
    # thread just before TerminateExecution. [] when no stack was captured (e.g. a
    # bare Isolate#terminate rather than a timeout). The same frames are also set
    # as the exception's #backtrace when present.
    def js_backtrace = @js_backtrace || []
  end
  # Raised when JS allocation exceeds the isolate's heap ceiling (the configured
  # memory_limit, or V8's default ceiling when none was set). Catchable like any
  # eval error — a runaway script fails its own eval instead of aborting the
  # process. The space-axis twin of ScriptTerminatedError (the time axis).
  class V8OutOfMemoryError < EvalError; end
  class SnapshotError < Error; end
  class PlatformAlreadyInitialized < Error; end

  # Raised when a disposed Isolate, Context, Module or Script is used — or one
  # whose Isolate has been disposed, which takes everything it handed out with
  # it. Disposal is deliberate, so this says "you used it after you released
  # it", not "something went wrong".
  class DisposedError < Error; end

  # A broken invariant inside the binding rather than anything the caller did:
  # an unexpected reply kind, or an operation that panicked. Reaching one is a
  # bug in rusty_racer and worth reporting. It exists so that EVERY error the
  # extension raises is a RustyRacer::Error, and one rescue can cover the
  # library.
  class InternalError < Error; end

  # Raised when an Isolate (or a Context/Module/Script it handed out) is used
  # from a thread other than the one that created it. An isolate is
  # thread-confined: every operation must run on its owner thread. The lone
  # exception is Isolate#terminate, which is safe from any thread.
  class WrongThreadError < Error; end

  # A V8 isolate. Owns the VM and its lifecycle; hands out Contexts to run JS in.
  class Isolate
    # Keyword-arg constructor over the positional Rust primitive. A snapshot
    # (RustyRacer::Snapshot) boots the isolate with its baked-in state;
    # timeout_ms caps each eval/call (0 = no limit) against in-V8 infinite
    # loops. memory_limit caps the V8 heap in bytes: a script that exceeds the
    # ceiling is terminated and raises V8OutOfMemoryError rather than aborting
    # the process, and the isolate stays usable afterward. 0 (the default) does
    # NOT disable this — it leaves V8's own platform-derived default ceiling
    # (typically ~2 GB on 64-bit) in place, so a runaway is still catchable
    # instead of a fatal abort; pass a smaller value for a tighter bound. It is
    # a soft limit — V8 enforces it at GC granularity, so usage may briefly
    # overshoot, and an explicit limit must comfortably exceed the isolate's
    # baseline (and any snapshot's baked-in heap), since it is only armed once
    # the isolate has booted. Caveat: if the process's available memory (e.g. a
    # container cgroup limit) is below the active ceiling, the OS may kill the
    # process before V8's callback fires — set an explicit memory_limit under
    # that bound to keep the error catchable.
    # microtasks mirrors V8's kAuto/kExplicit: :auto (default) drains
    # the microtask queue when the outermost eval/call/run/evaluate completes
    # (the standard embedder contract); :explicit drains only on
    # #perform_microtask_checkpoint.
    # host_namespace names the object every realm gets rusty_racer's own
    # functions in (drainMicrotasks, contextGlobal, contextOf, ...): a property
    # of the global (`'MiniRacer'`), or, as `'@MiniRacer'`, of the realm's host
    # object, which no script of a page's can reach (see HOST_OBJECT).
    def self.new(host_namespace: nil, snapshot: nil, timeout_ms: 0, memory_limit: 0, microtasks: :auto)
      unless %i[auto explicit].include?(microtasks)
        raise ArgumentError, "microtasks must be :auto or :explicit, got #{microtasks.inspect}"
      end
      raise ArgumentError, 'host_namespace: "@" names no property of the host object' if host_namespace == '@'

      _new(host_namespace, snapshot, timeout_ms, memory_limit, microtasks == :explicit)
    end

    # ->(import) { ... } for JS import(): handed each import()'s
    # RustyRacer::DynamicImport — the specifier, its import attributes, the
    # referring script's name and host-defined value, the realm it was called in
    # — which the handler settles now or later (a fetch begun now, settled on a
    # later task): #resolve with a Module, #reject / #reject_with with a value.
    # Until then the import()'s promise is pending. Whatever leaves the handler
    # other than by returning — any exception, an Interrupt or a Timeout
    # included — rejects it with an Error of its message, unless it settled it
    # first, and goes no further; nil clears the handler, an import() then
    # rejected. The native side keeps the proc alive (in the isolate's roots
    # array, marked by its wrappers), so `dispose` drops whatever it captured.
    def dynamic_import_handler=(handler)
      _set_dynamic_import_handler(handler)
    end

    # ->(specifier, referrer_url, context) { Module } for JS import(), answered at
    # once: the dynamic_import_handler that resolves each import() with the
    # Module the resolver returns — linked through the resolver too (a static
    # import met while linking it asks it, with the same realm), and evaluated —
    # or rejects it where the resolver returns nil. nil clears it.
    def dynamic_import_resolver=(resolver)
      self.dynamic_import_handler = resolver && lambda do |import|
        context = import.context
        mod = resolver.call(import.specifier, import.referrer.to_s, context)
        # (…raised: the import rejected with an Error of the realm's own, natively)
        raise Error, "import() was not resolved to a module (#{import.specifier.inspect})" unless mod

        import.resolve(mod) {|specifier, referrer| resolver.call(specifier, referrer, context) }
      end
    end
  end

  # HOST_OBJECT (defined natively): an argument that crosses into JS as the
  # realm's HOST OBJECT — an object with no prototype that no global holds, which
  # the host and the realm's own bindings share and no other script can reach.
  # Every path the host names (Context#attach / #attach_many / #call /
  # #call_void / #function, Module#evaluate's report:, Isolate.new's
  # host_namespace:) starts from it when it begins with `@`:
  #
  #   ctx.attach('@fetch', ->(url) { ... })          # host.fetch, no global
  #   ctx.call('installBindings', RustyRacer::HOST_OBJECT)
  #   step = ctx.function('@steps.query')             # host.steps.query
  #
  # An `@` path walks own properties alone. HOST_OBJECT is the host object of
  # the realm the operation runs in (a Context's, a Function handle's, a host
  # function's own, for its result), and that object crosses back as
  # HOST_OBJECT. Each realm has its own, made as it is first used; a reset makes
  # the realm, and so its host object, anew. Hand it to the bindings before any
  # script of a page's runs, by a name they delete as they take it.

  # A v8::Context (realm) handed out by an Isolate: where JS actually runs.
  class Context
    # `timeout_ms` (0 = the isolate default) caps this eval; `filename` names the
    # script in stack traces and parse-error locations.
    def eval(source, timeout_ms: 0, filename: '<eval>')
      _eval(source, timeout_ms, filename, 0)
    end

    # Run `source` for its EFFECT and return nil, without marshalling its
    # completion value — the counterpart of #call_void, and what a <script> tag
    # does. Worth reaching for whenever the value is incidental: a statement
    # list still has one (its last statement's), and marshalling reads the
    # value, which runs JS — a getter or a Proxy trap can throw, failing an eval
    # whose writes all landed. `globalThis.win = someProxy` is the shape that
    # bites.
    def eval_void(source, timeout_ms: 0, filename: '<eval>')
      _eval(source, timeout_ms, filename, 1)
    end

    # Run `source` and return its completion value only where it is a primitive — a string, a number, a boolean, a
    # BigInt, null — and nil for an object or a symbol, which is never walked: for a caller that wants a string of it
    # (a javascript: URL's evaluation), whatever object the source happens to end with.
    def eval_primitive(source, timeout_ms: 0, filename: '<eval>')
      _eval(source, timeout_ms, filename, 2)
    end

    # Compile a classic <script>; returns a RustyRacer::Script to #run.
    # cached_data:/produce_cache: are the bytecode cache (see #compile_module).
    # eager: (see #compile_module).
    # host_defined: a primitive (Integer, Float, String, true, false; not a
    # Symbol) its ScriptOrigin carries, which an import() it makes is handed
    # (DynamicImport#host_defined) — how a host tells one referring script from
    # another of the same name. Code an `eval` or `new Function` of it compiles
    # carries its value too — but V8 caches such code by its source, so the
    # same source evaluated by another script may keep the first one's.
    def compile(source, filename: '<compile>', cached_data: nil, produce_cache: false, eager: false, host_defined: nil)
      _compile(source, filename, cached_data, produce_cache, eager, host_defined)
    end

    # Compile an ES module; returns a RustyRacer::Module to instantiate/evaluate.
    # cached_data: a binary bytecode cache to consume (skip reparse); the result
    # reports #cache_rejected? if stale. produce_cache: collect a fresh cache,
    # readable via Module#cached_data for cross-process reuse.
    #
    # eager: (default false) compiles every function up front
    # (CompileOptions::EagerCompile) instead of V8's default lazy top-level-only
    # compile. It roughly doubles compile time and uses more memory, so it's only
    # worth it when producing a cache to reuse. Ignored when cached_data: is given
    # (V8 forbids consuming a cache and eager-compiling at once). NOTE: as of
    # V8-150, create_code_cache at compile time still doesn't serialize the eager
    # inner functions, so eager: alone doesn't change the cache yet — it's a
    # forward-looking, semantically-correct switch. For a cache that DOES carry
    # inner functions, run the script/module and call #create_code_cache.
    #
    # host_defined: (see #compile) — handed to an import() the module makes, and
    # to its realm's import-meta handler (NS.setImportMetaHandler).
    def compile_module(source, filename: '<compile_module>', cached_data: nil, produce_cache: false, eager: false, host_defined: nil)
      _compile_module(source, filename, cached_data, produce_cache, eager, host_defined)
    end

    # A synthetic module (V8 CreateSyntheticModule) exporting `default` alone,
    # under `filename` — what HTML makes of a JSON, CSS or text module script —
    # its value one of:
    #
    #   synthetic_module('text', filename: '/a.txt')            # a Ruby value, marshalled
    #   synthetic_module(json: '{"a":1}', filename: '/a.json')  # %JSON.parse% of a JSON text (ECMA-262 JSON modules)
    #   synthetic_module(call: ['@makeSheet', css], filename: '/a.css')
    #                                                           # what the function at a path (as #call names one) returns
    #
    # A JSON text that is none raises RustyRacer::ParseError, as a module's
    # SyntaxError does; a function that throws, RustyRacer::RuntimeError.
    # Linked and evaluated as any module is; it has no requests.
    def synthetic_module(*value, filename: '<synthetic_module>', json: nil, call: nil)
      given = [!value.empty?, !json.nil?, !call.nil?].count(true)
      raise ArgumentError, 'synthetic_module takes one of a value, json: or call:' unless given == 1 && value.size <= 1

      if json
        _synthetic_module(filename, 1, json, [])
      elsif call
        path, *args = call
        _synthetic_module(filename, 2, path, args)
      else
        _synthetic_module(filename, 0, value.first, [])
      end
    end
  end

  # A compiled (or synthetic) ES module. Defined natively besides: #requests ->
  # [[specifier, attributes], ...], what the module imports (V8
  # GetModuleRequests), in source order, each with its frozen import attributes —
  # the requests linking it asks its resolve block for, one by one.
  class Module
    # instantiate { |specifier, referrer_url, attributes| dependency_module } —
    # the block resolves each import to an already-compiled Module; attributes
    # is the request's import attributes, {'type' => 'json'} (a request with
    # none: one frozen empty Hash). Returns self.
    def instantiate(&block)
      raise ArgumentError, 'instantiate requires a resolver block' unless block

      _instantiate(block)
      self
    end

    # Evaluate the instantiated module. Its evaluation's rejection — a top-level throw, a top-level await's — raises
    # (RustyRacer::RuntimeError, as far as the drain after it settles), or, `report:` naming a function of its context's
    # global (an `@` path: on its host object, HOST_OBJECT), is that function's to report: called with the rejection's
    # reason — a top-level throw's before this returns, a top-level await's as it settles — and nothing raised (HTML
    # "run a module script": upon rejection, report the exception).
    def evaluate(report: nil)
      _evaluate(report&.to_s)
    end

    # The V8 module status: :uninstantiated, :instantiating, :instantiated,
    # :evaluating, :evaluated or :errored.
    def status
      _status.to_sym
    end
  end

  # An import() waiting for its host — handed to Isolate#dynamic_import_handler.
  # #specifier, #attributes ({'type' => 'json'}), #referrer (the name of the
  # script that called import(), or nil), #host_defined (that script's
  # host_defined:, or nil) and #context (the realm it was called in, whose
  # modules it is settled with) say what was asked; #resolve, #reject and
  # #reject_with settle it — now, inside the handler, or on a later task — each
  # answering whether it was still waiting (false once settled, or once its
  # realm was reset or disposed). Let go of unsettled, its promise is let go of
  # too, pending.
  class DynamicImport
    # resolve(module) { |specifier, referrer_url, attributes| dependency } —
    # fulfil the import() with `module`'s namespace once it is evaluated:
    # linked first, where it is not yet, by the block (as Module#instantiate's
    # is), then evaluated (its top-level await awaited). A link or evaluation
    # error — a block that raised included — rejects the import() with it. A
    # module still linking or evaluating (an import() of itself, settled at once
    # from inside the handler) rejects it as busy: settle such a one later, from
    # a later turn. An evaluation terminated (a timeout, Isolate#terminate)
    # raises ScriptTerminatedError and leaves the import() pending for good.
    def resolve(mod, &block)
      _resolve(mod, block)
    end

    # #reject(value): reject the import() with `value`, marshalled into its realm
    # as any argument is (nil, true/false, numbers, Strings, Arrays, Hashes, …,
    # HOST_OBJECT) — a plain value: an Error of the realm's own is #reject_with's.
    # #reject_with(path, *args): reject it with what the function at `path` (as
    # Context#call names one, in its realm) returns — `'@steps.typeError'`, say,
    # for an error of the host's own making — or with what it throws.
  end
end
