use criterion::{black_box, criterion_group, criterion_main, Criterion};
use lsm_store::MemTable;

fn memtable_put(c: &mut Criterion) {
    c.bench_function("memtable_put", |b| {
        b.iter(|| {
            let mut table = MemTable::new();
            for i in 0..256u32 {
                table.put(i.to_le_bytes().to_vec(), b"value".to_vec());
            }
            black_box(table.len());
        });
    });
}

fn memtable_get(c: &mut Criterion) {
    let mut table = MemTable::new();
    for i in 0..256u32 {
        table.put(i.to_le_bytes().to_vec(), b"value".to_vec());
    }
    c.bench_function("memtable_get", |b| {
        b.iter(|| {
            for i in 0..256u32 {
                black_box(table.get(&i.to_le_bytes()));
            }
        });
    });
}

criterion_group!(benches, memtable_put, memtable_get);
criterion_main!(benches);
