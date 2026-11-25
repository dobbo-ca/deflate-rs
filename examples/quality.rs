//! `quality <in> <out>`: the Rust side of the real-file check (docs/parity.md, "Real-file
//! check"), as `oracle -qdecode` does with Go. Each input `<stem>.w<W>.c<C>.p<P>.zlib` (or
//! `.deflate`, raw DEFLATE) is decoded, predictor 2 is undone if `P` is 2, and the samples go to
//! `<out>/<stem>.w<W>.c<C>.p<P>.pgm` (`C` 1) or `.ppm` (`C` 3), `W` wide; other `C` as a PGM
//! `W * C` wide. An input that fails, or whose output is empty or not whole rows, gets no file.
//! An input of 2 MiB or more goes to the parallel decode of one stream, on all CPUs.

use deflate::{Decoder, Limits, predictor, zlib};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};
use std::{env, fs, io, io::Write, path::Path};

/// `(stem, w, c, p, zlib)` of `<stem>.w<W>.c<C>.p<P>.<zlib|deflate>`.
fn qname(name: &str) -> Option<(&str, usize, usize, usize, bool)> {
    let (stem, ext) = name.rsplit_once('.')?;
    let z = match ext {
        "zlib" => true,
        "deflate" => false,
        _ => return None,
    };
    let mut parts = stem.rsplitn(4, '.');
    let num = |s: Option<&str>, pre: char| s?.strip_prefix(pre)?.parse::<usize>().ok();
    let p = num(parts.next(), 'p')?;
    let c = num(parts.next(), 'c')?;
    let w = num(parts.next(), 'w')?;
    parts.next()?;
    (w >= 1 && c >= 1 && (p == 1 || p == 2)).then_some((stem, w, c, p, z))
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let [_, inp, out] = args.as_slice() else {
        eprintln!("usage: quality <in> <out>");
        std::process::exit(2);
    };
    fs::create_dir_all(out)?;
    let mut names: Vec<String> = fs::read_dir(inp)?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<io::Result<_>>()?;
    names.sort();
    let (limits, cancel) = (Limits::default(), AtomicBool::new(false));
    let (mut d, mut zd) = (Decoder::new(), zlib::Decoder::new());
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let mut buf = Vec::new();
    let (mut n_out, mut n_fail) = (0, 0);
    // Decode time per input, for the log: the total and the slowest.
    let mut times: Vec<(Duration, usize, &str)> = Vec::new();
    for name in &names {
        let Some((stem, w, c, p, z)) = qname(name) else {
            continue;
        };
        let src = fs::read(Path::new(inp).join(name))?;
        buf.clear();
        let t = Instant::now();
        // A large stream goes to the parallel decode on all CPUs, which decodes
        // serially below 4 MiB.
        let r = match (z, src.len() >= 2 << 20) {
            (true, true) => zlib::decode_parallel_into(&src, &mut buf, &limits, threads, &cancel),
            (false, true) => {
                deflate::decode_parallel_into(&src, &mut buf, &limits, threads, &cancel)
            }
            (true, false) => zd.decode_into(&src, &mut buf, &limits, &cancel),
            (false, false) => d.decode_into(&src, &mut buf, &limits, &cancel),
        };
        times.push((t.elapsed(), buf.len(), name));
        let row = w * c;
        if r.is_err() || buf.is_empty() || !buf.len().is_multiple_of(row) {
            n_fail += 1;
            continue;
        }
        let h = buf.len() / row;
        if p == 2 {
            predictor::undo_horizontal(&mut buf, w, h, c, 1, &limits, &cancel)
                .map_err(io::Error::other)?;
        }
        let (magic, ext, pw) = match c {
            1 => ("P5", "pgm", w),
            3 => ("P6", "ppm", w),
            _ => ("P5", "pgm", row),
        };
        let mut f = io::BufWriter::new(fs::File::create(
            Path::new(out).join(format!("{stem}.{ext}")),
        )?);
        write!(f, "{magic}\n{pw} {h}\n255\n")?;
        f.write_all(&buf)?;
        f.flush()?;
        n_out += 1;
    }
    eprintln!("quality: {n_out} outputs, {n_fail} inputs without output");
    let total: Duration = times.iter().map(|t| t.0).sum();
    let bytes: usize = times.iter().map(|t| t.1).sum();
    eprintln!("quality: decode {total:?} for {bytes} bytes; the slowest:");
    times.sort_by_key(|a| std::cmp::Reverse(a.0));
    for (t, n, name) in times.iter().take(10) {
        eprintln!("  {t:?}\t{n}\t{name}");
    }
    Ok(())
}
