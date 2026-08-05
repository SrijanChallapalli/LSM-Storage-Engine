//! Stages 4–5 — the write-ahead log (planned).
//!
//! The WAL makes writes recoverable after a crash. It builds on the
//! [`record`](crate::record) format from Stage 3: each [`Operation`] is encoded,
//! appended, and synced *before* the memtable is updated, so a replay on startup
//! can rebuild the exact last committed state.
