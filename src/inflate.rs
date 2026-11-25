//! The DEFLATE decoder: a port of the careful path of Go 1.27 compress/flate inflate.go (see
//! NOTICE), one input byte at a time, so every error and every read position equals Go's. The
//! fast loop (`fast.rs`) decodes the common symbols in between and hands back exactly the state
//! this path would have, as in deflate-go; both read the same tables.
//!
//! One decoder serves both the whole-buffer entry points (`oneshot`, over a `&[u8]`) and
//! [`Reader`](crate::Reader). It is a step machine: a step runs until the block ends, the output
//! hits `stop_at`, or an error, and the next call resumes where it stopped.

use crate::{
    Error, Limits, adler32,
    fast::{self, DIST_BITS, LIT, LL_BITS, STOP},
    header::{CODE_ORDER, MAX_NUM_DIST, MAX_NUM_LIT, NUM_CODES, SliceBits, read_dynamic},
    huffman::{CHUNK_BITS, COUNT_MASK, Huff, NUM_CHUNKS, VALUE_SHIFT, accept_lengths},
};
use std::cell::RefCell;
use std::io::BufRead;
use std::sync::atomic::{AtomicBool, Ordering};

/// Go's history window, and the size of a [`Reader`](crate::Reader) output step.
pub(crate) const WINDOW: usize = 1 << 15;
/// The longest dynamic block header in bytes, with room: 14 + 19 * 3 bits, then at most 316
/// code length symbols of up to 7 + 7 bits.
const HEADER_MAX: usize = 600;
/// Output bytes between cancel checks inside one Huffman block.
const CANCEL_STEP: usize = 1 << 16;
/// Spare room the fast loop asks for when less is left: the output grows from there by doubling.
const FAST_ROOM: usize = 4 << 10;
/// Output bytes between the sums of a fused Adler-32, so that it sums bytes still in cache
/// (deflate-go `sumChunk`).
const SUM_STEP: usize = 32 << 10;

/// Why a step stopped. Go keeps these in `f.err` (EOF) and the Reader pause flag.
pub(crate) enum Halt {
    /// The final block ended: Go's `io.EOF`.
    Eof,
    /// The output is at `stop_at` and more is to be written (oneshot).
    Limit,
    /// The output is at `stop_at` (Reader): hand it to the caller.
    Pause,
    /// A sync flush (an empty stored block) ended, in a Reader: Go returns the output so far.
    Flush,
    Err(Error),
}

impl From<Error> for Halt {
    fn from(e: Error) -> Self {
        Self::Err(e)
    }
}

/// The input and the bit buffer: Go's `r`, `b`, `nb` and `roffset`.
pub(crate) struct Bits<R> {
    pub r: R,
    b: u32,
    nb: u32,
    /// Bytes consumed from `r`.
    pub roffset: u64,
}

impl<R: BufRead> Bits<R> {
    fn next_byte(&mut self) -> Result<u8, Error> {
        let buf = self.r.fill_buf()?;
        let c = *buf.first().ok_or(Error::UnexpectedEof)?;
        self.r.consume(1);
        self.roffset += 1;
        Ok(c)
    }

    fn more_bits(&mut self) -> Result<(), Error> {
        let c = self.next_byte()?;
        self.b |= u32::from(c) << self.nb;
        self.nb += 8;
        Ok(())
    }

    fn need(&mut self, n: u32) -> Result<(), Error> {
        while self.nb < n {
            self.more_bits()?;
        }
        Ok(())
    }

    /// Reads `n` (at most 16) bits.
    fn take(&mut self, n: u32) -> Result<u32, Error> {
        self.need(n)?;
        let v = self.b & ((1 << n) - 1);
        self.b >>= n;
        self.nb -= n;
        Ok(v)
    }

    /// Reads the next symbol of `h`: Go's `huffSym`. The lookup runs on zero-padded bits, so the
    /// first guess is a lower bound on the code length and more bytes are read only as needed.
    fn huff_sym(&mut self, h: &Huff) -> Result<usize, Error> {
        let mut n = h.min;
        let (mut nb, mut b) = (self.nb, self.b);
        loop {
            while nb < n {
                match self.next_byte() {
                    Ok(c) => {
                        b |= u32::from(c) << (nb & 31);
                        nb += 8;
                    }
                    Err(e) => {
                        self.b = b;
                        self.nb = nb;
                        return Err(e);
                    }
                }
            }
            let mut chunk = h.chunks[b as usize & (NUM_CHUNKS - 1)];
            n = chunk & COUNT_MASK;
            if n > CHUNK_BITS {
                let i = ((chunk >> VALUE_SHIFT) << h.link_bits) + ((b >> CHUNK_BITS) & h.link_mask);
                chunk = h.links[i as usize];
                n = chunk & COUNT_MASK;
            }
            if n <= nb {
                if n == 0 {
                    // Empty or incomplete code: no symbol here.
                    self.b = b;
                    self.nb = nb;
                    return Err(Error::Corrupt(self.roffset));
                }
                self.b = b >> (n & 31);
                self.nb = nb - n;
                return Ok((chunk >> VALUE_SHIFT) as usize);
            }
        }
    }

    /// Reads the next symbol of the fast table `t` (`table_bits` index bits) with minimum code
    /// length `min`, and returns its entry: `huff_sym` on the same codes, with the same reads and
    /// errors (deflate-go `tableSym`).
    fn table_sym(&mut self, t: &[u32], table_bits: u32, min: u32) -> Result<u32, Error> {
        let mut n = min;
        let (mut nb, mut b) = (self.nb, self.b);
        loop {
            while nb < n {
                match self.next_byte() {
                    Ok(c) => {
                        b |= u32::from(c) << (nb & 31);
                        nb += 8;
                    }
                    Err(e) => {
                        self.b = b;
                        self.nb = nb;
                        return Err(e);
                    }
                }
            }
            let mut e = t[(b & ((1 << table_bits) - 1)) as usize];
            if e & fast::LINK != 0 {
                e = t[((e >> 16) + ((b >> table_bits) & ((1 << (e >> 8 & 15)) - 1))) as usize];
            }
            n = e >> 8 & 15;
            if n <= nb {
                if n == 0 {
                    // Missing from an incomplete code.
                    self.b = b;
                    self.nb = nb;
                    return Err(Error::Corrupt(self.roffset));
                }
                self.b = b >> (n & 31);
                self.nb = nb - n;
                return Ok(e);
            }
        }
    }

    /// True if no input is left.
    fn at_end(&mut self) -> Result<bool, Error> {
        Ok(self.r.fill_buf()?.is_empty())
    }
}

/// The output buffer. It holds the history too: a match reads back into `buf`.
pub(crate) struct Out {
    pub buf: Vec<u8>,
    /// `buf.len()` where output stops: the DecodeLimit bound (oneshot) or the end of the step
    /// (Reader).
    pub stop_at: usize,
    /// `buf.len()` where `max_output` is exceeded.
    pub max_len: usize,
    max_alloc: usize,
    pub oneshot: bool,
    /// `buf.len()` where this call's history starts: matches may not reach below it, and
    /// `max_alloc` does not charge what was there. Go's `histBase`.
    pub base: usize,
    /// The fused Adler-32 (zlib, whole buffer) of `buf[base..sum_pos]`, if one is kept: Go's
    /// `FuseAdler`.
    adler: Option<u32>,
    sum_pos: usize,
}

impl Out {
    /// A Reader step is full: hand the output over.
    fn full(&self) -> bool {
        !self.oneshot && self.buf.len() >= self.stop_at
    }

    /// Room for `extra` more bytes, charged against `max_alloc` before it is allocated.
    fn reserve(&mut self, extra: usize) -> Result<(), Error> {
        let len = self.buf.len();
        if self.buf.capacity() - len >= extra {
            return Ok(());
        }
        let need = len.checked_add(extra).ok_or(Error::AllocLimit)?;
        if need - self.base > self.max_alloc {
            return Err(Error::AllocLimit);
        }
        let target = need
            .max(len.saturating_mul(2).max(4096))
            .min(self.base.saturating_add(self.max_alloc));
        self.buf
            .try_reserve_exact(target - len)
            .map_err(|_| Error::OutOfMemory)
    }

    /// Adds the output since the last call to the fused Adler-32, if there is one.
    fn sum_to(&mut self) {
        if let Some(a) = self.adler {
            self.adler = Some(adler32::sum(a, &self.buf[self.sum_pos..]));
            self.sum_pos = self.buf.len();
        }
    }

    fn put(&mut self, c: u8) -> Result<(), Halt> {
        let len = self.buf.len();
        if len >= self.stop_at {
            return Err(Halt::Limit);
        }
        if len >= self.max_len {
            return Err(Error::OutputLimit.into());
        }
        self.reserve(1)?;
        self.buf.push(c);
        Ok(())
    }

    /// Copies a match of `*copy_len` bytes at `dist` as far as `stop_at` and the limits allow.
    /// A Reader that stops at `stop_at` returns with `*copy_len > 0`.
    fn write_copy(&mut self, copy_len: &mut usize, dist: usize) -> Result<(), Halt> {
        while *copy_len > 0 {
            let len = self.buf.len();
            if len >= self.stop_at {
                return if self.oneshot {
                    Err(Halt::Limit)
                } else {
                    Ok(())
                };
            }
            if len >= self.max_len {
                return Err(Error::OutputLimit.into());
            }
            let n = (*copy_len).min(self.stop_at - len).min(self.max_len - len);
            self.reserve(n)?;
            // The last `dist` bytes repeat, so each pass can copy all that is there so far.
            let start = len - dist;
            let mut done = 0;
            while done < n {
                let chunk = (dist + done).min(n - done);
                self.buf.extend_from_within(start..start + chunk);
                done += chunk;
            }
            *copy_len -= n;
        }
        Ok(())
    }
}

enum Step {
    NextBlock,
    /// In a Huffman block; `copy_len > 0` is a match still to write.
    Huffman,
    /// In a stored block; `copy_len` bytes are still to copy.
    CopyData,
}

/// The tables of a dynamic block.
struct Dyn {
    /// The code length code.
    h1: Huff,
    /// The litlen and distance tables, for the careful path and the fast loop.
    tables: fast::Tables,
    lens: [u8; MAX_NUM_LIT + MAX_NUM_DIST],
    codebits: [u8; NUM_CODES],
}

thread_local! {
    /// The last dynamic block's tables (some 40 KB), handed on from one decode to the next on this
    /// thread: a small decode would otherwise spend a good part of its time allocating them.
    static SPARE: RefCell<Option<Box<Dyn>>> = const { RefCell::new(None) };
}

pub(crate) struct Inflate<'a, R> {
    pub bits: Bits<R>,
    pub out: Out,
    /// What a dynamic block needs, made at the first one: a stream of stored and fixed blocks
    /// never pays for it.
    dy: Option<Box<Dyn>>,
    /// stdlib's minimum litlen code length of the dynamic block.
    ll_min: u32,
    /// Run the fast loop (off: the careful path alone, the oracle of the tests).
    use_fast: bool,
    step: Step,
    /// The Huffman block uses the fixed codes.
    fixed: bool,
    final_: bool,
    copy_len: usize,
    copy_dist: usize,
    cancel: &'a AtomicBool,
}

impl<R> Drop for Inflate<'_, R> {
    fn drop(&mut self) {
        if let Some(d) = self.dy.take() {
            // Gone with the thread, or in use (a drop inside a `with`): the tables are just freed.
            let _ = SPARE.try_with(|s| s.try_borrow_mut().map(|mut s| *s = Some(d)));
        }
    }
}

/// A reusable raw DEFLATE decoder: Go's `deflate.Decoder`. It keeps the Huffman tables of the
/// last dynamic block (about 40 KB, not charged to `max_alloc`, as the fixed tables), so a
/// worker that decodes tile after tile into one reused `Vec` allocates nothing after its first
/// call. The output, the limits and the errors are those of the free functions; a `Decoder`
/// holds no other state between calls.
///
/// ```
/// use deflate::{Decoder, Limits};
/// use std::sync::atomic::AtomicBool;
///
/// # fn main() -> Result<(), deflate::Error> {
/// let (limits, cancel) = (Limits { max_output: 1 << 20, max_alloc: 64 << 20 }, AtomicBool::new(false));
/// let mut d = Decoder::new();
/// let mut out = Vec::new();
/// // Raw DEFLATE of b"hello", then an empty stream.
/// let tiles: [&[u8]; 2] = [&[0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x07, 0x00], &[0x03, 0x00]];
/// for tile in tiles {
///     out.clear();
///     d.decode_into(tile, &mut out, &limits, &cancel)?;
/// }
/// assert!(out.is_empty());
/// # Ok(())
/// # }
/// ```
#[derive(Default)]
pub struct Decoder {
    dy: Option<Box<Dyn>>,
}

impl Drop for Decoder {
    fn drop(&mut self) {
        if let Some(d) = self.dy.take() {
            // Back to this thread's spare (as a finished decode does), so the next free-function
            // call on the thread does not allocate the tables again.
            let _ = SPARE.try_with(|s| s.try_borrow_mut().map(|mut s| *s = Some(d)));
        }
    }
}

impl std::fmt::Debug for Decoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decoder").finish_non_exhaustive()
    }
}

impl Decoder {
    /// A decoder with no tables yet: the first dynamic block makes them.
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
        run(src, usize::MAX, out, limits, cancel, true, None, Some(self)).map(|_| ())
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
        if n == 0 {
            return Ok(());
        }
        run(src, n, out, limits, cancel, true, None, Some(self)).map(|_| ())
    }
}

impl<'a, R: BufRead> Inflate<'a, R> {
    pub fn new(r: R, limits: &Limits, cancel: &'a AtomicBool) -> Self {
        Self {
            bits: Bits {
                r,
                b: 0,
                nb: 0,
                roffset: 0,
            },
            out: Out {
                buf: Vec::new(),
                stop_at: usize::MAX,
                max_len: limits.max_output,
                max_alloc: limits.max_alloc,
                oneshot: false,
                base: 0,
                adler: None,
                sum_pos: 0,
            },
            dy: None,
            ll_min: 0,
            use_fast: true,
            step: Step::NextBlock,
            fixed: false,
            final_: false,
            copy_len: 0,
            copy_dist: 0,
            cancel,
        }
    }

    /// Runs steps until one stops.
    pub fn run(&mut self) -> Halt {
        loop {
            let r = match self.step {
                Step::NextBlock => self.next_block(),
                Step::Huffman => self.huffman_block(),
                Step::CopyData => self.copy_data(),
            };
            if let Err(h) = r {
                return h;
            }
        }
    }

    fn finish_block(&mut self) -> Result<(), Halt> {
        self.step = Step::NextBlock;
        if self.final_ { Err(Halt::Eof) } else { Ok(()) }
    }

    fn next_block(&mut self) -> Result<(), Halt> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled.into());
        }
        let hdr = self.bits.take(3)?;
        self.final_ = hdr & 1 == 1;
        match hdr >> 1 {
            0 => self.data_block(),
            1 => {
                self.fixed = true;
                self.step = Step::Huffman;
                Ok(())
            }
            2 => {
                self.read_huffman()?;
                self.fixed = false;
                self.step = Step::Huffman;
                Ok(())
            }
            // 3 is reserved.
            _ => Err(Error::Corrupt(self.bits.roffset).into()),
        }
    }

    /// RFC 1951 section 3.2.7: dynamic Huffman codes.
    fn read_huffman(&mut self) -> Result<(), Error> {
        let d = self.dy.get_or_insert_with(|| {
            SPARE
                .try_with(|s| s.try_borrow_mut().ok().and_then(|mut s| s.take()))
                .ok()
                .flatten()
                .unwrap_or_else(|| {
                    Box::new(Dyn {
                        h1: Huff::new(),
                        tables: fast::Tables::new(),
                        lens: [0; MAX_NUM_LIT + MAX_NUM_DIST],
                        codebits: [0; NUM_CODES],
                    })
                })
        });
        let bits = &mut self.bits;
        // The header from the input buffer with a 64-bit bit buffer when the buffer holds the
        // longest one (`HEADER_MAX` bytes), else (or on any error) the careful read below, one
        // byte at a time. Both leave the same state (header.rs).
        let fast = match bits.r.fill_buf() {
            Ok(buf) if self.use_fast && buf.len() >= HEADER_MAX => {
                let mut s = SliceBits::new(buf, u64::from(bits.b), bits.nb);
                read_dynamic(&mut s, &mut d.codebits, &mut d.lens)
                    .and_then(|r| Some((r, s.handoff()?, (s.pulled - u64::from(bits.nb)) / 8)))
            }
            _ => None,
        };
        let (nlit, ndist) = if let Some((r, (b, nb), pulled)) = fast {
            bits.b = b;
            bits.nb = nb;
            bits.r.consume(pulled as usize);
            bits.roffset += pulled;
            r
        } else {
            careful_header(bits, d)?
        };
        let n = nlit + ndist;

        let (min_ll, ok_ll) = accept_lengths(&d.lens[..nlit]);
        let (_, ok_dist) = accept_lengths(&d.lens[nlit..n]);
        if !ok_ll || !ok_dist {
            return Err(Error::Corrupt(bits.roffset));
        }
        // Every block ends with an end-of-block code, so reading at least that many bits first
        // never reads past the end of the stream.
        self.ll_min = min_ll.max(u32::from(d.lens[256]));
        fast::build(
            &mut d.tables,
            &d.lens[..nlit],
            &d.lens[nlit..n],
            self.ll_min,
        );
        Ok(())
    }

    /// Decodes a Huffman block: RFC 1951 section 3.2.3.
    fn huffman_block(&mut self) -> Result<(), Halt> {
        // A Reader step ends at `WINDOW` bytes, before `next_check` below: check the flag on
        // every resume (once per step) so a block of any length can be cancelled.
        if self.cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled.into());
        }
        let (ft, ll_min) = if self.fixed {
            (fast::fixed(), 7)
        } else {
            match &self.dy {
                Some(d) => (&d.tables, self.ll_min),
                None => return Err(Error::Internal("no tables").into()),
            }
        };
        let (bits, out) = (&mut self.bits, &mut self.out);
        let mut resume = self.copy_len > 0;
        let mut next_check = out.buf.len() + CANCEL_STEP;
        loop {
            if out.buf.len() >= next_check {
                if self.cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled.into());
                }
                out.sum_to();
                next_check = out.buf.len() + CANCEL_STEP;
            }
            if !resume {
                if self.use_fast {
                    fast_run(bits, out, ft);
                    if out.full() {
                        self.step = Step::Huffman;
                        return Err(Halt::Pause);
                    }
                }
                let e = bits.table_sym(ft.ll(), LL_BITS, ll_min)?;
                if e & LIT != 0 {
                    out.put((e >> 16) as u8)?;
                    if out.full() {
                        self.step = Step::Huffman;
                        return Err(Halt::Pause);
                    }
                    continue;
                }
                if e & STOP != 0 {
                    if e >> 16 == 256 {
                        self.step = Step::NextBlock;
                        return if self.final_ { Err(Halt::Eof) } else { Ok(()) };
                    }
                    return Err(Error::Corrupt(bits.roffset).into()); // 286, 287
                }
                // A reference to older data: the table entry has the base and the extra bits.
                let length = (e >> 16) as usize + bits.take((e & 31) - (e >> 8 & 15))? as usize;

                let d = bits.table_sym(ft.dt(), DIST_BITS, 0)?;
                if d & STOP != 0 {
                    return Err(Error::Corrupt(bits.roffset).into()); // 30, 31
                }
                let dist = (d >> 16) as usize + bits.take((d & 31) - (d >> 8 & 15))? as usize;
                // No check on length: an encoder may be prescient.
                if dist > (out.buf.len() - out.base).min(WINDOW) {
                    return Err(Error::Corrupt(bits.roffset).into());
                }
                self.copy_len = length;
                self.copy_dist = dist;
            }
            resume = false;
            out.write_copy(&mut self.copy_len, self.copy_dist)?;
            if out.full() || self.copy_len > 0 {
                self.step = Step::Huffman;
                return Err(Halt::Pause);
            }
        }
    }

    /// Starts a stored block.
    fn data_block(&mut self) -> Result<(), Halt> {
        // Discard the rest of the current byte.
        let bits = &mut self.bits;
        bits.nb = 0;
        bits.b = 0;
        // LEN and NLEN: all four bytes at once when they are there (a short input reads, and
        // fails, byte by byte).
        let mut h = [0u8; 4];
        match bits.r.fill_buf() {
            Ok(buf) if buf.len() >= 4 => {
                h.copy_from_slice(&buf[..4]);
                bits.r.consume(4);
                bits.roffset += 4;
            }
            _ => {
                for x in &mut h {
                    *x = bits.next_byte()?;
                }
            }
        }
        let n = usize::from(h[0]) | (usize::from(h[1]) << 8);
        let nn = usize::from(h[2]) | (usize::from(h[3]) << 8);
        if nn != !n & 0xffff {
            return Err(Error::Corrupt(bits.roffset).into());
        }
        if n == 0 {
            let out = &self.out;
            if out.oneshot && out.buf.len() == out.stop_at && !self.final_ {
                return Err(Halt::Limit); // stdlib flushes here
            }
            self.finish_block()?;
            // A sync flush: stdlib's Read stops here too.
            return if self.out.oneshot {
                Ok(())
            } else {
                Err(Halt::Flush)
            };
        }
        self.copy_len = n;
        self.copy_data()
    }

    /// Copies `copy_len` bytes of a stored block from the input to the output.
    fn copy_data(&mut self) -> Result<(), Halt> {
        let (bits, out) = (&mut self.bits, &mut self.out);
        while self.copy_len > 0 {
            let len = out.buf.len();
            if len >= out.stop_at {
                self.step = Step::CopyData;
                if !out.oneshot {
                    return Err(Halt::Pause);
                }
                // Stdlib would have read the next bytes and failed on a short input.
                return Err(if bits.at_end()? {
                    Error::UnexpectedEof.into()
                } else {
                    Halt::Limit
                });
            }
            if len >= out.max_len {
                return Err(Error::OutputLimit.into());
            }
            let n = self.copy_len.min(out.stop_at - len).min(out.max_len - len);
            out.reserve(n)?;
            // A fused Adler-32 sums the bytes before these, then these as it copies them.
            out.sum_to();
            let mut left = n;
            while left > 0 {
                let buf = bits.r.fill_buf().map_err(Error::from)?;
                if buf.is_empty() {
                    return Err(Error::UnexpectedEof.into());
                }
                let k = left.min(buf.len());
                if let Some(a) = out.adler {
                    out.adler = Some(adler32::extend_sum(&mut out.buf, &buf[..k], a));
                    out.sum_pos = out.buf.len();
                } else {
                    out.buf.extend_from_slice(&buf[..k]);
                }
                bits.r.consume(k);
                bits.roffset += k as u64;
                left -= k;
            }
            self.copy_len -= n;
        }
        // A full Reader step: stdlib flushes before the next block.
        if out.full() {
            self.step = Step::CopyData;
            return Err(Halt::Pause);
        }
        self.finish_block()
    }
}

/// The rest of a dynamic block header (after its 3 bits), one input byte at a time: Go's
/// `readHuffman` up to the checks of the litlen and distance codes. Returns `(nlit, ndist)`.
fn careful_header<R: BufRead>(bits: &mut Bits<R>, d: &mut Dyn) -> Result<(usize, usize), Error> {
    // HLIT[5], HDIST[5], HCLEN[4].
    bits.need(5 + 5 + 4)?;
    let nlit = (bits.b & 0x1f) as usize + 257;
    if nlit > MAX_NUM_LIT {
        return Err(Error::Corrupt(bits.roffset));
    }
    bits.b >>= 5;
    let ndist = (bits.b & 0x1f) as usize + 1;
    if ndist > MAX_NUM_DIST {
        return Err(Error::Corrupt(bits.roffset));
    }
    bits.b >>= 5;
    // At most 4 + 15 = NUM_CODES.
    let nclen = (bits.b & 0xf) as usize + 4;
    bits.b >>= 4;
    bits.nb -= 5 + 5 + 4;

    for &o in &CODE_ORDER[..nclen] {
        d.codebits[o] = bits.take(3)? as u8;
    }
    for &o in &CODE_ORDER[nclen..] {
        d.codebits[o] = 0;
    }
    if !d.h1.init(&d.codebits) {
        return Err(Error::Corrupt(bits.roffset));
    }

    // HLIT + 257 code lengths, then HDIST + 1, with the code length code.
    let n = nlit + ndist;
    let mut i = 0;
    while i < n {
        let x = bits.huff_sym(&d.h1)?;
        if x < 16 {
            d.lens[i] = x as u8;
            i += 1;
            continue;
        }
        // Repeat the previous length or zero.
        let (rep, nb, b) = match x {
            16 => {
                if i == 0 {
                    return Err(Error::Corrupt(bits.roffset));
                }
                (3, 2, d.lens[i - 1])
            }
            17 => (3, 3, 0),
            18 => (11, 7, 0),
            _ => return Err(Error::Internal("unexpected length code")),
        };
        let rep = rep + bits.take(nb)? as usize;
        if i + rep > n {
            return Err(Error::Corrupt(bits.roffset));
        }
        d.lens[i..i + rep].fill(b);
        i += rep;
    }
    Ok((nlit, ndist))
}

/// How a whole-buffer decode ended.
pub(crate) enum Ended {
    /// The final block ended.
    Eof,
    /// The output reached the limit.
    Stop,
}

/// Decodes `src` to at most `limit` bytes, as Go `Decoder.DecodeLimit`, `usize::MAX` for all of
/// it, appending to `out` as Go's `dst`: the bytes before `out.len()` on entry are not history.
/// `out` holds what was decoded, also on an error. Returns the bytes of `src` Go's reader would
/// have read, and how it ended. With `adler`, the Adler-32 of the output is summed during the
/// decode and stored there when the final block ends. With `dec`, the Huffman tables come from
/// and go back to that [`Decoder`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    src: &[u8],
    limit: usize,
    out: &mut Vec<u8>,
    limits: &Limits,
    cancel: &AtomicBool,
    fast: bool,
    adler: Option<&mut u32>,
    dec: Option<&mut Decoder>,
) -> Result<(usize, Ended), Error> {
    let mut f = Inflate::new(src, limits, cancel);
    f.use_fast = fast;
    let mut dec = dec;
    if let Some(d) = dec.as_deref_mut() {
        f.dy = d.dy.take();
    }
    let base = out.len();
    f.out.buf = std::mem::take(out);
    f.out.base = base;
    f.out.max_len = limits.max_output.saturating_add(base);
    f.out.oneshot = true;
    f.out.stop_at = limit.saturating_add(base);
    if adler.is_some() {
        f.out.adler = Some(1);
        f.out.sum_pos = base;
    }
    let h = f.run();
    if let (Halt::Eof, Some(a)) = (&h, adler) {
        f.out.sum_to();
        *a = f.out.adler.unwrap_or(1);
    }
    let n = f.out.buf.len() - base;
    let roffset = f.bits.roffset as usize;
    *out = std::mem::take(&mut f.out.buf);
    if let Some(d) = dec {
        d.dy = f.dy.take();
    }
    // Stdlib's window flush at limit ends the read there, whatever follows.
    let flush = limit != usize::MAX && n == limit && limit.is_multiple_of(WINDOW);
    match h {
        Halt::Err(e) if !(flush && e.is_stream()) => Err(e),
        Halt::Eof if !flush => Ok((roffset, Ended::Eof)),
        _ => Ok((roffset, Ended::Stop)),
    }
}

/// Inflates the raw DEFLATE stream `src`. Trailing bytes after the final block are ignored.
/// Runs on the calling thread; enforces `limits.max_output` and `limits.max_alloc`.
///
/// # Errors
///
/// [`Error::Corrupt`], [`Error::UnexpectedEof`] and [`Error::Internal`] for a bad or short
/// stream (offsets as Go reports them); [`Error::OutputLimit`] if the output would pass
/// `limits.max_output`; [`Error::AllocLimit`] if the output buffer would pass `limits.max_alloc`;
/// [`Error::OutOfMemory`] if the allocator refuses a size within the limits; [`Error::Cancelled`]
/// if `cancel` is set (checked at each block and every 64 KiB of output). No other variant.
pub fn decode(src: &[u8], limits: &Limits, cancel: &AtomicBool) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    decode_into(src, &mut out, limits, cancel)?;
    Ok(out)
}

/// As [`decode`], appending to `out`. Like Go's `Decode(dst, src)`, the bytes already in `out`
/// are not history (a match into them is [`Error::Corrupt`]), and on an error `out` holds the
/// output before it.
///
/// # Errors
///
/// [`Error::Corrupt`], [`Error::UnexpectedEof`] and [`Error::Internal`] for a bad or short
/// stream (offsets as Go reports them); [`Error::OutputLimit`] if the output would pass
/// `limits.max_output`; [`Error::AllocLimit`] if the output buffer would pass `limits.max_alloc`;
/// [`Error::OutOfMemory`] if the allocator refuses a size within the limits; [`Error::Cancelled`]
/// if `cancel` is set (checked at each block and every 64 KiB of output). No other variant.
pub fn decode_into(
    src: &[u8],
    out: &mut Vec<u8>,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    run(src, usize::MAX, out, limits, cancel, true, None, None).map(|_| ())
}

/// As [`decode_into`], with up to `max_threads` threads (the calling thread included) on one
/// large stream: a speculative search for block boundaries in parallel, checked, with the
/// serial decode whenever anything is not as expected (docs/threading.md). The output, and any
/// error with its partial output, are those of [`decode_into`] (for a [`Error::Cancelled`],
/// `out` holds no new bytes). A stream under 4 MiB, or `max_threads` under 4, decodes serially on
/// the calling thread and spawns nothing. `limits.max_alloc` bounds the peak of all threads
/// together: the parallel decode holds about 4 to 5 times the output at its peak, and decodes
/// serially when that does not fit.
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

/// [`decode_parallel_into`] with chunks of at least `min_chunk` compressed bytes per worker
/// instead of 1 MiB, so tests, parity and fuzzing split small streams. Not API.
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
    if threads >= 2 && crate::parallel::decode(src, out, limits, threads, cancel)?.is_some() {
        return Ok(());
    }
    decode_into(src, out, limits, cancel)
}

/// Inflates at most `n` bytes of `src`, stopping where `io.LimitReader(n)` over Go's
/// `flate.NewReader` stops: after `n` bytes, or at the first error or end of stream before that.
/// Whatever follows `n` bytes is not looked at, except for the steps that write nothing.
/// Enforces `limits.max_output` (which `n` may be under) and `limits.max_alloc`.
///
/// # Errors
///
/// [`Error::Corrupt`], [`Error::UnexpectedEof`] and [`Error::Internal`] for a bad or short
/// stream (offsets as Go reports them); [`Error::OutputLimit`] if the output would pass
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
/// stream (offsets as Go reports them); [`Error::OutputLimit`] if the output would pass
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
    if n == 0 {
        return Ok(());
    }
    run(src, n, out, limits, cancel, true, None, None).map(|_| ())
}

/// As [`decode_limit_into`] (`usize::MAX` for all) with the careful path alone: the oracle that
/// tests and fuzzing compare the fast loop with. Not API.
#[doc(hidden)]
pub fn decode_careful_into(
    src: &[u8],
    n: usize,
    out: &mut Vec<u8>,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    if n == 0 {
        return Ok(());
    }
    run(src, n, out, limits, cancel, false, None, None).map(|_| ())
}

/// Runs the fast loop at a symbol boundary of a Huffman block, and restores the careful path's
/// `b`, `nb` and read position from where the loop stopped (deflate-go `fastRun`).
fn fast_run<R: BufRead>(bits: &mut Bits<R>, out: &mut Out, t: &fast::Tables) {
    let len = out.buf.len();
    // Where the loop may start a pass: all it can write in one fits below the limits, a cancel
    // check comes every `CANCEL_STEP` bytes, and a fused Adler-32 sums every `SUM_STEP`.
    let Some(cap) = out.stop_at.min(out.max_len).checked_sub(fast::MAX_STEP) else {
        return;
    };
    let step = if out.adler.is_some() {
        SUM_STEP
    } else {
        CANCEL_STEP
    };
    let cap = cap.min(len.saturating_add(step));
    if cap < len {
        return;
    }
    let Ok(inp) = bits.r.fill_buf() else { return };
    if inp.len() <= fast::IN_MARGIN {
        return;
    }
    // The loop writes into the spare capacity, `OUT_MARGIN` past where it may start a pass. A
    // limit is the careful path's error to report, so a refused reserve only shortens or skips
    // the loop.
    if out.buf.capacity() - len < FAST_ROOM + fast::OUT_MARGIN {
        let _ = out.reserve(FAST_ROOM + fast::OUT_MARGIN);
    }
    let Some(room) = (out.buf.capacity() - len).checked_sub(fast::OUT_MARGIN) else {
        return;
    };
    let mut s = fast::State {
        ip: 0,
        bitbuf: u64::from(bits.b),
        bits: bits.nb,
        slack: 0,
    };
    fast::run(
        inp,
        &mut out.buf,
        len + room.min(cap - len),
        out.base,
        t,
        &mut s,
    );
    // The bit position counts from `inp[0]`; it is negative when `bitbuf` held bits from before.
    let pos = (s.ip * 8) as i64 - i64::from(s.bits);
    let pulled = ((pos + i64::from(s.slack) + 7) >> 3).max(0);
    let nb = pulled * 8 - pos;
    bits.b = (s.bitbuf & ((1u64 << nb) - 1)) as u32;
    bits.nb = nb as u32;
    bits.roffset += pulled as u64;
    bits.r.consume(pulled as usize);
    out.sum_to();
}
