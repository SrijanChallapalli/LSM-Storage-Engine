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
//! - **Durability of `put`:** durable. A `put` (or `delete`) is appended to the
//!   write-ahead log and `fsync`ed before it is applied in memory, so once the
//!   call returns `Ok` the write survives a crash and is recovered on the next
//!   [`open`](Engine::open).
//! - **`get` return type:** owned `Vec<u8>`. Returning borrowed bytes would tie
//!   the value's lifetime to internal state that later stages mutate (flushes,
//!   compaction), so the engine hands back an owned copy.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::memtable::{Entry, MemTable};
use crate::record::Operation;
use crate::wal::Wal;

/// Name of the write-ahead log file inside the database directory.
const WAL_FILE_NAME: &str = "wal.log";

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
    /// Write-ahead log that makes writes durable and recoverable.
    wal: Wal,
    /// In-memory store of the newest writes.
    memtable: MemTable,
}

impl Engine {
    /// Opens (creating if necessary) the database rooted at `path`.
    ///
    /// The directory is created if it does not exist, and the write-ahead log is
    /// replayed to rebuild the last committed state, so a database reopened after
    /// a crash recovers every acknowledged write.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        fs::create_dir_all(&path)?;

        let mut wal = Wal::open(path.join(WAL_FILE_NAME))?;
        let mut memtable = MemTable::new();
        for operation in wal.replay()? {
            Self::apply(&mut memtable, operation);
        }

        Ok(Self {
            path,
            wal,
            memtable,
        })
    }

    /// The directory this database lives in.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stores `value` under `key`, replacing any existing value.
    ///
    /// The write is logged and synced before it becomes visible, so it is
    /// durable once this returns `Ok`. Returns [`Error::EmptyKey`],
    /// [`Error::KeyTooLarge`], or [`Error::ValueTooLarge`] for invalid input.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        Self::validate_key(key)?;
        Self::validate_value(value)?;
        self.write(Operation::Put {
            key: key.to_vec(),
            value: value.to_vec(),
        })
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
    ///
    /// Like [`put`](Self::put), the deletion is logged and synced before it
    /// takes effect.
    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        Self::validate_key(key)?;
        self.write(Operation::Delete { key: key.to_vec() })
    }

    /// Ensures durable state is on disk.
    ///
    /// Each write is already synced individually, so today this just syncs the
    /// log. Stage 6 will additionally flush the memtable into an SSTable and
    /// rotate the log here.
    pub fn flush(&mut self) -> Result<()> {
        self.wal.sync()
    }

    /// Closes the database, consuming the handle after syncing the log.
    pub fn close(mut self) -> Result<()> {
        self.wal.sync()
    }

    /// Logs an operation durably, then applies it to the memtable.
    ///
    /// The order is deliberate: the record is appended and `fsync`ed before the
    /// memtable is touched, so a visible write is always already on disk and a
    /// failed append leaves in-memory state untouched.
    fn write(&mut self, operation: Operation) -> Result<()> {
        self.wal.append(&operation)?;
        self.wal.sync()?;
        Self::apply(&mut self.memtable, operation);
        Ok(())
    }

    /// Applies a recovered or freshly logged operation to the memtable.
    fn apply(memtable: &mut MemTable, operation: Operation) {
        match operation {
            Operation::Put { key, value } => memtable.put(key, value),
            Operation::Delete { key } => memtable.delete(key),
        }
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
            std::fs::create_dir_all(&path).unwrap();
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

    #[test]
    fn writes_survive_reopen_without_clean_shutdown() {
        let dir = TestDir::new();

        // Write, then drop the engine WITHOUT calling close() — a stand-in for
        // the process dying after a successful write.
        {
            let mut db = Engine::open(dir.path()).unwrap();
            db.put(b"a", b"1").unwrap();
            db.put(b"b", b"2").unwrap();
            db.delete(b"a").unwrap();
        }

        // Reopening replays the WAL and recovers the exact last state.
        let db = Engine::open(dir.path()).unwrap();
        assert_eq!(db.get(b"a").unwrap(), None); // deletion survived
        assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec())); // value survived
    }

    #[test]
    fn overwrites_recover_the_latest_value() {
        let dir = TestDir::new();
        {
            let mut db = Engine::open(dir.path()).unwrap();
            db.put(b"k", b"old").unwrap();
            db.put(b"k", b"new").unwrap();
        }
        let db = Engine::open(dir.path()).unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(b"new".to_vec()));
    }
}
