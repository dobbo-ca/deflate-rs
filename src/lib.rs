//! DEFLATE (RFC 1951) and zlib (RFC 1950) decoding, plus TIFF predictor 2 and RGBA expand.
//!
//! The output of a decode that succeeds, and the error of one that fails, equal Go 1.27
//! `compress/flate` and `compress/zlib` for every input. As in Go, a failed decode has output
//! too: the `*_into` functions leave it in the caller's `Vec`; the others return `Err` only.
//! Raw DEFLATE is at the crate root, zlib in [`zlib`], the predictor in [`predictor`].
//! Every entry point takes [`Limits`] and a cancel flag.
//!
//! # Resource limits
//!
//! Every call is bounded by a [`Limits`] and a cancel flag (`&AtomicBool`, set from another
//! thread). `Limits` has two fields:
//!
//! - `max_output` (bytes, default 1 GiB): the most output; more is [`Error::OutputLimit`]. For the
//!   predictor it is the bytes the call processes.
//! - `max_alloc` (bytes, default 1 GiB): the peak working memory of the whole call. Every
//!   allocation that grows with the input (the output buffer, a reader's window, a preset
//!   dictionary) is charged when it is allocated and released when the call returns or the reader
//!   is dropped; there is no up-front estimate. Fixed tables (under 64 KiB) are not charged. A
//!   growing output `Vec` that cannot be resized in place briefly holds the old block too (the
//!   one exception to the peak; allocator-dependent). More
//!   is [`Error::AllocLimit`]; an allocator that refuses a size within the limit is
//!   [`Error::OutOfMemory`].
//!
//! The cancel flag is checked at each DEFLATE block, and inside a block every 64 KiB of output;
//! a set flag is [`Error::Cancelled`] (a reader's `io::Error` has kind `Other`).
//!
//! Threads: `Limits` has no `max_threads` field, deliberately (an exception to the shared
//! convention), so that tiff-rs, which builds the struct literal `Limits { max_output, max_alloc
//! }`, compiles unchanged. Every function runs on the calling thread only, except
//! [`decode_parallel_into`] and [`zlib::decode_parallel_into`], which take `max_threads` as an
//! argument (the calling thread included; 1 spawns nothing): they decode one large stream with
//! up to that many threads, spawned for the call (`std::thread::scope`, no pool), and give the
//! same output and errors as the serial decode. `max_alloc` is the peak of all of them together.
//!
//! ```
//! use deflate::Limits;
//! use std::sync::atomic::AtomicBool;
//!
//! # fn main() -> Result<(), deflate::Error> {
//! // zlib stream of b"hello".
//! let z = [
//!     0x78, 0x9c, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00, 0x06, 0x2c, 0x02, 0x15,
//! ];
//! let limits = Limits { max_output: 1 << 20, max_alloc: 64 << 20 };
//! let cancel = AtomicBool::new(false);
//! assert_eq!(deflate::zlib::decode(&z, &limits, &cancel)?, b"hello");
//! // "hello" is 5 bytes, over a max_output of 4.
//! let tight = Limits { max_output: 4, ..limits };
//! assert_eq!(deflate::zlib::decode(&z, &tight, &cancel), Err(deflate::Error::OutputLimit));
//! # Ok(())
//! # }
//! ```
#![deny(
    unsafe_code,
    unsafe_op_in_unsafe_fn,
    clippy::undocumented_unsafe_blocks
)]
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented
    )
)]

/// The checksum kernel, public only so tests and the fuzz target reach it; it is not API.
#[doc(hidden)]
pub mod adler32;
mod error;
mod fast;
mod header;
mod huffman;
mod inflate;
mod limits;
mod parallel;
pub mod predictor;
mod reader;
pub mod zlib;

pub use error::Error;
pub use inflate::{
    Decoder, decode, decode_into, decode_limit, decode_limit_into, decode_parallel_into,
};
#[doc(hidden)]
pub use inflate::{decode_careful_into, decode_parallel_chunked_into};
pub use limits::Limits;
pub use reader::Reader;

/// How many parallel decodes of two spans or more in this process did not fall back to the serial
/// decode. Not API: the tests check that the parallel path runs.
#[doc(hidden)]
pub fn parallel_decodes() -> usize {
    parallel::DONE.load(std::sync::atomic::Ordering::Relaxed)
}

/// The most threads the parallel decode had spawned and alive at once since the last call (the
/// calling thread not counted), and resets it. Not API: the tests check it against `max_threads`.
#[doc(hidden)]
pub fn parallel_thread_peak() -> usize {
    parallel::PEAK.swap(0, std::sync::atomic::Ordering::Relaxed)
}

/// The kernel tiers dispatch picked on this CPU, as `[("adler32", tier), ("fast", tier)]`:
/// adler32 (its sum and its fused copy) is "avx512_vnni", "avx2", "neon" or "scalar", the decode
/// loop "avx2" or "generic". Not API: the tests check that the fast tiers run.
#[doc(hidden)]
pub fn tiers() -> [(&'static str, &'static str); 2] {
    [("adler32", adler32::tier()), ("fast", fast::tier())]
}
