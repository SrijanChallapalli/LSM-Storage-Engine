# Benchmark methodology and results

All numbers below were collected on Windows with a release build. Debug
builds are not comparable and were not used.

```bash
cargo build --release
cargo test --release
cargo run --release --bin workload -- --ops 20000 --workload <name>
cargo bench
```

## Workload harness

`src/bin/workload.rs` runs a fixed number of operations against a fresh
directory and prints:

- operations per second
- bytes per second (writes only)
- average / p50 / p95 / p99 latency
- Bloom-filter skips
- SSTable files consulted per process

Workloads:

| Name | Shape |
|---|---|
| `sequential` | keys `key-000001`, `key-000002`, … |
| `random` | keys uniform over a 50k keyspace |
| `read-heavy` | 90% get / 10% put |
| `write-heavy` | 20% get / 80% put |
| `negative` | gets for keys that do not exist |
| `overwrite` | puts rotating over 64 hot keys |

Each run creates a new `data/bench` directory so leftover tables cannot
skew the next measurement.

## Recorded workload results

Machine: Windows 10, release, 20 000 operations, 64-byte values.
These are order-of-magnitude figures for this tree — rerun the command
above to reproduce on another host.

| Workload | ops/s (approx) | p50 | p95 | p99 | notes |
|---|---|---|---|---|---|
| sequential | 3–8k | sub-ms | WAL sync bound | WAL sync bound | every put `fsync`s |
| random | similar to sequential | sub-ms | WAL sync | WAL sync | flushes mid-run |
| read-heavy | higher than write-heavy | low µs on memtable hits | flush tails | flush tails | Bloom skips on negatives |
| write-heavy | WAL bound | sub-ms | sync | sync | compaction in background |
| negative | highest read throughput | low µs | | | Bloom skip rate is the lever |
| overwrite | WAL bound | sub-ms | sync | sync | small working set |

The dominant cost on the write path is `File::sync_all`, not encoding or
the `BTreeMap` insert. That matches the Stage 18 profiling notes: most
CPU-visible time in a write-heavy run is spent waiting on the filesystem.

## Criterion microbenchmarks

```bash
cargo bench
```

Targets:

| Bench | What it isolates |
|---|---|
| `memtable_put` / `memtable_get` | in-memory BTreeMap path |
| `record_encode` / `record_decode` | CRC32 + length prefixing |
| `wal_append` | encode + `write_all` (no sync) |
| `sstable_lookup` | index binary search + block decode |
| `bloom_lookup` | hashing + bit test |
| `cache_hit` / `cache_miss` | LRU map |
| `two_table_merge` / `multi_table_merge` | k-way compaction |

HTML reports land in `target/criterion/`. Compare before/after a change
by keeping the previous `target/criterion` directory or copying the
estimates out of `estimates.json`.

## Optimization log

| Before | Change | Why | After | Helped? |
|---|---|---|---|---|
| Full-file scan on `get` | per-block index + binary search | avoid reading every record | point reads touch one block | yes |
| Always opened every SSTable | Bloom filter in the footer | skip files that cannot contain the key | fewer `sstable_reads` on negatives | yes |
| Re-decoded the same block | LRU `BlockCache` | repeated gets share decoded pairs | cache hits on hot keys | yes |
| One record per write, no grouping | still sync every put | durability requirement | write throughput stays fsync-bound | n/a — by design |

## Write and read amplification

- **Write amplification:** each key is written to the WAL, then to an L0
  table, then once per compaction into a lower level. With
  `level_size_multiplier = 10` this is the usual leveled-LSM curve.
- **Read amplification:** a get may inspect the memtable, L0 tables, and
  one table per lower level. Bloom filters and key-range bounds cut the
  number of files actually read; `Engine::sstable_reads` and
  `Engine::bloom_skips` expose those counters.
