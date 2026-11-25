//go:build !purego && (amd64 || arm64)

package predictor

// In-place prefix sums per channel, one group of step bytes each;
// asm reads and writes p[:(n-1)*step+16].
//
//go:noescape
func undo8(p *byte, n, step int, t *[6][16]byte)

//go:noescape
func undo16(p *byte, n, step int, t *[6][16]byte)

// undoTab drives undo8 and undo16. Masks are PSHUFB/TBL indexes:
// shift by p, 2p, 4p, 8p bytes; broadcast the group's last pixel;
// keep (0xff) the g lanes of the group, so lanes past g store back
// the bytes they loaded.
type undoTab struct {
	m [6][16]byte
	g int
}

// undoTabs[bytesPerSample-1][spp-1].
var undoTabs = func() (t [2][4]undoTab) {
	for b := 1; b <= 2; b++ {
		for s := 1; s <= 4; s++ {
			p := s * b
			e := &t[b-1][s-1]
			e.g = 16 / p * p
			for i := range 16 {
				for k := range 4 {
					e.m[k][i] = 0x80
					if sh := p << k; i >= sh {
						e.m[k][i] = byte(i - sh)
					}
				}
				e.m[4][i] = byte(e.g - p + i%p)
				if i < e.g {
					e.m[5][i] = 0xff
				}
			}
		}
	}
	return t
}()

// undoVec undoes a prefix of row and returns its length, a multiple
// of spp*bps. Rows are one asm call each, so they stay preemptible.
func undoVec(row []byte, spp, bps int) int {
	if !useSIMD || spp > 4 || len(row) < 16 {
		return 0
	}
	t := &undoTabs[bps-1][spp-1]
	n := (len(row)-16)/t.g + 1
	if bps == 1 {
		undo8(&row[0], n, t.g, &t.m)
	} else {
		undo16(&row[0], n, t.g, &t.m)
	}
	return n * t.g
}
