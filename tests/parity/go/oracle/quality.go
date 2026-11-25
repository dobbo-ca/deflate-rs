// The real-file check (docs/parity.md, "Real-file check"):
//
//	oracle -qgen <tiles> <fixtures> <out> <slide>...   write the gate's deflate inputs
//	oracle -qdecode <in> <out>                         decode each input to a PGM or PPM
//
// examples/quality.rs is the Rust side of -qdecode. An input's name carries what the decode needs:
// <stem>.w<W>.c<C>.p<P>.zlib (or .deflate for raw DEFLATE): W samples per row, C channels,
// predictor P (1 none, 2 horizontal). The output is <stem>.w<W>.c<C>.p<P>.pgm (C = 1) or .ppm
// (C = 3), W wide; other C are written as a PGM W*C wide.
package main

import (
	"bytes"
	"compress/zlib"
	"encoding/binary"
	"errors"
	"fmt"
	"hash/fnv"
	"image"
	"image/jpeg"
	"io"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	deflate "github.com/dobbo-ca/deflate-go"
	"github.com/dobbo-ca/deflate-go/predictor"
	dzlib "github.com/dobbo-ca/deflate-go/zlib"
)

const (
	// The most deflate tiles taken from one IFD of a slide, evenly spread.
	maxPerIFD = 1000
	// A JPEG tile larger than this (pixels) is skipped: NDPI levels are single strips of
	// gigapixels.
	maxTilePixels = 16 << 20
	// The large streams (one per slide, for the first maxLargeStreams slides: the pixels of its
	// tiles in name order) stop at this size.
	maxLarge        = 64 << 20
	maxLargeStreams = 2
)

// largeStream compresses one slide's tile pixels as they come, so the oracle never holds more
// than one tile (the oracle runs inside BuildKit: its memory stays bounded).
type largeStream struct {
	slide string
	f     *os.File
	z     *zlib.Writer
	n     int
}

func (l *largeStream) open(out string) error {
	f, err := os.Create(filepath.Join(out, "large-"+l.slide+".w4096.c1.p1.zlib"))
	if err != nil {
		return err
	}
	z, err := zlib.NewWriterLevel(f, 6)
	if err != nil {
		f.Close()
		return err
	}
	l.f, l.z = f, z
	return nil
}

func (l *largeStream) write(pix []byte) error {
	if l.z == nil || l.n >= maxLarge {
		return nil
	}
	pix = pix[:min(len(pix), maxLarge-l.n)]
	l.n += len(pix)
	_, err := l.z.Write(pix)
	return err
}

// close pads the stream to whole rows of 4096 and closes it.
func (l *largeStream) close() error {
	if l.z == nil {
		return nil
	}
	if _, err := l.z.Write(make([]byte, (4096-l.n%4096)%4096)); err != nil {
		return err
	}
	if err := l.z.Close(); err != nil {
		return err
	}
	fmt.Fprintf(os.Stderr, "qgen: large-%s: %d bytes\n", l.slide, l.n)
	l.z = nil
	return l.f.Close()
}

// qgen writes the gate's inputs into out:
//   - deflate tiles: every deflate-compressed tile or strip (TIFF compression 8 or 32946) of the
//     slides, as it is in the file, at most maxPerIFD per IFD;
//   - tiles: every JPEG tile that `quality tiles` wrote, decoded with image/jpeg, then zlib
//     compressed by compress/zlib as a TIFF writer does (level 1, 6 or 9 and predictor 1 or 2,
//     picked by a hash of the name);
//   - large: per slide, its decoded tiles in name order as one zlib stream (level 6, 4096 wide,
//     at most maxLarge), the input of the parallel decode of one stream;
//   - corpus: the fixtures that decode without a dictionary, as bytes (1 wide).
func qgen(tiles, fixtures, out string, slides []string) error {
	if err := os.MkdirAll(out, 0o755); err != nil {
		return err
	}
	var nDeflate int
	for _, s := range slides {
		n, err := deflateTiles(s, out)
		if err != nil {
			return fmt.Errorf("%s: %w", s, err)
		}
		fmt.Fprintf(os.Stderr, "qgen: %s: %d deflate tiles\n", filepath.Base(s), n)
		nDeflate += n
	}
	files, err := os.ReadDir(tiles)
	if err != nil {
		return err
	}
	var lg largeStream
	var nTiles, nSkipped, nLarge int
	for _, f := range files {
		name := f.Name()
		if !strings.HasSuffix(name, ".jpg") {
			continue
		}
		data, err := os.ReadFile(filepath.Join(tiles, name))
		if err != nil {
			return err
		}
		cfg, err := jpeg.DecodeConfig(bytes.NewReader(data))
		if err != nil || cfg.Width*cfg.Height > maxTilePixels {
			nSkipped++
			continue
		}
		img, err := jpeg.Decode(bytes.NewReader(data))
		if err != nil {
			nSkipped++
			continue
		}
		pix, c := samples(img)
		if pix == nil {
			nSkipped++
			continue
		}
		w := img.Bounds().Dx()
		stem := strings.TrimSuffix(name, ".jpg")
		h := fnv.New32a()
		h.Write([]byte(stem))
		k := h.Sum32()
		level := []int{1, 6, 9}[k%3]
		pred := 1 + int(k/3%2)
		z, err := compress(pix, w, c, pred, level)
		if err != nil {
			return err
		}
		if err := os.WriteFile(filepath.Join(out, fmt.Sprintf("%s.w%d.c%d.p%d.zlib", stem, w, c, pred)), z, 0o644); err != nil {
			return err
		}
		nTiles++
		// The tiles of one slide are next to each other in name order.
		slide, _, _ := strings.Cut(stem, ".ifd")
		if slide != lg.slide {
			if err := lg.close(); err != nil {
				return err
			}
			lg = largeStream{slide: slide}
			if nLarge < maxLargeStreams {
				if err := lg.open(out); err != nil {
					return err
				}
				nLarge++
			}
		}
		if err := lg.write(pix); err != nil {
			return err
		}
	}
	if err := lg.close(); err != nil {
		return err
	}
	fmt.Fprintf(os.Stderr, "qgen: %d JPEG tiles to zlib, %d skipped\n", nTiles, nSkipped)
	fx, err := os.ReadDir(fixtures)
	if err != nil {
		return err
	}
	var nCorpus int
	for _, f := range fx {
		name := f.Name()
		ext := ""
		switch {
		case strings.HasSuffix(name, ".zlib"):
			ext = "zlib"
		case strings.HasSuffix(name, ".raw.deflate"):
			ext = "deflate"
		default:
			continue
		}
		data, err := os.ReadFile(filepath.Join(fixtures, name))
		if err != nil {
			return err
		}
		stem := "corpus-" + strings.ReplaceAll(name, ".", "_")
		if err := os.WriteFile(filepath.Join(out, fmt.Sprintf("%s.w1.c1.p1.%s", stem, ext)), data, 0o644); err != nil {
			return err
		}
		nCorpus++
	}
	fmt.Fprintf(os.Stderr, "qgen: %d deflate tiles in the slides, %d JPEG tiles as zlib, %d large streams, %d corpus files\n",
		nDeflate, nTiles, nLarge, nCorpus)
	return nil
}

// samples returns the 8-bit samples of img, row by row, and the channels (1 or 3); nil for a
// model it does not convert (CMYK).
func samples(img image.Image) ([]byte, int) {
	b := img.Bounds()
	w, h := b.Dx(), b.Dy()
	switch m := img.(type) {
	case *image.Gray:
		pix := make([]byte, 0, w*h)
		for y := 0; y < h; y++ {
			pix = append(pix, m.Pix[y*m.Stride:y*m.Stride+w]...)
		}
		return pix, 1
	case *image.YCbCr:
		pix := make([]byte, 0, w*h*3)
		for y := b.Min.Y; y < b.Max.Y; y++ {
			for x := b.Min.X; x < b.Max.X; x++ {
				c := m.YCbCrAt(x, y)
				r, g, bl, _ := c.RGBA()
				pix = append(pix, byte(r>>8), byte(g>>8), byte(bl>>8))
			}
		}
		return pix, 3
	}
	return nil, 0
}

// compress applies predictor 2 (when pred is 2) to a copy of pix, w samples of c channels per
// row, then zlib-compresses it at level.
func compress(pix []byte, w, c, pred, level int) ([]byte, error) {
	if pred == 2 {
		p := append([]byte(nil), pix...)
		row := w * c
		for y := 0; y+row <= len(p); y += row {
			for i := row - 1; i >= c; i-- {
				p[y+i] -= p[y+i-c]
			}
		}
		pix = p
	}
	var b bytes.Buffer
	zw, err := zlib.NewWriterLevel(&b, level)
	if err != nil {
		return nil, err
	}
	if _, err := zw.Write(pix); err != nil {
		return nil, err
	}
	if err := zw.Close(); err != nil {
		return nil, err
	}
	return b.Bytes(), nil
}

// tiffFile reads the IFDs of a classic or BigTIFF file.
type tiffFile struct {
	f   *os.File
	bo  binary.ByteOrder
	big bool
}

func (t *tiffFile) read(off int64, n int) ([]byte, error) {
	b := make([]byte, n)
	_, err := t.f.ReadAt(b, off)
	return b, err
}

func (t *tiffFile) uint(b []byte) uint64 {
	switch len(b) {
	case 1:
		return uint64(b[0])
	case 2:
		return uint64(t.bo.Uint16(b))
	case 4:
		return uint64(t.bo.Uint32(b))
	}
	return t.bo.Uint64(b)
}

// ifds returns, per IFD, its tags as tag -> values (integer types only).
func (t *tiffFile) ifds() ([]map[uint16][]uint64, error) {
	h, err := t.read(0, 16)
	if err != nil {
		return nil, err
	}
	switch string(h[:2]) {
	case "II":
		t.bo = binary.LittleEndian
	case "MM":
		t.bo = binary.BigEndian
	default:
		return nil, errors.New("not a TIFF")
	}
	var next uint64
	switch t.bo.Uint16(h[2:]) {
	case 42:
		next = uint64(t.bo.Uint32(h[4:]))
	case 43:
		t.big = true
		next = t.bo.Uint64(h[8:])
	default:
		return nil, errors.New("not a TIFF")
	}
	cntSize, entSize, offSize := 2, 12, 4
	if t.big {
		cntSize, entSize, offSize = 8, 20, 8
	}
	var out []map[uint16][]uint64
	seen := map[uint64]bool{}
	for next != 0 && !seen[next] && len(out) < 1000 {
		seen[next] = true
		cb, err := t.read(int64(next), cntSize)
		if err != nil {
			return nil, err
		}
		n := int(t.uint(cb))
		if n > 4096 {
			return nil, errors.New("IFD too long")
		}
		eb, err := t.read(int64(next)+int64(cntSize), n*entSize+offSize)
		if err != nil {
			return nil, err
		}
		tags := map[uint16][]uint64{}
		for i := 0; i < n; i++ {
			e := eb[i*entSize:]
			tag, typ := t.bo.Uint16(e), t.bo.Uint16(e[2:])
			cnt := t.uint(e[4 : 4+offSize])
			size := map[uint16]int{1: 1, 3: 2, 4: 4, 16: 8}[typ]
			if size == 0 || cnt > 1<<24 {
				continue
			}
			val := e[4+offSize : 4+2*offSize]
			raw := val[:min(len(val), int(cnt)*size)]
			if int(cnt)*size > offSize {
				if raw, err = t.read(int64(t.uint(val)), int(cnt)*size); err != nil {
					return nil, err
				}
			}
			vs := make([]uint64, cnt)
			for j := range vs {
				vs[j] = t.uint(raw[j*size : (j+1)*size])
			}
			tags[tag] = vs
		}
		out = append(out, tags)
		next = t.uint(eb[n*entSize:])
	}
	return out, nil
}

// deflateTiles writes the deflate-compressed tiles and strips of the slide into out, at most
// maxPerIFD per IFD evenly spread, and returns how many.
func deflateTiles(slide, out string) (int, error) {
	f, err := os.Open(slide)
	if err != nil {
		return 0, err
	}
	defer f.Close()
	t := &tiffFile{f: f}
	ifds, err := t.ifds()
	if err != nil {
		return 0, err
	}
	one := func(tags map[uint16][]uint64, tag uint16, def uint64) uint64 {
		if v := tags[tag]; len(v) > 0 {
			return v[0]
		}
		return def
	}
	base := strings.ReplaceAll(filepath.Base(slide), ".", "_")
	var n int
	for k, tags := range ifds {
		if c := one(tags, 259, 1); c != 8 && c != 32946 {
			continue
		}
		offs, lens := tags[324], tags[325]
		w := one(tags, 322, 0)
		if len(offs) == 0 {
			offs, lens = tags[273], tags[279]
			w = one(tags, 256, 0)
		}
		spp, bps := one(tags, 277, 1), one(tags, 258, 8)
		pred := one(tags, 317, 1)
		if bps != 8 || one(tags, 284, 1) != 1 || pred > 2 || w == 0 {
			// Not 8-bit chunky samples: compare the inflated bytes, 1 wide.
			w, spp, pred = 1, 1, 1
		}
		cnt := min(len(offs), len(lens))
		take := min(cnt, maxPerIFD)
		for j := 0; j < take; j++ {
			i := j * cnt / take
			if lens[i] == 0 || lens[i] > 1<<30 {
				continue
			}
			data, err := t.read(int64(offs[i]), int(lens[i]))
			if err != nil {
				return n, err
			}
			ext := "zlib"
			if len(data) < 2 || (uint16(data[0])<<8|uint16(data[1]))%31 != 0 || data[0]&0x0f != 8 {
				ext = "deflate"
			}
			name := fmt.Sprintf("%s.ifd%d.%d.w%d.c%d.p%d.%s", base, k, i, w, spp, pred, ext)
			if err := os.WriteFile(filepath.Join(out, name), data, 0o644); err != nil {
				return n, err
			}
			n++
		}
	}
	return n, nil
}

// qlarge writes the bench's large stream: the outputs of the fixtures that decode, in name
// order and repeated, up to 64 MiB, zlib level 6 (compress/zlib).
func qlarge(fixtures, out string) error {
	fx, err := os.ReadDir(fixtures)
	if err != nil {
		return err
	}
	var parts [][]byte
	for _, f := range fx {
		name := f.Name()
		if strings.Contains(name, "@") || !(strings.HasSuffix(name, ".zlib") || strings.HasSuffix(name, ".raw.deflate")) {
			continue
		}
		data, err := os.ReadFile(filepath.Join(fixtures, name))
		if err != nil {
			return err
		}
		var b []byte
		if strings.HasSuffix(name, ".zlib") {
			b, err = dzlib.Decode(nil, data)
		} else {
			b, err = deflate.Decode(nil, data)
		}
		if err == nil && len(b) > 0 {
			parts = append(parts, b)
		}
	}
	if len(parts) == 0 {
		return errors.New("qlarge: no fixture decodes")
	}
	const size = 64 << 20
	pix := make([]byte, 0, size)
	for i := 0; len(pix) < size; i++ {
		p := parts[i%len(parts)]
		pix = append(pix, p[:min(len(p), size-len(pix))]...)
	}
	z, err := compress(pix, len(pix), 1, 1, 6)
	if err != nil {
		return err
	}
	return os.WriteFile(out, z, 0o644)
}

// qname parses <stem>.w<W>.c<C>.p<P>.<ext>.
func qname(name string) (stem string, w, c, p int, z bool, ok bool) {
	ext := filepath.Ext(name)
	if ext != ".zlib" && ext != ".deflate" {
		return "", 0, 0, 0, false, false
	}
	stem = strings.TrimSuffix(name, ext)
	parts := strings.Split(stem, ".")
	if len(parts) < 4 {
		return "", 0, 0, 0, false, false
	}
	get := func(s, pre string) int {
		if !strings.HasPrefix(s, pre) {
			return -1
		}
		v, err := strconv.Atoi(s[len(pre):])
		if err != nil {
			return -1
		}
		return v
	}
	n := len(parts)
	w, c, p = get(parts[n-3], "w"), get(parts[n-2], "c"), get(parts[n-1], "p")
	if w < 1 || c < 1 || (p != 1 && p != 2) {
		return "", 0, 0, 0, false, false
	}
	return stem, w, c, p, ext == ".zlib", true
}

// qdecode decodes every input of in with deflate-go and writes the PGM/PPM into out. An input
// that fails to decode, or whose output is empty or not whole rows, gets no output file: the
// gate then fails on a file present on one side only, unless Rust fails the same way.
func qdecode(in, out string) error {
	if err := os.MkdirAll(out, 0o755); err != nil {
		return err
	}
	files, err := os.ReadDir(in)
	if err != nil {
		return err
	}
	var d deflate.Decoder
	var zd dzlib.Decoder
	var buf []byte
	var nOut, nFail int
	var spent time.Duration
	for _, f := range files {
		stem, w, c, p, z, ok := qname(f.Name())
		if !ok {
			continue
		}
		src, err := os.ReadFile(filepath.Join(in, f.Name()))
		if err != nil {
			return err
		}
		t := time.Now()
		if z {
			buf, err = zd.Decode(buf[:0], src)
		} else {
			buf, err = d.Decode(buf[:0], src)
		}
		spent += time.Since(t)
		row := w * c
		if err != nil || len(buf) == 0 || len(buf)%row != 0 {
			nFail++
			continue
		}
		h := len(buf) / row
		if p == 2 {
			predictor.UndoHorizontal(buf, w, h, c, 1)
		}
		if err := writePNM(filepath.Join(out, stem), buf, w, h, c); err != nil {
			return err
		}
		nOut++
	}
	fmt.Fprintf(os.Stderr, "qdecode: %d outputs, %d inputs without output, decode %v\n", nOut, nFail, spent)
	return nil
}

func writePNM(stem string, pix []byte, w, h, c int) error {
	magic, ext := "P5", ".pgm"
	if c == 3 {
		magic, ext = "P6", ".ppm"
	} else if c != 1 {
		w *= c
	}
	f, err := os.Create(stem + ext)
	if err != nil {
		return err
	}
	if _, err := fmt.Fprintf(f, "%s\n%d %d\n255\n", magic, w, h); err != nil {
		f.Close()
		return err
	}
	if _, err := io.Copy(f, bytes.NewReader(pix)); err != nil {
		f.Close()
		return err
	}
	return f.Close()
}
