//! Port of hash/adler32 `update` (see NOTICE).

use std::mem::MaybeUninit;

const MOD: u32 = 65521;
/// Most bytes summed before `s1` and `s2` need a reduction.
const NMAX: usize = 5552;

pub(super) fn update(adler: u32, p: &[u8]) -> u32 {
    let (mut s1, mut s2) = (adler & 0xffff, adler >> 16);
    for chunk in p.chunks(NMAX) {
        for &x in chunk {
            s1 += u32::from(x);
            s2 += s1;
        }
        s1 %= MOD;
        s2 %= MOD;
    }
    (s2 << 16) | s1
}

/// Copies `src` to `dst`, which is as long, and returns `update(adler, src)`: the reference of the
/// copy kernels, and the tail of the SIMD ones. It writes every byte of `dst`.
pub(super) fn copy_sum(adler: u32, src: &[u8], dst: &mut [MaybeUninit<u8>]) -> u32 {
    for (d, &s) in dst.iter_mut().zip(src) {
        d.write(s);
    }
    update(adler, src)
}
