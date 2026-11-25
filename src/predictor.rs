//! TIFF predictor 2 (horizontal differencing) and expansion of 8-bit grey, RGB and RGBA samples
//! to RGBA. Output equals the Go `predictor` package of deflate-go (tests/parity/go/upstream) byte for byte.
//!
//! These write into caller buffers and allocate nothing. The bytes they process are checked
//! against [`Limits::max_output`], and a cancel flag is checked per row. Go panics on a short
//! slice, a bad `spp`, or a size that overflows; here that is [`Error::InvalidArgument`].

use crate::{Error, Limits};
use std::sync::atomic::{AtomicBool, Ordering};

/// Bytes of one row between cancel checks.
const CANCEL_STEP: usize = 1 << 16;

/// Undoes predictor 2 on 8-bit samples and writes `w * h` RGBA pixels (stride `4 * w`, A = 255)
/// to `dst`. `src` is not modified.
///
/// `spp` is 1 (grey: v becomes (v, v, v), or 255 - v when `invert_grey`, after the undo),
/// 3 (RGB) or 4 (RGB, the 4th sample dropped). Only the first `w * h * spp` bytes of `src` and
/// `4 * w * h` bytes of `dst` are used. `4 * w * h` past `limits.max_output` is
/// [`Error::OutputLimit`]; a set `cancel` is [`Error::Cancelled`].
///
/// # Errors
///
/// [`Error::InvalidArgument`] for a short slice, an unsupported `spp` or an overflowing size;
/// [`Error::OutputLimit`] if `4 * w * h` passes `limits.max_output`; [`Error::Cancelled`] if
/// `cancel` is set. Allocates nothing, so `max_alloc` does not apply.
#[allow(clippy::too_many_arguments)] // Go's parameters plus limits and cancel
pub fn undo_expand_rgba(
    dst: &mut [u8],
    src: &[u8],
    w: usize,
    h: usize,
    spp: usize,
    invert_grey: bool,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    expand(dst, src, w, h, spp, true, invert_grey, limits, cancel)
}

/// [`undo_expand_rgba`] without the predictor (no compression, LZW, Deflate without predictor).
///
/// # Errors
///
/// As [`undo_expand_rgba`]: [`Error::InvalidArgument`], [`Error::OutputLimit`] (`4 * w * h` past
/// `limits.max_output`) and [`Error::Cancelled`].
#[allow(clippy::too_many_arguments)] // Go's parameters plus limits and cancel
pub fn expand_rgba(
    dst: &mut [u8],
    src: &[u8],
    w: usize,
    h: usize,
    spp: usize,
    invert_grey: bool,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    expand(dst, src, w, h, spp, false, invert_grey, limits, cancel)
}

/// Undoes predictor 2 in place on little-endian samples of `bytes_per_sample` (1 or 2) bytes;
/// only the first `h` rows are touched. Rows past `limits.max_output` bytes are
/// [`Error::OutputLimit`]; a set `cancel` is [`Error::Cancelled`].
///
/// # Errors
///
/// [`Error::InvalidArgument`] for a short `buf`, `spp` 0, a `bytes_per_sample` other than 1 or 2,
/// or an overflowing size; [`Error::OutputLimit`] if `w * h * spp * bytes_per_sample` passes
/// `limits.max_output`; [`Error::Cancelled`] if `cancel` is set. Allocates nothing, so
/// `max_alloc` does not apply.
pub fn undo_horizontal(
    buf: &mut [u8],
    w: usize,
    h: usize,
    spp: usize,
    bytes_per_sample: usize,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    if spp < 1 || !matches!(bytes_per_sample, 1 | 2) {
        return Err(Error::InvalidArgument);
    }
    // Zero-sized calls are no-ops whatever the other factors (so `w * spp * bps` may overflow).
    if w == 0 || h == 0 {
        return Ok(());
    }
    let rb = size(w, 1, spp, bytes_per_sample)?;
    let n = size(rb, h, 1, 1)?;
    if n > buf.len() {
        return Err(Error::InvalidArgument);
    }
    if n > limits.max_output {
        return Err(Error::OutputLimit);
    }
    // No overflow: rb, a multiple of it, was checked above.
    let p = spp * bytes_per_sample;
    for row in buf.chunks_exact_mut(rb).take(h) {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        // `CANCEL_STEP` bytes at a time (even, so a 16-bit sample never straddles two): one row
        // can be a gigabyte.
        for start in (p..row.len()).step_by(CANCEL_STEP) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let end = (start + CANCEL_STEP).min(row.len());
            if bytes_per_sample == 1 {
                for i in start..end {
                    row[i] = row[i].wrapping_add(row[i - p]);
                }
            } else {
                for i in (start..end.min(row.len() - 1)).step_by(2) {
                    let v = u16::from_le_bytes([row[i], row[i + 1]])
                        .wrapping_add(u16::from_le_bytes([row[i - p], row[i - p + 1]]));
                    row[i..i + 2].copy_from_slice(&v.to_le_bytes());
                }
            }
        }
    }
    Ok(())
}

/// `w * h * a * b`, or an error on overflow.
fn size(w: usize, h: usize, a: usize, b: usize) -> Result<usize, Error> {
    w.checked_mul(h)
        .and_then(|n| n.checked_mul(a))
        .and_then(|n| n.checked_mul(b))
        .ok_or(Error::InvalidArgument)
}

#[allow(clippy::too_many_arguments)] // Go's parameters plus limits and cancel
fn expand(
    dst: &mut [u8],
    src: &[u8],
    w: usize,
    h: usize,
    spp: usize,
    undo: bool,
    inv: bool,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    if !matches!(spp, 1 | 3 | 4) {
        return Err(Error::InvalidArgument);
    }
    let (sn, dn) = (size(w, h, spp, 1)?, size(w, h, 4, 1)?);
    if src.len() < sn || dst.len() < dn {
        return Err(Error::InvalidArgument);
    }
    if dn > limits.max_output {
        return Err(Error::OutputLimit);
    }
    if sn == 0 {
        return Ok(());
    }
    let rows = src[..sn]
        .chunks_exact(w * spp)
        .zip(dst[..dn].chunks_exact_mut(4 * w));
    for (s, d) in rows {
        expand_row(d, s, spp, undo, inv, cancel)?;
    }
    Ok(())
}

/// One row; with `undo` the sums start at 0 at its left edge. `CANCEL_STEP` bytes of output at a
/// time, with the flag checked before each: one row can be a gigabyte. The sums carry across.
fn expand_row(
    d: &mut [u8],
    s: &[u8],
    spp: usize,
    undo: bool,
    inv: bool,
    cancel: &AtomicBool,
) -> Result<(), Error> {
    const PIXELS: usize = CANCEL_STEP / 4;
    let xor = if inv { 0xff } else { 0 };
    let mut v = 0u8;
    let mut rgb = [0u8; 3];
    for (d, s) in d.chunks_mut(4 * PIXELS).zip(s.chunks(spp * PIXELS)) {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if spp == 1 {
            for (px, &x) in d.as_chunks_mut::<4>().0.iter_mut().zip(s) {
                v = if undo { v.wrapping_add(x) } else { x };
                let o = v ^ xor;
                *px = [o, o, o, 0xff];
            }
        } else {
            for (px, p) in d.as_chunks_mut::<4>().0.iter_mut().zip(s.chunks_exact(spp)) {
                for (c, &x) in rgb.iter_mut().zip(p) {
                    *c = if undo { c.wrapping_add(x) } else { x };
                }
                *px = [rgb[0], rgb[1], rgb[2], 0xff];
            }
        }
    }
    Ok(())
}
