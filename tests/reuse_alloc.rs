//! Buffer reuse: a worker that decodes tile after tile with one [`deflate::Decoder`] (or
//! [`deflate::zlib::Decoder`]) and one cleared output `Vec` allocates nothing after the first
//! decode. A counting global allocator counts the allocation calls of the second decode of the
//! same stream, which must be 0, for every fixture without a dictionary.
//!
//! One `#[test]` only, so the harness runs no other thread that allocates meanwhile.

mod common;
use common::*;
use deflate::{Decoder, Limits, zlib};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

/// Calls of `alloc`, `alloc_zeroed` and `realloc`.
static CALLS: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: every method forwards to `System` with the caller's arguments; it only adds a counter.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        CALLS.fetch_add(1, Relaxed);
        // SAFETY: the caller upholds `alloc`'s contract, which `System` shares.
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        CALLS.fetch_add(1, Relaxed);
        // SAFETY: as `alloc`.
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: `p` came from this allocator, so from `System`, with layout `l`.
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        CALLS.fetch_add(1, Relaxed);
        // SAFETY: as `dealloc`, and the caller upholds `realloc`'s contract.
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static A: Counting = Counting;

#[test]
fn second_decode_of_the_same_size_allocates_nothing() {
    let (l, c) = (Limits::default(), &NO_CANCEL);
    let mut checked = 0;
    for f in fixtures().into_iter().filter(|f| f.dict.is_empty()) {
        let z = f.file.ends_with(".zlib");
        let (mut d, mut zd) = (Decoder::new(), zlib::Decoder::new());
        let mut out = Vec::new();
        let mut decode = |out: &mut Vec<u8>| {
            out.clear();
            if z {
                zd.decode_into(&f.data, out, &l, c)
            } else {
                d.decode_into(&f.data, out, &l, c)
            }
        };
        let first = decode(&mut out);
        let want = out.clone();
        let before = CALLS.load(Relaxed);
        let second = decode(&mut out);
        let calls = CALLS.load(Relaxed) - before;
        assert_eq!(first, second, "{}", f.file);
        assert!(out == want, "{}: the second output differs", f.file);
        if first.is_ok() {
            assert_eq!(calls, 0, "{}: the second decode allocated", f.file);
            checked += 1;
        }
    }
    assert!(checked > 100, "{checked} fixtures");
}
