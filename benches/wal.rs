use criterion::{black_box, criterion_group, criterion_main, Criterion};
use lsm_store::record::{decode, encode, Operation};
use lsm_store::wal::Wal;
use std::sync::atomic::{AtomicU64, Ordering};

fn tmp_wal() -> (std::path::PathBuf, Wal) {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("lsm-bench-wal-{id}"));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("wal.log");
    let wal = Wal::open(&path).unwrap();
    (dir, wal)
}

fn record_encode(c: &mut Criterion) {
    let op = Operation::Put {
        key: b"name".to_vec(),
        value: b"Srijan".to_vec(),
    };
    c.bench_function("record_encode", |b| {
        b.iter(|| black_box(encode(black_box(&op)).unwrap()));
    });
}

fn record_decode(c: &mut Criterion) {
    let encoded = encode(&Operation::Put {
        key: b"name".to_vec(),
        value: b"Srijan".to_vec(),
    })
    .unwrap();
    c.bench_function("record_decode", |b| {
        b.iter(|| black_box(decode(black_box(&encoded)).unwrap()));
    });
}

fn wal_append(c: &mut Criterion) {
    c.bench_function("wal_append", |b| {
        let (_dir, mut wal) = tmp_wal();
        let op = Operation::Put {
            key: b"k".to_vec(),
            value: vec![1, 2, 3, 4],
        };
        b.iter(|| {
            wal.append(black_box(&op)).unwrap();
        });
    });
}

criterion_group!(benches, record_encode, record_decode, wal_append);
criterion_main!(benches);
