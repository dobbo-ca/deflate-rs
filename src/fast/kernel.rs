//! The decode loop (deflate-go `fast_generic.go` and the copies of `fast_amd64.s`; the AVX2
//! instantiation copies short periods with PSHUFB and 32-byte stores). It writes
//! straight into the spare capacity of the output `Vec` through a raw pointer, as Go's assembly
//! does: no zero-fill first, no bounds check per store, and `set_len` once at the end. The input
//! and the tables are read through safe slices.
//!
//! The invariants every access through `p` relies on, checked in safe code at the top of
//! `run_impl` and kept by the loop:
//!
//! - W1, writes: a pass starts only with `op <= out_limit`, and `out_limit + OUT_MARGIN` is at
//!   most the capacity. A pass writes below `op + 4 + 289`: two literal steps of up to two bytes,
//!   then a match of at most 258 bytes whose word copies run past its end by at most 31 (the
//!   period stores: `i < length`, then 16 bytes at `i + step`, `step <= 16`). That is below
//!   `out_limit + OUT_MARGIN` (320).
//! - W2, reads: a match source starts at `op - dist >= hist >= 0` (`dist <= op - hist` is checked
//!   before it is used) and every byte read is below the write position of its copy, so it is
//!   output: bytes the `Vec` held at entry, or bytes this call wrote. Nothing reads the spare
//!   capacity before it is written.
//! - W3, length: every byte from the old length to `op` is written: a literal step writes the
//!   bytes it moves `op` over, and a match copy writes `[op, op + length)` and more.
//! - R1, input: the input is read through `inp`'s pointer at `ip`, as Go's assembly does, with no
//!   check per refill. Between two checks of `inp.len() - ip >= IN_MARGIN` the loop refills at
//!   most three times (at the top of a pass, before a match, after a match), each reading 8 bytes
//!   at `ip` and moving it by at most 7, so it reads below the checked `ip + 22`.

use super::{
    DIST_BITS, DIST_MASK, DT_OFF, IN_MARGIN, LINK, LIT, LL_BITS, LL_MASK, LL_OFF, OUT_MARGIN,
    PT_OFF, STOP, State, TOTAL, Tables,
};
use std::ptr;

/// The largest multiple of `d` that is at most 16, for the periods 1..=15.
const PERIODS: [usize; 16] = [
    16, 16, 16, 15, 16, 15, 12, 14, 16, 9, 10, 11, 12, 13, 14, 15,
];

/// The largest multiple of `d` that is at most 32, for the periods 1..=15 (the AVX2 stores).
#[cfg(target_arch = "x86_64")]
const PERIODS32: [usize; 16] = [
    32, 32, 32, 30, 32, 30, 30, 28, 32, 27, 30, 22, 24, 26, 28, 30,
];

/// PSHUFB masks of the AVX2 short-period copy, per period `d` in 2..=15: from the 16 bytes
/// before the output position, byte `j` of the 32-byte pattern is byte `16 - d + j % d` (the
/// last `d` bytes, repeated).
#[cfg(target_arch = "x86_64")]
const SHUF: [[u8; 32]; 16] = shuf_masks();

#[cfg(target_arch = "x86_64")]
const fn shuf_masks() -> [[u8; 32]; 16] {
    let mut t = [[0u8; 32]; 16];
    let mut d = 1;
    while d < 16 {
        let mut j = 0;
        while j < 32 {
            t[d][j] = (16 - d + j % d) as u8;
            j += 1;
        }
        d += 1;
    }
    t
}

/// `x` below bit `n` of an entry `n` whose bits 5-7 are clear (its low byte is below 64): BZHI
/// reads only that byte, so the AVX2 instantiation needs no mask (deflate-go's BZHIQ).
#[inline(always)]
fn low<const AVX2: bool>(x: u64, n: u32) -> u64 {
    #[cfg(target_arch = "x86_64")]
    if AVX2 {
        // SAFETY: `AVX2` is true only in the instantiation that `avx2.rs` calls from its
        // `avx2,bmi1,bmi2` target-feature function after detection, so the CPU has BMI2.
        return unsafe { std::arch::x86_64::_bzhi_u64(x, n) };
    }
    x & ((1u64 << (n & 63)) - 1)
}

/// The loop; `PAIR` takes the primary litlen lookups from the pair table. See `super::run`.
/// `AVX2` (x86_64 only) copies a short-period match with PSHUFB and 32-byte stores: only the
/// AVX2 tier (`avx2.rs`, after detection) instantiates it.
#[inline(always)]
pub(super) fn run_impl<const PAIR: bool, const AVX2: bool>(
    inp: &[u8],
    out: &mut Vec<u8>,
    out_limit: usize,
    hist: usize,
    t: &Tables,
    s: &mut State,
) {
    let mut op = out.len();
    // W1 and W2 start here.
    if hist > op
        || out_limit
            .checked_add(OUT_MARGIN)
            .is_none_or(|end| end > out.capacity())
    {
        return;
    }
    let p = out.as_mut_ptr();
    let tab: &[u32; TOTAL] = &t.a;
    let inb = inp.as_ptr();
    let mut ip = 0usize;
    let (mut bitbuf, mut bits) = (s.bitbuf, s.bits);
    // The last litlen symbol, from which the slack follows at the end: a literal's entry, or for
    // a match its pull in bits 12-15 and the bits it used (code, extra bits, distance) below.
    let mut last = 0u32;
    // The short-period match before: the `dist <= 8` bytes that follow it in phase, its period,
    // and its end.
    let (mut pn, mut pd, mut pend) = (0u64, 0usize, 0usize);

    macro_rules! refill {
        () => {
            // SAFETY: R1: `[ip, ip + 8)` is below the checked `ip + 22 <= inp.len()`.
            let w = unsafe { inb.add(ip).cast::<u64>().read_unaligned() };
            bitbuf |= u64::from_le(w) << (bits & 63);
            ip += ((!bits & 63) >> 3) as usize;
            bits |= 56;
        };
    }
    // The primary entry. A literal is never a link, so only the other entries are resolved, in
    // the branch after the literals, as deflate-go's assembly does.
    macro_rules! lookup {
        () => {{
            #[cfg(target_arch = "x86_64")]
            let i = if AVX2 {
                // SAFETY: `AVX2` is true only in the instantiation that `avx2.rs` calls from its
                // `avx2,bmi1,bmi2` target-feature function after detection, so the CPU has BMI2.
                // One non-destructive BZHI, as deflate-go, where `& LL_MASK` needs a copy first.
                unsafe { std::arch::x86_64::_bzhi_u64(bitbuf, LL_BITS) as usize }
            } else {
                (bitbuf & LL_MASK) as usize
            };
            #[cfg(not(target_arch = "x86_64"))]
            let i = (bitbuf & LL_MASK) as usize;
            // SAFETY: `i < 1 << LL_BITS`, and the primary table at `PT_OFF` or `LL_OFF` has that
            // many entries (`TOTAL = PT_OFF + PRIMARY`, `LL_SIZE > PRIMARY`).
            *unsafe { tab.get_unchecked((if PAIR { PT_OFF } else { LL_OFF }) + i) }
        }};
    }
    macro_rules! literal {
        ($e:expr) => {
            // The entry rotated by 16, as deflate-go's RORX: the byte(s) in the low half, the
            // entry's low half above (LIT at bit 23, the code length at 24-27, the pull at
            // 28-31), kept for the slack.
            last = $e.rotate_right(16);
            bitbuf >>= $e & 63;
            bits = bits.wrapping_sub($e);
            if PAIR {
                // A pair's second byte, or a single's junk, goes next.
                // SAFETY: W1: at most two literal steps came before in this pass, so the two
                // bytes at `op <= out_limit + 4` are within the capacity.
                unsafe {
                    p.add(op)
                        .cast::<[u8; 2]>()
                        .write((last as u16).to_le_bytes())
                };
                op += 1 + ($e >> 6 & 1) as usize;
            } else {
                // SAFETY: W1, as for the pair.
                unsafe { p.add(op).write(last as u8) };
                op += 1;
            }
        };
    }

    // `ip <= in_end` is `inp.len() - ip >= IN_MARGIN` (R1).
    let Some(in_end) = inp.len().checked_sub(IN_MARGIN) else {
        return;
    };
    'outer: while ip <= in_end && op <= out_limit {
        refill!();
        let mut e = lookup!();
        loop {
            if e & LIT != 0 {
                // Three literal steps fit in what the output limit leaves (`MAX_STEP`).
                literal!(e);
                e = lookup!();
                if e & LIT != 0 {
                    literal!(e);
                    e = lookup!();
                    if e & LIT != 0 {
                        literal!(e);
                        // Three literal steps use at most 33 of the 56 bits, so the lookup goes
                        // before the refill, off its path (deflate-go `b2lit3`).
                        e = lookup!();
                        if ip > in_end || op > out_limit {
                            break 'outer;
                        }
                        refill!();
                        continue;
                    }
                }
                // Refill for the match; e stays valid.
                refill!();
            }
            if e & (LINK | STOP) != 0 {
                if e & LINK != 0 {
                    let i = (e >> 16) + ((bitbuf >> LL_BITS) as u32 & ((1 << (e >> 8 & 15)) - 1));
                    e = tab[LL_OFF + i as usize];
                    if e & LIT != 0 {
                        // A literal with a long code; the next pass refills.
                        literal!(e);
                        continue 'outer;
                    }
                }
                if e & STOP != 0 {
                    break 'outer;
                }
            }

            // Commit only once the distance is valid.
            // `e` here has none of LIT, LINK, STOP (bits 5-7): its low byte is code + extra bits.
            let length = (e >> 16) as usize + (low::<AVX2>(bitbuf, e) >> (e >> 8 & 15)) as usize;
            let bb = bitbuf >> (e & 63);
            let mut d = tab[DT_OFF + (bb & DIST_MASK) as usize];
            if d & (LINK | STOP) != 0 {
                if d & LINK != 0 {
                    let i = (d >> 16) + ((bb >> DIST_BITS) as u32 & ((1 << (d >> 8 & 15)) - 1));
                    d = tab[DT_OFF + i as usize];
                }
                if d & STOP != 0 {
                    break 'outer;
                }
            }
            // `d` is a distance entry, never LIT, and here neither LINK nor STOP.
            let dist = (d >> 16) as usize + (low::<AVX2>(bb, d) >> (d >> 8 & 15)) as usize;
            if dist > op - hist {
                break 'outer;
            }
            bitbuf = bb >> (d & 63);
            // As deflate-go (MOVWLZX, ADD): the low half of `e` plus the distance's bits. Bits 5-7
            // of `e` and `d` are clear, so the low byte is the bits used (at most 20 + 28, no carry
            // into bit 8), bits 12-15 are the pull and bit 23 is clear (not a literal): all the
            // slack reads. `bits` is used mod 64, and bits 8 and up of `e` are multiples of 64.
            last = (e & 0xffff) + (d & 31);
            bits = bits.wrapping_sub(last);

            // Preload: overlap the lookup with copying. Every match follows a refill, after which
            // all 64 bits of `bitbuf` are input (the 8 bytes it ORs in reach bit 63), and a match
            // uses at most 48 of them, so the `LL_BITS` the lookup reads are input before this
            // refill as after it: the lookup goes first, off the refill's path.
            e = lookup!();
            refill!();

            // Word copies, as deflate-go's assembly: each word is read after the words before it
            // are written, so a period shorter than the copy repeats.
            let src = op - dist;
            if dist >= 16 {
                // The first 32 bytes with no loop (most matches are shorter), as deflate-go's
                // assembly: a loop here unrolls into a trip-count prologue that costs more than
                // the copy. Each word is read after the words before it are written.
                // SAFETY: W1 (32 bytes from `op` end below `op + length + 31`, `length >= 3`) and
                // W2 (the source `[src, src + 16)` ends at or below `op`; the second word's
                // source ends at `src + 32 <= op + 16`, which the first word wrote).
                unsafe {
                    ptr::copy_nonoverlapping(p.add(src), p.add(op), 16);
                    ptr::copy_nonoverlapping(p.add(src + 16), p.add(op + 16), 16);
                }
                if length > 32 {
                    let w = if dist >= 32 { 32 } else { 16 };
                    let mut i = 32;
                    while i < length {
                        // SAFETY: W1 (`i < length`: the words end below `op + length + 31`) and
                        // W2 (the source ends at `src + i + w <= op + i`, written before).
                        unsafe {
                            if w == 32 {
                                ptr::copy_nonoverlapping(p.add(src + i), p.add(op + i), 32);
                            } else {
                                ptr::copy_nonoverlapping(p.add(src + i), p.add(op + i), 16);
                                ptr::copy_nonoverlapping(
                                    p.add(src + i + 16),
                                    p.add(op + i + 16),
                                    16,
                                );
                            }
                        }
                        i += 32;
                    }
                }
            } else if AVX2 && op < 16 && dist > 1 {
                // Only in the first 16 bytes of the output: byte by byte, so the AVX2 loop keeps
                // no period cache (its registers go to the hot paths).
                for i in 0..length {
                    // SAFETY: W1 (`i < length`) and W2 (the source `op + i - dist` is below the
                    // byte written before it).
                    unsafe { p.add(op + i).write(p.add(src + i).read()) };
                }
            } else if AVX2 {
                // A short period: the last `dist` bytes repeated over 32, stored at multiples of
                // the period (deflate-go does it with PSHUFB and 16-byte stores).
                #[cfg(target_arch = "x86_64")]
                {
                    use std::arch::x86_64::{
                        __m256i, _mm_loadu_si128, _mm_shuffle_epi8, _mm256_set_m128i,
                        _mm256_set1_epi8, _mm256_storeu_si256,
                    };
                    // SAFETY: `AVX2` is true only in the instantiation that `avx2.rs` calls from
                    // its `avx2,bmi1,bmi2` target-feature function after `is_x86_feature_detected!`
                    // saw them, so every intrinsic here has its CPU features. W2: `src < op`, and
                    // with `op >= 16` the 16 bytes `[op - 16, op)` are output; the masks are 32
                    // bytes of `SHUF`.
                    let pat: __m256i = unsafe {
                        if dist == 1 {
                            _mm256_set1_epi8(p.add(src).read() as i8)
                        } else {
                            let w = _mm_loadu_si128(p.add(op - 16).cast());
                            let m = SHUF[dist].as_ptr();
                            _mm256_set_m128i(
                                _mm_shuffle_epi8(w, _mm_loadu_si128(m.add(16).cast())),
                                _mm_shuffle_epi8(w, _mm_loadu_si128(m.cast())),
                            )
                        }
                    };
                    let step = PERIODS32[dist];
                    let mut i = 0;
                    while i < length {
                        // SAFETY: W1: `i < length`, so the store ends below `op + length + 31`;
                        // and the CPU has AVX, as above.
                        unsafe { _mm256_storeu_si256(p.add(op + i).cast(), pat) };
                        i += step;
                    }
                }
            } else {
                // A short period: spread the `dist` bytes over 16 and store that, at multiples of
                // the period (deflate-go does it with PSHUFB).
                let x = if dist == 1 {
                    // SAFETY: W2: `src < op`.
                    u128::from(unsafe { p.add(src).read() })
                        * 0x0101_0101_0101_0101_0101_0101_0101_0101
                } else {
                    let mut x = if dist <= 8 && dist == pd && op == pend {
                        // The match before had this period and ended here: its bytes, kept. A
                        // load of them would wait for its stores.
                        u128::from(pn & (u64::MAX >> (64 - 8 * dist)))
                    } else if op >= 16 {
                        // The 16 bytes before `op`; the period is the last `dist` of them.
                        // SAFETY: W2: `[op - 16, op)` is output.
                        let w = unsafe { p.add(op - 16).cast::<[u8; 16]>().read() };
                        u128::from_le_bytes(w) >> (8 * (16 - dist))
                    } else {
                        let mut w = [0u8; 16];
                        // SAFETY: W2: `[src, src + dist)` is `[op - dist, op)`, and `dist < 16`
                        // bytes fit in `w`.
                        unsafe { ptr::copy_nonoverlapping(p.add(src), w.as_mut_ptr(), dist) };
                        u128::from_le_bytes(w)
                    };
                    x |= x << (8 * dist);
                    if dist < 8 {
                        x |= x << (16 * dist);
                        if dist < 4 {
                            x |= x << (32 * dist);
                        }
                    }
                    // Byte `j` of `x` is that at `op + j`, so the bytes after the match start at
                    // `length % dist`; for `dist <= 8` the next eight are in `x`.
                    (pn, pd, pend) = ((x >> (8 * (length % dist))) as u64, dist, op + length);
                    x
                };
                let bytes = x.to_le_bytes();
                let step = PERIODS[dist];
                let mut i = 0;
                while i < length {
                    // SAFETY: W1: `i < length` and `step <= 16`, so the stores end below
                    // `op + length + 31`.
                    unsafe {
                        p.add(op + i).cast::<[u8; 16]>().write(bytes);
                        p.add(op + i + step).cast::<[u8; 16]>().write(bytes);
                    }
                    i += 2 * step;
                }
            }
            op += length;
            if ip <= in_end && op <= out_limit {
                continue;
            }
            break 'outer;
        }
    }

    s.ip = ip;
    s.bitbuf = bitbuf;
    s.bits = bits & 63;
    s.slack = if last & (LIT << 16) != 0 {
        // A literal (or the pair's second), rotated by 16: its pull less its own length.
        (last >> 28) - (last >> 24 & 15)
    } else {
        (last >> 12 & 15).saturating_sub(last & 0xff)
    };
    // SAFETY: W3: the bytes up to `op` are written, and W1: `op <= out_limit + 262` is within the
    // capacity.
    unsafe { out.set_len(op) };
}
