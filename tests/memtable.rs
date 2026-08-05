//! Integration tests exercising the memtable through the crate's public API.

use lsm_store::{Entry, MemTable};

#[test]
fn latest_write_wins_across_many_operations() {
    let mut table = MemTable::new();

    for round in 0..100u32 {
        table.put(b"counter".to_vec(), round.to_le_bytes().to_vec());
    }

    let expected = 99u32.to_le_bytes().to_vec();
    assert_eq!(table.get(b"counter"), Some(&Entry::Value(expected)));
    assert_eq!(table.len(), 1);
}

#[test]
fn delete_then_reinsert_is_visible() {
    let mut table = MemTable::new();

    table.put(b"k".to_vec(), b"v1".to_vec());
    table.delete(b"k".to_vec());
    assert_eq!(table.get(b"k"), Some(&Entry::Tombstone));

    table.put(b"k".to_vec(), b"v2".to_vec());
    assert_eq!(table.get(b"k"), Some(&Entry::Value(b"v2".to_vec())));
}

#[test]
fn keys_are_kept_sorted() {
    let mut table = MemTable::new();
    for key in [b"banana", b"apple", b"cherry"] {
        table.put(key.to_vec(), b"1".to_vec());
    }
    assert_eq!(table.len(), 3);
    assert!(table.contains_key(b"apple"));
    assert!(table.contains_key(b"banana"));
    assert!(table.contains_key(b"cherry"));
}
