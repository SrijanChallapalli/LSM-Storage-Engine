# LSM Storage Engine

A [log-structured merge-tree](https://en.wikipedia.org/wiki/Log-structured_merge-tree)
storage engine written in Rust, built up one stage at a time. The goal is a
single-node, embedded key–value store with durable writes, crash recovery, and
compaction — implemented from scratch, without pulling in a storage crate.

> **Status:** early days. Stage 0 (project setup) and Stage 1 (the in-memory
> memtable) are done. The remaining stages are scaffolded and land next.

## Planned final API

```rust
let mut db = Engine::open("./data")?;

db.put(b"name", b"Srijan")?;
db.put(b"language", b"Rust")?;
assert_eq!(db.get(b"name")?, Some(b"Srijan".to_vec()));

db.delete(b"name")?;
assert_eq!(db.get(b"name")?, None);

db.flush()?;
db.close()?;

// After reopening, the last committed state is recovered:
let db = Engine::open("./data")?;
assert_eq!(db.get(b"name")?, None);
assert_eq!(db.get(b"language")?, Some(b"Rust".to_vec()));
```

## Architecture (target)

```
        put / delete                         get
             │                                │
             ▼                                ▼
      ┌─────────────┐   flush   ┌──────────────────────────┐
write │  WAL (log)  │ ────────▶ │  read path: memtable ▶    │
ahead └─────────────┘           │  immutable memtable ▶     │
             │                  │  SSTables newest ▶ oldest │
             ▼                  └──────────────────────────┘
      ┌─────────────┐                        ▲
      │  memtable   │ ── flush ─▶ SSTables ──┘
      │ (BTreeMap)  │                 ▲
      └─────────────┘          compaction merges
                               overlapping tables
```

- **Write path:** encode the operation → append + sync to the WAL → apply to the
  memtable. The memtable is never updated before the WAL append succeeds.
- **Read path:** search newest to oldest — mutable memtable, immutable memtable,
  then SSTables from newest to oldest — stopping at the first value or tombstone.
- **Durability:** the WAL is replayed on startup to rebuild the last committed
  state; the manifest records which SSTables are live, updated atomically so a
  crash always leaves a valid state.

## Module layout

| Module          | Stage  | Status        |
| --------------- | ------ | ------------- |
| `memtable`      | 1      | ✅ implemented |
| `engine`        | 2      | 🚧 planned     |
| `error`         | 2–5    | ✅ in progress |
| `wal`           | 3–5    | 🚧 planned     |
| `sstable`       | 6–7    | 🚧 planned     |
| `manifest`      | 8      | 🚧 planned     |
| `compaction`    | 9–12   | 🚧 planned     |

### The memtable (Stage 1)

The memtable holds the newest writes in a `BTreeMap<Vec<u8>, Entry>`, keeping
keys sorted so a flush can stream them out in order. Each key maps to an `Entry`:

```rust
pub enum Entry {
    Value(Vec<u8>),
    Tombstone, // records a deletion so it can shadow older values
}
```

Deletes insert a tombstone rather than removing the key, and `approximate_size`
tracks key + value bytes so the engine can decide when to flush.

## Building and testing

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

Every stage must pass all three before the next one begins.

## Roadmap

Phased build order, from in-memory storage to a benchmarked leveled LSM tree:

1. **In-memory storage** — memtable, public engine API
2. **Durability** — binary record format, WAL, crash recovery
3. **Immutable disk storage** — SSTable flush, full read path
4. **Metadata & crash consistency** — manifest
5. **Compaction** — synchronous, then background
6. **LSM-tree structure** — levels, tombstone garbage collection
7. **Performance** — Bloom filters, block cache, range scans
8. **Benchmarking & optimization** — workload generator, Criterion, profiling
9. **Reliability & polish** — fault injection, documentation

The full stage-by-stage plan lives in [`docs/stage-plan.pdf`](docs/stage-plan.pdf).

## License

MIT
