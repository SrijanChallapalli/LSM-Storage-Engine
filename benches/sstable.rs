use criterion::{black_box, criterion_group, criterion_main, Criterion};
use lsm_store::bloom::BloomFilter;
use lsm_store::cache::{BlockCache, BlockKey};
use lsm_store::memtable::Entry;
use lsm_store::sstable::SsTable;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

fn tmp_dir() -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("lsm-bench-sst-{id}"));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn sstable_lookup(c: &mut Criterion) {
    let dir = tmp_dir();
    let entries =
        (0..512u32).map(|i| (format!("k{i:04}").into_bytes(), Entry::Value(b"v".to_vec())));
    let table = SsTable::create(dir.join("000001.sst"), 1, entries, 0.01, 256).unwrap();
    c.bench_function("sstable_lookup", |b| {
        b.iter(|| {
            black_box(table.get(black_box(b"k0256"), None).unwrap());
        });
    });
}

fn bloom_lookup(c: &mut Criterion) {
    let mut filter = BloomFilter::new(512, 0.01);
    for i in 0..512u32 {
        filter.insert(format!("k{i:04}").as_bytes());
    }
    c.bench_function("bloom_lookup", |b| {
        b.iter(|| {
            black_box(filter.might_contain(black_box(b"k0256")));
            black_box(filter.might_contain(black_box(b"missing")));
        });
    });
}

fn cache_hit_miss(c: &mut Criterion) {
    let mut cache = BlockCache::new(4096);
    let key = BlockKey {
        table_id: 1,
        offset: 0,
    };
    cache.insert(
        key,
        Arc::new(vec![(b"a".to_vec(), Entry::Value(b"1".to_vec()))]),
    );
    c.bench_function("cache_hit", |b| {
        b.iter(|| {
            black_box(cache.get(black_box(key)));
        });
    });
    let missing = BlockKey {
        table_id: 9,
        offset: 99,
    };
    c.bench_function("cache_miss", |b| {
        b.iter(|| {
            black_box(cache.get(black_box(missing)));
        });
    });
}

criterion_group!(benches, sstable_lookup, bloom_lookup, cache_hit_miss);
criterion_main!(benches);
