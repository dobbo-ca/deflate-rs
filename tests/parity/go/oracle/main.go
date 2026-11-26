// Command oracle is the Go side of the parity check (see docs/parity.md).
//
//	oracle -gen <fixtures> <inputs>   write the test inputs
//	oracle <inputs> <manifest>        decode every input
//	oracle -bench N <fixtures>        time the decode of every fixture
//	oracle -qgen <tiles> <fixtures> <out> <slide>...   the real-file inputs (quality.go)
//	oracle -qdecode <in> <out>        decode the real-file inputs to PGM/PPM
//	oracle -qlarge <fixtures> <file>  the bench's 64 MiB stream
//
// Decode mode writes one manifest file, with a line per input (in byte order of the names) and
// operation: "<name>.<op> out <len> <fnv64a-hex>" (the output bytes), or "<name>.<op> err <class>"
// (a short error class), then "<name>.<op> partial <len> <fnv64a-hex>" if bytes came back with the
// error. examples/oracle.rs does the same with the Rust crate.
package main

import (
	"bufio"
	"bytes"
	"compress/flate"
	"compress/zlib"
	"encoding/binary"
	"errors"
	"flag"
	"fmt"
	"hash/fnv"
	"io"
	"os"
	"path/filepath"
	"strings"
	"time"

	deflate "github.com/dobbo-ca/deflate-go"
	"github.com/dobbo-ca/deflate-go/predictor"
	dzlib "github.com/dobbo-ca/deflate-go/zlib"
)

// limits are the n of the DecodeLimit operations, besides outLen-1, outLen and outLen+1 (lo-1,
// lo+0, lo+1) of a stream that decodes, windowLimits of it, and half (lh) and 1000 more (lp) than
// the bytes that Decode returns, with or without an error. An input whose name starts with "x-"
// runs only dec, rd, rdc and lr1 (the exhaustive truncations and bit flips).
var limits = []int{0, 1, 2, 100, 1000, 32767, 32768, 32769, 65535, 65536, 65537, 70000}

// windowLimits are k-1, k, k+1 for every multiple k of 32 KiB (from 64 KiB) near the end of an output
// of outLen bytes, as limitsFor in the Go tests.
func windowLimits(outLen int) []int {
	var l []int
	for k := 2 << 15; k <= outLen+32<<10; k += 32 << 10 {
		if k > outLen-32<<10 {
			l = append(l, k-1, k, k+1)
		}
	}
	return l
}

var (
	errBadInput = errors.New("bad input")
	errInvalid  = errors.New("invalid argument")
)

func main() {
	gen := flag.Bool("gen", false, "write the inputs: -gen <fixtures> <inputs>")
	bench := flag.Int("bench", 0, "decode every fixture N times: -bench N <fixtures>")
	qg := flag.Bool("qgen", false, "real-file inputs: -qgen <tiles> <fixtures> <out> <slide>...")
	qd := flag.Bool("qdecode", false, "real-file decode: -qdecode <in> <out>")
	ql := flag.Bool("qlarge", false, "the bench's large stream: -qlarge <fixtures> <out-file>")
	flag.Parse()
	a := flag.Args()
	switch {
	case *qg && len(a) >= 3:
		check(qgen(a[0], a[1], a[2], a[3:]))
	case *qd && len(a) == 2:
		check(qdecode(a[0], a[1]))
	case *ql && len(a) == 2:
		check(qlarge(a[0], a[1]))
	case *bench > 0 && len(a) == 1:
		check(benchDir(a[0], *bench))
	case *gen && len(a) == 2:
		check(generate(a[0], a[1]))
	case !*gen && !*qg && !*qd && !*ql && *bench == 0 && len(a) == 2:
		check(decodeDir(a[0], a[1]))
	default:
		flag.Usage()
		os.Exit(2)
	}
}

func check(err error) {
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

// class is the stable token of an error; docs/parity.md has the table.
func class(err error) string {
	var c flate.CorruptInputError
	var in flate.InternalError
	switch {
	case errors.As(err, &c):
		return fmt.Sprintf("corrupt@%d", int64(c))
	case errors.As(err, &in):
		return "internal"
	case errors.Is(err, io.ErrUnexpectedEOF):
		return "unexpected-eof"
	case errors.Is(err, zlib.ErrHeader):
		return "header"
	case errors.Is(err, zlib.ErrChecksum):
		return "checksum"
	case errors.Is(err, zlib.ErrDictionary):
		return "dictionary"
	case errors.Is(err, errInvalid):
		return "invalid-argument"
	case errors.Is(err, errBadInput):
		return "bad-input"
	}
	return "other:" + err.Error()
}

func decodeDir(in, manifest string) error {
	files, err := os.ReadDir(in) // sorted by name, bytewise: the Rust side sorts the same way
	if err != nil {
		return err
	}
	mf, err := os.Create(manifest)
	if err != nil {
		return err
	}
	defer mf.Close()
	w := bufio.NewWriterSize(mf, 1<<20)
	for _, f := range files {
		if f.IsDir() {
			continue
		}
		name := f.Name()
		data, err := os.ReadFile(filepath.Join(in, name))
		if err != nil {
			return err
		}
		emit := func(op string, b []byte, err error) {
			if err == nil {
				fmt.Fprintf(w, "%s.%s out %d %016x\n", name, op, len(b), fnv64(b))
				return
			}
			fmt.Fprintf(w, "%s.%s err %s\n", name, op, class(err))
			if len(b) > 0 { // what came before the error
				fmt.Fprintf(w, "%s.%s partial %d %016x\n", name, op, len(b), fnv64(b))
			}
		}
		ext := filepath.Ext(name)
		// <id>@<dict> names a preset dictionary in <inputs>/dicts.
		var dict []byte
		if _, d, ok := strings.Cut(strings.TrimSuffix(name, ext), "@"); ok {
			if dict, err = os.ReadFile(filepath.Join(in, "dicts", d+".dict")); err != nil {
				return err
			}
		}
		switch ext {
		case ".raw", ".zlib":
			runStream(emit, name, data, dict, ext == ".zlib", strings.HasPrefix(name, "x-"), strings.HasPrefix(name, "lim-"), chunked(name))
		case ".pred":
			b, err := runPred(data)
			emit("run", b, err)
		}
	}
	return w.Flush()
}

func fnv64(b []byte) uint64 {
	h := fnv.New64a()
	h.Write(b)
	return h.Sum64()
}

// runStream runs every operation on one stream: Decode, a Reader over the same bytes, and
// DecodeLimit at each of limits.
func runStream(emit func(string, []byte, error), name string, data, dict []byte, z, light, lim, chunks bool) {
	decode, limit := deflate.Decode, deflate.DecodeLimit
	open := func(r io.Reader) (io.ReadCloser, error) { return deflate.NewReaderDict(r, dict), nil }
	if z {
		decode, limit = dzlib.Decode, dzlib.DecodeLimit
		open = func(r io.Reader) (io.ReadCloser, error) { return dzlib.NewReaderDict(r, dict) }
	}
	b, err := decode(nil, data)
	emit("dec", b, err)
	// par: the Rust parallel decode of one stream must give what Decode gives.
	emit("par", b, err)
	// The random n of lr1 to lr3: FNV-1a 64 of the name and k, modulo the output length + 1.
	randLimit := func(k byte) int {
		h := fnv.New64a()
		h.Write([]byte(name))
		h.Write([]byte{k})
		return int(h.Sum64() % uint64(len(b)+1))
	}
	if light {
		b2, err2 := limit(nil, data, randLimit(1))
		emit("lr1", b2, err2)
		b, used, err := readAll(open, data)
		emit("rd", b, err)
		emit("rdc", used, nil)
		return
	}
	for _, c := range []struct {
		op string
		n  int
	}{{"lh", len(b) / 2}, {"lp", len(b) + 1000}} {
		b2, err2 := limit(nil, data, c.n)
		emit(c.op, b2, err2)
	}
	if strings.Contains(name, "~") && chunked(name) {
		// TestDecodeLimit: a random n for one mutation in eight (as `chunked`).
		for k := byte(1); k <= 3; k++ {
			b2, err2 := limit(nil, data, randLimit(k))
			emit(fmt.Sprintf("lr%d", k), b2, err2)
		}
	}
	var more []int
	if lim {
		more = []int{3, 4, 999, 1001} // the Go test's extra limits for limitStreams
	}
	if err != nil {
		// A stop at the length of the partial output, and 1 either side.
		for d := -1; d <= 1; d++ {
			if n := len(b) + d; n >= 0 {
				b2, err2 := limit(nil, data, n)
				emit(fmt.Sprintf("lq%+d", d), b2, err2)
			}
		}
	}
	if err == nil {
		more = append(more, windowLimits(len(b))...)
		for d := -1; d <= 1; d++ {
			if n := len(b) + d; n >= 0 {
				b2, err2 := limit(nil, data, n)
				emit(fmt.Sprintf("lo%+d", d), b2, err2)
			}
		}
	}
	b, used, err := readAll(open, data)
	emit("rd", b, err)
	emit("rdc", used, nil)
	for _, p := range readSizes {
		if !chunks {
			break
		}
		b, sizes, used, err := readChunks(open, data, p)
		emit(fmt.Sprintf("c%d", p), append(append(digest(b), sizes...), used...), err)
	}
	for _, n := range append(limits, more...) {
		b, err = limit(nil, data, n)
		emit(fmt.Sprintf("l%d", n), b, err)
	}
}

// digest is the FNV-1a 64 and the length of b, as a line: the chunking operations compare it, then
// their read sizes and the consumed-input line, as one blob (the manifest line is its FNV-1a 64).
func digest(b []byte) []byte {
	return fmt.Appendf(nil, "%016x %d\n", fnv64(b), len(b))
}

// chunked says whether the chunking operations run on the input: every input that is not a
// mutation (no "~" in its name), and one mutation in eight (by the sum of the bytes of its name),
// as the Go reader tests sample the mutations. It keeps the number of output files down.
func chunked(name string) bool {
	n := 0
	for _, c := range []byte(name) {
		n += int(c)
	}
	return !strings.Contains(name, "~") || n%8 == 0
}

// readSizes are the Read sizes of the chunking operations c<p>: the digest of the bytes, then the
// sizes of the non-empty Reads as "size*count" runs, one per line, the "consumed <n>" line (the
// input bytes the reader took) and the error; the Go reader tests'.
var readSizes = []int{1, 100, 4096, 1 << 16}

func readChunks(open func(io.Reader) (io.ReadCloser, error), data []byte, p int) (out, sizes, used []byte, err error) {
	src := bytes.NewReader(data)
	defer func() { used = consumed(data, src) }()
	r, err := open(src)
	if err != nil {
		return nil, nil, nil, err
	}
	buf := make([]byte, p)
	last, run := 0, 0
	flush := func() {
		if run > 0 {
			sizes = fmt.Appendf(sizes, "%d*%d\n", last, run)
		}
	}
	for {
		n, err := r.Read(buf)
		out = append(out, buf[:n]...)
		if n > 0 {
			if n != last {
				flush()
				last, run = n, 0
			}
			run++
		}
		if err == io.EOF {
			err = nil
		}
		if err != nil || n == 0 { // Go's readers do not return 0, nil; stop rather than spin
			flush()
			return out, sizes, nil, err
		}
	}
}

func readAll(open func(io.Reader) (io.ReadCloser, error), data []byte) (b, used []byte, err error) {
	src := bytes.NewReader(data)
	defer func() { used = consumed(data, src) }()
	r, err := open(src)
	if err != nil {
		return nil, nil, err
	}
	b, err = io.ReadAll(r)
	return b, nil, err
}

// consumed is the number of input bytes a reader took from src, as a line: reader_test.go's
// compareRC compares it after every reader comparison, error paths included.
func consumed(data []byte, src *bytes.Reader) []byte {
	return fmt.Appendf(nil, "consumed %d\n", len(data)-src.Len())
}

// runPred runs one predictor input: [op][spp][flag][w u32 LE][h u32 LE][src...]. op 0 is
// UndoExpandRGBA and 1 is ExpandRGBA (flag is invertGrey); both write to a zeroed dst of 4*w*h
// bytes, or none when w*h passes 1<<20. op 2 is UndoHorizontal in place (flag is bytesPerSample).
// The output is dst, or src for op 2. A panic is "invalid-argument".
func runPred(data []byte) (out []byte, err error) {
	if len(data) < 11 {
		return nil, errBadInput
	}
	op, spp, flag := data[0], int(data[1]), data[2]
	w, h := int(binary.LittleEndian.Uint32(data[3:])), int(binary.LittleEndian.Uint32(data[7:]))
	src := bytes.Clone(data[11:])
	defer func() {
		if recover() != nil {
			out, err = nil, errInvalid
		}
	}()
	switch op {
	case 0, 1:
		var dst []byte
		if uint64(w)*uint64(h) <= 1<<20 {
			dst = make([]byte, 4*w*h)
		}
		if op == 0 {
			predictor.UndoExpandRGBA(dst, src, w, h, spp, flag != 0)
		} else {
			predictor.ExpandRGBA(dst, src, w, h, spp, flag != 0)
		}
		return dst, nil
	case 2:
		predictor.UndoHorizontal(src, w, h, spp, int(flag))
		return src, nil
	}
	return nil, errBadInput
}

// rng is xorshift64: the inputs must not depend on the Go version.
type rng uint64

func (r *rng) next() uint64 {
	x := uint64(*r)
	x ^= x << 13
	x ^= x >> 7
	x ^= x << 17
	*r = rng(x)
	return x
}

func (r *rng) intn(n int) int { return int(r.next() >> 1 % uint64(n)) }

func (r *rng) bytes(n int) []byte {
	b := make([]byte, n)
	for i := range b {
		b[i] = byte(r.next() >> 24)
	}
	return b
}

func seed(name string) *rng {
	h := fnv.New64a()
	h.Write([]byte(name))
	r := rng(h.Sum64() | 1)
	return &r
}

type variant struct {
	tag  string
	data []byte
}

// mutate returns truncations, bit flips and header, length and trailer edits of the stream s.
func mutate(s []byte, z bool, r *rng) []variant {
	var vs []variant
	n := len(s)
	edit := func(tag string, f func(b []byte)) {
		b := bytes.Clone(s)
		f(b)
		vs = append(vs, variant{tag, b})
	}
	seen := map[int]bool{}
	for _, c := range []int{0, 1, 2, 5, n / 3, n / 2, n - 5, n - 3, n - 1} {
		if c >= 0 && c < n && !seen[c] {
			seen[c] = true
			vs = append(vs, variant{fmt.Sprintf("t%d", c), s[:c]})
		}
	}
	// The flips and byte sets past Go's 32 and 16 of the first set are tagged "w": generate writes
	// them as `x-` inputs (dec, rd and rdc only), to bound the run time.
	for i := 0; n > 0 && i < 64; i++ {
		p, tag := r.intn(n*8), "f"
		if i >= 32 {
			tag = "wf"
		}
		edit(fmt.Sprintf("%s%d", tag, p), func(b []byte) { b[p/8] ^= 1 << (p % 8) })
	}
	// Byte sets to 0x00, 0xff and a random value, as mutations in the Go tests.
	for i := 0; n > 0 && i < 32; i++ {
		p, v, tag := r.intn(n), []byte{0x00, 0xff, byte(r.next() >> 24)}[i%3], "s"
		if i >= 16 {
			tag = "ws"
		}
		edit(fmt.Sprintf("%s%d-%02x", tag, p, v), func(b []byte) { b[p] = v })
	}
	vs = append(vs, variant{"app", append(bytes.Clone(s), 0xde, 0xad, 0xbe)})
	o := 0 // start of the DEFLATE stream
	if z && n >= 2 {
		o = 2
		if s[1]&0x20 != 0 {
			o = 6
		}
	}
	if n > o+4 {
		// The first block header, then bytes 1..3 after it (a stored block's LEN and NLEN, a dynamic
		// block's code lengths).
		for _, v := range []byte{0x00, 0x01, 0x02, 0x03, 0x05, 0x06, 0x07} {
			edit(fmt.Sprintf("b%02x", v), func(b []byte) { b[o] = v })
		}
		for i := 1; i <= 3; i++ {
			edit(fmt.Sprintf("x%d", i), func(b []byte) { b[o+i] ^= 0xff })
		}
		edit("lenok", func(b []byte) { copy(b[o:], []byte{0x01, 0xff, 0xff, 0x00, 0x00}) })
		edit("lenbad", func(b []byte) { copy(b[o:], []byte{0x01, 0xff, 0xff, 0xff, 0xff}) })
	}
	if z && n >= 6 {
		fix := func(b []byte) { // a valid FCHECK
			v := int(b[0])<<8 | int(b[1]&0xe0)
			b[1] = b[1]&0xe0 | byte((31-v%31)%31)
		}
		for _, c := range []byte{0x08, 0x18, 0x48, 0x68, 0x77, 0x78, 0x79, 0x88, 0xf8} {
			edit(fmt.Sprintf("c%02x", c), func(b []byte) { b[0] = c; fix(b) })
		}
		edit("fdict", func(b []byte) { b[1] |= 0x20; fix(b) })
		edit("nodict", func(b []byte) { b[1] &^= 0x20; fix(b) })
		edit("fcheck", func(b []byte) { b[1] ^= 1 })
		edit("adler0", func(b []byte) { b[n-1] ^= 1 })
		edit("adler1", func(b []byte) { b[n-4] ^= 0x80 })
	}
	return vs
}

// generate writes the fixtures, their mutations, random streams and predictor inputs.
func generate(fixtures, out string) error {
	if err := os.MkdirAll(filepath.Join(out, "dicts"), 0o755); err != nil {
		return err
	}
	put := func(name string, b []byte) error { return os.WriteFile(filepath.Join(out, name), b, 0o644) }
	files, err := os.ReadDir(fixtures)
	if err != nil {
		return err
	}
	for _, f := range files {
		id, ext, ok := strings.Cut(f.Name(), ".")
		switch {
		case !ok:
			continue
		case ext == "dict":
			b, err := os.ReadFile(filepath.Join(fixtures, f.Name()))
			if err != nil {
				return err
			}
			if err := put("dicts/"+f.Name(), b); err != nil {
				return err
			}
			continue
		case ext == "raw.deflate":
			ext = "raw"
		case ext != "zlib":
			continue
		}
		s, err := os.ReadFile(filepath.Join(fixtures, f.Name()))
		if err != nil {
			return err
		}
		suffix := ""
		if _, err := os.Stat(filepath.Join(fixtures, id+".dict")); err == nil {
			suffix = "@" + id
		}
		id = "fx-" + id
		if err := put(id+suffix+"."+ext, s); err != nil {
			return err
		}
		// helpers_test.go mutations(): every cut of a stream under 4 KiB (a few of the others are in
		// mutate), checked by dec and rd only (the `x-` inputs).
		for c := 0; len(s) < 4096 && c < len(s); c++ {
			if err := put(fmt.Sprintf("x-%s~t%d%s.%s", id, c, suffix, ext), s[:c]); err != nil {
				return err
			}
		}
		// The others (4096 bytes or more): helpers_test.go's 32 cuts at the head, 32 at the tail and 32
		// random ones (xorshift here, not Go's PCG).
		if n := len(s); n >= 4096 {
			cr, seen := seed(f.Name()+"~cuts"), map[int]bool{}
			for k := 0; k < 96; k++ {
				c := k
				if k >= 32 {
					c = n - 1 - (k - 32)
				}
				if k >= 64 {
					c = cr.intn(n)
				}
				if seen[c] {
					continue
				}
				seen[c] = true
				if err := put(fmt.Sprintf("x-%s~t%d%s.%s", id, c, suffix, ext), s[:c]); err != nil {
					return err
				}
			}
		}
		for _, v := range mutate(s, ext == "zlib", seed(f.Name())) {
			pre := ""
			if strings.HasPrefix(v.tag, "w") {
				pre = "x-"
			}
			if err := put(pre+id+"~"+v.tag+suffix+"."+ext, v.data); err != nil {
				return err
			}
		}
	}
	// Random bytes: any first byte, so every block type, and a valid zlib header.
	r := seed("random")
	for i := 0; i < 300; i++ {
		b := r.bytes(r.intn(80))
		if err := put(fmt.Sprintf("r-%03d.raw", i), b); err != nil {
			return err
		}
		if err := put(fmt.Sprintf("z-%03d.zlib", i), append([]byte{0x78, 0x9c}, b...)); err != nil {
			return err
		}
	}
	// compress/flate and compress/zlib at every level, sizes around the window.
	for level := flate.HuffmanOnly; level <= flate.BestCompression; level++ {
		for _, n := range []int{0, 1, 100, 32767, 32768, 32769, 70000, 1<<20 + 13} {
			if n > 70000 && level != flate.HuffmanOnly && level != 6 {
				continue
			}
			data := genData(r, n)
			var raw, z bytes.Buffer
			fw, _ := flate.NewWriter(&raw, level)
			zw, _ := zlib.NewWriterLevel(&z, level)
			fw.Write(data)
			zw.Write(data)
			fw.Close()
			zw.Close()
			name := fmt.Sprintf("gen-l%d-%d", level, n)
			if err := put(name+".raw", raw.Bytes()); err != nil {
				return err
			}
			if err := put(name+".zlib", z.Bytes()); err != nil {
				return err
			}
			// TestCoreZlib: each also with a 5-byte tail.
			if err := put(name+"+tail.zlib", append(bytes.Clone(z.Bytes()), "tail!"...)); err != nil {
				return err
			}
		}
	}
	for i := 0; i < 600; i++ {
		if err := put(fmt.Sprintf("p-%04d.pred", i), predInput(r, i%3)); err != nil {
			return err
		}
	}
	// deflate-go's TestCoreEdgeTrees: multi-block streams with random complete codes up to 15
	// bits and degenerate trees, each with bit flips.
	er := seed("edge-trees")
	for _, st := range edgeStreams(er) {
		if err := put("tree-"+st.tag+".raw", st.data); err != nil {
			return err
		}
		step := 1 + len(st.data)*8/treeFlips
		for i := 0; i < len(st.data)*8; i += step {
			b := bytes.Clone(st.data)
			b[i/8] ^= 1 << (i % 8)
			if err := put(fmt.Sprintf("tree-%s~f%d.raw", st.tag, i), b); err != nil {
				return err
			}
		}
		// The rest of Go's flips (every 1+len/16-th bit), as `x-` inputs: dec, rd and rdc only.
		for i := 0; i < len(st.data)*8; i += 1 + len(st.data)/16 {
			if i%step == 0 {
				continue
			}
			b := bytes.Clone(st.data)
			b[i/8] ^= 1 << (i % 8)
			if err := put(fmt.Sprintf("x-tree-%s~f%d.raw", st.tag, i), b); err != nil {
				return err
			}
		}
	}
	// TestCoreRandomStreams: random bytes with a dynamic block header forced.
	dr := seed("dyn-random")
	for i := 0; i < 3000; i++ {
		b := dr.bytes(1 + dr.intn(200))
		b[0] = b[0]&^6 | 4
		if err := put(fmt.Sprintf("dyn-%04d.raw", i), b); err != nil {
			return err
		}
	}
	if err := generateDictLong(put, r); err != nil {
		return err
	}
	if err := generateSmall(put); err != nil {
		return err
	}
	if err := generateSmallZlib(put); err != nil {
		return err
	}
	lr := seed("limit-streams")
	for _, st := range limitStreams(lr) {
		if err := put("lim-"+st.tag+".raw", st.data); err != nil {
			return err
		}
	}
	for _, st := range zlibLimitStreams(lr) {
		if err := put("lim-z"+st.tag+".zlib", st.data); err != nil {
			return err
		}
	}
	for _, st := range eobLongTrees(seed("eob-long")) {
		for i := 0; i <= len(st.data); i++ {
			if err := put(fmt.Sprintf("x-%s~t%d.raw", st.tag, i), st.data[:i]); err != nil {
				return err
			}
		}
	}
	return generateFixedStored(put)
}

func deflateLevel(data []byte, level int) []byte {
	var b bytes.Buffer
	w, _ := flate.NewWriter(&b, level)
	w.Write(data)
	w.Close()
	return b.Bytes()
}

// generateDictLong is deflate-go's TestAuditDictLong (audit_test.go): for each n around the 32 KiB
// window, a stream of text[n:n+20000] written with the dictionary text[:n], and a fixed-code stream
// with matches at distance 32768, each paired with the right dictionary, dict[1:] (short), one
// with a changed first byte (head-changed) and dict[:32767] (far-short): dl-<n>-<case>@<dict>.raw,
// with the dictionary in dicts/<dict>.dict.
func generateDictLong(put func(string, []byte) error, r *rng) error {
	text := genData(r, 200<<10)
	far := build(func(w *bitWriter) { w.fixed(true, match(258, 32768), tok{sym: 'x', dsym: -1}, match(100, 32768)) })
	for _, n := range []int{32767, 32768, 32769, 65536, 100000} {
		dict := text[:n]
		var buf bytes.Buffer
		fw, err := flate.NewWriterDict(&buf, 6, dict)
		if err != nil {
			return err
		}
		fw.Write(text[n : n+20000])
		fw.Close()
		changedHead := bytes.Clone(dict)
		changedHead[0] ^= 1
		for _, c := range []struct {
			name string
			dict []byte
			data []byte
		}{
			{"right", dict, buf.Bytes()}, {"short", dict[1:], buf.Bytes()}, {"head-changed", changedHead, buf.Bytes()},
			{"far", dict, far}, {"far-short", dict[:32767], far},
		} {
			id := fmt.Sprintf("dl%d-%s", n, c.name)
			if err := put("dicts/"+id+".dict", c.dict); err != nil {
				return err
			}
			if err := put("dl-"+id+"@"+id+".raw", c.data); err != nil {
				return err
			}
		}
		if n < 32769 {
			continue
		}
		// zlib TestAuditDictLong: the stream with the right and the short dictionary, with none, and
		// cut in the header, the dictionary id and the trailer.
		var zb bytes.Buffer
		zw, err := zlib.NewWriterLevelDict(&zb, 6, dict)
		if err != nil {
			return err
		}
		zw.Write(text[n : n+20000])
		zw.Close()
		zs := zb.Bytes()
		for _, name := range []string{
			fmt.Sprintf("dlz-%d-right@dl%d-right.zlib", n, n),
			fmt.Sprintf("dlz-%d-short@dl%d-short.zlib", n, n),
			fmt.Sprintf("dlz-%d-nodict.zlib", n),
		} {
			if err := put(name, zs); err != nil {
				return err
			}
		}
		for _, cut := range []int{0, 1, 2, 3, 5, 6, 7, len(zs) - 4, len(zs) - 3, len(zs) - 1} {
			if err := put(fmt.Sprintf("x-dlz-%d~t%d@dl%d-right.zlib", n, cut, n), zs[:cut]); err != nil {
				return err
			}
		}
	}
	return nil
}

// generateSmall is deflate-go's TestCoreTruncations and TestCoreBitFlips: every cut and every bit
// flip of six small streams, as x-<name>~t<cut>.raw and x-<name>~f<bit>.raw.
func generateSmall(put func(string, []byte) error) error {
	r := seed("small-streams")
	small := []named{
		{"textL6", deflateLevel(genData(r, 3000), 6)},
		{"textL1", deflateLevel(genData(r, 2000), 1)},
		{"huffOnly", deflateLevel(genData(r, 1500), flate.HuffmanOnly)},
		{"stored", deflateLevel(r.bytes(300), 0)},
		{"fixed", fixedStream(r, 2, 200, 1<<15)},
		{"imageL9", deflateLevel(genData(r, 1200), 9)},
	}
	for _, st := range small {
		for i := 0; i <= len(st.data); i++ {
			if err := put(fmt.Sprintf("x-%s~t%d.raw", st.tag, i), st.data[:i]); err != nil {
				return err
			}
		}
		for i := 0; i < len(st.data)*8; i++ {
			b := bytes.Clone(st.data)
			b[i/8] ^= 1 << (i % 8)
			if err := put(fmt.Sprintf("x-%s~f%d.raw", st.tag, i), b); err != nil {
				return err
			}
		}
	}
	return nil
}

// generateSmallZlib is deflate-go's TestCoreZlibBroken: every cut and every 3rd bit flip of a zlib
// stream of genData(3000), and every cut of an FDICT stream whose dictionary id is that of no
// dictionary (1): x-zsmall~t<cut>.zlib, x-zsmall~f<bit>.zlib, x-zfdict-empty~t<cut>.zlib.
func generateSmallZlib(put func(string, []byte) error) error {
	r := seed("small-zlib")
	var z bytes.Buffer
	zw, _ := zlib.NewWriterLevel(&z, 6)
	zw.Write(genData(r, 3000))
	zw.Close()
	s := z.Bytes()
	for i := 0; i <= len(s); i++ {
		if err := put(fmt.Sprintf("x-zsmall~t%d.zlib", i), s[:i]); err != nil {
			return err
		}
	}
	for i := 0; i < len(s)*8; i += 3 {
		b := bytes.Clone(s)
		b[i/8] ^= 1 << (i % 8)
		if err := put(fmt.Sprintf("x-zsmall~f%d.zlib", i), b); err != nil {
			return err
		}
	}
	// 0x78 0xbb is 0x78 0x9c with FDICT and a valid FCHECK; the id of the empty dictionary is 1.
	fd := append([]byte{0x78, 0xbb, 0, 0, 0, 1}, s[2:]...)
	for i := 0; i <= len(fd); i++ {
		if err := put(fmt.Sprintf("x-zfdict-empty~t%d.zlib", i), fd[:i]); err != nil {
			return err
		}
	}
	return nil
}

// generateFixedStored is deflate-go's TestCoreFixedStored (fixed-code and stored-block streams,
// each also with trailing bytes: fs-<name>.raw and fs-<name>+tail.raw) and the junk code-length
// sets of TestTableSymMatchesStdlib (400 dynamic blocks whose trees are random complete codes,
// empty, degenerate or junk lengths: junk-<nnn>.raw).
func generateFixedStored(put func(string, []byte) error) error {
	r := seed("fixed-stored")
	m := []named{
		{"fixed1", fixedStream(r, 1, 50, 1<<15)},
		{"fixedMany", fixedStream(r, 5, 3000, 1<<15)},
		{"fixedNear", fixedStream(r, 3, 5000, 64)},
		{"stored0", storedStream(r, 0)},
		{"storedMix", storedStream(r, 0, 5, 65535, 1, 40000, 0)},
		{"storedBig", storedStream(r, 65535, 65535, 65535)},
		{"storedL0text", deflateLevel(genData(r, 200<<10), 0)},
	}
	for _, st := range m {
		for suffix, b := range map[string][]byte{"": st.data, "+tail": append(bytes.Clone(st.data), "trailing"...)} {
			if err := put("fs-"+st.tag+suffix+".raw", b); err != nil {
				return err
			}
		}
	}
	// A tree of the kinds of the Go test, sized for a litlen (ll) or a distance (!ll) code.
	tree := func(ll bool, must int) []int {
		var n int
		if ll {
			n = 257 + r.intn(30)
		} else {
			n = 1 + r.intn(30)
		}
		switch r.intn(5) {
		case 0:
			return randomLengths(r, n, 2+r.intn(287), must)
		case 1:
			return make([]int, n)
		case 2:
			dg := make([]int, n)
			dg[r.intn(n)] = 1
			return dg
		case 3:
			return randomLengths(r, n, 2+r.intn(31), -1)
		}
		junk := make([]int, n)
		for j := range junk {
			if r.intn(3) == 0 {
				junk[j] = r.intn(16)
			}
		}
		return junk
	}
	for i := 0; i < 400; i++ {
		var w bitWriter
		produced := 0
		dynBlock(&w, r, true, tree(true, []int{256, -1}[i%2]), tree(false, -1), 20+r.intn(200), &produced)
		if err := put(fmt.Sprintf("junk-%03d.raw", i), w.flush()); err != nil {
			return err
		}
	}
	return nil
}

// fixedLit writes a literal/length symbol with the fixed code.
func (w *bitWriter) fixedLit(sym int) {
	switch {
	case sym < 144:
		w.code(uint(0x30+sym), 8)
	case sym < 256:
		w.code(uint(0x190+sym-144), 9)
	case sym < 280:
		w.code(uint(sym-256), 7)
	default:
		w.code(uint(0xc0+sym-280), 8)
	}
}

// fixedStream is deflate-go's fixedStream: nblocks fixed-code blocks of nsyms literals and
// matches (distance up to maxDist).
func fixedStream(r *rng, nblocks, nsyms, maxDist int) []byte {
	var w bitWriter
	produced := 0
	for b := 0; b < nblocks; b++ {
		final := uint64(0)
		if b == nblocks-1 {
			final = 1
		}
		w.bits(final|1<<1, 3)
		for i := 0; i < nsyms; i++ {
			if produced == 0 || r.intn(3) == 0 {
				w.fixedLit(r.intn(256))
				produced++
				continue
			}
			length := 3 + r.intn(256)
			if r.intn(4) == 0 {
				length = 3 + r.intn(8)
			}
			dist := 1 + r.intn(min(produced, maxDist))
			if r.intn(3) == 0 {
				dist = 1 + r.intn(min(produced, 9))
			}
			sym := 257
			for sym < 285 {
				if base, _ := lenSym(sym + 1); base > length {
					break
				}
				sym++
			}
			w.fixedLit(sym)
			base, extra := lenSym(sym)
			w.bits(uint64(length-base), extra)
			dc := 0
			for dc < 29 {
				if base, _ := distSym(dc + 1); base > dist {
					break
				}
				dc++
			}
			w.code(uint(dc), 5)
			base, extra = distSym(dc)
			w.bits(uint64(dist-base), extra)
			produced += length
		}
		w.fixedLit(256)
	}
	return w.flush()
}

// storedStream is deflate-go's storedStream: stored blocks of the given sizes.
func storedStream(r *rng, sizes ...int) []byte {
	var w bitWriter
	for i, n := range sizes {
		final := uint64(0)
		if i == len(sizes)-1 {
			final = 1
		}
		w.bits(final, 3)
		w.buf = w.flush()
		w.acc, w.n = 0, 0
		w.buf = append(w.buf, byte(n), byte(n>>8), ^byte(n), ^byte(n>>8))
		w.buf = append(w.buf, r.bytes(n)...)
	}
	return w.flush()
}

// treeFlips is the bit flips per edge-tree stream (deflate-go's test flips about 128).
const treeFlips = 40

func (r *rng) perm(n int) []int {
	p := make([]int, n)
	for i := range p {
		p[i] = i
	}
	for i := n - 1; i > 0; i-- {
		j := r.intn(i + 1)
		p[i], p[j] = p[j], p[i]
	}
	return p
}

// bitWriter writes DEFLATE bits, LSB first.
type bitWriter struct {
	buf []byte
	acc uint64
	n   uint
}

func (w *bitWriter) bits(v uint64, n uint) {
	w.acc |= v << w.n
	w.n += n
	for w.n >= 8 {
		w.buf = append(w.buf, byte(w.acc))
		w.acc >>= 8
		w.n -= 8
	}
}

// code writes a Huffman code, MSB first.
func (w *bitWriter) code(c, n uint) {
	var r uint64
	for i := uint(0); i < n; i++ {
		r |= uint64(c>>i&1) << (n - 1 - i)
	}
	w.bits(r, n)
}

func (w *bitWriter) flush() []byte {
	if w.n > 0 {
		w.buf = append(w.buf, byte(w.acc))
	}
	return w.buf
}

var codeOrder = [...]int{16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15}

// lenSym is the base length and extra bits of litlen symbol 257..285.
func lenSym(s int) (base int, extra uint) {
	switch {
	case s < 265:
		return s - 254, 0
	case s < 285:
		extra = uint(s-261) / 4
		return (s-261)%4<<extra + 1<<(extra+2) + 3, extra
	}
	return 258, 0
}

// distSym is the base distance and extra bits of distance symbol d < 30.
func distSym(d int) (base int, extra uint) {
	if d < 4 {
		return d + 1, 0
	}
	extra = uint(d-2) >> 1
	return 1<<(extra+1) + 1 + (d&1)<<extra, extra
}

// randomLengths makes a random complete code of n symbols among size; symbol must (if >= 0) is
// one of them.
func randomLengths(r *rng, size, n, must int) []int {
	n = min(n, size)
	leaves := []int{0}
	for len(leaves) < n {
		i := r.intn(len(leaves))
		if r.intn(3) == 0 {
			i = len(leaves) - 1 // deepen: long codes
		}
		if leaves[i] == 15 {
			continue
		}
		l := leaves[i] + 1
		leaves[i] = l
		leaves = append(leaves, l)
	}
	lens := make([]int, size)
	perm := r.perm(size)
	if must >= 0 {
		for i, p := range perm {
			if p == must {
				perm[0], perm[i] = perm[i], perm[0]
			}
		}
	}
	for i, l := range leaves {
		lens[perm[i]] = l
	}
	return lens
}

func canonical(lens []int) []uint {
	var count, next [16]uint
	for _, l := range lens {
		count[l]++
	}
	count[0] = 0
	code := uint(0)
	for i := 1; i < 16; i++ {
		code = (code + count[i-1]) << 1
		next[i] = code
	}
	codes := make([]uint, len(lens))
	for s, l := range lens {
		if l > 0 {
			codes[s] = next[l]
			next[l]++
		}
	}
	return codes
}

// dynBlock writes a mostly valid dynamic block: the code lengths with a flat 4 bit code, then
// nsyms random symbols (matches whose distance is within what was produced, mostly).
func dynBlock(w *bitWriter, r *rng, final bool, ll, dl []int, nsyms int, produced *int) {
	f := uint64(0)
	if final {
		f = 1
	}
	w.bits(f|2<<1, 3)
	w.bits(uint64(len(ll)-257), 5)
	w.bits(uint64(len(dl)-1), 5)
	w.bits(15, 4)
	for _, s := range codeOrder {
		if s < 16 {
			w.bits(4, 3)
		} else {
			w.bits(0, 3)
		}
	}
	for _, l := range append(append([]int(nil), ll...), dl...) {
		w.code(uint(l), 4)
	}
	lc, dc := canonical(ll), canonical(dl)
	var lsyms, dsyms []int
	for s, l := range ll {
		if l > 0 && s != 256 {
			lsyms = append(lsyms, s)
		}
	}
	for s, l := range dl {
		if l > 0 {
			dsyms = append(dsyms, s)
		}
	}
	for i := 0; i < nsyms && len(lsyms) > 0; i++ {
		s := lsyms[r.intn(len(lsyms))]
		if s > 256 {
			var ok []int
			for _, d := range dsyms {
				if b, _ := distSym(d); b <= *produced || r.intn(500) == 0 {
					ok = append(ok, d)
				}
			}
			if len(ok) == 0 && r.intn(50) != 0 {
				continue
			}
			w.code(lc[s], uint(ll[s]))
			base, extra := lenSym(s)
			x := r.next() & (1<<extra - 1)
			w.bits(x, extra)
			*produced += base + int(x)
			if len(ok) == 0 {
				break // the decoder fails here
			}
			d := ok[r.intn(len(ok))]
			w.code(dc[d], uint(dl[d]))
			_, de := distSym(d)
			w.bits(r.next()&(1<<de-1), de)
			continue
		}
		w.code(lc[s], uint(ll[s]))
		*produced++
	}
	if ll[256] > 0 {
		w.code(lc[256], uint(ll[256]))
	}
}

type named struct {
	tag  string
	data []byte
}

// edgeStreams is deflate-go's edgeStreams: 60 streams of one to three dynamic blocks with empty,
// degenerate, two-code and long-code trees.
func edgeStreams(r *rng) []named {
	var m []named
	lit := func(n int) []int { return randomLengths(r, 257+r.intn(30), n, 256) }
	for i := 0; i < 60; i++ {
		var w bitWriter
		produced := 0
		var name string
		nblocks := 1 + r.intn(3)
		for b := 0; b < nblocks; b++ {
			var ll, dl []int
			switch k := (i + b) % 6; k {
			case 0: // empty distance tree
				ll, dl = lit(2+r.intn(200)), make([]int, 1+r.intn(30))
			case 1: // degenerate distance tree
				ll, dl = lit(2+r.intn(200)), make([]int, 1+r.intn(30))
				dl[r.intn(len(dl))] = 1
			case 2: // degenerate litlen tree
				ll, dl = make([]int, 271+r.intn(15)), randomLengths(r, 30, 2+r.intn(20), -1)
				ll[[]int{256, 65, 270}[r.intn(3)]] = 1
			case 3: // two-code litlen tree
				ll, dl = lit(2), randomLengths(r, 30, 2, -1)
			default: // long codes in both trees
				ll, dl = lit(30+r.intn(250)), randomLengths(r, 1+29, 2+r.intn(28), -1)
			}
			name += fmt.Sprintf("%d", (i+b)%6)
			dynBlock(&w, r, b == nblocks-1, ll, dl, 50+r.intn(3000), &produced)
		}
		m = append(m, named{fmt.Sprintf("%02d_%s", i, name), w.flush()})
	}
	return m
}

// genData is n bytes with literals, runs and repeats of earlier bytes, so every level has
// something to match.
func genData(r *rng, n int) []byte {
	b := make([]byte, 0, n+300)
	for len(b) < n {
		switch r.intn(3) {
		case 0:
			b = append(b, r.bytes(1+r.intn(40))...)
		case 1:
			b = append(b, bytes.Repeat([]byte{byte(r.next())}, 1+r.intn(300))...)
		default:
			if len(b) > 0 {
				s := r.intn(len(b))
				b = append(b, b[s:min(len(b), s+1+r.intn(200))]...)
			}
		}
	}
	return b[:n]
}

// predInput is a predictor input: mostly valid sizes, sometimes a bad spp, a short src or a
// huge w or h.
func predInput(r *rng, op int) []byte {
	spp, flag := []int{1, 3, 4}[r.intn(3)], r.intn(2)
	if op == 2 {
		spp, flag = 1+r.intn(4), 1+r.intn(2)
	}
	if r.intn(8) == 0 {
		spp, flag = r.intn(6), r.intn(4)
	}
	dim := func() int {
		switch r.intn(12) {
		case 0:
			return 0
		case 1:
			return []int{1 << 20, 1<<32 - 1}[r.intn(2)]
		}
		return 1 + r.intn(40)
	}
	w, h := dim(), dim()
	n := w * h * spp
	if op == 2 {
		n *= flag
	}
	if n > 1<<16 || n < 0 {
		n = r.intn(64)
	} else {
		n += []int{0, 0, 0, -1, 5, min(w, 64)}[r.intn(6)]
	}
	b := []byte{byte(op), byte(spp), byte(flag), 0, 0, 0, 0, 0, 0, 0, 0}
	binary.LittleEndian.PutUint32(b[3:], uint32(w))
	binary.LittleEndian.PutUint32(b[7:], uint32(h))
	return append(b, r.bytes(max(n, 0))...)
}

// benchDir prints name, output bytes and ns per decode for each fixture that decodes without a
// dictionary.
func benchDir(dir string, n int) error {
	files, err := os.ReadDir(dir)
	if err != nil {
		return err
	}
	var d deflate.Decoder
	var zd dzlib.Decoder
	for _, f := range files {
		z := strings.HasSuffix(f.Name(), ".zlib")
		if !z && !strings.HasSuffix(f.Name(), ".raw.deflate") {
			continue
		}
		src, err := os.ReadFile(filepath.Join(dir, f.Name()))
		if err != nil {
			return err
		}
		decode := d.Decode
		if z {
			decode = zd.Decode
		}
		buf, err := decode(nil, src)
		if err != nil {
			continue
		}
		t := time.Now()
		for i := 0; i < n; i++ {
			if buf, err = decode(buf[:0], src); err != nil {
				return err
			}
		}
		fmt.Printf("%s\t%d\t%d\n", f.Name(), len(buf), time.Since(t).Nanoseconds()/int64(n))
	}
	return nil
}
