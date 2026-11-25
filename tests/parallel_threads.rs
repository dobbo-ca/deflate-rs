//! `max_threads` of the parallel decode is the peak of its threads, the calling thread included:
//! the crate counts its spawned workers while they run (`deflate::parallel_thread_peak`, exact,
//! unlike a sample of `/proc`, which still lists a joined thread for a moment), and the peak stays
//! at most `max_threads - 1`. One `#[test]` only, so no other test decodes meanwhile.

mod common;
use common::*;
use deflate::{Limits, decode_parallel_chunked_into};

#[test]
fn peak_threads_within_max_threads() {
    let streams: Vec<Vec<u8>> = fixtures()
        .into_iter()
        .filter(|f| f.file.starts_with("flush-") && f.file.ends_with(".raw.deflate"))
        .map(|f| f.data)
        .collect();
    assert!(!streams.is_empty());
    let mut ran = deflate::parallel_decodes();
    for max_threads in [1, 2, 3, 5, 8] {
        deflate::parallel_thread_peak();
        for s in &streams {
            for _ in 0..20 {
                let mut out = Vec::new();
                let r = decode_parallel_chunked_into(
                    s,
                    &mut out,
                    &Limits::default(),
                    max_threads,
                    512,
                    &NO_CANCEL,
                );
                assert!(r.is_ok());
            }
        }
        let p = deflate::parallel_thread_peak();
        assert!(
            p < max_threads,
            "max_threads {max_threads}: {p} spawned at once"
        );
        let now = deflate::parallel_decodes();
        // Under 4 workers (threads or CPUs) the decode is serial.
        let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
        assert_eq!(
            now > ran,
            max_threads.min(cpus) >= 4,
            "max_threads {max_threads}"
        );
        ran = now;
    }
}
