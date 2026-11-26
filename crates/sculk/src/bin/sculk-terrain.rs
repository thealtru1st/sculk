//! Benchmarks `write_level_chunk` (generation + encoding, no compression).
//!
//! `cargo run --release -p sculk --bin sculk-terrain -- --chunks 5000 --seed 0`
//! Build with `--features allocation-metrics` for real allocation counts.

use flate2::{Compress, Compression, FlushCompress, Status};
use sculk::allocation_metrics;
use sculk::chunk::{write_level_chunk, LEVEL_CHUNK_SIZE_HINT};
use sculk::metrics::LatencyHistogram;
use sculk::world::World;
use std::time::Instant;

fn main() {
    let mut chunks: u64 = 2000;
    let mut seed: u64 = 0;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag.to_owned(), Some(value.to_owned())),
            None => (arg.clone(), None),
        };
        let mut value = || {
            inline
                .clone()
                .or_else(|| args.next())
                .unwrap_or_else(|| fail(&format!("missing value for {flag}")))
        };
        match flag.as_str() {
            "--chunks" => chunks = value().parse().unwrap_or_else(|_| fail("bad --chunks")),
            "--seed" => seed = value().parse().unwrap_or_else(|_| fail("bad --seed")),
            "-h" | "--help" => {
                println!("usage: sculk-terrain [--chunks N] [--seed S]");
                return;
            }
            other => fail(&format!("unknown argument {other}")),
        }
    }
    let chunks = chunks.max(1);

    let world = World::new(seed);
    let side = (chunks as f64).sqrt().ceil() as i64;
    let coord = |i: u64| {
        let i = i as i64;
        ((i % side - side / 2) as i32, (i / side - side / 2) as i32)
    };

    let mut out = Vec::with_capacity(LEVEL_CHUNK_SIZE_HINT);
    let mut compressed = Vec::with_capacity(LEVEL_CHUNK_SIZE_HINT);
    let mut zlib = Compress::new(Compression::new(1), true);
    // Warm the per-thread scratch buffers outside the measurement.
    write_level_chunk(&world, 100_000, 0, &mut out);

    let mut latency = LatencyHistogram::default();
    let mut allocation_calls = 0u64;
    let mut allocation_bytes = 0u64;
    let mut compressed_total = 0u64;
    let mut checksum = 0xcbf2_9ce4_8422_2325u64;
    let mut generating = std::time::Duration::ZERO;
    for i in 0..chunks {
        let (cx, cz) = coord(i);
        out.clear();
        let before = allocation_metrics::snapshot();
        let t0 = Instant::now();
        write_level_chunk(&world, cx, cz, &mut out);
        let elapsed = t0.elapsed();
        let allocations = allocation_metrics::snapshot().delta(before);
        generating += elapsed;
        latency.record(elapsed);
        allocation_calls += allocations.calls;
        allocation_bytes += allocations.bytes;

        for word in out.chunks(8) {
            let mut bytes = [0u8; 8];
            bytes[..word.len()].copy_from_slice(word);
            checksum = (checksum ^ u64::from_le_bytes(bytes)).wrapping_mul(0x0000_0100_0000_01b3);
        }

        zlib.reset();
        compressed.clear();
        compressed.reserve(out.len() + 1024);
        loop {
            let status = zlib
                .compress_vec(&out, &mut compressed, FlushCompress::Finish)
                .expect("zlib");
            if status == Status::StreamEnd {
                break;
            }
            compressed.reserve(4096);
        }
        compressed_total += compressed.len() as u64;
    }

    let elapsed_secs = generating.as_secs_f64();
    let n = chunks as f64;
    println!(
        "{{\"chunks\":{chunks},\"elapsed_secs\":{elapsed_secs:.6},\"chunks_per_sec\":{:.1},\
\"latency_avg_us\":{:.2},\"latency_p95_us\":{:.2},\"latency_p99_us\":{:.2},\
\"latency_max_us\":{:.2},\"allocation_metrics\":{},\"allocation_calls\":{allocation_calls},\
\"allocation_bytes\":{allocation_bytes},\"allocations_per_chunk\":{:.4},\
\"allocation_bytes_per_chunk\":{:.1},\"compressed_bytes_avg\":{:.1},\"checksum\":\"{checksum:016x}\"}}",
        n / elapsed_secs,
        latency.average_us(),
        latency.percentile_us(0.95),
        latency.percentile_us(0.99),
        latency.max_us(),
        allocation_metrics::ENABLED,
        allocation_calls as f64 / n,
        allocation_bytes as f64 / n,
        compressed_total as f64 / n,
    );
}

fn fail(message: &str) -> ! {
    eprintln!("sculk-terrain: {message}");
    std::process::exit(2)
}
