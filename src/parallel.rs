//! Parallel decode of one large DEFLATE stream: a speculative search for block
//! boundaries, a check that each guess is a real boundary, and the serial decoder whenever
//! anything is not as expected. The output is byte for byte that of the serial decode.
//!
//! ```text
//!  src:  |-- chunk 0 --|-- chunk 1 --|-- chunk 2 --|   one chunk per worker
//!  1. find:   worker k >= 1 looks, in the first SEARCH bytes of its chunk, for a bit position
//!             where a dynamic block header and its whole block decode (any table, any distance
//!             <= 32 KiB): the guess s_k. No guess: chunk k joins span k-1.
//!  2. decode: span k decodes from s_k to s_(k+1) into u16 symbols: a byte, or a marker for a
//!             byte of the 32 KiB before s_k, which the span does not know yet. It must end a
//!             block exactly at s_(k+1), and the last span must end the final block.
//!  3. join:   the window of each span from the spans before it (32 KiB each, in order), then
//!             every span to bytes in its slice of the output, in parallel.
//! ```
//!
//! Why the output is exact: span 0 starts at the real start, so it decodes the real stream.
//! Span k is used only if span k-1 (decoded from a real block start, by induction) ended a block
//! exactly at s_k, so s_k is a real block start too, and every span decodes real blocks. Each
//! check the serial decoder makes (header fields, code completeness by its own `accept_lengths`,
//! symbols 286/287 and distances 30/31, distances past the start of the history, LEN/NLEN, the
//! output and alloc limits) gives up here instead, and then the serial decoder decodes the whole
//! stream: so an error, its offset and the partial output are the serial ones. A give-up costs
//! the time spent, never a different result.

use crate::{
    Error, Limits,
    fast::{self, DIST_BITS, LINK, LIT, LL_BITS, STOP},
    header::{MAX_NUM_DIST, MAX_NUM_LIT, NUM_CODES, SliceBits, read_dynamic},
    huffman::accept_lengths,
};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const WINDOW: usize = 1 << 15;
/// Compressed bytes per worker at least: a shorter stream, or a share below this, decodes
/// serially (spawning and searching would cost more than they save).
pub(crate) const MIN_CHUNK: usize = 1 << 20;
/// Symbols between cancel checks inside one block, as the serial decoder's 64 KiB.
const CANCEL_STEP: usize = 1 << 16;
/// The fewest workers the parallel decode runs with (`workers`).
const MIN_WORKERS: usize = 4;
/// Bytes from the start of its chunk where a worker looks for a block start. A hostile input with
/// no block start costs each worker this much search, not its whole chunk.
const SEARCH: usize = 256 << 10;
/// Parallel decodes of two spans or more that did not give up, for the tests
/// (`deflate::parallel_decodes`).
pub(crate) static DONE: AtomicUsize = AtomicUsize::new(0);
/// Spawned workers alive now, and the most alive at once since the last read: the tests check the
/// peak against `max_threads` (`deflate::parallel_thread_peak`).
static LIVE: AtomicUsize = AtomicUsize::new(0);
pub(crate) static PEAK: AtomicUsize = AtomicUsize::new(0);

/// Counts a spawned worker while it lives.
struct Live;

impl Live {
    fn new() -> Self {
        let n = LIVE.fetch_add(1, Ordering::Relaxed) + 1;
        PEAK.fetch_max(n, Ordering::Relaxed);
        Self
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Symbols one worker's trial decodes in the search may write in all: a hostile input of
/// headers whose block never ends costs each worker this much, not a decode per header.
const TRIAL: usize = 16 << 20;
/// A u16 at or above this is a marker: byte `v - MARK` of the window before the span.
const MARK: u16 = 256;

/// Memory charged against `max_alloc` by all workers together: charge before allocating.
struct Budget {
    used: AtomicUsize,
    max: usize,
}

impl Budget {
    fn charge(&self, n: usize) -> bool {
        let prev = self.used.fetch_add(n, Ordering::Relaxed);
        if prev.checked_add(n).is_none_or(|u| u > self.max) {
            self.used.fetch_sub(n, Ordering::Relaxed);
            return false;
        }
        true
    }
}

/// Why a speculative decode stopped short: give up (the serial decoder decides), or the cancel.
enum Stop {
    GiveUp,
    Cancelled,
}

/// One span: its symbols, and where it ended.
struct Span {
    data: Vec<u16>,
    /// The bit after its last block.
    end: u64,
}

/// The output so far of one span, grown in steps that are charged first.
struct Sink<'a> {
    data: Vec<u16>,
    budget: &'a Budget,
    max_output: usize,
    /// Bytes charged for `data`.
    charged: usize,
}

impl Sink<'_> {
    /// Room for `n` more symbols.
    #[inline]
    fn room(&mut self, n: usize) -> Result<(), Stop> {
        if self.data.capacity() - self.data.len() >= n {
            return Ok(());
        }
        let need = self.data.len().checked_add(n).ok_or(Stop::GiveUp)?;
        if need > self.max_output {
            return Err(Stop::GiveUp);
        }
        let extra = need.max(self.data.len() * 2).max(1 << 16) - self.data.len();
        if !self.budget.charge(extra * 2) {
            return Err(Stop::GiveUp);
        }
        self.charged += extra * 2;
        if self.data.try_reserve_exact(extra).is_err() {
            return Err(Stop::GiveUp);
        }
        Ok(())
    }
}

/// The tables of a worker's dynamic blocks.
struct Dyn {
    tables: fast::Tables,
    lens: [u8; MAX_NUM_LIT + MAX_NUM_DIST],
    codebits: [u8; NUM_CODES],
}

impl Dyn {
    fn new() -> Box<Self> {
        Box::new(Self {
            tables: fast::Tables::new(),
            lens: [0; MAX_NUM_LIT + MAX_NUM_DIST],
            codebits: [0; NUM_CODES],
        })
    }

    /// Reads a dynamic header (after its 3 bits) and builds the tables; false for anything the
    /// serial decoder would refuse.
    fn read(&mut self, s: &mut SliceBits<'_>) -> bool {
        let Some((nlit, ndist)) = read_dynamic(s, &mut self.codebits, &mut self.lens) else {
            return false;
        };
        let n = nlit + ndist;
        let (min_ll, ok_ll) = accept_lengths(&self.lens[..nlit]);
        let (_, ok_dist) = accept_lengths(&self.lens[nlit..n]);
        if !ok_ll || !ok_dist {
            return false;
        }
        let ll_min = min_ll.max(u32::from(self.lens[256]));
        fast::build(
            &mut self.tables,
            &self.lens[..nlit],
            &self.lens[nlit..n],
            ll_min,
        );
        true
    }
}

/// Decodes the Huffman data of one block onto `out` with the tables `t`, to its end of block.
/// `marked`: a distance past the span's start is a marker into the window before it (else a
/// give-up: the start of the stream).
fn huffman(
    s: &mut SliceBits<'_>,
    t: &fast::Tables,
    out: &mut Sink<'_>,
    marked: bool,
    cancel: &AtomicBool,
) -> Result<(), Stop> {
    let (ll, dt) = (t.ll(), t.dt());
    let mask = |n: u32| (1u64 << n) - 1;
    // The cancel flag inside a block every `CANCEL_STEP` symbols, as the serial decoder.
    let mut next_check = out.data.len() + CANCEL_STEP;
    loop {
        if out.data.len() >= next_check {
            if cancel.load(Ordering::Relaxed) {
                return Err(Stop::Cancelled);
            }
            next_check = out.data.len() + CANCEL_STEP;
        }
        out.room(258)?;
        // 48 bits hold the longest symbol: 15 + 5 for a length, 15 + 13 for its distance.
        let (bits, avail) = s.fill_max();
        let mut e = ll[(bits & mask(LL_BITS)) as usize];
        if e & LINK != 0 {
            e = ll[((e >> 16) as u64 + ((bits >> LL_BITS) & mask(e >> 8 & 15))) as usize];
        }
        let tot = e & 31;
        if e & LIT != 0 {
            if tot > avail {
                return Err(Stop::GiveUp);
            }
            s.skip(tot);
            out.data.push(u16::from((e >> 16) as u8));
            continue;
        }
        if e & STOP != 0 {
            let n = e >> 8 & 15;
            // The end of the block, or a symbol or missing code the serial decoder rejects.
            if e >> 16 != 256 || n == 0 || n > avail {
                return Err(Stop::GiveUp);
            }
            s.skip(n);
            return Ok(());
        }
        let length = (e >> 16) as usize + ((bits & mask(tot)) >> (e >> 8 & 15)) as usize;
        let bb = bits >> tot;
        let mut d = dt[(bb & mask(DIST_BITS)) as usize];
        if d & LINK != 0 {
            d = dt[((d >> 16) as u64 + ((bb >> DIST_BITS) & mask(d >> 8 & 15))) as usize];
        }
        let dtot = d & 31;
        if d & STOP != 0 || tot + dtot > avail {
            return Err(Stop::GiveUp);
        }
        let dist = (d >> 16) as usize + ((bb & mask(dtot)) >> (d >> 8 & 15)) as usize;
        s.skip(tot + dtot);
        let len = out.data.len();
        if dist >= length && dist <= len {
            // No overlap: one copy within the span.
            out.data.extend_from_within(len - dist..len - dist + length);
        } else if dist <= len {
            for i in len..len + length {
                let v = out.data[i - dist];
                out.data.push(v);
            }
        } else if marked && dist <= len + WINDOW {
            for i in len..len + length {
                let v = match i.checked_sub(dist) {
                    Some(j) => out.data[j],
                    // Byte `WINDOW + i - dist` of the window, below `WINDOW`.
                    None => MARK + (WINDOW + i - dist) as u16,
                };
                out.data.push(v);
            }
        } else {
            return Err(Stop::GiveUp);
        }
    }
}

/// Decodes blocks from bit `start` of `src` until a block ends at bit `stop` (or, for the last
/// span, `stop == u64::MAX`, until the final block ends).
#[allow(clippy::too_many_arguments)]
fn decode_span(
    src: &[u8],
    start: u64,
    stop: u64,
    marked: bool,
    dy: &mut Dyn,
    budget: &Budget,
    max_output: usize,
    cancel: &AtomicBool,
) -> Result<Span, Stop> {
    let mut s = SliceBits::at(src, start).ok_or(Stop::GiveUp)?;
    let mut out = Sink {
        data: Vec::new(),
        budget,
        max_output,
        charged: 0,
    };
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Stop::Cancelled);
        }
        let at = start + s.pos;
        if at == stop {
            return Ok(Span {
                data: out.data,
                end: at,
            });
        }
        if at > stop {
            return Err(Stop::GiveUp);
        }
        let h = s.take(3).ok_or(Stop::GiveUp)?;
        match h >> 1 {
            0 => {
                s.align();
                let n = s.take(16).ok_or(Stop::GiveUp)?;
                if s.take(16).ok_or(Stop::GiveUp)? != !n & 0xffff {
                    return Err(Stop::GiveUp);
                }
                let b = s.bytes(n as usize).ok_or(Stop::GiveUp)?;
                out.room(b.len())?;
                out.data.extend(b.iter().map(|&c| u16::from(c)));
            }
            1 => huffman(&mut s, fast::fixed(), &mut out, marked, cancel)?,
            2 => {
                if !dy.read(&mut s) {
                    return Err(Stop::GiveUp);
                }
                huffman(&mut s, &dy.tables, &mut out, marked, cancel)?;
            }
            _ => return Err(Stop::GiveUp),
        }
        if h & 1 == 1 {
            // The final block: the stream ends here, so it must be the last span.
            return if stop == u64::MAX {
                Ok(Span {
                    data: out.data,
                    end: start + s.pos,
                })
            } else {
                Err(Stop::GiveUp)
            };
        }
    }
}

/// The first bit in `[lo, hi)` of `src` where a non-final dynamic block starts and decodes to its
/// end (with any 32 KiB window), or None. The trial decodes write at most `TRIAL` symbols in all.
fn find_start(
    src: &[u8],
    lo: u64,
    hi: u64,
    dy: &mut Dyn,
    budget: &Budget,
    cancel: &AtomicBool,
) -> Option<u64> {
    let mut trial = TRIAL;
    for bit in lo..hi {
        // The cheap test first, on a window of the next 120 bits: most positions fail in a few
        // instructions, and only a complete code length code gets the full header read.
        let Some(w) = window(src, bit) else { break };
        if !plausible(w) {
            continue;
        }
        let mut s = SliceBits::at(src, bit)?;
        s.take(3)?;
        if !dy.read(&mut s) {
            continue;
        }
        // The whole block must decode. Its output is charged while it exists, then dropped.
        let mut sink = Sink {
            data: Vec::new(),
            budget,
            max_output: trial,
            charged: 0,
        };
        let r = huffman(&mut s, &dy.tables, &mut sink, true, cancel);
        trial -= sink.data.len().min(trial);
        drop(sink.data);
        budget.used.fetch_sub(sink.charged, Ordering::Relaxed);
        match r {
            Ok(()) => return Some(bit),
            // Stop searching: the span decode reports the cancel.
            Err(Stop::Cancelled) => return None,
            Err(Stop::GiveUp) => {}
        }
    }
    None
}

/// The 120 bits of `src` from bit `bit` on (bit 0 first), or None near the end.
#[inline]
fn window(src: &[u8], bit: u64) -> Option<u128> {
    let i = usize::try_from(bit / 8).ok()?;
    let b: [u8; 16] = src.get(i..i.checked_add(16)?)?.try_into().ok()?;
    Some(u128::from_le_bytes(b) >> (bit % 8))
}

/// Whether the bits `w` can start a non-final dynamic block: BFINAL 0, BTYPE 2, HLIT and HDIST
/// at most 29, and a complete code length code (a degenerate or incomplete one, which the serial
/// decoder may accept, is skipped: the search only loses that guess).
#[inline]
fn plausible(w: u128) -> bool {
    if w & 7 != 0b100 || (w >> 3) & 0x1f > 29 || (w >> 8) & 0x1f > 29 {
        return false;
    }
    let hclen = ((w >> 13) & 15) as usize + 4;
    let mut kraft = 0u32;
    for k in 0..hclen {
        let l = ((w >> (17 + 3 * k)) & 7) as u32;
        if l != 0 {
            kraft += 128 >> l;
        }
    }
    kraft == 128
}

/// Workers for a stream of `len` compressed bytes, `min_chunk` or more each: 1 means serial.
/// Fewer than `MIN_WORKERS` is serial too: a span decodes into u16 symbols several times slower
/// than the serial loop, so two or three workers do not pay for the search and the join.
pub(crate) fn workers(len: usize, max_threads: usize, min_chunk: usize) -> usize {
    // The cheap bounds first: `available_parallelism` reads cgroup files on Linux on every call.
    let n = max_threads.min(len / min_chunk.max(1));
    if n < MIN_WORKERS {
        return 1;
    }
    let n = n.min(std::thread::available_parallelism().map_or(1, |c| c.get()));
    if n < MIN_WORKERS { 1 } else { n }
}

/// Decodes the raw DEFLATE stream `src` with `threads` (at least 2) workers onto `out`. Returns
/// the bit after the final block, None if it gave up (`out` unchanged), or
/// [`Error::Cancelled`].
pub(crate) fn decode(
    src: &[u8],
    out: &mut Vec<u8>,
    limits: &Limits,
    threads: usize,
    cancel: &AtomicBool,
) -> Result<Option<u64>, Error> {
    let budget = Budget {
        used: AtomicUsize::new(0),
        max: limits.max_alloc,
    };
    let len = src.len() as u64;
    let chunk = |k: usize| len * k as u64 / threads as u64;
    // Each worker's tables count against `max_alloc` too, before they are built.
    if !budget.charge(threads * (std::mem::size_of::<Dyn>() + fast::Tables::BYTES)) {
        return Ok(None);
    }
    // 1. find (worker k >= 1, in its chunk), then 2. decode each span to the next start found.
    let mut dys: Vec<Mutex<Box<Dyn>>> = (0..threads).map(|_| Mutex::new(Dyn::new())).collect();
    let starts = run_all(threads, |k| {
        if k == 0 {
            return Some(0);
        }
        let lo = chunk(k) * 8;
        let hi = (chunk(k) + SEARCH as u64).min(chunk(k + 1)) * 8;
        let mut dy = dys[k].lock().ok()?;
        find_start(src, lo, hi, &mut dy, &budget, cancel)
    });
    // No worker found a start: one span would only be a slow serial decode, so the caller's
    // serial decode is better.
    if starts[1..].iter().all(Option::is_none) {
        return Ok(None);
    }
    let spans = run_all(threads, |k| {
        let start = starts[k]?;
        let stop = starts[k + 1..]
            .iter()
            .flatten()
            .next()
            .copied()
            .unwrap_or(u64::MAX);
        let Ok(mut dy) = dys[k].lock() else {
            return Some(Err(Stop::GiveUp));
        };
        Some(decode_span(
            src,
            start,
            stop,
            k > 0,
            &mut dy,
            &budget,
            limits.max_output,
            cancel,
        ))
    });
    dys.clear();
    let spans: Vec<Span> = {
        let mut got = Vec::new();
        for r in spans.into_iter().flatten() {
            match r {
                Ok(s) => got.push(s),
                Err(Stop::GiveUp) => return Ok(None),
                Err(Stop::Cancelled) => return Err(Error::Cancelled),
            }
        }
        got
    };
    let Some(end) = spans.last().map(|s| s.end) else {
        return Ok(None);
    };
    // 3. join. The output, charged first: the spans are still held.
    let total = spans
        .iter()
        .try_fold(0usize, |t, s| t.checked_add(s.data.len()));
    let Some(total) = total.filter(|&t| t <= limits.max_output) else {
        return Ok(None);
    };
    if !budget.charge(total) || out.try_reserve_exact(total).is_err() {
        return Ok(None);
    }
    let base = out.len();
    out.resize(base + total, 0);
    // The window before each span, in order: the last 32 KiB of the output before it, from the
    // span before it and that span's own window. A marker that reaches before the stream's
    // start (window bytes that do not exist) gives up: the serial decoder reports it.
    if !budget.charge((spans.len() + 2) * WINDOW) {
        out.truncate(base);
        return Ok(None);
    }
    let mut windows: Vec<Vec<u8>> = Vec::with_capacity(spans.len());
    let mut win: Vec<u8> = Vec::new();
    for s in &spans {
        windows.push(win.clone());
        let tail = &s.data[s.data.len().saturating_sub(WINDOW)..];
        let mut next = Vec::with_capacity(WINDOW);
        let keep = WINDOW - tail.len();
        next.extend_from_slice(&win[win.len().saturating_sub(keep)..]);
        for &v in tail {
            next.push(match resolve(v, &win) {
                Some(b) => b,
                None => {
                    out.truncate(base);
                    return Ok(None);
                }
            });
        }
        win = next;
    }
    // One thread per span, at most `threads` with the calling thread, which takes the last.
    let bad = AtomicBool::new(false);
    let convert = |head: &mut [u8], s: &Span, w: &[u8]| {
        for (o, &v) in head.iter_mut().zip(&s.data) {
            match resolve(v, w) {
                Some(b) => *o = b,
                None => {
                    bad.store(true, Ordering::Relaxed);
                    return;
                }
            }
        }
    };
    std::thread::scope(|sc| {
        let mut rest = &mut out[base..];
        let n = spans.len();
        for (i, (s, w)) in spans.iter().zip(&windows).enumerate() {
            let (head, tail) = rest.split_at_mut(s.data.len());
            rest = tail;
            if i + 1 == n {
                convert(head, s, w);
            } else {
                let convert = &convert;
                // A thread the OS refuses: the calling thread does that span.
                let r = std::thread::Builder::new().spawn_scoped(sc, move || {
                    let _live = Live::new();
                    convert(head, s, w);
                });
                if let Err(e) = r {
                    drop(e);
                    bad.store(true, Ordering::Relaxed);
                }
            }
        }
    });
    if bad.load(Ordering::Relaxed) {
        out.truncate(base);
        return Ok(None);
    }
    if spans.len() > 1 {
        DONE.fetch_add(1, Ordering::Relaxed);
    }
    Ok(Some(end))
}

/// `f(0)` to `f(n - 1)` on up to `n` threads (the calling thread does `f(0)`), in index order.
/// A thread the OS refuses: the calling thread does that index too.
fn run_all<T: Send>(n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    std::thread::scope(|sc| {
        let f = &f;
        let handles: Vec<_> = (1..n)
            .map(|k| {
                std::thread::Builder::new().spawn_scoped(sc, move || {
                    let _live = Live::new();
                    f(k)
                })
            })
            .collect();
        let mut out = vec![f(0)];
        for (k, h) in (1..n).zip(handles) {
            out.push(match h.ok().map(|h| h.join()) {
                Some(Ok(v)) => v,
                _ => f(k),
            });
        }
        out
    })
}

/// The byte of symbol `v` with the window `w` (the up to 32 KiB before its span): None for a
/// marker before the start of `w`.
#[inline]
fn resolve(v: u16, w: &[u8]) -> Option<u8> {
    if v < MARK {
        return Some(v as u8);
    }
    let i = usize::from(v - MARK);
    // The window is the last `w.len()` of WINDOW bytes.
    w.get(i.checked_sub(WINDOW - w.len())?).copied()
}
