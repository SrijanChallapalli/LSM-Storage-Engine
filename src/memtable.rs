//! Stage 1 — the memtable.
//!
//! The memtable holds the newest writes in memory before they are flushed to an
//! on-disk SSTable. Keys are kept sorted so that a flush can stream them out in
//! order, which is why we build on top of a [`BTreeMap`].
//!
//! Every logical key maps to an [`Entry`]: either a live value or a
//! [`Tombstone`](Entry::Tombstone) marking a deletion. Deletions are recorded
//! rather than removed so that they can shadow older values living in lower
//! layers of the tree.

use std::collections::BTreeMap;

/// A single logical entry stored for a key.
///
/// A [`Value`](Entry::Value) carries the bytes written by the user. A
/// [`Tombstone`](Entry::Tombstone) records that the key was deleted; it hides any
/// older value for the same key until compaction is able to drop it safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A live value for the key.
    Value(Vec<u8>),
    /// A deletion marker for the key.
    Tombstone,
}

impl Entry {
    /// Number of value bytes this entry contributes to the memtable's size.
    ///
    /// A tombstone carries no value bytes, so it contributes `0`.
    fn value_len(&self) -> usize {
        match self {
            Entry::Value(value) => value.len(),
            Entry::Tombstone => 0,
        }
    }
}

/// An in-memory, sorted collection of the most recent writes.
#[derive(Debug, Default)]
pub struct MemTable {
    entries: BTreeMap<Vec<u8>, Entry>,
    /// Running estimate of the key + value bytes held by the table.
    approximate_size: usize,
}

impl MemTable {
    /// Creates an empty memtable.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a live value for `key`, replacing any existing entry.
    pub fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.insert(key, Entry::Value(value));
    }

    /// Records a deletion for `key` by inserting a tombstone.
    ///
    /// Deleting a key that is not present still inserts a tombstone: lower layers
    /// of the tree may hold an older value that this marker must shadow.
    pub fn delete(&mut self, key: Vec<u8>) {
        self.insert(key, Entry::Tombstone);
    }

    /// Returns the entry stored for `key`, if any.
    ///
    /// A returned [`Entry::Tombstone`] means the key was deleted; callers must
    /// treat that as "absent" rather than falling through to an older layer.
    pub fn get(&self, key: &[u8]) -> Option<&Entry> {
        self.entries.get(key)
    }

    /// Returns `true` if the memtable holds any entry (value *or* tombstone) for
    /// `key`.
    pub fn contains_key(&self, key: &[u8]) -> bool {
        self.entries.contains_key(key)
    }

    /// Number of distinct keys currently tracked, including tombstones.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if the memtable holds no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Approximate number of key and value bytes held in memory.
    ///
    /// This is used to decide when the memtable should be flushed. It counts the
    /// key bytes plus the value bytes of the current entry for each key, so
    /// overwriting a key does not double-count it.
    pub fn approximate_size(&self) -> usize {
        self.approximate_size
    }

    /// Removes every entry and resets the size estimate.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.approximate_size = 0;
    }

    /// Inserts an entry for `key`, keeping [`approximate_size`](Self::approximate_size)
    /// consistent whether the key is new or overwritten.
    fn insert(&mut self, key: Vec<u8>, entry: Entry) {
        let added = key.len() + entry.value_len();
        if let Some(previous) = self.entries.get(&key) {
            // Overwriting an existing key: drop the old value's contribution
            // before adding the new one. The key bytes stay, so only the value
            // length changes.
            self.approximate_size -= previous.value_len();
            self.approximate_size += entry.value_len();
        } else {
            self.approximate_size += added;
        }
        self.entries.insert(key, entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_and_retrieve_a_value() {
        let mut table = MemTable::new();
        table.put(b"name".to_vec(), b"Srijan".to_vec());
        assert_eq!(table.get(b"name"), Some(&Entry::Value(b"Srijan".to_vec())));
    }

    #[test]
    fn missing_key_returns_none() {
        let table = MemTable::new();
        assert_eq!(table.get(b"absent"), None);
    }

    #[test]
    fn newest_write_replaces_an_older_write() {
        let mut table = MemTable::new();
        table.put(b"lang".to_vec(), b"Python".to_vec());
        table.put(b"lang".to_vec(), b"Rust".to_vec());
        assert_eq!(table.get(b"lang"), Some(&Entry::Value(b"Rust".to_vec())));
    }

    #[test]
    fn replacing_a_value_does_not_increase_len() {
        let mut table = MemTable::new();
        table.put(b"lang".to_vec(), b"Python".to_vec());
        table.put(b"lang".to_vec(), b"Rust".to_vec());
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn delete_creates_a_tombstone() {
        let mut table = MemTable::new();
        table.put(b"name".to_vec(), b"Srijan".to_vec());
        table.delete(b"name".to_vec());
        assert_eq!(table.get(b"name"), Some(&Entry::Tombstone));
    }

    #[test]
    fn deleting_a_missing_key_creates_a_tombstone() {
        let mut table = MemTable::new();
        table.delete(b"ghost".to_vec());
        assert_eq!(table.get(b"ghost"), Some(&Entry::Tombstone));
        assert!(table.contains_key(b"ghost"));
    }

    #[test]
    fn tombstones_hide_older_values() {
        let mut table = MemTable::new();
        table.put(b"name".to_vec(), b"Srijan".to_vec());
        table.delete(b"name".to_vec());
        // The tombstone is present and must be interpreted as "absent".
        assert_eq!(table.get(b"name"), Some(&Entry::Tombstone));
    }

    #[test]
    fn approximate_size_is_updated_correctly() {
        let mut table = MemTable::new();
        assert_eq!(table.approximate_size(), 0);

        // 4-byte key + 6-byte value.
        table.put(b"name".to_vec(), b"Srijan".to_vec());
        assert_eq!(table.approximate_size(), 10);

        // Overwrite with a shorter value: key stays, value shrinks 6 -> 4.
        table.put(b"name".to_vec(), b"Srij".to_vec());
        assert_eq!(table.approximate_size(), 8);

        // Deleting replaces the value with a tombstone (0 value bytes), but the
        // 4-byte key remains counted.
        table.delete(b"name".to_vec());
        assert_eq!(table.approximate_size(), 4);
    }

    #[test]
    fn clearing_removes_everything() {
        let mut table = MemTable::new();
        table.put(b"a".to_vec(), b"1".to_vec());
        table.put(b"b".to_vec(), b"2".to_vec());
        table.clear();

        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert_eq!(table.approximate_size(), 0);
        assert_eq!(table.get(b"a"), None);
    }

    #[test]
    fn binary_keys_and_values_round_trip() {
        let mut table = MemTable::new();
        let key = vec![0x00, 0xff, 0x10, 0x00];
        let value = vec![0xde, 0xad, 0xbe, 0xef];
        table.put(key.clone(), value.clone());
        assert_eq!(table.get(&key), Some(&Entry::Value(value)));
    }
}
