//! Stage 16 — repeatable workload generator.
//!
//! Run with:
//!
//! ```text
//! cargo run --release --bin workload -- --workload random --ops 20000
//! ```

use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use lsm_store::{Engine, Options};

#[derive(Clone, Copy)]
enum Workload {
    Sequential,
    Random,
    ReadHeavy,
    WriteHeavy,
    Negative,
    Overwrite,
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut workload = Workload::Random;
    let mut ops = 20_000usize;
    let mut dir = PathBuf::from("data/bench");

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--workload" => {
                i += 1;
                workload = parse_workload(args.get(i).map(String::as_str).unwrap_or("random"));
            }
            "--ops" => {
                i += 1;
                ops = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(ops);
            }
            "--dir" => {
                i += 1;
                if let Some(path) = args.get(i) {
                    dir = PathBuf::from(path);
                }
            }
            _ => {}
        }
        i += 1;
    }

    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create bench dir");

    let options = Options {
        memtable_size_bytes: 512 * 1024,
        l0_compaction_trigger: 4,
        ..Options::default()
    };
    let mut db = Engine::open_with(&dir, options).expect("open engine");

    let mut rng = 0xDEAD_BEEF_u64;
    let mut latencies = Vec::with_capacity(ops);
    let mut bytes = 0u64;
    let start = Instant::now();

    for i in 0..ops {
        let op_start = Instant::now();
        match workload {
            Workload::Sequential => {
                let key = format!("key-{i:06}");
                let value = vec![b'x'; 64];
                db.put(key.as_bytes(), &value).unwrap();
                bytes += (key.len() + value.len()) as u64;
            }
            Workload::Random => {
                let key = random_key(&mut rng, 50_000);
                let value = vec![b'r'; 64];
                db.put(key.as_bytes(), &value).unwrap();
                bytes += (key.len() + value.len()) as u64;
            }
            Workload::ReadHeavy => {
                if i % 10 == 0 {
                    let key = random_key(&mut rng, 5_000);
                    let value = vec![b'w'; 32];
                    db.put(key.as_bytes(), &value).unwrap();
                    bytes += (key.len() + value.len()) as u64;
                } else {
                    let key = random_key(&mut rng, 5_000);
                    let _ = db.get(key.as_bytes()).unwrap();
                }
            }
            Workload::WriteHeavy => {
                if i % 5 == 0 {
                    let key = random_key(&mut rng, 8_000);
                    let _ = db.get(key.as_bytes()).unwrap();
                } else {
                    let key = random_key(&mut rng, 8_000);
                    let value = vec![b'w'; 64];
                    db.put(key.as_bytes(), &value).unwrap();
                    bytes += (key.len() + value.len()) as u64;
                }
            }
            Workload::Negative => {
                let key = format!("missing-{i:08}");
                let _ = db.get(key.as_bytes()).unwrap();
            }
            Workload::Overwrite => {
                let key = format!("hot-{}", i % 64);
                let value = vec![b'o'; 64];
                db.put(key.as_bytes(), &value).unwrap();
                bytes += (key.len() + value.len()) as u64;
            }
        }
        latencies.push(op_start.elapsed().as_nanos() as u64);
    }

    db.flush().unwrap();
    let elapsed = start.elapsed();
    latencies.sort_unstable();

    let ops_per_sec = ops as f64 / elapsed.as_secs_f64();
    let bytes_per_sec = bytes as f64 / elapsed.as_secs_f64();
    println!("workload          {:?}", workload_name(workload));
    println!("ops               {ops}");
    println!("elapsed_ms        {:.2}", elapsed.as_secs_f64() * 1000.0);
    println!("ops_per_sec       {ops_per_sec:.1}");
    println!("bytes_per_sec     {bytes_per_sec:.1}");
    println!("avg_latency_us    {:.2}", mean(&latencies) / 1000.0);
    println!(
        "p50_latency_us    {:.2}",
        percentile(&latencies, 0.50) / 1000.0
    );
    println!(
        "p95_latency_us    {:.2}",
        percentile(&latencies, 0.95) / 1000.0
    );
    println!(
        "p99_latency_us    {:.2}",
        percentile(&latencies, 0.99) / 1000.0
    );
    println!("bloom_skips       {}", db.bloom_skips());
    println!("sstable_reads     {}", db.sstable_reads());
    db.close().unwrap();
}

fn parse_workload(name: &str) -> Workload {
    match name {
        "sequential" => Workload::Sequential,
        "read-heavy" => Workload::ReadHeavy,
        "write-heavy" => Workload::WriteHeavy,
        "negative" => Workload::Negative,
        "overwrite" => Workload::Overwrite,
        _ => Workload::Random,
    }
}

fn workload_name(workload: Workload) -> &'static str {
    match workload {
        Workload::Sequential => "sequential",
        Workload::Random => "random",
        Workload::ReadHeavy => "read-heavy",
        Workload::WriteHeavy => "write-heavy",
        Workload::Negative => "negative",
        Workload::Overwrite => "overwrite",
    }
}

fn random_key(rng: &mut u64, space: u64) -> String {
    format!("key-{:06}", xorshift(rng) % space)
}

fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn mean(values: &[u64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<u64>() as f64 / values.len() as f64
}

fn percentile(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx] as f64
}
