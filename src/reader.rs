use crate::{
    Error, Limits,
    inflate::{Halt, Inflate, WINDOW},
};
use std::io::{self, BufRead, Read};
use std::sync::atomic::AtomicBool;

/// Streaming raw DEFLATE decoder: Go's `flate.NewReader`.
///
/// It reads from `R` only the bytes it consumes, so after `Read` returns 0 the stream's
/// trailer (zlib's Adler-32, a PNG chunk) is next in `R`. Wrap a plain reader in
/// [`std::io::BufReader`]. Errors from `R` come back as their `io::ErrorKind`, and the
/// [`Error`] of a bad stream is the source of the `io::Error`. An error is final; one of kind
/// `Interrupted` from `R` is returned as `Other`, because `read_to_end` retries `Interrupted`.
/// `Limits::max_alloc` must be at least about 100 KiB: the window and one step of output.
///
/// `cancel` is checked at each DEFLATE block, and inside a Huffman block every 64 KiB of output:
/// a stream can produce no output for a long time (empty blocks), so a `read` can run long. Once
/// it is set, `read` returns [`Error::Cancelled`] (kind `Other`).
pub struct Reader<'a, R> {
    inf: Inflate<'a, R>,
    /// `inf.out.buf[..rpos]` has been returned.
    rpos: usize,
    /// Output dropped from the front of `inf.out.buf`: `shifted + rpos` is the position in the history.
    shifted: usize,
    err: Option<Error>,
    eof: bool,
    /// The last step ended at a sync flush: what is there is returned, short of the buffer.
    flushed: bool,
}

impl<'a, R: BufRead> Reader<'a, R> {
    /// A reader over the raw DEFLATE stream in `r`. It allocates nothing until `read`.
    ///
    /// Every `read` can return, as the source of an `io::Error`: [`Error::Corrupt`],
    /// [`Error::UnexpectedEof`], [`Error::Internal`] for a bad or short stream, [`Error::Io`] for
    /// an error from `r`, [`Error::OutputLimit`] past `limits.max_output`, [`Error::AllocLimit`]
    /// past `limits.max_alloc` (the window needs about 100 KiB), [`Error::OutOfMemory`], and
    /// [`Error::Cancelled`] once `cancel` is set. The memory is released when the reader is
    /// dropped.
    pub fn new(r: R, limits: &Limits, cancel: &'a AtomicBool) -> Self {
        Self::with_dict(r, &[], limits, cancel)
    }

    /// As [`Reader::new`], with a preset dictionary (the last 32 KiB of it count). It is charged
    /// against `Limits::max_alloc`; if it does not fit, the first `read` fails with
    /// [`Error::AllocLimit`] (or [`Error::OutOfMemory`] if the allocator refuses it). `read`
    /// returns the errors of [`Reader::new`].
    pub fn with_dict(r: R, dict: &[u8], limits: &Limits, cancel: &'a AtomicBool) -> Self {
        let dict = &dict[dict.len().saturating_sub(WINDOW)..];
        let mut inf = Inflate::new(r, limits, cancel);
        let mut err = None;
        if dict.len() > limits.max_alloc {
            err = Some(Error::AllocLimit);
        } else if inf.out.buf.try_reserve_exact(dict.len()).is_err() {
            err = Some(Error::OutOfMemory);
        } else {
            inf.out.buf.extend_from_slice(dict);
        }
        let n = inf.out.buf.len();
        inf.out.max_len = limits.max_output.saturating_add(n);
        Self {
            inf,
            rpos: n,
            shifted: 0,
            err,
            eof: false,
            flushed: false,
        }
    }

    /// As [`Reader::new`], but the output and window buffer starts as `buf` (cleared), so a reader
    /// over a buffer taken from [`Reader::into_buffer`] allocates nothing if it needs no more than
    /// the last one did. The held capacity counts against `limits.max_alloc` as in `new`: a `buf`
    /// with more capacity than `limits.max_alloc` is shrunk to it. `read` returns the errors of
    /// [`Reader::new`].
    pub fn with_buffer(r: R, buf: Vec<u8>, limits: &Limits, cancel: &'a AtomicBool) -> Self {
        let mut s = Self::new(r, limits, cancel);
        s.use_buffer(buf, limits);
        s
    }

    /// Starts the output buffer from `buf`; only before the first `read`, with no dictionary.
    pub(crate) fn use_buffer(&mut self, mut buf: Vec<u8>, limits: &Limits) {
        buf.clear();
        buf.shrink_to(limits.max_alloc);
        self.inf.out.buf = buf;
    }

    /// The output and window buffer, cleared, with its capacity kept, to start another reader with
    /// [`Reader::with_buffer`].
    pub fn into_buffer(mut self) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.inf.out.buf);
        buf.clear();
        buf
    }

    /// The underlying reader, for what follows the stream.
    pub(crate) fn get_mut(&mut self) -> &mut R {
        &mut self.inf.bits.r
    }
}

impl<R: BufRead> Read for Reader<'_, R> {
    fn read(&mut self, p: &mut [u8]) -> io::Result<usize> {
        if p.is_empty() {
            // As Go's `Read(nil)`: the stored error, else nothing (Go also runs a decode step).
            return match &self.err {
                Some(e) => Err(e.clone().into()),
                None => Ok(0),
            };
        }
        loop {
            let out = &mut self.inf.out;
            // As Go's (and the standard library's) chunks: a `read` ends at each 32 KiB of the
            // history, and returns less than `p` only there, or at a sync flush, the end or an error.
            let avail = out.buf.len() - self.rpos;
            let want = p.len().min(WINDOW - (self.shifted + self.rpos) % WINDOW);
            if avail >= want || (avail > 0 && (self.err.is_some() || self.eof || self.flushed)) {
                let n = want.min(avail);
                p[..n].copy_from_slice(&out.buf[self.rpos..self.rpos + n]);
                self.rpos += n;
                return Ok(n);
            }
            if let Some(e) = &self.err {
                return Err(e.clone().into());
            }
            if self.eof {
                return Ok(0);
            }
            // Drop what is returned, but keep the last window as history.
            if out.buf.len() >= 2 * WINDOW {
                let k = (out.buf.len() - WINDOW).min(self.rpos);
                out.buf.drain(..k);
                out.max_len = out.max_len.saturating_sub(k);
                self.shifted += k;
                self.rpos -= k;
            }
            out.stop_at = out.buf.len() + WINDOW;
            self.flushed = false;
            match self.inf.run() {
                Halt::Pause => {}
                Halt::Flush => self.flushed = true,
                Halt::Eof | Halt::Limit => self.eof = true,
                Halt::Err(e) => self.err = Some(e),
            }
        }
    }
}
