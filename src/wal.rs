//! Stages 3–5 — the write-ahead log (planned).
//!
//! The WAL makes writes recoverable after a crash. Operations are encoded to a
//! binary record format, appended, and synced *before* the memtable is updated,
//! so a replay on startup can rebuild the exact last committed state.
