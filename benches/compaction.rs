use criterion::{black_box, criterion_group, criterion_main, Criterion};
use lsm_store::compaction::compact;
use lsm_store::memtable::Entry;
use lsm_store::sstable::SsTable;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

fn tmp_dir() -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("lsm-bench-cmp-{id}"));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn make_table(dir: &std::path::Path, id: u64, start: u32, count: u32) -> Arc<SsTable> {
    let entries = (start..start + count)
        .map(|i| (format!("k{i:04}").into_bytes(), Entry::Value(b"v".to_vec())));
    Arc::new(SsTable::create(dir.join(format!("{id:06}.sst")), id, entries, 0.01, 256).unwrap())
}

fn two_table_merge(c: &mut Criterion) {
    let dir = tmp_dir();
    let a = make_table(&dir, 1, 0, 128);
    let b = make_table(&dir, 2, 64, 128);
    c.bench_function("two_table_merge", |bch| {
        let mut n = 10u64;
        bch.iter(|| {
            n += 1;
            let out = dir.join(format!("{n:06}.sst"));
            black_box(compact(&[a.clone(), b.clone()], &out, n, 0.01, 256, false).unwrap());
        });
    });
}

fn multi_table_merge(c: &mut Criterion) {
    let dir = tmp_dir();
    let tables: Vec<_> = (0..5u64)
        .map(|i| make_table(&dir, i + 1, (i as u32) * 20, 80))
        .collect();
    c.bench_function("multi_table_merge", |bch| {
        let mut n = 20u64;
        bch.iter(|| {
            n += 1;
            let out = dir.join(format!("{n:06}.sst"));
            black_box(compact(&tables, &out, n, 0.01, 256, false).unwrap());
        });
    });
}

criterion_group!(benches, two_table_merge, multi_table_merge);
criterion_main!(benches);
