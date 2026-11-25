//! AVX-512 VNNI Adler-32: 256 bytes per step, the scheme of deflate-go's `updateVNNI512`.
//!
//! Per block of `n` bytes (a multiple of 64, at most `BLOCK`):
//! `s2 += n*s1 + sum((n-j) * p[j])` and `s1 += sum(p)`. The bytes go in rounds of 128, after a
//! round of 64 when `n / 64` is odd. In a round, VPDPBUSD weights byte `g` by `127 - g` (they fit
//! its signed bytes); VPSADBW gives the byte sums. `v1` holds the sum so far and `vp` adds `v1`
//! before each round, so `sum((n-j) * p[j])` is the weighted sums, plus `sum(p)`, plus `128 * vp`.
//! The weighted sums fit 32 bits for a block of 64 KiB. The copy kernel stores each 64 bytes it
//! sums.
//!
//! The kernel is a safe `#[target_feature]` fn; the `unsafe` is in `update` and `copy_sum` (the
//! calls, next to `detect`) and in `mem` (the loads and stores).

use super::Tier;
use std::arch::x86_64::*;
use std::mem::MaybeUninit;

const MOD: u64 = 65521;
/// Bytes per block.
const BLOCK: usize = 1 << 16;
/// The weights 127 down to 0 of a round of 128 bytes.
static WEIGHTS: [[u8; 64]; 2] = {
    let mut w = [[0u8; 64]; 2];
    let mut i = 0;
    while i < 128 {
        w[i / 64][i % 64] = (127 - i) as u8;
        i += 1;
    }
    w
};

pub(super) fn detect() -> Option<Tier> {
    let ok = is_x86_feature_detected!("avx512f")
        && is_x86_feature_detected!("avx512bw")
        && is_x86_feature_detected!("avx512vl")
        && is_x86_feature_detected!("avx512vnni");
    ok.then_some(Tier {
        name: "avx512_vnni",
        sum: update,
        copy: copy_sum,
    })
}

/// Only reachable through `detect`, which checked the CPU.
#[allow(unsafe_code)]
fn update(adler: u32, p: &[u8]) -> u32 {
    let (c, tail) = p.as_chunks::<64>();
    // SAFETY: the one requirement of the call is that the CPU has AVX512F, BW, VL and VNNI, and
    // `detect` hands out this fn only after `is_x86_feature_detected!` saw all four.
    let a = unsafe { kernel::<false>(adler, c, &mut []) };
    super::scalar::update(a, tail)
}

/// Only reachable through `detect`, which checked the CPU. `dst` is as long as `src`, so the
/// kernel and the scalar tail write every byte of it.
#[allow(unsafe_code)]
fn copy_sum(adler: u32, src: &[u8], dst: &mut [MaybeUninit<u8>]) -> u32 {
    let (c, tail) = src.as_chunks::<64>();
    let (d, dtail) = dst.as_chunks_mut::<64>();
    // SAFETY: as in `update`: `detect` saw the four features.
    let a = unsafe { kernel::<true>(adler, c, d) };
    super::scalar::copy_sum(a, tail, dtail)
}

/// The Adler-32 of `adler` followed by `p`; with `COPY`, also stores `p` into `dst` (as long).
#[target_feature(enable = "avx512f,avx512bw,avx512vl,avx512vnni")]
fn kernel<const COPY: bool>(adler: u32, p: &[[u8; 64]], dst: &mut [[MaybeUninit<u8>; 64]]) -> u32 {
    let (mut s1, mut s2) = (u64::from(adler & 0xffff), u64::from(adler >> 16));
    let (w0, w1) = (mem::load(&WEIGHTS[0]), mem::load(&WEIGHTS[1]));
    let zero = _mm512_setzero_si512();
    let mut d = dst.iter_mut();
    // The weighted sum of the 64 bytes `c` with weights `w`, added to `acc`; returns the byte
    // sums. The rounds run in the order of `p`, so each takes the next `dst` chunk.
    macro_rules! round {
        ($acc:ident, $c:expr, $w:ident) => {{
            let v = mem::load($c);
            if COPY && let Some(x) = d.next() {
                mem::store(x, v);
            }
            $acc = _mm512_dpbusd_epi32($acc, v, $w);
            _mm512_sad_epu8(v, zero)
        }};
    }
    for block in p.chunks(BLOCK / 64) {
        let n = 64 * block.len();
        let (mut a0, mut a1, mut a2, mut a3) = (zero, zero, zero, zero);
        let (mut v1, mut vp) = (zero, zero);
        let mut rest = block;
        if let ([head], r) = rest.split_at(block.len() % 2) {
            v1 = _mm512_add_epi64(v1, round!(a0, head, w1));
            rest = r;
        }
        // An even number of rounds of 64 is left: quads, then 0 or 2.
        let (quads, tail) = rest.as_chunks::<4>();
        for q in quads {
            vp = _mm512_add_epi64(vp, v1);
            let r0 = round!(a0, &q[0], w0);
            let r1 = round!(a1, &q[1], w1);
            let r2 = round!(a2, &q[2], w0);
            let r3 = round!(a3, &q[3], w1);
            let (s01, s23) = (_mm512_add_epi64(r0, r1), _mm512_add_epi64(r2, r3));
            v1 = _mm512_add_epi64(v1, s01);
            vp = _mm512_add_epi64(vp, v1);
            v1 = _mm512_add_epi64(v1, s23);
        }
        if let [t0, t1] = tail {
            vp = _mm512_add_epi64(vp, v1);
            let r0 = round!(a0, t0, w0);
            let r1 = round!(a1, t1, w1);
            v1 = _mm512_add_epi64(v1, _mm512_add_epi64(r0, r1));
        }
        let weighted = hsum32(_mm512_add_epi32(
            _mm512_add_epi32(a0, a1),
            _mm512_add_epi32(a2, a3),
        ));
        let sum = hsum64(v1);
        s2 += n as u64 * s1 + weighted + sum + 128 * hsum64(vp);
        s1 += sum;
        s1 %= MOD;
        s2 %= MOD;
    }
    ((s2 << 16) | s1) as u32
}

/// Sum of the u32 lanes, as u64.
#[target_feature(enable = "avx512f,avx2")]
fn hsum32(v: __m512i) -> u64 {
    hsum64(_mm512_add_epi64(
        _mm512_cvtepu32_epi64(_mm512_castsi512_si256(v)),
        _mm512_cvtepu32_epi64(_mm512_extracti64x4_epi64::<1>(v)),
    ))
}

/// Sum of the u64 lanes.
#[target_feature(enable = "avx512f,avx2")]
fn hsum64(v: __m512i) -> u64 {
    let s = _mm256_add_epi64(_mm512_castsi512_si256(v), _mm512_extracti64x4_epi64::<1>(v));
    (_mm256_extract_epi64::<0>(s)
        + _mm256_extract_epi64::<1>(s)
        + _mm256_extract_epi64::<2>(s)
        + _mm256_extract_epi64::<3>(s)) as u64
}

/// The loads and stores: the array types are the bounds.
#[allow(unsafe_code)]
mod mem {
    use std::arch::x86_64::*;
    use std::mem::MaybeUninit;

    #[inline]
    #[target_feature(enable = "avx512f")]
    pub(super) fn load(a: &[u8; 64]) -> __m512i {
        // SAFETY: `a` is 64 readable bytes, and loadu needs no alignment; the CPU has AVX512F,
        // as this fn's target feature requires of its callers.
        unsafe { _mm512_loadu_si512(a.as_ptr().cast()) }
    }

    #[inline]
    #[target_feature(enable = "avx512f")]
    pub(super) fn store(a: &mut [MaybeUninit<u8>; 64], v: __m512i) {
        // SAFETY: `a` is 64 writable bytes, and storeu needs no alignment; AVX512F as above.
        unsafe { _mm512_storeu_si512(a.as_mut_ptr().cast(), v) }
    }
}
