//! Stage 14 — a bounded LRU block cache.
//!
//! Cached values are decoded data blocks, keyed by `(table_id, file_offset)`
//! so two tables never collide. The cache is shared across readers through
//! [`std::sync::Mutex`].

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::memtable::Entry;

/// Identity of a cached data block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockKey {
    pub table_id: u64,
    pub offset: u64,
}

/// One decoded data block: sorted `(key, entry)` pairs.
pub type BlockData = Arc<Vec<(Vec<u8>, Entry)>>;

/// A simple LRU cache of decoded SSTable blocks.
#[derive(Debug)]
pub struct BlockCache {
    map: HashMap<BlockKey, BlockData>,
    order: VecDeque<BlockKey>,
    current_bytes: usize,
    max_bytes: usize,
    pub hits: u64,
    pub misses: u64,
}

impl BlockCache {
    /// Creates a cache that evicts once `max_bytes` of decoded data is held.
    pub fn new(max_bytes: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            current_bytes: 0,
            max_bytes: max_bytes.max(1),
            hits: 0,
            misses: 0,
        }
    }

    /// Returns a previously loaded block, promoting it to most-recently used.
    pub fn get(&mut self, key: BlockKey) -> Option<BlockData> {
        if let Some(block) = self.map.get(&key).cloned() {
            self.hits += 1;
            self.touch(key);
            Some(block)
        } else {
            self.misses += 1;
            None
        }
    }

    /// Inserts `block`, evicting least-recently used entries if needed.
    pub fn insert(&mut self, key: BlockKey, block: BlockData) {
        let size = block_size(&block);
        if let Some(previous) = self.map.insert(key, block) {
            self.current_bytes -= block_size(&previous);
            self.touch(key);
        } else {
            self.order.push_back(key);
        }
        self.current_bytes += size;
        self.evict_to_fit();
    }

    /// Number of blocks currently resident.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the cache holds no blocks.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn touch(&mut self, key: BlockKey) {
        if let Some(idx) = self.order.iter().position(|k| *k == key) {
            self.order.remove(idx);
        }
        self.order.push_back(key);
    }

    fn evict_to_fit(&mut self) {
        while self.current_bytes > self.max_bytes {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(block) = self.map.remove(&oldest) {
                self.current_bytes = self.current_bytes.saturating_sub(block_size(&block));
            }
        }
    }
}

fn block_size(block: &BlockData) -> usize {
    block
        .iter()
        .map(|(k, e)| {
            k.len()
                + match e {
                    Entry::Value(v) => v.len(),
                    Entry::Tombstone => 0,
                }
        })
        .sum::<usize>()
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(key: &[u8], val: &[u8]) -> (Vec<u8>, Entry) {
        (key.to_vec(), Entry::Value(val.to_vec()))
    }

    #[test]
    fn cache_returns_previously_loaded_block() {
        let mut cache = BlockCache::new(1024);
        let key = BlockKey {
            table_id: 1,
            offset: 0,
        };
        let block = Arc::new(vec![value(b"a", b"1")]);
        cache.insert(key, block.clone());
        assert_eq!(cache.get(key).unwrap(), block);
        assert_eq!(cache.hits, 1);
    }

    #[test]
    fn cache_evicts_when_over_capacity() {
        let mut cache = BlockCache::new(8);
        let a = BlockKey {
            table_id: 1,
            offset: 0,
        };
        let b = BlockKey {
            table_id: 1,
            offset: 64,
        };
        cache.insert(a, Arc::new(vec![value(b"aaaa", b"1111")]));
        cache.insert(b, Arc::new(vec![value(b"bbbb", b"2222")]));
        assert!(
            cache.get(a).is_none(),
            "the first block should have been evicted"
        );
        assert!(cache.get(b).is_some());
    }

    #[test]
    fn different_tables_do_not_collide() {
        let mut cache = BlockCache::new(4096);
        let a = BlockKey {
            table_id: 1,
            offset: 0,
        };
        let b = BlockKey {
            table_id: 2,
            offset: 0,
        };
        cache.insert(a, Arc::new(vec![value(b"k", b"one")]));
        cache.insert(b, Arc::new(vec![value(b"k", b"two")]));
        match cache.get(a).unwrap().first() {
            Some((_, Entry::Value(v))) => assert_eq!(v, b"one"),
            _ => panic!("expected table 1's value"),
        }
        match cache.get(b).unwrap().first() {
            Some((_, Entry::Value(v))) => assert_eq!(v, b"two"),
            _ => panic!("expected table 2's value"),
        }
    }
}
