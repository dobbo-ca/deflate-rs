// Package predictor undoes TIFF predictor 2 (horizontal differencing)
// and expands 8-bit grey, RGB and RGBA samples to RGBA, with SIMD
// loops on amd64 and arm64. Output equals the plain per-pixel
// Go loops (predictor_generic.go) byte for byte.
package predictor

import (
	"encoding/binary"
	"math"
)

// UndoExpandRGBA undoes TIFF predictor 2 on 8-bit samples and writes w*h
// RGBA pixels (stride 4*w, A=255) to dst. src is not modified.
//
// spp is 1 (grey: v -> (v,v,v), or 255-v when invertGrey, after the
// undo), 3 (RGB) or 4 (RGB, 4th sample dropped). Only the first w*h*spp
// bytes of src and 4*w*h bytes of dst are used. It panics if a slice is
// short, spp is not 1, 3 or 4, or w or h is negative.
func UndoExpandRGBA(dst, src []byte, w, h, spp int, invertGrey bool) {
	expand(dst, src, w, h, spp, true, invertGrey)
}

// ExpandRGBA is UndoExpandRGBA without the predictor (CompNone, LZW,
// Deflate without predictor).
func ExpandRGBA(dst, src []byte, w, h, spp int, invertGrey bool) {
	expand(dst, src, w, h, spp, false, invertGrey)
}

// UndoHorizontal undoes predictor 2 in place on little-endian samples of
// bytesPerSample (1 or 2) bytes; only the first h rows are touched.
// It panics if buf is shorter than w*h*spp*bytesPerSample, spp < 1, or
// w or h is negative.
func UndoHorizontal(buf []byte, w, h, spp, bytesPerSample int) {
	if spp < 1 {
		panic("predictor: spp < 1")
	}
	if bytesPerSample != 1 && bytesPerSample != 2 {
		panic("predictor: bytesPerSample not 1 or 2")
	}
	p := spp * bytesPerSample
	rb := size(buf, "buf", w, h, spp, bytesPerSample) / max(h, 1)
	if rb == 0 {
		return // w or h is 0: skip h empty rows
	}
	for y := range h {
		row := buf[y*rb : (y+1)*rb]
		undoGo(row, max(undoVec(row, spp, bytesPerSample), p), p, bytesPerSample)
	}
}

func expand(dst, src []byte, w, h, spp int, undo, inv bool) {
	if spp != 1 && spp != 3 && spp != 4 {
		panic("predictor: spp not 1, 3 or 4")
	}
	rb := size(src, "src", w, h, spp, 1) / max(h, 1)
	size(dst, "dst", w, h, 4, 1)
	if rb == 0 {
		return
	}
	for y := range h {
		s := src[y*rb:] // to the end: vector loads may read past the row
		d := dst[4*w*y : 4*w*(y+1)]
		expandGo(d, s[:rb], expandVec(d, s, w, spp, undo, inv), spp, undo, inv)
	}
}

// size returns w*h*a*b and panics if b is shorter or it overflows.
func size(buf []byte, name string, w, h, a, b int) int {
	if w < 0 || h < 0 {
		panic("predictor: negative w or h")
	}
	n := w
	for _, k := range [...]int{h, a, b} {
		if k != 0 && n > math.MaxInt/k {
			panic("predictor: " + name + " size overflows int")
		}
		n *= k
	}
	if len(buf) < n {
		panic("predictor: " + name + " too short")
	}
	return n
}

// expandGo writes pixels x.. of row d; with undo, the sums
// carry on from pixel x-1 in d.
func expandGo(d, s []byte, x, spp int, undo, inv bool) {
	var r, g, b byte
	if undo && x > 0 {
		r, g, b = d[4*x-4], d[4*x-3], d[4*x-2]
	}
	w := len(d) / 4
	if spp == 1 {
		var xor byte
		if inv {
			xor = 0xff // 255-v
		}
		v := r
		if x > 0 {
			v ^= xor
		}
		for ; x < w; x++ {
			if undo {
				v += s[x]
			} else {
				v = s[x]
			}
			o := v ^ xor
			d[4*x], d[4*x+1], d[4*x+2], d[4*x+3] = o, o, o, 0xff
		}
		return
	}
	for ; x < w; x++ {
		p := s[x*spp : x*spp+3]
		if undo {
			r, g, b = r+p[0], g+p[1], b+p[2]
		} else {
			r, g, b = p[0], p[1], p[2]
		}
		d[4*x], d[4*x+1], d[4*x+2], d[4*x+3] = r, g, b, 0xff
	}
}

// undoGo undoes bytes i.. of row; p is bytes per pixel, i >= p.
func undoGo(row []byte, i, p, bps int) {
	if bps == 1 {
		for ; i < len(row); i++ {
			row[i] += row[i-p]
		}
		return
	}
	for ; i+2 <= len(row); i += 2 {
		binary.LittleEndian.PutUint16(row[i:], binary.LittleEndian.Uint16(row[i:])+binary.LittleEndian.Uint16(row[i-p:]))
	}
}
