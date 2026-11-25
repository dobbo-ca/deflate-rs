//! `bench <fixtures> <n> [threads]`: prints name, output bytes and ns per decode for each fixture
//! that decodes without a dictionary, as `oracle -bench` does for Go. See `tests/parity/bench.sh`.
//! With `threads` (default: all CPUs), the inputs named `large-*` decode with
//! `zlib::decode_parallel_into` at that `max_threads`; the others serially with a `Decoder`.

use deflate::{Decoder, Limits, zlib};
use std::sync::atomic::AtomicBool;
use std::time::Instant;
use std::{env, fs, io};

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let (dir, n, threads) = match args.as_slice() {
        [_, dir, n] => (dir, n, None),
        [_, dir, n, t] => (dir, n, t.parse().ok()),
        _ => {
            eprintln!("usage: bench <fixtures> <n> [threads]");
            std::process::exit(2);
        }
    };
    let n: u32 = n.parse().unwrap_or(1).max(1);
    let threads: usize = threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, std::num::NonZero::get));
    let (limits, cancel) = (Limits::default(), AtomicBool::new(false));
    // The kernel tiers that run, for the log.
    eprintln!("tiers: {:?}", deflate::tiers());
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        let z = name.ends_with(".zlib");
        if !z && !name.ends_with(".raw.deflate") {
            continue;
        }
        let src = fs::read(format!("{dir}/{name}"))?;
        // Go reuses its Decoder and the output buffer, and so does this. The `large-*` inputs
        // time the parallel decode in their own function, so the fixtures' loop stays the same
        // code as before it (an 80 ns decode shows any change of the call around it).
        let r = if name.starts_with("large-") {
            time_parallel(&src, z, n, threads, &limits, &cancel)
        } else {
            time_serial(&src, z, n, &limits, &cancel)
        };
        if let Some((len, ns)) = r {
            println!("{name}\t{len}\t{ns}");
        }
    }
    Ok(())
}

/// The output length and ns per decode of `src` with a reused `Decoder` and buffer, None if it
/// does not decode.
fn time_serial(
    src: &[u8],
    z: bool,
    n: u32,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Option<(usize, u128)> {
    let mut out = Vec::new();
    let (mut d, mut zd) = (Decoder::new(), zlib::Decoder::new());
    let mut decode = |out: &mut Vec<u8>| {
        out.clear();
        if z {
            zd.decode_into(src, out, limits, cancel)
        } else {
            d.decode_into(src, out, limits, cancel)
        }
    };
    decode(&mut out).ok()?;
    let len = out.len();
    let t = Instant::now();
    for _ in 0..n {
        std::hint::black_box(decode(&mut out).is_ok());
    }
    Some((len, t.elapsed().as_nanos() / u128::from(n)))
}

/// As `time_serial`, with the parallel decode of one stream at `threads`.
fn time_parallel(
    src: &[u8],
    z: bool,
    n: u32,
    threads: usize,
    limits: &Limits,
    cancel: &AtomicBool,
) -> Option<(usize, u128)> {
    let mut out = Vec::new();
    let decode = |out: &mut Vec<u8>| {
        out.clear();
        if z {
            zlib::decode_parallel_into(src, out, limits, threads, cancel)
        } else {
            deflate::decode_parallel_into(src, out, limits, threads, cancel)
        }
    };
    decode(&mut out).ok()?;
    let len = out.len();
    let t = Instant::now();
    for _ in 0..n {
        std::hint::black_box(decode(&mut out).is_ok());
    }
    Some((len, t.elapsed().as_nanos() / u128::from(n)))
}
