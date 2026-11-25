//go:build !purego

package predictor

// Expand n groups of 16 pixels of spp bytes.
//
//go:noescape
func expandNEON(dst, src *byte, n, spp int, undo, inv bool)

const hasSIMD = true

var useSIMD = hasSIMD

// expandVec expands a prefix of row d and returns its pixel count.
func expandVec(d, s []byte, w, spp int, undo, inv bool) int {
	n := w / 16
	if !useSIMD || n == 0 {
		return 0
	}
	expandNEON(&d[0], &s[0], n, spp, undo, inv)
	return 16 * n
}
