//go:build !purego

package predictor

// Expand n groups of 16 grey pixels.
//
//go:noescape
func greySSSE3(dst, src *byte, n int, undo, inv bool, m *[7][16]byte)

// Expand n groups of 4 pixels, stride bytes of src each; reads 16.
//
//go:noescape
func rgbSSSE3(dst, src *byte, n, stride int, undo bool, shuf *[16]byte)

func cpuFeatures() (ssse3 bool)

var hasSIMD = cpuFeatures()

var useSIMD = hasSIMD

// masks: grey splats of bytes 4k..4k+3 to RGB0 (k = 0..3),
// broadcast byte 15, RGB to RGB0, RGBX to RGB0.
var masks = func() (m [7][16]byte) {
	for i := range 16 {
		m[4][i] = 15
		if i%4 == 3 {
			m[0][i], m[1][i], m[2][i], m[3][i], m[5][i], m[6][i] = 0x80, 0x80, 0x80, 0x80, 0x80, 0x80
			continue
		}
		for k := range 4 {
			m[k][i] = byte(4*k + i/4)
		}
		m[5][i] = byte(i/4*3 + i%4)
		m[6][i] = byte(i)
	}
	return m
}()

// expandVec expands a prefix of row d and returns its pixel count.
// s runs to the end of src.
func expandVec(d, s []byte, w, spp int, undo, inv bool) int {
	if !useSIMD {
		return 0
	}
	switch spp {
	case 1:
		n := w / 16
		if n > 0 {
			greySSSE3(&d[0], &s[0], n, undo, inv, &masks)
		}
		return 16 * n
	case 3:
		n := min(w/4, (len(s)-4)/12) // 16-byte loads stay in s
		if n > 0 {
			rgbSSSE3(&d[0], &s[0], n, 12, undo, &masks[5])
		}
		return 4 * max(n, 0)
	}
	n := w / 4
	if n > 0 {
		rgbSSSE3(&d[0], &s[0], n, 16, undo, &masks[6])
	}
	return 4 * n
}
