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

// The BOTTOM of the stack `addr` is on, and the highest address an op has been
// seen running at on it. Used for a Ruby Fiber, whose mmap'd stack pthread can't
// see: V8's limit must sit ABOVE this bottom or a deep fiber recursion overflows
// the real stack and SEGVs the unmapped guard. (0, 0) if unknown.
//
// Cached per native thread, for EVERY stack an op has run on, because a miss
// asks the OS and a workload can run each op on a stack it has never used before:
// Capybara filters each result set in an Enumerator, left suspended until GC
// frees it, so a thousand finds run on a thousand distinct fiber stacks. A cache
// of sixteen slots missed on nearly every one of them, and where a miss reads all
// of /proc/self/maps — whose length grows with Ruby's fiber pool — those reads
// were a fifth of the find loop (1003 per 1000 finds). Keeping every stack makes
// the steady state free: the pool hands the same stacks out again (its free list
// is LIFO, and it never unmaps them — below), so once each has had an op on it
// nothing misses. The table is as large as the set of stacks ops have run on,
// which that pool bounds.
//
// The mapping's top is never used, or even kept, as an upper bound. A fiber
// stack's mapping can extend far above the stack itself, where the kernel merged
// it with an adjacent anonymous mapping of the same protection that has nothing
// to do with the fiber pool (measured: a 644KB stack inside an 8840KB mapping,
// with an unrelated 400MB mapping directly above). An entry that answered on the
// strength of that top would let a later allocation in the neighbour's range
// inherit a bottom megabytes below its own, and V8 would be told it has that
// much headroom on a 644KB stack.
//
// Each entry instead carries `seen`: the highest address a V8 op has ACTUALLY
// run at on that stack. It is both the top of the matching window and the bound
// returned to callers, so the scan-start test in StackScope::enter — "is this
// enclosing op's marker on the stack I am on?" — rests on an address we watched
// an op occupy rather than on the kernel's idea of where the mapping ends. A
// frame higher than anything seen so far misses once and widens the window in
// place, and a miss always re-queries, so it is only HITS that this has to keep
// honest.
//
// What it does not do is tell two stacks apart that genuinely share one mapping,
// since entries are keyed by the mapping's bottom: an op running higher up such
// a mapping re-queries, gets the same bottom back, and widens that one entry.
// Both then get the lower bottom and a window that spans both. That stays
// SAFE — one mapping means no guard page in between, so neither the limit nor a
// scan can walk into an unmapped page — but the upper stack is handed more
// headroom than it owns. It needs a stack with no guard page below it, which
// Ruby's fiber pool and pthread both rule out for the stacks we run on.
//
// Within that window a cached bottom is the fiber's real bottom: Ruby brackets
// every fiber stack with PROT_NONE guard pages, so the mapping cannot start
// below the stack, and a pooled slot is handed to the next fiber with identical
// bounds. Entries are never invalidated, and that rests on the pool too: Ruby
// frees a pool allocation only when built with FIBER_POOL_ALLOCATION_FREE (cont.c
// defines it for a limited address space, i.e. 32-bit, which V8 doesn't ship for,
// and for RB_EXPERIMENTAL_FIBER_POOL), so the bounds an op saw on a stack stay
// the bounds of every later fiber there. Under a build that does unmap, an entry
// can outlive its memory; a hit is then still right when the memory is mapped
// again as the same pool's stacks at the same address — the same stride, so the
// same guards — and wrong, with a bottom below the new one, when a later mapping
// puts a guard page inside the old window. The same goes for a stack that is not
// Ruby's: a C extension's own coroutine stack, unmapped once it is done with and
// its addresses reused, can leave an entry that answers for memory it no longer
// describes. Ruby's M:N thread stacks are safe the way its fibers are: a freed
// one is only madvised, never unmapped.
#[derive(Clone, Copy, Debug, PartialEq)]
struct FiberStack {
    // The bottom of the stack's mapping.
    lo: usize,
    // Highest address a V8 op has run at on this stack.
    seen: usize,
}

// Sorted by `lo`, with disjoint [lo, seen] windows — so `seen` ascends too, and
// both lookups below are binary searches.
struct FiberStacks(Vec<FiberStack>);

impl FiberStacks {
    // The entry whose window holds `addr`.
    fn find(&self, addr: usize) -> Option<FiberStack> {
        let below = self.0.partition_point(|s| s.lo <= addr);
        let stack = *self.0.get(below.checked_sub(1)?)?;
        (addr <= stack.seen).then_some(stack)
    }

    // (bottom, highest-address-seen) of the stack `addr` is on: the entry's when
    // one holds it, else `query`'s answer for `addr`, recorded — 0 there being
    // "unknown", which is recorded nowhere.
    fn bounds(&mut self, addr: usize, query: impl FnOnce(usize) -> usize) -> (usize, usize) {
        if let Some(stack) = self.find(addr) {
            return (stack.lo, stack.seen);
        }
        let lo = query(addr);
        if lo == 0 {
            return (0, 0);
        }
        self.record(lo, addr);
        (lo, addr)
    }

    // Record an op at `addr` on the stack whose bottom was just looked up as
    // `lo`. Being fresh, that replaces every entry overlapping [lo, addr]: the
    // same stack's narrower window, or one describing memory since mapped afresh.
    // Every query checks that its mapping holds `addr`, so `lo <= addr`.
    fn record(&mut self, lo: usize, addr: usize) {
        debug_assert!(lo <= addr);
        let from = self.0.partition_point(|s| s.seen < lo);
        let to = self.0.partition_point(|s| s.lo <= addr);
        self.0.splice(from..to, [FiberStack { lo, seen: addr }]);
    }
}

thread_local! {
    static FIBER_STACKS: std::cell::RefCell<FiberStacks> =
        const { std::cell::RefCell::new(FiberStacks(Vec::new())) };
}

// (bottom, highest-address-seen) of the stack `addr` is on. `addr` must be an
// address the CALLER knows is on that stack and as high up it as it can offer —
// it both answers the lookup and becomes the entry's `seen` on a miss, so a
// caller that passes its topmost frame gets the widest sound window. An op run
// while the thread's locals are being torn down gets the fail-safe (0, 0).
fn current_region_bounds_cached(addr: usize) -> (usize, usize) {
    FIBER_STACKS
        .try_with(|stacks| stacks.borrow_mut().bounds(addr, query_region_bottom))
        .unwrap_or((0, 0))
}

// The bottom of the mapping containing `addr`, or 0 when it can't be
// established. Only called on a cache miss: a stack no op has run on yet, or a
// frame above any seen on it.
//
// 0 is the fail-safe answer, not an error: every caller falls back to a fixed
// budget below the SP, which is what this gem did on every platform before any
// of these lookups existed. So a platform without an implementation, or a lookup
// that returns something we can't vouch for, degrades to a smaller JS stack
// rather than to a wrong one.
//
// Linux asks the kernel for the one mapping (PROCMAP_QUERY, 6.11+: ~0.65us for
// the open, the ioctl and the close, measured beside 3000 mappings) and reads
// all of /proc/self/maps only where the ioctl isn't there — ~25-200us, growing
// with the map. The file is opened per call rather than kept: an open file
// would go on describing the PARENT's address space in a forked child.
#[cfg(target_os = "linux")]
fn query_region_bottom(addr: usize) -> usize {
    let Ok(mut maps) = std::fs::File::open("/proc/self/maps") else {
        return 0;
    };
    procmap_query_bottom(&maps, addr).unwrap_or_else(|| read_maps_bottom(&mut maps, addr))
}

// The bottom of the mapping containing `addr`, through the PROCMAP_QUERY ioctl
// on an open /proc/self/maps; None when the ioctl fails, which on a live stack
// address means a kernel without it (ENOTTY), or answers with a mapping that
// does not hold `addr`, which the kernel's never does but an emulated /proc
// might.
#[cfg(target_os = "linux")]
fn procmap_query_bottom(maps: &std::fs::File, addr: usize) -> Option<usize> {
    use std::os::fd::AsRawFd;
    // struct procmap_query, from linux/fs.h — not in libc. Only `vma_start` is
    // read back, but the ioctl number encodes the whole struct's size.
    #[repr(C)]
    #[derive(Default)]
    struct ProcmapQuery {
        size: u64,
        query_flags: u64,
        query_addr: u64,
        vma_start: u64,
        vma_end: u64,
        vma_flags: u64,
        vma_page_size: u64,
        vma_offset: u64,
        inode: u64,
        dev_major: u32,
        dev_minor: u32,
        vma_name_size: u32,
        build_id_size: u32,
        vma_name_addr: u64,
        build_id_addr: u64,
    }
    // Flags 0 ask for the mapping COVERING query_addr, not the next one above it.
    const PROCMAP_QUERY: libc::Ioctl = libc::_IOWR::<ProcmapQuery>(b'f' as u32, 17);
    let mut query = ProcmapQuery {
        size: std::mem::size_of::<ProcmapQuery>() as u64,
        query_addr: addr as u64,
        ..Default::default()
    };
    let rc = unsafe {
        libc::ioctl(
            maps.as_raw_fd(),
            PROCMAP_QUERY,
            &mut query as *mut ProcmapQuery,
        )
    };
    let (lo, hi) = (query.vma_start as usize, query.vma_end as usize);
    (rc == 0 && lo <= addr && addr < hi).then_some(lo)
}

// The bottom of the mapping containing `addr`, from the text of
// /proc/self/maps, or 0 if no line covers it.
#[cfg(target_os = "linux")]
fn read_maps_bottom(maps: &mut std::fs::File, addr: usize) -> usize {
    use std::io::Read;
    let mut buf = String::new();
    if maps.read_to_string(&mut buf).is_err() {
        return 0;
    }
    for line in buf.lines() {
        // e.g. "7f6a...000-7f6a...000 rw-p 00000000 00:00 0 ..."
        let Some((range, _)) = line.split_once(' ') else {
            continue;
        };
        let Some((lo, hi)) = range.split_once('-') else {
            continue;
        };
        let (Ok(lo), Ok(hi)) = (usize::from_str_radix(lo, 16), usize::from_str_radix(hi, 16))
        else {
            continue;
        };
        if addr >= lo && addr < hi {
            return lo;
        }
    }
    0
}

// macOS has no /proc, so ask the Mach VM directly. mach_vm_region reports the
// region containing `addr` — or, when `addr` sits in a hole, the next one ABOVE
// it — so the answer is checked for containment rather than trusted. It is also
// checked for being readable and writable: a stack always is, and a protection
// that isn't tells us we either walked into something that is not a stack or
// misread the reply, in which case the fixed-budget fallback is the safer answer.
#[cfg(target_os = "macos")]
fn query_region_bottom(addr: usize) -> usize {
    // Not in libc, which points at the `mach2` crate instead; these are the only
    // entry points we need, so declare them here rather than take the dependency.
    // (libc does carry mach_task_self_, but deprecated for the same reason.)
    unsafe extern "C" {
        static mach_task_self_: libc::mach_port_t;
        fn mach_vm_region(
            target_task: libc::vm_map_t,
            address: *mut libc::mach_vm_address_t,
            size: *mut libc::mach_vm_size_t,
            flavor: libc::c_int,
            info: *mut u32,
            info_count: *mut libc::mach_msg_type_number_t,
            object_name: *mut libc::mach_port_t,
        ) -> libc::kern_return_t;
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
    }
    // VM_REGION_BASIC_INFO_64, and the width of vm_region_basic_info_data_64_t
    // in u32s. Only `protection` (its first field) is read.
    const VM_REGION_BASIC_INFO_64: libc::c_int = 9;
    const VM_REGION_BASIC_INFO_COUNT_64: libc::mach_msg_type_number_t = 9;
    const KERN_SUCCESS: libc::kern_return_t = 0;
    const VM_PROT_READ_WRITE: u32 = 0x1 | 0x2;

    unsafe {
        let mut start = addr as libc::mach_vm_address_t;
        let mut size: libc::mach_vm_size_t = 0;
        let mut info = [0u32; VM_REGION_BASIC_INFO_COUNT_64 as usize];
        let mut count = VM_REGION_BASIC_INFO_COUNT_64;
        let mut object_name: libc::mach_port_t = 0;
        let kr = mach_vm_region(
            mach_task_self_,
            &mut start,
            &mut size,
            VM_REGION_BASIC_INFO_64,
            info.as_mut_ptr(),
            &mut count,
            &mut object_name,
        );
        // Documented to come back MACH_PORT_NULL for this flavor, but a leaked
        // port name in a per-fiber path would accumulate, so hand it back.
        if object_name != 0 {
            mach_port_deallocate(mach_task_self_, object_name);
        }
        if kr != KERN_SUCCESS || size == 0 || info[0] & VM_PROT_READ_WRITE != VM_PROT_READ_WRITE {
            return 0;
        }
        let (lo, hi) = (start as usize, start.saturating_add(size) as usize);
        if addr >= lo && addr < hi {
            lo
        } else {
            0 // `addr` was in a hole; this is the region after it
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn query_region_bottom(_addr: usize) -> usize {
    0
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
// native stack, anchor to its pthread bottom; on a fiber, look up the bottom of
// the mapping holding the SP (the fiber's real bottom — anchoring to
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
            // Anchor to the FIBER's real bottom (the mapping holding the SP),
            // not the SP: SP - fixed_guard can punch through the
            // bottom of a small/deep fiber stack and SEGV (Avo's deep Capybara
            // filter chain). Reserve FIBER_RESERVE above the bottom for the
            // throw, but keep the limit below the SP so we don't false-overflow;
            // on a nearly-full fiber that clamps the headroom down (an early but
            // CLEAN RangeError).
            // Look it up by `stack_top`, not the SP: it is this op's highest
            // frame, so it widens the cache's window as far as this op honestly
            // can, and the scan-start test below then has a bound it can trust.
            // Falls back to the SP if the caller had no marker to offer.
            region = current_region_bounds_cached(if stack_top != 0 { stack_top } else { sp });
            if region.0 != 0 {
                (region.0 + FIBER_RESERVE).min(sp.saturating_sub(8 * 1024))
            } else {
                // Region unknown. Both shipped platforms implement the lookup,
                // so reaching this on one of them means the lookup FAILED (see
                // query_region_bottom for what it refuses to vouch for) — worth
                // chasing rather than assuming, since the budget here is only a
                // guess: a fixed reserve below the SP, hoping the fiber is that
                // deep. It usually is for an OUTERMOST fiber op, whose SP sits
                // near the fiber's top, and much less reliably for a NESTED one,
                // whose SP is already far down the fiber — there this can land
                // below the fiber's mapped bottom, and V8 then grows through the
                // guard page (SEGV) instead of throwing.
                sp.saturating_sub(64 * 1024)
            }
        };
        // Opt-in diagnostics (RUSTY_RACER_STACK_DEBUG): the SP vs the native
        // stack [nbottom, ntop], the fiber window (if any), the per-op limit, and
        // whether the SP is above the limit. A crash with sp_above_limit=false
        // means the limit is wrong for the current stack. The window is printed
        // CLOSED because its top is the highest address an op was seen at on that
        // stack, not the mapping's end — see current_region_bounds_cached.
        if STACK_DEBUG.load(Ordering::Relaxed) {
            eprintln!(
                "[rusty stack] sp={sp:#x} nbottom={nbottom:#x} ntop={ntop:#x} \
                 window=[{:#x},{:#x}] limit={limit:#x} fiber={} sp_above_limit={} \
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
        let restore_limit = if nested && changed_limit {
            prev_limit
        } else {
            0
        };
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
        //     looked-up region's top: that mapping isn't reliably contiguous, so
        //     the scan could still hit a hole below it.
        //   * a fiber we were ALREADY on (a nested op under a host callback that
        //     didn't switch stacks) — the enclosing op's start, which is above
        //     ours and covers both ops' frames. Accepted only when it is at or
        //     below the highest address an op has been seen running at on this
        //     stack (the cache's `seen`, NOT the mapping top, which can reach far
        //     into an unrelated neighbour — see current_region_bounds_cached);
        //     otherwise we cannot tell it is on our stack at all, and adopting it
        //     would send the scan up through our guard page. The enclosing op
        //     ran on this stack at or below `seen` — it widened the entry to its
        //     own `stack_top` if nothing had — and an entry is only ever replaced
        //     by a fresh lookup overlapping it, which for a stack still mapped is
        //     the same stack with a higher `seen`. So the test holds for every
        //     enclosing op on our stack whose lookup went through this thread's
        //     table; one that ran on another native thread (an M:N Ruby thread
        //     that resumed elsewhere inside a host callback) is unknown here, and
        //     the scan narrows to this op's own frame — the residual below.
        let new_scan_start = if on_native {
            unsafe { v8__base__Stack__GetStackStart() }
        } else if nested && region.0 != 0 && prev_scan_start > sp && prev_scan_start <= region.1 {
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
            self.installed_limit
                .store(self.restore_limit, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stacks(entries: &[(usize, usize)]) -> FiberStacks {
        FiberStacks(
            entries
                .iter()
                .map(|&(lo, seen)| FiberStack { lo, seen })
                .collect(),
        )
    }

    #[test]
    fn a_lookup_hits_only_inside_a_window() {
        let table = stacks(&[(0x1000, 0x1800), (0x3000, 0x3400)]);
        assert_eq!(table.find(0x0fff), None);
        assert_eq!(
            table.find(0x1000),
            Some(FiberStack {
                lo: 0x1000,
                seen: 0x1800
            })
        );
        assert_eq!(table.find(0x1800).map(|s| s.lo), Some(0x1000));
        // Above `seen` is not known to be on that stack, though no other entry
        // claims it.
        assert_eq!(table.find(0x1801), None);
        assert_eq!(table.find(0x3200).map(|s| s.lo), Some(0x3000));
        assert_eq!(table.find(0x3401), None);
    }

    #[test]
    fn a_stack_is_looked_up_once_and_again_only_above_its_window() {
        let mut table = stacks(&[]);
        let queries = std::cell::Cell::new(0);
        let mut bounds = |addr| {
            table.bounds(addr, |_| {
                queries.set(queries.get() + 1);
                0x1000
            })
        };
        assert_eq!(bounds(0x1800), (0x1000, 0x1800));
        assert_eq!(bounds(0x1400), (0x1000, 0x1800));
        assert_eq!(bounds(0x1800), (0x1000, 0x1800));
        assert_eq!(queries.get(), 1);
        assert_eq!(bounds(0x1c00), (0x1000, 0x1c00));
        assert_eq!(bounds(0x1900), (0x1000, 0x1c00));
        assert_eq!(queries.get(), 2);
    }

    #[test]
    fn an_unknown_bottom_is_not_recorded() {
        let mut table = stacks(&[]);
        assert_eq!(table.bounds(0x1800, |_| 0), (0, 0));
        assert_eq!(table.0, Vec::new());
    }

    #[test]
    fn a_frame_above_the_window_widens_the_same_entry() {
        let mut table = stacks(&[(0x1000, 0x1800), (0x3000, 0x3400)]);
        table.record(0x1000, 0x1c00);
        assert_eq!(table.0, stacks(&[(0x1000, 0x1c00), (0x3000, 0x3400)]).0);
    }

    #[test]
    fn a_new_stack_is_inserted_in_order() {
        let mut table = stacks(&[(0x1000, 0x1800), (0x5000, 0x5400)]);
        table.record(0x3000, 0x3100);
        table.record(0x0100, 0x0200);
        table.record(0x9000, 0x9100);
        assert_eq!(
            table.0,
            stacks(&[
                (0x0100, 0x0200),
                (0x1000, 0x1800),
                (0x3000, 0x3100),
                (0x5000, 0x5400),
                (0x9000, 0x9100)
            ])
            .0
        );
    }

    #[test]
    fn a_fresh_bottom_replaces_every_entry_it_overlaps() {
        // Memory once holding two stacks, now one mapping from 0x1400: both old
        // windows reach into [0x1400, 0x3200], so neither may answer again.
        let mut table = stacks(&[
            (0x0100, 0x0200),
            (0x1000, 0x1800),
            (0x3000, 0x3400),
            (0x5000, 0x5400),
        ]);
        table.record(0x1400, 0x3200);
        assert_eq!(
            table.0,
            stacks(&[(0x0100, 0x0200), (0x1400, 0x3200), (0x5000, 0x5400)]).0
        );
        assert_eq!(table.find(0x1200), None);
    }

    // A mapping laid out as Ruby's fiber pool lays a stack out: read-write
    // pages with PROT_NONE guard pages among them. Unmapped on drop.
    #[cfg(target_os = "linux")]
    struct GuardedStack {
        base: usize,
        len: usize,
    }

    #[cfg(target_os = "linux")]
    impl GuardedStack {
        fn page() -> usize {
            unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
        }

        // `pages` pages, of which those at the `guards` indexes are PROT_NONE.
        fn map(pages: usize, guards: &[usize]) -> Self {
            let len = pages * Self::page();
            let stack = Self {
                base: Self::mmap(0, len, 0),
                len,
            };
            stack.protect(guards);
            stack
        }

        // Map the same addresses afresh (MAP_FIXED replaces the old mapping in
        // one step, so no other thread's mmap can land in between), with other
        // guards.
        fn remap(&self, guards: &[usize]) {
            assert_eq!(Self::mmap(self.base, self.len, libc::MAP_FIXED), self.base);
            self.protect(guards);
        }

        fn mmap(at: usize, len: usize, flags: libc::c_int) -> usize {
            let base = unsafe {
                libc::mmap(
                    at as *mut c_void,
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | flags,
                    -1,
                    0,
                )
            };
            assert_ne!(base, libc::MAP_FAILED);
            base as usize
        }

        fn protect(&self, guards: &[usize]) {
            for &guard in guards {
                let page = self.page_at(guard) as *mut c_void;
                assert_eq!(
                    unsafe { libc::mprotect(page, Self::page(), libc::PROT_NONE) },
                    0
                );
            }
        }

        fn page_at(&self, index: usize) -> usize {
            self.base + index * Self::page()
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for GuardedStack {
        fn drop(&mut self) {
            unsafe { libc::munmap(self.base as *mut c_void, self.len) };
        }
    }

    // Both ways of asking agree with each other and with the layout.
    #[cfg(target_os = "linux")]
    fn assert_bottom(addr: usize, expected: usize) {
        assert_eq!(query_region_bottom(addr), expected);
        let mut maps = std::fs::File::open("/proc/self/maps").unwrap();
        assert_eq!(read_maps_bottom(&mut maps, addr), expected);
        // Older kernels have no PROCMAP_QUERY; where there is one it must agree.
        if let Some(bottom) = procmap_query_bottom(&maps, addr) {
            assert_eq!(bottom, expected);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_bottom_is_the_page_above_the_guard() {
        let stack = GuardedStack::map(8, &[0]);
        assert_bottom(stack.page_at(5) + 17, stack.page_at(1));
        assert_bottom(stack.page_at(1), stack.page_at(1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_stack_mapped_again_with_other_guards_has_its_new_bottom() {
        // Unmap a fiber's stack and map the same addresses again with a guard
        // inside the old one: a lookup is a fresh question, so it answers with
        // the new bottom, never the one the old mapping had.
        let stack = GuardedStack::map(8, &[0]);
        assert_bottom(stack.page_at(6), stack.page_at(1));
        stack.remap(&[0, 4]);
        assert_bottom(stack.page_at(6), stack.page_at(5));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_live_stack_is_found_where_it_runs() {
        // This thread's own stack: the frame is somewhere above its bottom.
        let marker = 0u8;
        let addr = &marker as *const u8 as usize;
        let bottom = query_region_bottom(addr);
        assert_ne!(bottom, 0);
        assert!(bottom <= addr);
    }
}
