//! zlib (RFC 1950): the header, the DEFLATE body and the Adler-32 trailer. Output and errors
//! equal Go `compress/zlib`.

use crate::{
    Error, Limits, adler32,
    inflate::{Ended, WINDOW, run},
};
use std::io::{self, BufRead, Read};
use std::sync::atomic::{AtomicBool, Ordering};

const DEFLATE: u8 = 8;
const MAX_WINDOW: u8 = 7;

/// Checks the two header bytes, as Go's `Reader.Reset` and `Decoder.decode` do.
fn check_header(h: [u8; 2]) -> Result<(), Error> {
    if h[0] & 0x0f != DEFLATE || h[0] >> 4 > MAX_WINDOW || !u16::from_be_bytes(h).is_multiple_of(31)
    {
        return Err(Error::Header);
    }
    Ok(())
}

/// Inflates the zlib stream `src`, which must not use a preset dictionary other than the empty
/// one. Trailing bytes after the Adler-32 are ignored. Enforces `limits.max_output` and
/// `limits.max_alloc`.
///
/// # Errors
///
/// [`Error::Corrupt`], [`Error::UnexpectedEof`] and [`Error::Internal`] for a bad or short
/// stream (offsets as Go reports them); [`Error::Header`] for a bad zlib header,
/// [`Error::Dictionary`] if the stream names a preset dictionary (use [`Reader::with_dict`]),
/// [`Error::Checksum`] for a wrong Adler-32; [`Error::OutputLimit`] if the output would pass
/// `limits.max_output`; [`Error::AllocLimit`] if the output buffer would pass `limits.max_alloc`;
/// [`Error::OutOfMemory`] if the allocator refuses a size within the limits; [`Error::Cancelled`]
/// if `cancel` is set (checked at each block and every 64 KiB of output). No other variant.
pub fn decode(src: &[u8], limits: &Limits, cancel: &AtomicBool) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    decode_into(src, &mut out, limits, cancel)?;
    Ok(out)
}

/// As [`decode`], appending to `out`. Like Go's `Decode(dst, src)`, `out` holds the output
/// before an error (after a checksum error, all of it), and the bytes already in it are not
/// history.
///
/// # Errors
///
/// [`Error::Corrupt`], [`Error::UnexpectedEof`] and [`Error::Internal`] for a bad or short
/// stream (offsets as Go reports them); [`Error::Header`] for a bad zlib header,
/// [`Error::Dictionary`] if the stream names a preset dictionary (use [`Reader::with_dict`]),
/// [`Error::Checksum`] for a wrong Adler-32; [`Error::OutputLimit`] if the output would pass
/// `limits.max_output`; [`Error::AllocLimit`] if the output buffer would pass `limits.max_alloc`;
/// [`Error::OutOfMemory`] if the allocator refuses a size within the limits; [`Error::Cancelled`]
/// if `cancel` is set (checked at each block and every 64 KiB of output). No other variant.
pub fn decode_into(
    src: &[u8],
    out: &mut Vec<u8>,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    decode_n(src, usize::MAX, out, limits, cancel, None)
}

/// As [`decode_into`], with up to `max_threads` threads on one large stream, as
/// [`crate::decode_parallel_into`]: the same output, errors and limits as [`decode_into`]; the
/// Adler-32 is checked after the parallel decode, and a stream that fails any check is decoded
/// again serially, which reports the error.
///
/// # Errors
///
/// As [`decode_into`].
pub fn decode_parallel_into(
    src: &[u8],
    out: &mut Vec<u8>,
    limits: &Limits,
    max_threads: usize,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    decode_parallel_chunked_into(
        src,
        out,
        limits,
        max_threads,
        crate::parallel::MIN_CHUNK,
        cancel,
    )
}

/// [`decode_parallel_into`] with chunks of at least `min_chunk` compressed bytes per worker, as
/// [`crate::decode_parallel_chunked_into`]. Not API.
#[doc(hidden)]
pub fn decode_parallel_chunked_into(
    src: &[u8],
    out: &mut Vec<u8>,
    limits: &Limits,
    max_threads: usize,
    min_chunk: usize,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    let threads = crate::parallel::workers(src.len(), max_threads, min_chunk);
    if threads >= 2
        && let [a, b, body @ ..] = src
        && check_header([*a, *b]).is_ok()
        && b & 0x20 == 0
    {
        let base = out.len();
        if let Some(end) = crate::parallel::decode(body, out, limits, threads, cancel)? {
            // The Adler-32 follows the byte of the final block's last bit.
            let at = usize::try_from(end.div_ceil(8)).unwrap_or(usize::MAX);
            let t = body.get(at..).and_then(|r| r.first_chunk::<4>());
            if t.is_some_and(|t| u32::from_be_bytes(*t) == adler32::sum(1, &out[base..])) {
                return Ok(());
            }
            out.truncate(base);
        }
    }
    decode_into(src, out, limits, cancel)
}

/// Inflates at most `n` bytes of the zlib stream `src`: as `io.LimitReader(n)` over Go's
/// `zlib.NewReader`. A stop at `n` does not read the Adler-32, so a stream with a bad or
/// missing trailer decodes. Enforces `limits.max_output` and `limits.max_alloc`.
///
/// # Errors
///
/// [`Error::Corrupt`], [`Error::UnexpectedEof`] and [`Error::Internal`] for a bad or short
/// stream (offsets as Go reports them); [`Error::Header`] for a bad zlib header,
/// [`Error::Dictionary`] if the stream names a preset dictionary (use [`Reader::with_dict`]),
/// [`Error::Checksum`] for a wrong Adler-32; [`Error::OutputLimit`] if the output would pass
/// `limits.max_output`; [`Error::AllocLimit`] if the output buffer would pass `limits.max_alloc`;
/// [`Error::OutOfMemory`] if the allocator refuses a size within the limits; [`Error::Cancelled`]
/// if `cancel` is set (checked at each block and every 64 KiB of output). No other variant.
pub fn decode_limit(
    src: &[u8],
    n: usize,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    decode_limit_into(src, n, &mut out, limits, cancel)?;
    Ok(out)
}

/// As [`decode_limit`], appending to `out`, which holds the output before an error as
/// [`decode_into`] says.
///
/// # Errors
///
/// [`Error::Corrupt`], [`Error::UnexpectedEof`] and [`Error::Internal`] for a bad or short
/// stream (offsets as Go reports them); [`Error::Header`] for a bad zlib header,
/// [`Error::Dictionary`] if the stream names a preset dictionary (use [`Reader::with_dict`]),
/// [`Error::Checksum`] for a wrong Adler-32; [`Error::OutputLimit`] if the output would pass
/// `limits.max_output`; [`Error::AllocLimit`] if the output buffer would pass `limits.max_alloc`;
/// [`Error::OutOfMemory`] if the allocator refuses a size within the limits; [`Error::Cancelled`]
/// if `cancel` is set (checked at each block and every 64 KiB of output). No other variant.
pub fn decode_limit_into(
    src: &[u8],
    n: usize,
    out: &mut Vec<u8>,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    decode_n(src, n, out, limits, cancel, None)
}

/// `decode_into` for `n == usize::MAX`, else `decode_limit_into`.
fn decode_n(
    src: &[u8],
    n: usize,
    out: &mut Vec<u8>,
    limits: &Limits,
    cancel: &AtomicBool,
    dec: Option<&mut crate::Decoder>,
) -> Result<(), Error> {
    let [a, b, rest @ ..] = src else {
        return Err(Error::UnexpectedEof);
    };
    check_header([*a, *b])?;
    let mut off = 2;
    if b & 0x20 != 0 {
        let Some(id) = rest.first_chunk::<4>() else {
            return Err(Error::UnexpectedEof);
        };
        // No dictionary was given: adler32 of nothing.
        if u32::from_be_bytes(*id) != adler32::sum(1, &[]) {
            return Err(Error::Dictionary);
        }
        off = 6;
    }
    if n == 0 {
        return Ok(());
    }
    let base = out.len();
    // The Adler-32 is summed during the decode, while the output is in cache, as Go does. A stop
    // at `n` skips the trailer, so it is not summed when `n` is a multiple of 32 KiB, stdlib's
    // window flush, where a tile normally stops (deflate-go `zlib.Decoder.decode`).
    let fuse = n == usize::MAX || !n.is_multiple_of(WINDOW);
    let mut fused = 1;
    let (c, ended) = run(
        &src[off..],
        n,
        out,
        limits,
        cancel,
        true,
        fuse.then_some(&mut fused),
        dec,
    )?;
    if matches!(ended, Ended::Stop) {
        return Ok(());
    }
    let Some(t) = src[off + c..].first_chunk::<4>() else {
        return Err(Error::UnexpectedEof);
    };
    // Else the checksum of up to `max_output` bytes, with the cancel flag checked every 64 KiB.
    let mut sum = fused;
    if !fuse {
        for chunk in out[base..].chunks(1 << 16) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            sum = adler32::sum(sum, chunk);
        }
    }
    if u32::from_be_bytes(*t) != sum {
        return Err(Error::Checksum);
    }
    Ok(())
}

/// A reusable zlib decoder: Go's `zlib.Decoder`. As [`crate::Decoder`]: it keeps the Huffman
/// tables of the last dynamic block, so a second decode into a reused `Vec` allocates nothing;
/// output, limits and errors are those of [`decode_into`] and [`decode_limit_into`].
#[derive(Debug, Default)]
pub struct Decoder(crate::Decoder);

impl Decoder {
    /// A decoder with no tables yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// [`decode_into`] with this decoder's tables.
    ///
    /// # Errors
    ///
    /// As [`decode_into`].
    pub fn decode_into(
        &mut self,
        src: &[u8],
        out: &mut Vec<u8>,
        limits: &Limits,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        decode_n(src, usize::MAX, out, limits, cancel, Some(&mut self.0))
    }

    /// [`decode_limit_into`] with this decoder's tables.
    ///
    /// # Errors
    ///
    /// As [`decode_limit_into`].
    pub fn decode_limit_into(
        &mut self,
        src: &[u8],
        n: usize,
        out: &mut Vec<u8>,
        limits: &Limits,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        decode_n(src, n, out, limits, cancel, Some(&mut self.0))
    }
}

/// Streaming zlib decoder: Go's `zlib.NewReader`. See [`crate::Reader`] for the input rules.
/// The Adler-32 is checked when `read` reaches the end, which then returns 0. It enforces
/// `limits.max_output` and `limits.max_alloc` as [`crate::Reader`] does, on the calling thread.
pub struct Reader<'a, R> {
    inner: crate::Reader<'a, R>,
    digest: u32,
    err: Option<Error>,
    done: bool,
}

impl<'a, R: BufRead> Reader<'a, R> {
    /// Reads the zlib header from `r`; a stream that names a preset dictionary needs
    /// [`Reader::with_dict`].
    ///
    /// # Errors
    ///
    /// [`Error::Header`] for a bad header, [`Error::Dictionary`] if the stream names a preset
    /// dictionary (only an empty one matches), [`Error::UnexpectedEof`] if `r` ends in the header,
    /// [`Error::Io`] for an error from `r`. Nothing is allocated here, so no limit applies. After
    /// this, `read` returns the errors of [`crate::Reader`] and [`Error::Checksum`], as the source
    /// of an `io::Error`.
    pub fn new(r: R, limits: &Limits, cancel: &'a AtomicBool) -> Result<Self, Error> {
        Self::with_dict(r, &[], limits, cancel)
    }

    /// As [`Reader::new`], with the preset dictionary the stream was written with. A stream
    /// without a dictionary ignores `dict`.
    ///
    /// # Errors
    ///
    /// As [`Reader::new`], and [`Error::Dictionary`] if the Adler-32 of `dict` is not the one in
    /// the header. [`Error::OutputLimit`] if `dict` is longer than `limits.max_output` (it is
    /// checksummed first). The window and the last 32 KiB of `dict` are charged against
    /// `limits.max_alloc` and fail the first `read` with [`Error::AllocLimit`].
    pub fn with_dict(
        mut r: R,
        dict: &[u8],
        limits: &Limits,
        cancel: &'a AtomicBool,
    ) -> Result<Self, Error> {
        let mut h = [0u8; 2];
        r.read_exact(&mut h)?;
        check_header(h)?;
        let have_dict = h[1] & 0x20 != 0;
        if have_dict {
            let mut id = [0u8; 4];
            r.read_exact(&mut id)?;
            if u32::from_be_bytes(id) != adler32::update(1, dict, limits)? {
                return Err(Error::Dictionary);
            }
        }
        Ok(Self {
            inner: crate::Reader::with_dict(r, if have_dict { dict } else { &[] }, limits, cancel),
            digest: 1,
            err: None,
            done: false,
        })
    }

    /// As [`Reader::new`], but the output buffer starts as `buf`: see [`crate::Reader::with_buffer`].
    ///
    /// # Errors
    ///
    /// As [`Reader::new`].
    pub fn with_buffer(
        r: R,
        buf: Vec<u8>,
        limits: &Limits,
        cancel: &'a AtomicBool,
    ) -> Result<Self, Error> {
        let mut s = Self::new(r, limits, cancel)?;
        s.inner.use_buffer(buf, limits);
        Ok(s)
    }

    /// The output buffer, cleared, with its capacity kept: see [`crate::Reader::into_buffer`].
    pub fn into_buffer(self) -> Vec<u8> {
        self.inner.into_buffer()
    }
}

impl<R: BufRead> Read for Reader<'_, R> {
    fn read(&mut self, p: &mut [u8]) -> io::Result<usize> {
        if let Some(e) = &self.err {
            return Err(e.clone().into());
        }
        if p.is_empty() {
            // The stored error of the inner reader, else nothing, as Go's `Read(nil)` (which
            // also runs a decode step).
            return self.inner.read(p);
        }
        if self.done {
            return Ok(0);
        }
        let n = self.inner.read(p)?;
        if n > 0 {
            self.digest = adler32::sum(self.digest, &p[..n]);
            return Ok(n);
        }
        // The stream ended: check the trailer, big-endian.
        let mut t = [0u8; 4];
        let r = self
            .inner
            .get_mut()
            .read_exact(&mut t)
            .map_err(Error::from)
            .and_then(|()| {
                if u32::from_be_bytes(t) == self.digest {
                    Ok(())
                } else {
                    Err(Error::Checksum)
                }
            });
        match r {
            Ok(()) => {
                self.done = true;
                Ok(0)
            }
            Err(e) => {
                self.err = Some(e.clone());
                Err(e.into())
            }
        }
    }
}
