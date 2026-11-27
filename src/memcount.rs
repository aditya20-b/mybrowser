//! A global allocator that keeps count of the bytes each thread holds, so a script can be limited
//! by the memory it really uses (a counter of "bytes asked for" only goes up, and would stop an
//! ordinary script that builds and drops a lot of strings). The count is per thread: a script
//! runs on a thread of its own, so what other threads do (other pages, image decoding, tests
//! running side by side) doesn't count against it. A thread that frees what another allocated
//! just goes below zero, which is why the count is signed and only ever compared as a difference.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

pub struct Counting;

thread_local! {
    static LIVE: Cell<isize> = const { Cell::new(0) };
}

fn add(n: isize) {
    // (during thread teardown the slot may be gone; then the bytes simply aren't counted)
    let _ = LIVE.try_with(|c| c.set(c.get().wrapping_add(n)));
}

// SAFETY: every call is passed straight to the system allocator; the counter only adds and subtracts the sizes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            add(layout.size() as isize);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        add(-(layout.size() as isize));
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            add(layout.size() as isize);
        }
        p
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            add(new_size as isize - layout.size() as isize);
        }
        p
    }
}

/// Net bytes this thread has allocated (compare two readings; the absolute value means nothing).
pub fn live_bytes() -> isize {
    LIVE.try_with(|c| c.get()).unwrap_or(0)
}
