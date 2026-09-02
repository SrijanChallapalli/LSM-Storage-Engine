# LSM Storage Engine

A single-node, embedded key–value storage engine built from scratch in Rust.
Writes go to a memtable and a write-ahead log first; when the memtable fills,
it is frozen and flushed into an immutable SSTable. Background compaction
merges overlapping tables into a leveled LSM tree. After random writes,
updates, deletes, flushes, compactions, and restarts, the engine matches a
reference `BTreeMap`.

```rust
let mut db = lsm_store::Engine::open("./data")?;
db.put(b"name", b"Srijan")?;
db.put(b"language", b"Rust")?;
assert_eq!(db.get(b"name")?, Some(b"Srijan".to_vec()));
db.delete(b"name")?;
assert_eq!(db.get(b"name")?, None);
db.flush()?;
db.close()?;

let db = lsm_store::Engine::open("./data")?;
assert_eq!(db.get(b"name")?, None);
assert_eq!(db.get(b"language")?, Some(b"Rust".to_vec()));
```

## Features

- Durable `put` / `delete` via a checksummed WAL (`fsync` before apply)
- Crash recovery that ignores a torn tail and rejects mid-log corruption
- Block-based SSTables with a binary-searched index
- Per-table Bloom filters to skip negative lookups
- LRU block cache shared across readers
- Leveled compaction (L0 overlapping, L1+ non-overlapping)
- Tombstone GC only when no older overlapping value can remain
- Background compaction worker (`std::thread` + channels)
- Ordered range scans across every live layer
- Fault-injection hooks for WAL/SSTable/manifest failures
- Repeatable workload generator and Criterion microbenchmarks

## Architecture

```text
                    put/delete
                        |
                        v
                 +-------------+
                 |  WAL (sync) |
                 +-------------+
                        |
                        v
                 +-------------+     flush      +-----------+
                 |  MemTable   | -------------> |  SSTable  |
                 |  (BTreeMap) |                |  (L0)     |
                 +-------------+                +-----------+
                        ^                             |
                   get / scan                         v
                        |                      compaction
                 +-------------+                      |
                 |   Levels    | <--------------------+
                 | L0 / L1 / … |
                 +-------------+
                        ^
                        |
                   MANIFEST
```

### Write path

1. Reject empty or oversized keys/values.
2. Encode the mutation as a checksummed record.
3. Append it to `wal.log` and `fsync`.
4. Apply it to the memtable (value or tombstone).
5. If the memtable is past `memtable_size_bytes`, flush:
   - write a temporary SSTable, sync, rename
   - record the table in the manifest
   - truncate the WAL
   - request compaction when L0 hits its trigger

A write is never applied in memory until the WAL append and sync succeed.

### Read path

1. Mutable memtable.
2. Immutable memtable (present only during a flush).
3. L0 tables, newest first.
4. L1, L2, … — skip a table when the key is outside its range or the Bloom
   filter says it is absent; otherwise search the block index, then the
   cached data block.

The first value or tombstone wins. A miss in every layer returns `None`.

### WAL format

Little-endian, one record after another:

```text
checksum (u32 CRC32) | type (u8) | key_len (u32) | value_len (u32) | key | value
```

Type `1` is put, type `2` is delete (value length 0). A torn record at the
end of the file is ignored; a complete record with a bad checksum is an
error.

### SSTable format

```text
data block 0
data block 1
...
index block
bloom filter
footer (48 bytes)
```

Each data block is `count | entries | crc32`. The index stores the first and
last key of every block plus its file offset so `get` can binary-search.
The footer points at the index and Bloom filter and carries its own
checksum and the `LSM1` magic. Files are written as `NNNNNN.sst.tmp` and
renamed into place only after a successful sync.

### Manifest design

`MANIFEST` is an append-only log of:

- `AddTable { id, level }`
- `RemoveTable { id }`
- `SetNextTableId { id }`

Every record is checksummed and the file is synced after each update.
Recovery replays the log; a truncated final record is dropped. After a
crash the engine reopens in either the old or the new valid state — never
a half-applied one.

### Compaction strategy

- Flushes land in L0 (ranges may overlap).
- When L0 reaches `l0_compaction_trigger`, all L0 tables plus overlapping
  L1 tables are merged into L1.
- Lower levels compact into the next level when they exceed
  `memtable_size * l0_trigger * multiplier^(level-1)`.
- Newest version of a key wins. Tombstones are kept unless the merge
  includes every older overlapping table or the output is the last level.
- Source files are deleted only after the new table is in the manifest.

### Crash-recovery guarantees

- A successful `put`/`delete` (`Ok`) is recovered after a restart.
- A failed append or sync is not visible after restart.
- A torn WAL tail is skipped; mid-file corruption fails open.
- An SSTable written but not renamed is ignored (`.tmp`).
- An SSTable renamed but not yet in the manifest is ignored; WAL replay
  still has those keys.
- A manifest update that finished is the source of truth for live tables.

## Design decisions

| Question | Choice |
|---|---|
| Max key size | 64 KiB (`MAX_KEY_SIZE`) |
| Max value size | 64 MiB (`MAX_VALUE_SIZE`) |
| Empty keys | Rejected (`Error::EmptyKey`) |
| Empty values | Allowed; distinct from a delete |
| Delete missing key | Succeeds; records a tombstone |
| Durability of `put` | WAL append + `fsync` before apply |
| `get` return type | Owned `Vec<u8>` |

## How to run tests

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo test --release
```

## How to run benchmarks

Microbenchmarks (Criterion):

```bash
cargo bench
```

End-to-end workloads (release only):

```bash
cargo run --release --bin workload -- --workload sequential --ops 20000
cargo run --release --bin workload -- --workload random --ops 20000
cargo run --release --bin workload -- --workload read-heavy --ops 20000
cargo run --release --bin workload -- --workload write-heavy --ops 20000
cargo run --release --bin workload -- --workload negative --ops 20000
cargo run --release --bin workload -- --workload overwrite --ops 20000
```

Do not benchmark debug builds. Methodology and recorded numbers live in
[`docs/BENCHMARKS.md`](docs/BENCHMARKS.md). Profiling notes are in
[`docs/PROFILING.md`](docs/PROFILING.md).

## Known limitations

- Single process, single writer. There is no multi-process locking.
- No transactions, snapshots, or replication.
- Compaction is size-tiered by level, not a full RocksDB-style picker.
- Directory `fsync` is best-effort on Windows.
- The block cache is a simple mutex-protected LRU, not sharded.

## Future improvements

- Prefix Bloom filters and partitioned indexes
- Separate WAL recycling instead of truncate-in-place
- Rate-limited compaction and write stalls
- Compression of data blocks
- A proper allocator-aware arena for the memtable

## Progress

**Phase 1 — In-memory storage**
- [x] Stage 1 — Memtable
- [x] Stage 2 — Public engine API

**Phase 2 — Durability**
- [x] Stage 3 — Binary record format
- [x] Stage 4 — Write-ahead log
- [x] Stage 5 — Crash recovery

**Phase 3 — Immutable disk storage**
- [x] Stage 6 — SSTable flush
- [x] Stage 7 — Read path

**Phase 4 — Metadata & crash consistency**
- [x] Stage 8 — Manifest

**Phase 5 — Compaction**
- [x] Stage 9 — Synchronous compaction
- [x] Stage 10 — Background compaction

**Phase 6 — LSM-tree structure**
- [x] Stage 11 — Levels
- [x] Stage 12 — Tombstone garbage collection

**Phase 7 — Performance**
- [x] Stage 13 — Bloom filters
- [x] Stage 14 — Block-based SSTables & cache
- [x] Stage 15 — Range scans

**Phase 8 — Benchmarking & optimization**
- [x] Stage 16 — Workload generator
- [x] Stage 17 — Criterion benchmarks
- [x] Stage 18 — Profiling

**Phase 9 — Reliability & polish**
- [x] Stage 19 — Fault injection
- [x] Stage 20 — Documentation & final repository

The stage plan is in [`docs/stage-plan.pdf`](docs/stage-plan.pdf).
