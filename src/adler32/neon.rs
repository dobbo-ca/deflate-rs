//! NEON Adler-32: 16 bytes per step, the same scheme as `avx2.rs` with weights 16..=1. The copy
//! kernel stores each 16 bytes it sums.
//!
//! The kernel is a safe `#[target_feature]` fn; the `unsafe` is in `update` and `copy_sum` (the
//! calls, next to `detect`) and in `mem` (the loads and stores).

use super::Tier;
use std::arch::aarch64::*;
use std::mem::MaybeUninit;

const MOD: u64 = 65521;
/// Bytes per block: a multiple of 16, at most NMAX (5552).
const BLOCK: usize = 5552;

pub(super) fn detect() -> Option<Tier> {
    std::arch::is_aarch64_feature_detected!("neon").then_some(Tier {
        name: "neon",
        sum: update,
        copy: copy_sum,
    })
}

/// Only reachable through `detect`, which checked NEON.
#[allow(unsafe_code)]
fn update(adler: u32, p: &[u8]) -> u32 {
    let (c, tail) = p.as_chunks::<16>();
    // SAFETY: the one requirement of the call is that the CPU has NEON, and `detect` hands out
    // this fn only after `is_aarch64_feature_detected!("neon")` saw it.
    let a = unsafe { kernel::<false>(adler, c, &mut []) };
    super::scalar::update(a, tail)
}

/// Only reachable through `detect`, which checked NEON. `dst` is as long as `src`, so the kernel
/// and the scalar tail write every byte of it.
#[allow(unsafe_code)]
fn copy_sum(adler: u32, src: &[u8], dst: &mut [MaybeUninit<u8>]) -> u32 {
    let (c, tail) = src.as_chunks::<16>();
    let (d, dtail) = dst.as_chunks_mut::<16>();
    // SAFETY: as in `update`: `detect` saw NEON.
    let a = unsafe { kernel::<true>(adler, c, d) };
    super::scalar::copy_sum(a, tail, dtail)
}

/// The Adler-32 of `adler` followed by `p`; with `COPY`, also stores `p` into `dst` (as long).
#[target_feature(enable = "neon")]
fn kernel<const COPY: bool>(adler: u32, p: &[[u8; 16]], dst: &mut [[MaybeUninit<u8>; 16]]) -> u32 {
    const W: [u8; 16] = [16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1];
    let (mut s1, mut s2) = (u64::from(adler & 0xffff), u64::from(adler >> 16));
    let w = mem::load(&W);
    let mut d = dst.iter_mut();
    for block in p.chunks(BLOCK / 16) {
        let (mut v1, mut v2, mut sums) = (vdupq_n_u32(0), vdupq_n_u32(0), vdupq_n_u32(0));
        for c in block {
            let v = mem::load(c);
            if COPY && let Some(x) = d.next() {
                mem::store(x, v);
            }
            sums = vaddq_u32(sums, v1);
            v1 = vpadalq_u16(v1, vpaddlq_u8(v));
            v2 = vpadalq_u16(v2, vmull_u8(vget_low_u8(v), vget_low_u8(w)));
            v2 = vpadalq_u16(v2, vmull_high_u8(v, w));
        }
        s2 += s1 * (16 * block.len()) as u64 + 16 * vaddlvq_u32(sums) + vaddlvq_u32(v2);
        s1 += vaddlvq_u32(v1);
        s1 %= MOD;
        s2 %= MOD;
    }
    ((s2 << 16) | s1) as u32
}

/// The loads and stores: the array types are the bounds.
#[allow(unsafe_code)]
mod mem {
    use std::arch::aarch64::*;
    use std::mem::MaybeUninit;

    #[inline]
    #[target_feature(enable = "neon")]
    pub(super) fn load(a: &[u8; 16]) -> uint8x16_t {
        // SAFETY: `a` is 16 readable bytes, and vld1q_u8 needs no alignment; the CPU has NEON, as
        // this fn's target feature requires of its callers.
        unsafe { vld1q_u8(a.as_ptr()) }
    }

    #[inline]
    #[target_feature(enable = "neon")]
    pub(super) fn store(a: &mut [MaybeUninit<u8>; 16], v: uint8x16_t) {
        // SAFETY: `a` is 16 writable bytes, and vst1q_u8 needs no alignment; NEON as above.
        unsafe { vst1q_u8(a.as_mut_ptr().cast(), v) }
    }
}
