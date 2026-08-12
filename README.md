# LSM Storage Engine

A single-node, embedded key–value storage engine, built from scratch in Rust by
following a stage-by-stage plan. This repo is my own build — I'm implementing
each stage myself, in order.

## The plan

The full curriculum lives in [`docs/stage-plan.pdf`](docs/stage-plan.pdf). Build
in order, and don't move on to the next stage until all three of these pass:

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

## Getting started

Install the Rust toolchain from <https://rustup.rs>, then scaffold the crate in
this directory:

```bash
cargo init --lib
```

Stage 0 in the plan lists the module layout to create:

```
src/
├── lib.rs
├── engine.rs
├── error.rs
├── memtable.rs
├── wal.rs
├── sstable.rs
├── manifest.rs
└── compaction.rs
```

Then start on **Stage 1 — the memtable**.

## Progress

**Phase 1 — In-memory storage**
- [ ] Stage 1 — Memtable
- [ ] Stage 2 — Public engine API

**Phase 2 — Durability**
- [ ] Stage 3 — Binary record format
- [ ] Stage 4 — Write-ahead log
- [ ] Stage 5 — Crash recovery

**Phase 3 — Immutable disk storage**
- [ ] Stage 6 — SSTable flush
- [ ] Stage 7 — Read path

**Phase 4 — Metadata & crash consistency**
- [ ] Stage 8 — Manifest

**Phase 5 — Compaction**
- [ ] Stage 9 — Synchronous compaction
- [ ] Stage 10 — Background compaction

**Phase 6 — LSM-tree structure**
- [ ] Stage 11 — Levels
- [ ] Stage 12 — Tombstone garbage collection

**Phase 7 — Performance**
- [ ] Stage 13 — Bloom filters
- [ ] Stage 14 — Block-based SSTables & cache
- [ ] Stage 15 — Range scans

**Phase 8 — Benchmarking & optimization**
- [ ] Stage 16 — Workload generator
- [ ] Stage 17 — Criterion benchmarks
- [ ] Stage 18 — Profiling

**Phase 9 — Reliability & polish**
- [ ] Stage 19 — Fault injection
- [ ] Stage 20 — Documentation & final repository

## Reference implementation

A worked implementation of Stages 0–4 is preserved on the
[`reference`](../../tree/reference) branch — there to peek at when I'm stuck,
not to copy from.
