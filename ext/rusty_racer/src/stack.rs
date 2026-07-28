// V8 stack limit + conservative-GC-scan retargeting (in-thread: V8 runs on the
// calling Ruby thread's stack — a native pthread stack, or a Ruby Fiber's
// separate mmap'd stack), plus the current-isolate query the reentry path needs.
// Self-contained: only raw pointers, std, libc, and the exported V8 symbols below
// — no IsolateState/JsVal/marshalling. It also hosts current_real_isolate() (the
// entered-isolate query) since that too is just one of the exported V8 symbols
// below, kept here so the FFI block stays in one place. The crate uses
// discover_scan_start_field (once per isolate), StackScope (per op),
// current_real_isolate (per reentrant op), and STACK_DEBUG (set at init);
// everything else is private to this module.

use std::ffi::c_void;
// Only native_stack_bounds (linux) needs it; gated so non-linux builds (macOS)
// don't warn it unused.
#[cfg(target_os = "linux")]
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// rusty_v8 doesn't wrap the runtime `v8::Isolate::SetStackLimit(uintptr_t)`, so
// link the public V8 symbol directly (stable across V8 versions). It sets the
// lowest address V8's stack may reach before it throws RangeError.
unsafe extern "C" {
    #[link_name = "_ZN2v87Isolate13SetStackLimitEm"]
    fn v8__Isolate__SetStackLimit(isolate: *mut c_void, stack_limit: usize);
    // V8's own (exported) accessors down to the conservative-GC-scan Stack
    // object, so we can re-point its stack_start per op when V8 runs on a Ruby
    // Fiber (see StackScope / discover_scan_start_field). Member fns:
    // the first arg is `this`. The public v8::Isolate* IS i::Isolate*.
    #[link_name = "_ZN2v88internal7Isolate4heapEv"]
    fn v8__internal__Isolate__heap(isolate: *mut c_void) -> *mut c_void;
    #[link_name = "_ZN2v88internal4Heap5stackEv"]
    fn v8__internal__Heap__stack(heap: *mut c_void) -> *mut c_void;
    // Sets the scan stack_start to v8::base::Stack::GetStackStart() (the native
    // pthread top) — used only to positively identify the field during discovery.
    #[link_name = "_ZN2v88internal4Heap13SetStackStartEv"]
    fn v8__internal__Heap__SetStackStart(heap: *mut c_void);
    #[link_name = "_ZN2v84base5Stack13GetStackStartEv"]
    fn v8__base__Stack__GetStackStart() -> usize;
    // rusty_v8's C binding for v8::Isolate::GetCurrent() — the thread-local
    // "currently entered" isolate. No #[link_name]: the exported symbol is
    // literally this name (rusty_v8's binding glue), same as the crate's own
    // private declaration. Lets a re-entrant op tell whether ITS isolate is the
    // one currently entered, or a foreign isolate was entered on top of it (the
    // cross-isolate reentry case — see Core::run).
    fn v8__Isolate__GetCurrent() -> *mut c_void;
}

// The raw v8::Isolate* currently entered on this native thread (null if none).
// Compared by identity against an isolate's own raw pointer.
pub(crate) fn current_real_isolate() -> *mut c_void {
    unsafe { v8__Isolate__GetCurrent() }
}

// Locate V8's conservative-GC-scan stack_start field
// (heap::base::Stack::current_segment_.start) so StackScope can
// re-point it per op. The scanner walks [SP, stack_start); on a Ruby Fiber V8's
// stack_start is still the NATIVE thread top, a different region, so the walk
// runs off the fiber's mapped top into the guard page and SEGVs (the residual
// after the limit fix). We reach the Stack via V8's exported Isolate::heap()/
// Heap::stack(); the field is the first word of Stack (current_segment_ is its
// first member, .start the first field), but we VERIFY rather than trust the
// layout: Heap::SetStackStart() writes that field to base::Stack::GetStackStart(),
// so if poking a sentinel and re-calling SetStackStart restores the value at
// offset 0, that word IS the field. Any mismatch returns 0 (override disabled —
// V8 keeps its native start, i.e. the rare pre-fix crash, NEVER corruption).
// Must run with the isolate ENTERED. `real_isolate` is the raw v8::Isolate*.
pub(crate) fn discover_scan_start_field(real_isolate: *mut c_void) -> usize {
    const SENTINEL: usize = 0xA5A5_A5A5_A5A5_A5A5;
    unsafe {
        let heap = v8__internal__Isolate__heap(real_isolate);
        if heap.is_null() {
            return 0;
        }
        let stack = v8__internal__Heap__stack(heap);
        if stack.is_null() {
            return 0;
        }
        let nt = v8__base__Stack__GetStackStart();
        if nt == 0 {
            return 0;
        }
        v8__internal__Heap__SetStackStart(heap); // start := nt
        let field = stack as *mut usize; // expected &current_segment_.start
        if field.read() != nt {
            return 0; // offset 0 isn't the field (layout changed) — disable
        }
        field.write(SENTINEL);
        v8__internal__Heap__SetStackStart(heap); // must rewrite the same word
        if field.read() != nt {
            return 0; // SetStackStart doesn't own offset 0 — disable
        }
        stack as usize
    }
}

// The native thread's stack bounds are stable per NATIVE thread, but querying
// them (pthread, which reads /proc/self/maps for the main thread on Linux) is
// far too slow per op. Cache (bottom, top) in a native-thread-local — correct
// under M:N (each native thread caches its own stack) and ~free after the first
// op on a thread. (0, 0) if it can't be queried.
thread_local! {
    static STACK_BOUNDS: std::cell::Cell<(usize, usize)> =
        const { std::cell::Cell::new((0, 0)) };
}

fn native_stack_bounds_cached() -> (usize, usize) {
    STACK_BOUNDS.with(|c| {
        let cached = c.get();
        if cached.0 != 0 {
            return cached;
        }
        let bounds = native_stack_bounds();
        c.set(bounds);
        bounds
    })
}

// (bottom, top) of the CURRENT native thread's stack via pthread (uncached —
// callers go through native_stack_bounds_cached). The stack grows DOWN from top
// toward bottom. (0, 0) if it can't be queried. NB: this is the NATIVE thread's
// pthread stack; a Ruby Fiber runs on a separate mmap'd stack invisible here.
#[cfg(target_os = "linux")]
fn native_stack_bounds() -> (usize, usize) {
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return (0, 0);
        }
        let mut addr: *mut c_void = null_mut();
        let mut size: libc::size_t = 0;
        let rc = libc::pthread_attr_getstack(&attr, &mut addr, &mut size);
        libc::pthread_attr_destroy(&mut attr);
        if rc != 0 {
            return (0, 0);
        }
        (addr as usize, addr as usize + size)
    }
}

#[cfg(target_os = "macos")]
fn native_stack_bounds() -> (usize, usize) {
    unsafe {
        let top = libc::pthread_get_stackaddr_np(libc::pthread_self()) as usize;
        let size = libc::pthread_get_stacksize_np(libc::pthread_self());
        (top.saturating_sub(size), top)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn native_stack_bounds() -> (usize, usize) {
    (0, 0)
}

// Lower bound (and upper, for caching) of the memory region containing `addr`
// — i.e. the BOTTOM of the stack `addr` is on. Used for a Ruby Fiber, whose
// mmap'd stack pthread can't see: V8's limit must sit ABOVE this bottom or a
// deep fiber recursion overflows the real stack and SEGVs the unmapped guard.
// Cached per native thread keyed by the region (parsing /proc/self/maps is
// slow): reused while successive ops stay on the same fiber. (0, 0) if unknown.
thread_local! {
    static FIBER_REGION: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

fn current_region_bounds_cached(addr: usize) -> (usize, usize) {
    FIBER_REGION.with(|c| {
        let (lo, hi) = c.get();
        if lo != 0 && addr >= lo && addr < hi {
            return (lo, hi);
        }
        let bounds = query_region_bounds(addr);
        if bounds.0 != 0 {
            c.set(bounds);
        }
        bounds
    })
}

// The [start, end) of the /proc/self/maps mapping containing `addr`. Linux only;
// (0, 0) elsewhere (and the caller falls back). Reads the file fresh — slow, so
// only called on a cache miss (a new fiber).
//
// Everything that needs a FIBER's real bounds degrades on the (0, 0) platforms,
// macOS being the shipped one: the stack limit drops to a fixed budget below the
// SP (see StackScope::enter), and a nested op can no longer widen its GC-scan
// start to the enclosing op's, so it narrows to its own frame. Both are the
// pre-fiber-support behaviour rather than a crash, but a darwin implementation
// (mach_vm_region on the current task) would close the gap.
#[cfg(target_os = "linux")]
fn query_region_bounds(addr: usize) -> (usize, usize) {
    use std::io::Read;
    let mut buf = String::new();
    if std::fs::File::open("/proc/self/maps")
        .and_then(|mut f| f.read_to_string(&mut buf))
        .is_err()
    {
        return (0, 0);
    }
    for line in buf.lines() {
        // e.g. "7f6a...000-7f6a...000 rw-p 00000000 00:00 0 ..."
        let Some((range, _)) = line.split_once(' ') else {
            continue;
        };
        let Some((lo, hi)) = range.split_once('-') else {
            continue;
        };
        let (Ok(lo), Ok(hi)) = (
            usize::from_str_radix(lo, 16),
            usize::from_str_radix(hi, 16),
        ) else {
            continue;
        };
        if addr >= lo && addr < hi {
            return (lo, hi);
        }
    }
    (0, 0)
}

#[cfg(not(target_os = "linux"))]
fn query_region_bounds(_addr: usize) -> (usize, usize) {
    (0, 0)
}

// Set from RUSTY_RACER_STACK_DEBUG at init; gates the per-op stack diagnostics.
pub(crate) static STACK_DEBUG: AtomicBool = AtomicBool::new(false);

// How V8 describes the stack an op runs on: the stack limit, and — on a Ruby
// Fiber — the conservative-GC-scan start. Both live in the ISOLATE, not in the
// op, so an op that runs on a DIFFERENT stack than the one enclosing it (a host
// callback that resumes a Fiber which evals again) has to install its own and
// put the enclosing op's back on the way out. Hence a scope: after enter() V8
// describes the stack we are on now, after the drop the one it described before.
//
// In-thread V8 runs wherever the Ruby code is: usually the native thread's
// pthread stack, but also a Ruby Fiber's separate mmap'd stack (Capybara::Result
// is an Enumerator) that pthread can't see. The limit MUST sit between the
// current SP and the real bottom of whatever stack we're on:
//   * Too high (above SP) and V8 declares a FALSE overflow on entry.
//   * Too low (below the real bottom) and a deep recursion grows past the
//     mapped stack and SEGVs the unmapped guard page below it.
// So detect the stack by comparing the SP to the cached native bounds: on the
// native stack, anchor to its pthread bottom; on a fiber, find the bottom of the
// /proc/self/maps region holding the SP (the fiber's real bottom — anchoring to
// SP minus a fixed guard punched through the bottom of Avo's small/deep Capybara
// fibers and SEGV'd).
//
// The limit does double duty, which is why a STALE one is fatal rather than
// merely wrong: v8::Isolate::SetStackLimit also sets stack_size_ to
// (base::Stack::GetStackStart() - limit), and V8's "am I on the central stack?"
// test is (native_top - stack_size_ - margin) < addr <= native_top — that is,
// exactly (limit - margin, native_top]. Throwing (and GC) CHECKs that window and
// V8_Fatals the process on a miss. So an op running on a fiber under the
// ENCLOSING op's native-stack limit doesn't merely false-overflow: the
// RangeError it then tries to throw aborts. Installing the limit for the stack
// we are actually on fixes both at once.
//
// RESIDUAL (GC scan across a stack SWITCH): V8 tracks ONE scan segment per
// isolate, so while a nested op runs on a fiber its enclosing op's frames on the
// NATIVE stack aren't conservatively scanned — the two live in different
// mappings and only one range can be described. (A nested op that did NOT switch
// stacks is fine: it keeps the enclosing op's start, which covers both.) Handles
// are unaffected — this
// build has v8_enable_direct_handle off, so every Local lives in a HandleScope
// block and is iterated precisely, and JS frames are walked precisely from the
// frame pointers whichever stack they sit on. Only a raw Tagged<> in V8's own
// C++ frames would be missed, and V8 keeps those in handles across anything that
// can allocate. There's no fix available: Stack has no API to register a second
// segment for the same thread.
//
// LIMITATION (worker-thread fibers): only the window's LOWER bound follows the
// limit. Its upper bound is base::Stack::GetStackStart(), the native pthread
// top, cached per thread with no way to retarget it (on POSIX
// base::Stack::SetCurrentThreadStackBounds is UNREACHABLE()). A fiber mmap'd
// ABOVE that top — the common case on a NON-main native thread, whose stack sits
// below later fiber mmaps — is outside the window whatever limit we set, so V8
// aborts on the next throw or GC. On the main thread the process stack is the
// highest address, so every fiber is below it and the window covers it — the
// Capybara/Avo case. See README.
pub(crate) struct StackScope<'a> {
    real_isolate: *mut c_void,
    // Address of V8's conservative-GC-scan start field, or 0 when enter()
    // installed nothing (discovery failed, or no sane limit) — the drop then
    // restores nothing.
    scan_start_field: usize,
    // The limit V8 currently holds for this isolate. Tracked by the caller
    // because V8 exposes no getter, and a nested op needs the enclosing op's
    // value to put back. 0 before the isolate's first op.
    installed_limit: &'a AtomicUsize,
    // What the drop has to put back, or 0 for "nothing changed, nothing to undo".
    // Most re-entrant ops run in deeper frames of the SAME stack and so compute
    // the very values already installed; skipping those writes keeps a nested op
    // as cheap as it was before it started describing its own stack.
    restore_limit: usize,
    restore_scan_start: usize,
}

impl<'a> StackScope<'a> {
    // Describe the stack THIS call runs on to the isolate, until the drop. Must
    // be called with the isolate ENTERED: Isolate::Enter sets the scan start to
    // the native top, so entering afterwards would clobber a fiber override.
    //
    // `enclosing_scan_start` is the scan start of the op this one is nested in,
    // or None at the outermost op, where no enclosing op exists — then the drop
    // restores nothing (the values left behind belong to no live frame, and the
    // next op installs its own before any JS runs). Because Enter re-points the
    // field, it must be read BEFORE entering. It doubles as the value the drop
    // puts back and, when a nested op didn't switch stacks, as this op's own
    // start (see below). `real_isolate` is the raw v8::Isolate* read out of
    // iso_ptr; `stack_top` is a live address the caller captured ABOVE every V8
    // frame of this op (used only on a fiber).
    pub(crate) fn enter(
        real_isolate: *mut c_void,
        scan_start_field: usize,
        enclosing_scan_start: Option<usize>,
        installed_limit: &'a AtomicUsize,
        stack_top: usize,
    ) -> Self {
        // Only a nested op has an enclosing description to preserve and restore.
        let nested = enclosing_scan_start.is_some();
        // 0 doubles as "no enclosing value" throughout: it is never a real start.
        let prev_scan_start = enclosing_scan_start.unwrap_or(0);
        let sp_marker = 0u8;
        let sp = &sp_marker as *const u8 as usize;
        let (nbottom, ntop) = native_stack_bounds_cached();
        let on_native = nbottom != 0 && sp > nbottom && sp <= ntop;
        // Reserve below the limit for V8's own RangeError-throw frames.
        const NATIVE_GUARD: usize = 128 * 1024;
        // V8 throws when SP descends to the limit, then needs some real stack
        // BELOW it to build the RangeError (and V8 itself allows growing a little
        // past the limit — its overflow slack). On a fiber that reserve must NOT
        // cross the fiber's real bottom (the mapping below it is an unmapped
        // guard -> SEGV), so keep it comfortably above V8's slack.
        const FIBER_RESERVE: usize = 64 * 1024;
        let mut region = (0usize, 0usize);
        let limit = if on_native {
            nbottom + NATIVE_GUARD
        } else {
            // Anchor to the FIBER's real bottom (the /proc/self/maps region
            // holding the SP), not the SP: SP - fixed_guard can punch through the
            // bottom of a small/deep fiber stack and SEGV (Avo's deep Capybara
            // filter chain). Reserve FIBER_RESERVE above the bottom for the
            // throw, but keep the limit below the SP so we don't false-overflow;
            // on a nearly-full fiber that clamps the headroom down (an early but
            // CLEAN RangeError).
            region = current_region_bounds_cached(sp);
            if region.0 != 0 {
                (region.0 + FIBER_RESERVE).min(sp.saturating_sub(8 * 1024))
            } else {
                // Region unknown — only Linux can look a fiber's bounds up (see
                // query_region_bounds), so this is the macOS path. Best effort:
                // hand JS a fixed budget below the SP and hope the fiber is that
                // deep. It usually is for an OUTERMOST fiber op, whose SP sits
                // near the fiber's top, and much less reliably for a NESTED one,
                // whose SP is already far down the fiber — there this can land
                // below the fiber's mapped bottom, and V8 then grows through the
                // guard page (SEGV) instead of throwing. Fixing that properly
                // means a mach_vm_region lookup for darwin.
                sp.saturating_sub(64 * 1024)
            }
        };
        // Opt-in diagnostics (RUSTY_RACER_STACK_DEBUG): the SP vs the native
        // stack [nbottom, ntop], the fiber region (if any), the per-op limit, and
        // whether the SP is above the limit. A crash with sp_above_limit=false
        // means the limit is wrong for the current stack.
        if STACK_DEBUG.load(Ordering::Relaxed) {
            eprintln!(
                "[rusty stack] sp={sp:#x} nbottom={nbottom:#x} ntop={ntop:#x} \
                 region=[{:#x},{:#x}) limit={limit:#x} fiber={} sp_above_limit={} \
                 fiber_above_native={}",
                region.0,
                region.1,
                !on_native,
                sp > limit,
                !on_native && nbottom != 0 && sp > ntop,
            );
        }
        if limit == 0 {
            // Couldn't determine a sane limit — leave V8's default in place, and
            // with it a scope that restores nothing.
            return Self {
                real_isolate,
                scan_start_field: 0,
                installed_limit,
                restore_limit: 0,
                restore_scan_start: 0,
            };
        }
        // installed_limit is the sole record of what V8 holds (it has no getter),
        // so an equal value means the isolate is already described correctly and
        // the call can be skipped. Nothing else moves the limit behind our back:
        // Isolate::Enter/Exit leave the stack guard alone, and V8 re-points it
        // itself only under v8::Locker thread archiving (rusty_v8 exposes no
        // Locker, and an isolate here is thread-confined) or wasm stack
        // switching (no wasm stacks exist in this embedding).
        let prev_limit = installed_limit.load(Ordering::Relaxed);
        let changed_limit = limit != prev_limit;
        if changed_limit {
            unsafe { v8__Isolate__SetStackLimit(real_isolate, limit) };
            installed_limit.store(limit, Ordering::Relaxed);
        }
        // Only a nested op owes anything back: at the outermost op prev_limit is
        // the previous, already-finished op's, which describes no live frame.
        let restore_limit = if nested && changed_limit { prev_limit } else { 0 };
        // Point V8's conservative-GC-scan start at the stack we're on. The
        // scanner walks [marker, start), so on a fiber a native start runs it off
        // the fiber's mapped top into unmapped memory and SEGVs (Avo's Capybara
        // filter chain). Always take the WIDEST start that is still on this
        // stack, because everything between the marker and it gets scanned and an
        // enclosing op's frames sit above ours:
        //   * native stack — the native top, i.e. what V8 itself uses.
        //   * a fiber we ENTERED (the enclosing op is elsewhere) — this op's
        //     `stack_top`, which keeps the range between two real stack pointers
        //     (marker..stack_top) and so guaranteed mapped. We can't use the
        //     /proc/maps region top: that mapping isn't reliably contiguous, so
        //     the scan could still hit a hole below it.
        //   * a fiber we were ALREADY on (a nested op under a host callback that
        //     didn't switch stacks) — the enclosing op's start, which is above
        //     ours and in the same mapping, so it covers both ops' frames.
        let new_scan_start = if on_native {
            unsafe { v8__base__Stack__GetStackStart() }
        } else if nested && region.0 != 0 && prev_scan_start > sp && prev_scan_start < region.1 {
            prev_scan_start
        } else {
            stack_top
        };
        // Install it, and work out what the drop owes the enclosing op. The value
        // to put back is the caller's PRE-ENTER snapshot, not whatever the field
        // holds now: Isolate::Enter re-points the start at the native top, so on
        // the foreign-isolate reentry path the field has already been clobbered
        // by the time we get here, and restoring that would strand an enclosing
        // fiber op with a start on the wrong stack.
        let restore_scan_start = if scan_start_field != 0 && new_scan_start != 0 {
            let field = scan_start_field as *mut usize;
            if unsafe { field.read() } != new_scan_start {
                unsafe { field.write(new_scan_start) };
            }
            if nested && prev_scan_start != 0 && prev_scan_start != new_scan_start {
                prev_scan_start
            } else {
                0 // nothing enclosing to put back, or the drop would be a no-op
            }
        } else {
            0 // nothing installed -> nothing to undo
        };
        Self {
            real_isolate,
            scan_start_field,
            installed_limit,
            restore_limit,
            restore_scan_start,
        }
    }
}

impl Drop for StackScope<'_> {
    // Put the enclosing op's description back, in the reverse order enter()
    // installed it. A zero means there is nothing to put back: enter() changed
    // nothing, or this was the isolate's first op and there is no earlier
    // description. Leaving the last op's settings behind is harmless — every op
    // installs its own before any JS runs.
    fn drop(&mut self) {
        if self.scan_start_field != 0 && self.restore_scan_start != 0 {
            unsafe { (self.scan_start_field as *mut usize).write(self.restore_scan_start) };
        }
        if self.restore_limit != 0 {
            unsafe { v8__Isolate__SetStackLimit(self.real_isolate, self.restore_limit) };
            self.installed_limit.store(self.restore_limit, Ordering::Relaxed);
        }
    }
}
