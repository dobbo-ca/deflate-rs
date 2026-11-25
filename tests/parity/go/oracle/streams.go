package main

import (
	"bytes"
	"compress/flate"
	"compress/zlib"
	"fmt"
)

// tok is a fixed-code token: a literal or symbol, or a length/distance match.
type tok struct {
	sym   int
	ext   uint64
	next  uint
	dsym  int // -1: none
	dext  uint64
	dnext uint
}

func lits(b []byte) []tok {
	t := make([]tok, len(b))
	for i, c := range b {
		t[i] = tok{sym: int(c), dsym: -1}
	}
	return t
}

// match encodes (l, d) with the usual codes.
func match(l, d int) tok {
	t := tok{}
	for c := 28; c >= 0; c-- {
		base, extra := lenSym(257 + c)
		if l >= base && l-base < 1<<extra {
			t.sym, t.ext, t.next = 257+c, uint64(l-base), extra
			break
		}
	}
	for c := 29; c >= 0; c-- {
		if base, extra := distSym(c); d >= base {
			t.dsym, t.dext, t.dnext = c, uint64(d-base), extra
			break
		}
	}
	return t
}

func (w *bitWriter) header(final bool, typ uint64) {
	f := uint64(0)
	if final {
		f = 1
	}
	w.bits(f|typ<<1, 3)
}

// fixed writes a fixed-code block of the tokens and the end of block.
func (w *bitWriter) fixed(final bool, toks ...tok) {
	w.header(final, 1)
	for _, t := range append(toks, tok{sym: 256, dsym: -1}) {
		w.fixedLit(t.sym)
		w.bits(t.ext, t.next)
		if t.dsym >= 0 {
			w.code(uint(t.dsym), 5)
			w.bits(t.dext, t.dnext)
		}
	}
}

// storedRaw writes a stored block with the given LEN and NLEN (16 bits each).
func (w *bitWriter) storedRaw(final bool, n, nn int, data []byte) {
	w.header(final, 0)
	w.buf = w.flush()
	w.acc, w.n = 0, 0
	w.buf = append(w.buf, byte(n), byte(n>>8), byte(nn), byte(nn>>8))
	w.buf = append(w.buf, data...)
}

func build(f func(w *bitWriter)) []byte {
	var w bitWriter
	f(&w)
	return w.flush()
}

// limitStreams is deflate-go's limitStreams (the 15 of the root package and the 13 of zlib):
// streams that stop just after n in each way, as lim-<name>.raw and lim-<name>.zlib.
func limitStreams(r *rng) []named {
	abc := lits([]byte("abc"))
	win := lits(r.bytes(32 << 10))
	m := []named{
		{"eob-reserved", build(func(w *bitWriter) { w.fixed(false, abc...); w.header(true, 3) })},
		{"eob-stored-empty-garbage", build(func(w *bitWriter) { w.fixed(false, abc...); w.storedRaw(false, 0, 0xffff, nil); w.header(true, 3) })},
		{"eob-stored-empty-final", build(func(w *bitWriter) { w.fixed(false, abc...); w.storedRaw(true, 0, 0xffff, nil) })},
		{"eob-final", build(func(w *bitWriter) { w.fixed(true, abc...) })},
		{"bad-dist", build(func(w *bitWriter) { w.fixed(true, append(append([]tok(nil), abc...), match(3, 4))...) })},
		{"code286", build(func(w *bitWriter) { w.fixed(true, append(append([]tok(nil), abc...), tok{sym: 286, dsym: -1})...) })},
		{"eob-truncated", build(func(w *bitWriter) { w.fixed(false, abc...) })},
		{"stored-truncated", build(func(w *bitWriter) { w.fixed(false, abc...); w.storedRaw(true, 5, ^5&0xffff, nil) })},
		{"window-reserved", build(func(w *bitWriter) { w.fixed(false, win...); w.header(true, 3) })},
		{"window-truncated", build(func(w *bitWriter) { w.fixed(false, win...) })},
		{"window-match-reserved", build(func(w *bitWriter) {
			w.fixed(false, append(append([]tok(nil), win[:32<<10-10]...), match(10, 1000))...)
			w.header(true, 3)
		})},
		{"window-stored-truncated", deflateLevel(r.bytes(32<<10), 0)[:5+32<<10]},
	}
	// Sync flush at 1000, then corruption.
	var buf bytes.Buffer
	fw, _ := flate.NewWriter(&buf, 6)
	text := genData(r, 5000)
	fw.Write(text[:1000])
	fw.Flush()
	flushed := buf.Len()
	fw.Write(text[1000:])
	fw.Close()
	full := buf.Bytes()
	return append(m,
		named{"sync-flush", full},
		named{"sync-flush-truncated", full[:flushed]},
		named{"sync-flush-garbage", append(bytes.Clone(full[:flushed]), 0xff, 0xff, 0xff)})
}

// zlibLimitStreams are zlib's limitStreams: a missing, short or bad Adler-32 after n.
func zlibLimitStreams(r *rng) []named {
	var m []named
	for _, size := range []int{1000, 32 << 10, 64 << 10} {
		for _, level := range []int{0, 6} {
			var b bytes.Buffer
			zw, _ := zlib.NewWriterLevel(&b, level)
			zw.Write(genData(r, size))
			zw.Close()
			good := b.Bytes()
			bad := bytes.Clone(good)
			bad[len(bad)-1] ^= 1
			p := fmt.Sprintf("%d-level%d-", size, level)
			m = append(m, named{p + "no-adler", good[:len(good)-4]}, named{p + "short-adler", good[:len(good)-2]}, named{p + "bad-adler", bad})
		}
	}
	var b bytes.Buffer
	zw := zlib.NewWriter(&b)
	zw.Write(genData(r, 1000))
	zw.Flush()
	return append(m, named{"sync-flush-garbage", append(bytes.Clone(b.Bytes()), 0xff, 0xff)})
}

// eobLongTrees is deflate-go's eobLongTrees: 16 streams of random trees whose longest code is EOB's.
// Every cut of each is written as x-eoblong<i>~t<cut>.raw (TestAuditEOBLongTruncations).
func eobLongTrees(r *rng) []named {
	var m []named
	for i := 0; i < 16; i++ {
		var w bitWriter
		produced := 0
		nblocks := 1 + i%3
		for b := 0; b < nblocks; b++ {
			ll := randomLengths(r, 257+r.intn(30), 3+r.intn(60), -1)
			long := 0
			for s, l := range ll {
				if l > ll[long] {
					long = s
				}
			}
			ll[long], ll[256] = ll[256], ll[long]
			dl := randomLengths(r, 30, 2+r.intn(12), -1)
			dynBlock(&w, r, b == nblocks-1, ll, dl, 200+r.intn(800), &produced)
		}
		m = append(m, named{fmt.Sprintf("eoblong%d", i), w.flush()})
	}
	return m
}
