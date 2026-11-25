#![no_main]
use deflate::Limits;
use deflate::predictor::{expand_rgba, undo_expand_rgba, undo_horizontal};
use libfuzzer_sys::fuzz_target;
use std::sync::atomic::AtomicBool;

/// Undo predictor 2 on little-endian samples, one sample after the other: the scalar reference.
fn ref_horizontal(src: &[u8], w: usize, h: usize, spp: usize, bps: usize) -> Vec<u8> {
    let mut d = src.to_vec();
    let sample = |d: &[u8], i: usize| {
        if bps == 1 {
            u16::from(d[i])
        } else {
            u16::from_le_bytes([d[i], d[i + 1]])
        }
    };
    for y in 0..h {
        for x in 1..w {
            for c in 0..spp {
                let i = ((y * w + x) * spp + c) * bps;
                let v = sample(&d, i).wrapping_add(sample(&d, i - spp * bps));
                if bps == 1 {
                    d[i] = v as u8;
                } else {
                    d[i..i + 2].copy_from_slice(&v.to_le_bytes());
                }
            }
        }
    }
    d
}

// data = [w][h][spp][flags][src]; w and h are below 33, spp up to 5 (or near usize::MAX) and bytes_per_sample up to 3
// (so the invalid ones are tried). Any input is Ok or an error, never a panic, and undoing the
// predictor then expanding equals the fused undo_expand_rgba, and an Ok undo_horizontal equals
// the scalar reference above. Bits 3-5 of `flags` are modes: a
// max_output of one byte too few, a dst of one byte too few, a set cancel flag. With any of them
// a call that has pixels to write must fail.
fuzz_target!(|data: &[u8]| {
    let Some((&[w, h, spp, flags], src)) = data.split_first_chunk::<4>() else {
        return;
    };
    // spp 250.. is usize::MAX - 5.., so size arithmetic overflows.
    let spp = if spp >= 250 {
        usize::MAX - usize::from(spp - 250)
    } else {
        usize::from(spp % 6)
    };
    let (l, c) = (Limits::default(), AtomicBool::new(false));
    let (w, h) = (usize::from(w % 33), usize::from(h % 33));
    let (inv, bps) = (flags & 1 != 0, usize::from(flags >> 1 & 3));
    let mode = flags >> 3 & 7;
    if mode != 0 {
        let n = 4 * w * h;
        let lim = Limits {
            max_output: if mode & 1 != 0 {
                n.saturating_sub(1)
            } else {
                l.max_output
            },
            ..l
        };
        let stop = AtomicBool::new(mode & 4 != 0);
        let mut dst = vec![
            0u8;
            if mode & 2 != 0 {
                n.saturating_sub(1)
            } else {
                n
            }
        ];
        let r1 = undo_expand_rgba(&mut dst, src, w, h, spp, inv, &lim, &stop);
        let r2 = expand_rgba(&mut dst, src, w, h, spp, inv, &lim, &stop);
        let mut buf = src.to_vec();
        let r3 = undo_horizontal(&mut buf, w, h, spp, bps, &lim, &stop);
        if w * h > 0 {
            assert!(r1.is_err() && r2.is_err());
            if mode & 4 != 0 {
                assert!(r3.is_err());
            }
        }
        return;
    }
    let mut buf = src.to_vec();
    if undo_horizontal(&mut buf, w, h, spp, bps, &l, &c).is_ok() {
        assert_eq!(buf, ref_horizontal(src, w, h, spp, bps));
    }

    let (mut a, mut b) = (vec![0u8; 4 * w * h], vec![0u8; 4 * w * h]);
    let fused = undo_expand_rgba(&mut a, src, w, h, spp, inv, &l, &c);
    let mut sums = src.to_vec();
    let split = undo_horizontal(&mut sums, w, h, spp, 1, &l, &c)
        .and_then(|()| expand_rgba(&mut b, &sums, w, h, spp, inv, &l, &c));
    assert_eq!(fused.is_ok(), split.is_ok());
    if fused.is_ok() {
        assert_eq!(a, b);
        assert!(a.chunks_exact(4).all(|p| p[3] == 0xff));
    }
});
