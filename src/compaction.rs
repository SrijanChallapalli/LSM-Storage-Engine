//! Stages 9–12 — compaction (planned).
//!
//! Compaction merges overlapping SSTables into fewer, larger ones: newest value
//! per key wins, output stays sorted, and tombstones are preserved until it is
//! provably safe to drop them. It starts synchronous, then moves to a background
//! worker and a leveled layout.
