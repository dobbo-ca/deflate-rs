//! The predictor against plain per-pixel references. Go uses its generic loops as its
//! oracle; these are written from the definitions.

mod common;
use common::*;
use deflate::{Error, Limits, predictor};
use std::sync::atomic::AtomicBool;

/// An `spp` that overflows `spp * bytes_per_sample` is an error, not an arithmetic panic.
#[test]
fn undo_horizontal_size_overflow() {
    let bad = Err(Error::InvalidArgument);
    assert_eq!(
        predictor::undo_horizontal(
            &mut [],
            1,
            1,
            usize::MAX,
            2,
            &Limits::default(),
            &AtomicBool::new(false)
        ),
        bad
    );
    assert_eq!(
        predictor::undo_horizontal(
            &mut [],
            1,
            1,
            usize::MAX,
            1,
            &Limits::default(),
            &AtomicBool::new(false)
        ),
        bad
    );
    assert_eq!(
        predictor::undo_horizontal(
            &mut [],
            0,
            1,
            usize::MAX,
            2,
            &Limits::default(),
            &AtomicBool::new(false)
        ),
        Ok(())
    );
}

/// Zero rows is a no-op whatever `w * spp * bytes_per_sample` is, as in Go.
#[test]
fn undo_horizontal_zero_height() {
    let l = Limits::default();
    let no = AtomicBool::new(false);
    for (w, spp) in [
        (1, usize::MAX),
        (usize::MAX / 2 + 1, 3),
        (usize::MAX, usize::MAX),
    ] {
        for bps in [1, 2] {
            assert_eq!(
                predictor::undo_horizontal(&mut [], w, 0, spp, bps, &l, &no),
                Ok(()),
                "w {w} spp {spp} bps {bps}"
            );
        }
    }
    // The arguments are still validated.
    assert_eq!(
        predictor::undo_horizontal(&mut [], 1, 0, 0, 1, &l, &no),
        Err(Error::InvalidArgument)
    );
}

/// Undo predictor 2 per channel, then expand to RGBA.
fn ref_expand(src: &[u8], w: usize, h: usize, spp: usize, undo: bool, inv: bool) -> Vec<u8> {
    let mut d = src[..w * h * spp].to_vec();
    if undo {
        for y in 0..h {
            for x in 1..w {
                for c in 0..spp {
                    let i = (y * w + x) * spp + c;
                    d[i] = d[i].wrapping_add(d[i - spp]);
                }
            }
        }
    }
    let mut out = Vec::new();
    for px in d.chunks(spp) {
        if spp == 1 {
            let v = if inv { 255 - px[0] } else { px[0] };
            out.extend_from_slice(&[v, v, v, 255]);
        } else {
            out.extend_from_slice(&[px[0], px[1], px[2], 255]);
        }
    }
    out
}

const WIDTHS: [usize; 14] = [0, 1, 2, 3, 4, 5, 15, 16, 17, 31, 33, 70, 801, 1024];

#[test]
fn expand_rgba() {
    let mut r = Rng::new(1);
    for w in WIDTHS {
        for h in 0..4 {
            for spp in [1, 3, 4] {
                for extra in [0, 7] {
                    let src = rand_bytes(r.next_u64(), w * h * spp + extra);
                    let orig = src.clone();
                    for (undo, inv) in [(false, false), (false, true), (true, false), (true, true)]
                    {
                        let want = ref_expand(&src, w, h, spp, undo, inv);
                        let mut dst = vec![0xa5; 4 * w * h + 5];
                        let f = if undo {
                            predictor::undo_expand_rgba
                        } else {
                            predictor::expand_rgba
                        };
                        f(
                            &mut dst,
                            &src,
                            w,
                            h,
                            spp,
                            inv,
                            &Limits::default(),
                            &AtomicBool::new(false),
                        )
                        .unwrap();
                        assert!(
                            dst[..4 * w * h] == want,
                            "w {w} h {h} spp {spp} undo {undo} inv {inv}"
                        );
                        assert!(
                            dst[4 * w * h..].iter().all(|&b| b == 0xa5),
                            "wrote past 4*w*h"
                        );
                        assert_eq!(src, orig, "src modified");
                    }
                }
            }
        }
    }
}

/// Undo predictor 2 on little-endian samples, one sample after the other.
fn ref_horizontal(buf: &[u8], w: usize, h: usize, spp: usize, bps: usize) -> Vec<u8> {
    let mut d = buf.to_vec();
    let sample = |d: &[u8], i: usize| -> u16 {
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

#[test]
fn undo_horizontal() {
    let mut r = Rng::new(3);
    for w in WIDTHS.iter().copied().filter(|&w| w < 100).chain([801]) {
        for h in 0..4 {
            for spp in 1..=5 {
                for bps in [1, 2] {
                    // Rows past h are left alone.
                    let buf = rand_bytes(r.next_u64(), (w * (h + 1)) * spp * bps + 3);
                    let want = ref_horizontal(&buf, w, h, spp, bps);
                    let mut got = buf.clone();
                    predictor::undo_horizontal(
                        &mut got,
                        w,
                        h,
                        spp,
                        bps,
                        &Limits::default(),
                        &AtomicBool::new(false),
                    )
                    .unwrap();
                    assert!(got == want, "w {w} h {h} spp {spp} bps {bps}");
                }
            }
        }
    }
}

#[test]
fn bad_arguments_are_errors() {
    let (mut dst, src) = (vec![0u8; 64], vec![0u8; 64]);
    let bad = Err(Error::InvalidArgument);
    for spp in [0, 2, 5] {
        assert_eq!(
            predictor::expand_rgba(
                &mut dst,
                &src,
                2,
                2,
                spp,
                false,
                &Limits::default(),
                &AtomicBool::new(false)
            ),
            bad
        );
        assert_eq!(
            predictor::undo_expand_rgba(
                &mut dst,
                &src,
                2,
                2,
                spp,
                false,
                &Limits::default(),
                &AtomicBool::new(false)
            ),
            bad
        );
    }
    // Short slices.
    assert_eq!(
        predictor::expand_rgba(
            &mut dst,
            &src[..11],
            2,
            2,
            3,
            false,
            &Limits::default(),
            &AtomicBool::new(false)
        ),
        bad
    );
    assert_eq!(
        predictor::expand_rgba(
            &mut dst[..15],
            &src,
            2,
            2,
            3,
            false,
            &Limits::default(),
            &AtomicBool::new(false)
        ),
        bad
    );
    // Sizes that overflow.
    let big = usize::MAX / 2 + 1;
    assert_eq!(
        predictor::expand_rgba(
            &mut dst,
            &src,
            big,
            3,
            3,
            false,
            &Limits::default(),
            &AtomicBool::new(false)
        ),
        bad
    );
    assert_eq!(
        predictor::undo_horizontal(
            &mut dst,
            big,
            1,
            3,
            2,
            &Limits::default(),
            &AtomicBool::new(false)
        ),
        bad
    );
    assert_eq!(
        predictor::undo_horizontal(
            &mut dst,
            4,
            usize::MAX,
            1,
            1,
            &Limits::default(),
            &AtomicBool::new(false)
        ),
        bad
    );
    // undo_horizontal's own arguments.
    assert_eq!(
        predictor::undo_horizontal(
            &mut dst,
            4,
            2,
            0,
            1,
            &Limits::default(),
            &AtomicBool::new(false)
        ),
        bad
    );
    assert_eq!(
        predictor::undo_horizontal(
            &mut dst,
            4,
            2,
            1,
            3,
            &Limits::default(),
            &AtomicBool::new(false)
        ),
        bad
    );
    assert_eq!(
        predictor::undo_horizontal(
            &mut dst[..7],
            4,
            2,
            1,
            1,
            &Limits::default(),
            &AtomicBool::new(false)
        ),
        bad
    );
    // Empty images are fine, and need no data.
    assert_eq!(
        predictor::expand_rgba(
            &mut [],
            &[],
            0,
            5,
            3,
            false,
            &Limits::default(),
            &AtomicBool::new(false)
        ),
        Ok(())
    );
    assert_eq!(
        predictor::expand_rgba(
            &mut [],
            &[],
            5,
            0,
            3,
            false,
            &Limits::default(),
            &AtomicBool::new(false)
        ),
        Ok(())
    );
    assert_eq!(
        predictor::undo_horizontal(
            &mut [],
            0,
            5,
            3,
            2,
            &Limits::default(),
            &AtomicBool::new(false)
        ),
        Ok(())
    );
}

/// The bytes processed are charged against `max_output`, and a set cancel flag stops the call.
#[test]
fn limits_and_cancel() {
    let (mut dst, src) = (vec![0u8; 64], vec![0u8; 64]);
    let stop = AtomicBool::new(true);
    let lim = |max_output| Limits {
        max_output,
        ..Limits::default()
    };
    // 4 * 2 * 2 = 16 bytes out, 4 * 2 * 3 * 1 = 24 bytes in place.
    assert_eq!(
        predictor::expand_rgba(&mut dst, &src, 2, 2, 3, false, &lim(15), &NO_CANCEL),
        Err(Error::OutputLimit)
    );
    assert_eq!(
        predictor::undo_expand_rgba(&mut dst, &src, 2, 2, 3, false, &lim(15), &NO_CANCEL),
        Err(Error::OutputLimit)
    );
    assert_eq!(
        predictor::undo_horizontal(&mut dst, 4, 2, 3, 1, &lim(23), &NO_CANCEL),
        Err(Error::OutputLimit)
    );
    assert_eq!(
        predictor::expand_rgba(&mut dst, &src, 2, 2, 3, false, &lim(16), &NO_CANCEL),
        Ok(())
    );
    assert_eq!(
        predictor::undo_horizontal(&mut dst, 4, 2, 3, 1, &lim(24), &NO_CANCEL),
        Ok(())
    );
    let l = Limits::default();
    assert_eq!(
        predictor::expand_rgba(&mut dst, &src, 2, 2, 3, false, &l, &stop),
        Err(Error::Cancelled)
    );
    assert_eq!(
        predictor::undo_expand_rgba(&mut dst, &src, 2, 2, 3, false, &l, &stop),
        Err(Error::Cancelled)
    );
    assert_eq!(
        predictor::undo_horizontal(&mut dst, 4, 2, 3, 1, &l, &stop),
        Err(Error::Cancelled)
    );
}

/// One row of 16 Mi pixels is checked for cancel inside the row, not only between rows: a flag
/// set while it runs stops it before the end (and the sums carry across the checks).
#[test]
fn expand_checks_cancel_inside_a_row() {
    let w = 1 << 24;
    let src = vec![1u8; w];
    let mut dst = vec![0u8; 4 * w];
    let stop = AtomicBool::new(false);
    let r = std::thread::scope(|sc| {
        sc.spawn(|| stop.store(true, std::sync::atomic::Ordering::Relaxed));
        predictor::undo_expand_rgba(&mut dst, &src, w, 1, 1, false, &Limits::default(), &stop)
    });
    assert_eq!(r, Err(Error::Cancelled));
    assert_eq!(dst[4 * w - 1], 0, "the row ran to the end");
}

/// The chunking of a row does not change the sums: a row of 40,000 pixels (three chunks).
#[test]
fn expand_row_chunks_carry_the_sums() {
    let (w, l, no) = (40_000, Limits::default(), AtomicBool::new(false));
    for spp in [1usize, 3, 4] {
        let src: Vec<u8> = (0..w * spp).map(|i| (i * 7 + 3) as u8).collect();
        let mut dst = vec![0u8; 4 * w];
        predictor::undo_expand_rgba(&mut dst, &src, w, 1, spp, false, &l, &no).unwrap();
        let mut sum = [0u8; 3];
        for (x, px) in dst.as_chunks::<4>().0.iter().enumerate() {
            for (c, sm) in sum.iter_mut().enumerate().take(spp.min(3)) {
                *sm = sm.wrapping_add(src[x * spp + c]);
            }
            let want = if spp == 1 { [sum[0]; 3] } else { sum };
            assert_eq!(&px[..3], &want, "spp {spp} pixel {x}");
        }
    }
}
