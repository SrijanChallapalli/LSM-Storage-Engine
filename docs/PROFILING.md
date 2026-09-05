# Stage 18 — profiling notes

This engine was developed on Windows, so Linux `perf` + flame-graph
captures were not the day-to-day workflow. The same questions were
answered with Criterion, the workload harness, and a few targeted
release-mode timings.

## What the engine actually spends time on

| Question | Observation |
|---|---|
| Encoding records | Cheap. CRC32 over a small buffer is noise next to `fsync`. |
| Copying bytes | `Vec` clones on `get` are visible in microbenchmarks but not in WAL-bound workloads. |
| Hashing | Bloom hashing is a few dozen nanoseconds; it saves a disk read. |
| File synchronization | **The write-path bottleneck.** Every `put`/`delete` calls `sync_all` on the WAL. |
| Searching indexes | Binary search over a few dozen block entries is not measurable next to I/O. |
| Waiting for locks | The block cache mutex is held only for map lookup/insert, never during file reads of a miss that is already being decoded by the same thread. Compaction takes a version snapshot and drops the lock before merging. |
| Compaction CPU | k-way merge is iterator-based and does not load whole tables. It becomes noticeable only when L0 is flushing faster than the worker can merge. |
| File reads per lookup | Range check → Bloom → one index binary search → one data block. Negative lookups often stop at the Bloom filter (`bloom_skips`). |
| Allocations per operation | A put allocates the key, the value, the encoded record, and (on flush) block buffers. The cache holds decoded blocks so repeated gets avoid re-parsing. |

## How to capture a Linux flame graph later

```bash
cargo build --release
perf record -F 99 -g -- ./target/release/workload --workload random --ops 100000
perf script | stackcollapse-perf.pl | flamegraph.pl > flame.svg
```

Build with debug symbols so frames resolve:

```toml
[profile.release]
debug = 1
```

Useful `perf` views:

```bash
perf stat -e cycles,instructions,cache-misses,syscalls:sys_enter_fsync -- \
  ./target/release/workload --workload write-heavy --ops 20000
```

Expect `sys_enter_fsync` (or `fdatasync`) to dominate write-heavy runs.
That is the durability tradeoff this engine chose: a successful `put`
survives a crash.

## Optimization rule used here

For every change in [`BENCHMARKS.md`](BENCHMARKS.md):

1. Record the before metric.
2. Make one change.
3. Write down why it should help.
4. Re-run the same release command.
5. Keep the change only if the metric moved.
