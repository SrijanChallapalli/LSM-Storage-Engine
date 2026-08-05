//! Stage 2 — the public `Engine` API (planned).
//!
//! This module will expose the database interface users actually call
//! (`open`, `put`, `get`, `delete`, `flush`, `close`) and hide every internal
//! detail — memtable, WAL, SSTables, manifest, and compaction — behind it.
