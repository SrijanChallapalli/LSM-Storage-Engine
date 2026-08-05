//! Stage 2 — the public [`Engine`] API.
//!
//! This is the surface users of the database call. It hides every internal
//! detail — the memtable now, and the WAL, SSTables, manifest, and compaction
//! later — behind a small key/value interface.
//!
//! ## Design decisions
//!
//! - **Maximum key size:** [`MAX_KEY_SIZE`] (64 KiB).
//! - **Maximum value size:** [`MAX_VALUE_SIZE`] (64 MiB).
//! - **Empty keys:** not allowed; they return [`Error::EmptyKey`].
//! - **Empty values:** allowed. An empty value is a real, retrievable value and
//!   is distinct from a deletion (a tombstone), so `get` returns `Some(vec![])`
//!   for it rather than `None`.
//! - **Deleting a missing key:** succeeds and is a no-op from the caller's point
//!   of view — `get` afterwards returns `None`. (Internally it records a
//!   tombstone, which matters once lower storage layers exist.)
//! - **Durability of `put`:** *none yet.* At this stage writes live only in the
//!   in-memory memtable and are lost when the process exits. The write-ahead log
//!   in Stage 4 is what will make `put` durable.
//! - **`get` return type:** owned `Vec<u8>`. Returning borrowed bytes would tie
//!   the value's lifetime to internal state that later stages mutate (flushes,
//!   compaction), so the engine hands back an owned copy.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::memtable::{Entry, MemTable};

/// Maximum allowed key size, in bytes (64 KiB).
pub const MAX_KEY_SIZE: usize = 64 * 1024;

/// Maximum allowed value size, in bytes (64 MiB).
pub const MAX_VALUE_SIZE: usize = 64 * 1024 * 1024;

/// An embedded key/value database.
///
/// See the [module documentation](self) for the guarantees each operation makes.
pub struct Engine {
    /// Directory that holds this database's on-disk files.
    path: PathBuf,
    /// In-memory store of the newest writes.
    memtable: MemTable,
}

impl Engine {
    /// Opens (creating if necessary) the database rooted at `path`.
    ///
    /// The directory is created if it does not exist. Later stages will replay a
    /// write-ahead log here to recover the last committed state; for now a fresh
    /// engine always starts empty.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        fs::create_dir_all(&path)?;
        Ok(Self {
            path,
            memtable: MemTable::new(),
        })
    }

    /// The directory this database lives in.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stores `value` under `key`, replacing any existing value.
    ///
    /// Returns [`Error::EmptyKey`], [`Error::KeyTooLarge`], or
    /// [`Error::ValueTooLarge`] for invalid input.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        Self::validate_key(key)?;
        Self::validate_value(value)?;
        self.memtable.put(key.to_vec(), value.to_vec());
        Ok(())
    }

    /// Returns the value stored under `key`, or `None` if the key is absent or
    /// has been deleted.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Self::validate_key(key)?;
        match self.memtable.get(key) {
            Some(Entry::Value(value)) => Ok(Some(value.clone())),
            Some(Entry::Tombstone) | None => Ok(None),
        }
    }

    /// Deletes `key`. Deleting a key that is not present succeeds.
    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        Self::validate_key(key)?;
        self.memtable.delete(key.to_vec());
        Ok(())
    }

    /// Persists in-memory writes to disk.
    ///
    /// A no-op today — there is no on-disk storage yet. Stage 6 will flush the
    /// memtable into an SSTable here.
    pub fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    /// Closes the database, consuming the handle.
    ///
    /// From Stage 4 onward this will flush and sync pending state before
    /// returning; today it simply drops the in-memory engine.
    pub fn close(self) -> Result<()> {
        Ok(())
    }

    fn validate_key(key: &[u8]) -> Result<()> {
        if key.is_empty() {
            return Err(Error::EmptyKey);
        }
        if key.len() > MAX_KEY_SIZE {
            return Err(Error::KeyTooLarge);
        }
        Ok(())
    }

    fn validate_value(value: &[u8]) -> Result<()> {
        if value.len() > MAX_VALUE_SIZE {
            return Err(Error::ValueTooLarge);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A scratch directory that deletes itself when dropped.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "lsm-store-test-{}-{nanos}-{unique}",
                std::process::id()
            ));
            TestDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn engine_can_be_created() {
        let dir = TestDir::new();
        let engine = Engine::open(dir.path()).unwrap();
        assert_eq!(engine.path(), dir.path());
        assert!(dir.path().is_dir());
    }

    #[test]
    fn put_and_get_work() {
        let dir = TestDir::new();
        let mut engine = Engine::open(dir.path()).unwrap();
        engine.put(b"name", b"Srijan").unwrap();
        assert_eq!(engine.get(b"name").unwrap(), Some(b"Srijan".to_vec()));
        assert_eq!(engine.get(b"missing").unwrap(), None);
    }

    #[test]
    fn delete_works() {
        let dir = TestDir::new();
        let mut engine = Engine::open(dir.path()).unwrap();
        engine.put(b"name", b"Srijan").unwrap();
        engine.delete(b"name").unwrap();
        assert_eq!(engine.get(b"name").unwrap(), None);
    }

    #[test]
    fn deleting_a_missing_key_succeeds() {
        let dir = TestDir::new();
        let mut engine = Engine::open(dir.path()).unwrap();
        engine.delete(b"ghost").unwrap();
        assert_eq!(engine.get(b"ghost").unwrap(), None);
    }

    #[test]
    fn empty_values_work() {
        let dir = TestDir::new();
        let mut engine = Engine::open(dir.path()).unwrap();
        engine.put(b"k", b"").unwrap();
        // An empty value is a value, not a deletion.
        assert_eq!(engine.get(b"k").unwrap(), Some(Vec::new()));
    }

    #[test]
    fn invalid_inputs_return_errors() {
        let dir = TestDir::new();
        let mut engine = Engine::open(dir.path()).unwrap();

        assert!(matches!(engine.put(b"", b"v"), Err(Error::EmptyKey)));
        assert!(matches!(engine.get(b""), Err(Error::EmptyKey)));
        assert!(matches!(engine.delete(b""), Err(Error::EmptyKey)));

        let big_key = vec![0u8; MAX_KEY_SIZE + 1];
        assert!(matches!(
            engine.put(&big_key, b"v"),
            Err(Error::KeyTooLarge)
        ));

        let big_value = vec![0u8; MAX_VALUE_SIZE + 1];
        assert!(matches!(
            engine.put(b"k", &big_value),
            Err(Error::ValueTooLarge)
        ));
    }

    #[test]
    fn user_can_interact_without_internal_knowledge() {
        let dir = TestDir::new();

        // A user drives the database entirely through the public API — no
        // mention of memtables, WALs, or SSTables.
        let mut db = Engine::open(dir.path()).unwrap();
        db.put(b"name", b"Srijan").unwrap();
        db.put(b"language", b"Rust").unwrap();
        assert_eq!(db.get(b"name").unwrap(), Some(b"Srijan".to_vec()));

        db.delete(b"name").unwrap();
        assert_eq!(db.get(b"name").unwrap(), None);
        assert_eq!(db.get(b"language").unwrap(), Some(b"Rust".to_vec()));

        db.flush().unwrap();
        db.close().unwrap();
    }
}
