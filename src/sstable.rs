//! Stages 6–7 — sorted string tables (planned).
//!
//! An SSTable is a sorted, immutable on-disk file produced by flushing a
//! memtable. It carries a data region, an index mapping keys to file offsets,
//! and a footer, and forms the persistent layers the read path searches from
//! newest to oldest.
