# LSM Storage Engine

A [log-structured merge-tree](https://en.wikipedia.org/wiki/Log-structured_merge-tree)
storage engine written in Rust, built up one stage at a time. The goal is a
single-node, embedded key–value store with durable writes, crash recovery, and
compaction — implemented from scratch, without pulling in a storage crate.

> **Status:** early days. Stages 0–4 are done — project setup, the in-memory
> memtable, the public `Engine` API, the binary record format, and the
> write-ahead log. Writes are now crash-durable and recovered on reopen. The
> remaining stages are scaffolded and land next.

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
| `engine`        | 2      | ✅ implemented |
| `record`        | 3      | ✅ implemented |
| `wal`           | 4      | ✅ implemented |
| `error`         | 2–5    | ✅ in progress |
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

### The record format (Stage 3)

Every write is an `Operation` (`Put` or `Delete`) serialized to a self-describing,
checksummed record — the unit the WAL and SSTables are built from:

```text
checksum (4) │ record type (1) │ key len (4) │ value len (4) │ key │ value
```

Integers are little-endian; the leading CRC32 covers everything after it. `decode`
is strict — it rejects empty input, truncated or oversized records, trailing
bytes, unknown record types, and checksum mismatches — so `decode(encode(op)) == op`
holds for every valid operation and corruption is caught rather than trusted.

### The write-ahead log (Stage 4)

Every `put`/`delete` is made durable before it is visible. The engine follows a
strict order:

1. encode the operation → 2. append it to the WAL → 3. `fsync` the WAL →
4. apply it to the memtable → 5. return success.

The memtable is never updated before the append and sync succeed, so an
acknowledged write is always already on disk. On `Engine::open` the log is
replayed oldest-to-newest to rebuild the memtable. A crash can leave a torn
record at the tail of the log; that unacknowledged fragment is ignored on
replay, while a *complete* record that fails its checksum is surfaced as an
error rather than trusted. The upshot: write, kill the process, reopen — and the
exact last committed state comes back.

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
