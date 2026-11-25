//! The Rust side of the parity check: `oracle <inputs> <manifest>` decodes every input like
//! `tests/parity/go/oracle` and writes one manifest file: per input (in byte order of the names) and
//! operation `<name>.<op> out <len> <fnv64a-hex>` or `<name>.<op> err <class>`, then
//! `<name>.<op> partial <len> <fnv64a-hex>` for the bytes before an error. The class table is in
//! `docs/parity.md`.

use deflate::{Error, Limits, predictor, zlib};
use std::env;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::sync::atomic::AtomicBool;

/// The n of the DecodeLimit operations, besides `window_limits` of a stream that decodes, and
/// `lh` and `lp` (half, and 1000 more than, the bytes `Decode` returns).
const LIMITS: [usize; 12] = [
    0, 1, 2, 100, 1000, 32767, 32768, 32769, 65535, 65536, 65537, 70000,
];

/// k-1, k, k+1 for every multiple k of 32 KiB (from 64 KiB) near the end of an output of `len`.
fn window_limits(len: usize) -> Vec<usize> {
    let mut l = Vec::new();
    for k in (65536..=len + 32768).step_by(32768) {
        if k + 32768 > len {
            l.extend([k - 1, k, k + 1]);
        }
    }
    l
}

fn class(e: &Error) -> String {
    match e {
        Error::Corrupt(off) => format!("corrupt@{off}"),
        Error::UnexpectedEof => "unexpected-eof".into(),
        Error::Internal(_) => "internal".into(),
        Error::Header => "header".into(),
        Error::Checksum => "checksum".into(),
        Error::Dictionary => "dictionary".into(),
        Error::InvalidArgument => "invalid-argument".into(),
        e => format!("other:{e}"),
    }
}

/// The bytes of an operation and its error class: both can be there, as Go returns both.
type Res = (Vec<u8>, Option<String>);
type Out = Result<Vec<u8>, String>;

fn res(out: Vec<u8>, e: Result<(), Error>) -> Res {
    (out, e.err().map(|e| class(&e)))
}

fn read_class(e: io::Error) -> String {
    match e.get_ref().and_then(|s| s.downcast_ref::<Error>()) {
        Some(e) => class(e),
        None => format!("other:{e}"),
    }
}

/// Reads the whole stream through a reader with `p`-sized reads, as Go's `readChunks`: the bytes,
/// the error if any, the sizes of the non-empty reads as `size*count` runs, one per line, and the
/// input bytes the reader consumed, as a `consumed <n>` line.
fn read_chunks(
    src: &[u8],
    dict: &[u8],
    z: bool,
    limits: &Limits,
    p: usize,
) -> (Res, Vec<u8>, Vec<u8>) {
    let cancel = AtomicBool::new(false);
    let mut cur = src;
    let (res, sizes) = read_from(&mut cur, dict, z, limits, p, &cancel);
    let used = format!("consumed {}\n", src.len() - cur.len()).into_bytes();
    (res, sizes, used)
}

fn read_from(
    src: &mut &[u8],
    dict: &[u8],
    z: bool,
    limits: &Limits,
    p: usize,
    cancel: &AtomicBool,
) -> (Res, Vec<u8>) {
    let mut r: Box<dyn Read + '_> = if z {
        match zlib::Reader::with_dict(src, dict, limits, cancel) {
            Ok(r) => Box::new(r),
            Err(e) => return ((Vec::new(), Some(class(&e))), Vec::new()),
        }
    } else {
        Box::new(deflate::Reader::with_dict(src, dict, limits, cancel))
    };
    let (mut out, mut buf) = (Vec::new(), vec![0u8; p]);
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let err = loop {
        match r.read(&mut buf) {
            Ok(0) => break None,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                match runs.last_mut() {
                    Some((size, count)) if *size == n => *count += 1,
                    _ => runs.push((n, 1)),
                }
            }
            Err(e) => break Some(read_class(e)),
        }
    };
    let sizes = runs
        .iter()
        .map(|(s, c)| format!("{s}*{c}\n"))
        .collect::<String>();
    ((out, err), sizes.into_bytes())
}

/// Reads the whole stream through a reader, as Go's `io.ReadAll`: the bytes, the error if any, and
/// the consumed-input line.
fn read_all(src: &[u8], dict: &[u8], z: bool, limits: &Limits) -> (Res, Vec<u8>) {
    let (res, _, used) = read_chunks(src, dict, z, limits, 1 << 16);
    (res, used)
}

/// FNV-1a 64 of `b`.
fn fnv(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &c| {
        (h ^ u64::from(c)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// FNV-1a 64 and the length of `b`, as a line: `digest` of the Go oracle.
fn digest(b: &[u8]) -> Vec<u8> {
    format!("{:016x} {}\n", fnv(b), b.len()).into_bytes()
}

/// Whether the chunking operations run on the input: `chunked` of the Go oracle.
fn chunked(name: &str) -> bool {
    !name.contains('~') || name.bytes().map(usize::from).sum::<usize>() % 8 == 0
}

/// The Read sizes of the `c<p>` operations.
const READ_SIZES: [usize; 4] = [1, 100, 4096, 1 << 16];

#[allow(clippy::too_many_arguments)]
fn run_stream(
    emit: &mut impl FnMut(&str, Vec<u8>, Option<String>),
    name: &str,
    data: &[u8],
    dict: &[u8],
    z: bool,
    light: bool,
    lim: bool,
    chunks: bool,
) {
    let (limits, cancel) = (Limits::default(), AtomicBool::new(false));
    let whole = |n: Option<usize>| {
        let mut out = Vec::new();
        let r = match (z, n) {
            (true, None) => zlib::decode_into(data, &mut out, &limits, &cancel),
            (true, Some(n)) => zlib::decode_limit_into(data, n, &mut out, &limits, &cancel),
            (false, None) => deflate::decode_into(data, &mut out, &limits, &cancel),
            (false, Some(n)) => deflate::decode_limit_into(data, n, &mut out, &limits, &cancel),
        };
        res(out, r)
    };
    let (dec, dec_err) = whole(None);
    // par: the parallel decode of one stream, 4 threads and 1 KiB chunks so that small inputs
    // split too; it must give what `dec` gives.
    let mut out = Vec::new();
    let r = if z {
        zlib::decode_parallel_chunked_into(data, &mut out, &limits, 4, 1024, &cancel)
    } else {
        deflate::decode_parallel_chunked_into(data, &mut out, &limits, 4, 1024, &cancel)
    };
    let (b, e) = res(out, r);
    emit("par", b, e);
    // The random n of `lr1` to `lr3`: FNV-1a 64 of the name and k, modulo the output length + 1.
    let rand_limit = |k: u8| {
        let h = name
            .bytes()
            .chain([k])
            .fold(0xcbf2_9ce4_8422_2325u64, |h, c| {
                (h ^ u64::from(c)).wrapping_mul(0x0100_0000_01b3)
            });
        (h % (dec.len() as u64 + 1)) as usize
    };
    if light {
        // Only dec, rd, rdc and lr1: the exhaustive truncations and bit flips (`x-` inputs).
        let (b, e) = whole(Some(rand_limit(1)));
        emit("lr1", b, e);
        emit("dec", dec, dec_err);
        let ((out, err), used) = read_all(data, dict, z, &limits);
        emit("rd", out, err);
        emit("rdc", used, None);
        return;
    }
    for (op, n) in [("lh", dec.len() / 2), ("lp", dec.len() + 1000)] {
        let (b, e) = whole(Some(n));
        emit(op, b, e);
    }
    if name.contains('~') && chunked(name) {
        for k in 1u8..=3 {
            let (b, e) = whole(Some(rand_limit(k)));
            emit(&format!("lr{k}"), b, e);
        }
    }
    let mut more = Vec::new();
    if lim {
        more = vec![3, 4, 999, 1001]; // the Go test's extra limits for the `lim-` streams
    }
    if dec_err.is_some() {
        // A stop at the length of the partial output, and 1 either side.
        for d in [-1, 0, 1] {
            if let Some(n) = dec.len().checked_add_signed(d) {
                let (b, e) = whole(Some(n));
                emit(&format!("lq{d:+}"), b, e);
            }
        }
    }
    if dec_err.is_none() {
        more.extend(window_limits(dec.len()));
        for d in [-1, 0, 1] {
            if let Some(n) = dec.len().checked_add_signed(d) {
                let (b, e) = whole(Some(n));
                emit(&format!("lo{d:+}"), b, e);
            }
        }
    }
    emit("dec", dec, dec_err);
    let ((out, err), used) = read_all(data, dict, z, &limits);
    emit("rd", out, err);
    emit("rdc", used, None);
    for p in READ_SIZES.into_iter().filter(|_| chunks) {
        let ((out, err), sizes, used) = read_chunks(data, dict, z, &limits, p);
        emit(&format!("c{p}"), [digest(&out), sizes, used].concat(), err);
    }
    for n in LIMITS.into_iter().chain(more) {
        let (b, e) = whole(Some(n));
        emit(&format!("l{n}"), b, e);
    }
}

/// One predictor input: see `run_pred` in `tests/parity/go/oracle/main.go`.
fn run_pred(data: &[u8]) -> Out {
    let Some((&[op, spp, flag], rest)) = data.split_first_chunk::<3>() else {
        return Err("bad-input".into());
    };
    let Some((dims, src)) = rest.split_first_chunk::<8>() else {
        return Err("bad-input".into());
    };
    let w = u32::from_le_bytes([dims[0], dims[1], dims[2], dims[3]]) as usize;
    let h = u32::from_le_bytes([dims[4], dims[5], dims[6], dims[7]]) as usize;
    let (spp, mut src) = (usize::from(spp), src.to_vec());
    let to_class = |e: Error| class(&e);
    match op {
        0 | 1 => {
            let n = if w * h <= 1 << 20 { 4 * w * h } else { 0 };
            let mut dst = vec![0u8; n];
            let f = if op == 0 {
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
                flag != 0,
                &Limits::default(),
                &AtomicBool::new(false),
            )
            .map_err(to_class)?;
            Ok(dst)
        }
        2 => {
            predictor::undo_horizontal(
                &mut src,
                w,
                h,
                spp,
                usize::from(flag),
                &Limits::default(),
                &AtomicBool::new(false),
            )
            .map_err(to_class)?;
            Ok(src)
        }
        _ => Err("bad-input".into()),
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let [_, input, manifest] = args.as_slice() else {
        eprintln!("usage: oracle <inputs> <manifest>");
        std::process::exit(2);
    };
    let input = Path::new(input);
    // Sorted bytewise, as Go's `os.ReadDir`.
    let mut names = Vec::new();
    for entry in fs::read_dir(input)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    let mut m = BufWriter::with_capacity(1 << 20, File::create(manifest)?);
    for name in &names {
        let path = input.join(name);
        let data = fs::read(&path)?;
        let mut emit = |op: &str, bytes: Vec<u8>, err: Option<String>| {
            let r = match err {
                None => writeln!(m, "{name}.{op} out {} {:016x}", bytes.len(), fnv(&bytes)),
                Some(c) => writeln!(m, "{name}.{op} err {c}").and_then(|()| {
                    if bytes.is_empty() {
                        return Ok(());
                    }
                    // what came before the error
                    writeln!(
                        m,
                        "{name}.{op} partial {} {:016x}",
                        bytes.len(),
                        fnv(&bytes)
                    )
                }),
            };
            r.unwrap_or_else(|e| panic!("{name}: {e}"));
        };
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        // <id>@<dict> names a preset dictionary in <inputs>/dicts.
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let dict = match stem.split_once('@') {
            Some((_, d)) => fs::read(input.join("dicts").join(format!("{d}.dict")))?,
            None => Vec::new(),
        };
        match ext {
            "raw" | "zlib" => run_stream(
                &mut emit,
                name,
                &data,
                &dict,
                ext == "zlib",
                name.starts_with("x-"),
                name.starts_with("lim-"),
                chunked(name),
            ),
            "pred" => match run_pred(&data) {
                Ok(b) => emit("run", b, None),
                Err(c) => emit("run", Vec::new(), Some(c)),
            },
            _ => {}
        }
    }
    m.flush()
}
