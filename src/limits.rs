/// Caps on one call. Go had none; the defaults are finite. Every public entry point takes
/// `&Limits` and a cancel flag.
///
/// # Threading
///
/// There is no `max_threads` field, on purpose: an exception to the shared convention that every
/// crate's `Limits` has one, kept so that tiff-rs, which builds the `Limits { max_output,
/// max_alloc }` struct literal, compiles unchanged. Every call runs on the calling thread only
/// (the equivalent of `max_threads = 1`), except [`decode_parallel_into`](crate::decode_parallel_into)
/// and [`zlib::decode_parallel_into`](crate::zlib::decode_parallel_into), which take
/// `max_threads` as an argument; `max_alloc` is then the peak of all their threads together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most bytes of output (default 1 GiB), in bytes. For the predictor it is the bytes the call
    /// processes. More is [`Error::OutputLimit`](crate::Error::OutputLimit).
    pub max_output: usize,
    /// The peak working memory one call may hold (default 1 GiB), in bytes: the output buffer
    /// (charged as it grows, before each allocation), a [`Reader`](crate::Reader)'s window and a
    /// preset dictionary (released when the reader is dropped). It is not an up-front estimate and
    /// it is never taken from a size in the input. Fixed tables (under 64 KiB) are not charged.
    /// One exception to the peak: a growing output `Vec` that cannot be resized in place briefly
    /// holds the old block too (allocator-dependent).
    /// More is [`Error::AllocLimit`](crate::Error::AllocLimit); an allocator that refuses a size
    /// within the limit is [`Error::OutOfMemory`](crate::Error::OutOfMemory).
    pub max_alloc: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_output: 1 << 30,
            max_alloc: 1 << 30,
        }
    }
}
