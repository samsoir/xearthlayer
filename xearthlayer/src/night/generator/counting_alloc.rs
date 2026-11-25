//! A global allocator for tests that measures how many heap bytes a closure
//! holds at its peak, on the calling thread, beyond what that thread held
//! when it started (#440). Test-only: the library's unit tests install it
//! for the lit encode's timing test (`encode_timing`), and the integration
//! test binary `lit_encode_memory` includes this file by path and installs
//! it there. Each user declares the `#[global_allocator]` itself.
//!
//! Counts are per thread, so other tests running at the same time do not
//! disturb a measurement: a thread's count is what it allocated less what
//! it freed. Memory another thread frees is counted on that thread; the
//! measured closures allocate and free on their own thread. Allocations by
//! C code (`malloc`) are not seen; the lit encode's kernels make none.
//! Every other thread pays two thread-local updates per allocation.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// [`System`], counting bytes per thread.
pub struct CountingAllocator;

thread_local! {
    /// Bytes this thread holds: allocated here, less freed here.
    static HELD: Cell<isize> = const { Cell::new(0) };
    /// The most [`HELD`] has been since a measurement began.
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

/// Adds `delta` to this thread's count. Ignored once the thread's locals
/// are gone, as they are while the thread exits.
fn count(delta: isize) {
    let _ = HELD.try_with(|held| {
        let now = held.get() + delta;
        held.set(now);
        let _ = PEAK.try_with(|peak| peak.set(peak.get().max(now)));
    });
}

// SAFETY: every call is forwarded to `System` with its own arguments; the
// counting touches only `Cell`s of the calling thread and never allocates.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            count(layout.size() as isize);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            count(layout.size() as isize);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        count(-(layout.size() as isize));
    }

    /// Counted as the change in size: a reallocation that moves holds both
    /// blocks for a moment, which this does not see.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let moved = unsafe { System.realloc(ptr, layout, new_size) };
        if !moved.is_null() {
            count(new_size as isize - layout.size() as isize);
        }
        moved
    }
}

/// What `f` returns, and the most heap bytes the calling thread held while
/// it ran beyond what it held when `f` started (what `f` returns included,
/// if it is on the heap). Requires [`CountingAllocator`] to be the global
/// allocator; measurements on one thread must not nest.
pub fn held_at_peak<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let start = HELD.with(Cell::get);
    PEAK.with(|peak| peak.set(start));
    let out = f();
    let peak = PEAK.with(Cell::get);
    (out, usize::try_from(peak - start).unwrap_or(0))
}
