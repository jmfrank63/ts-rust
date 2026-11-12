//! The counting allocator of the test binaries: `System`, with the bytes
//! it holds in `LIVE`. The wasm tests read it (`release_checker`), and so
//! do the memory soaks (`soak`, feature `soak`), whose trace hooks it
//! calls on each allocation.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The bytes that the test binary holds: allocated and not yet freed.
pub static LIVE: AtomicUsize = AtomicUsize::new(0);

/// `System`, counting in `LIVE`.
struct Counting;

// SAFETY: each call goes to `System` with the caller's arguments.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller keeps the `GlobalAlloc::alloc` contract.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
            #[cfg(feature = "soak")]
            crate::soak::on_alloc(ptr, layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        #[cfg(feature = "soak")]
        crate::soak::on_dealloc(ptr);
        // SAFETY: the caller keeps the `GlobalAlloc::dealloc` contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller keeps the `GlobalAlloc::realloc` contract.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            LIVE.fetch_add(new_size, Ordering::Relaxed);
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            #[cfg(feature = "soak")]
            {
                crate::soak::on_dealloc(ptr);
                crate::soak::on_alloc(new, new_size);
            }
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;
