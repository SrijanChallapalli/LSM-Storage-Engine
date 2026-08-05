//! Stage 8 — the manifest (planned).
//!
//! The manifest is a crash-consistent log recording which SSTables make up the
//! database: the next table id, the active tables and their ordering, and
//! compaction state. Atomic updates to it are what let a crash leave the engine
//! in either the old or the new valid state, never a half-applied one.
