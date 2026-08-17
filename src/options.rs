//! Tunable engine options.
//!
//! Defaults are conservative enough for tests and small databases. Production
//! workloads should raise [`Options::memtable_size_bytes`] and the cache size.

/// Configuration for an [`crate::Engine`].
#[derive(Debug, Clone)]
pub struct Options {
    /// Flush the memtable once it holds roughly this many key+value bytes.
    pub memtable_size_bytes: usize,
    /// Compact L0 once it contains at least this many tables.
    pub l0_compaction_trigger: usize,
    /// Each level after L0 may grow by this factor over the previous level.
    pub level_size_multiplier: usize,
    /// Maximum number of levels, including L0.
    pub max_levels: usize,
    /// Target false-positive rate for per-table Bloom filters.
    pub bloom_false_positive_rate: f64,
    /// Uncompressed data-block size target, in bytes.
    pub block_size: usize,
    /// Maximum decoded block-cache size, in bytes.
    pub cache_size_bytes: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            memtable_size_bytes: 4 * 1024 * 1024,
            l0_compaction_trigger: 4,
            level_size_multiplier: 10,
            max_levels: 7,
            bloom_false_positive_rate: 0.01,
            block_size: 4 * 1024,
            cache_size_bytes: 8 * 1024 * 1024,
        }
    }
}

impl Options {
    /// Options that keep tests small and deterministic.
    pub fn for_test() -> Self {
        Self {
            memtable_size_bytes: 2 * 1024,
            l0_compaction_trigger: 2,
            level_size_multiplier: 2,
            max_levels: 4,
            bloom_false_positive_rate: 0.01,
            block_size: 256,
            cache_size_bytes: 16 * 1024,
        }
    }
}
