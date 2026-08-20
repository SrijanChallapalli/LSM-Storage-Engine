//! Stage 4 — the write-ahead log.
//!
//! The WAL is what makes a write survive a crash. Every mutation is encoded with
//! the Stage 3 [`record`](crate::record) format, appended to the log, and synced
//! to disk *before* it is applied to the in-memory memtable. On startup the log
//! is replayed to rebuild the exact last committed state.
//!
//! ## Durable write order
//!
//! 1. Encode the [`Operation`].
//! 2. Append the record to the log.
//! 3. `fsync` the log.
//! 4. Apply the operation to the memtable.
//! 5. Report success.
//!
//! The memtable is never updated before the append and sync succeed, so a
//! visible write is always already on disk.
//!
//! ## Recovery policy
//!
//! Records are read from oldest to newest. A crash can leave a torn record at
//! the very end of the log (a partial header or a payload cut short by the
//! process dying mid-write); that trailing fragment is ignored, since it was
//! never acknowledged. A *complete* record whose checksum fails is treated as
//! corruption and surfaced as an error rather than silently skipped.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::crc::read_u32_le;
use crate::engine::{MAX_KEY_SIZE, MAX_VALUE_SIZE};
use crate::error::{Error, Result};
use crate::record::{
    decode, encode, Operation, KEY_LEN_OFFSET, RECORD_HEADER_LEN_PUB, VALUE_LEN_OFFSET,
};

/// An append-only write-ahead log backed by a single file.
pub struct Wal {
    file: File,
    path: PathBuf,
}

impl Wal {
    /// Opens the log at `path`, creating it if it does not exist.
    ///
    /// The file is opened for reading and writing so the same handle can both
    /// append new records and replay existing ones.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        Ok(Self { file, path })
    }

    /// The path this log is stored at.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Encodes `operation` and appends it to the end of the log.
    ///
    /// Returns the byte offset at which the record begins. Encoding failures
    /// (for example an oversized key) are reported before anything is written,
    /// so a failed append never leaves a partial record behind.
    pub fn append(&mut self, operation: &Operation) -> Result<u64> {
        if failpoints::should_fail(failpoints::Kind::WalAppend) {
            return Err(Error::Io(std::io::Error::other(
                "injected WAL append failure",
            )));
        }
        let bytes = encode(operation)?;
        let offset = self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&bytes)?;
        Ok(offset)
    }

    /// Flushes and `fsync`s the log so far to durable storage.
    pub fn sync(&mut self) -> Result<()> {
        if failpoints::should_fail(failpoints::Kind::WalSync) {
            return Err(Error::Io(std::io::Error::other(
                "injected WAL sync failure",
            )));
        }
        self.file.flush()?;
        self.file.sync_all()?;
        Ok(())
    }

    /// Replays the log from the beginning, returning every committed operation
    /// in the order it was written.
    pub fn replay(&mut self) -> Result<Vec<Operation>> {
        self.iter()?.collect()
    }

    /// Returns a streaming iterator over committed operations.
    pub fn iter(&mut self) -> Result<WalIterator> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut contents = Vec::new();
        self.file.read_to_end(&mut contents)?;
        Ok(WalIterator::from_bytes(contents))
    }

    /// Empties the log, producing a zero-length file on disk.
    pub fn truncate(&mut self) -> Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.sync_all()?;
        Ok(())
    }

    /// The current size of the log in bytes.
    pub fn len(&self) -> Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// Whether the log currently holds no bytes.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
}

/// Streaming parser over a WAL buffer.
///
/// Each item is one committed [`Operation`], or an error if a complete record
/// is corrupt. A torn tail yields no further items rather than an error.
pub struct WalIterator {
    data: Vec<u8>,
    pos: usize,
}

impl WalIterator {
    /// Parse from an owned buffer. Used by [`Wal::iter`].
    fn from_bytes(data: Vec<u8>) -> Self {
        Self { data, pos: 0 }
    }
}

impl Iterator for WalIterator {
    type Item = Result<Operation>;

    fn next(&mut self) -> Option<Self::Item> {
        let data = &self.data[self.pos..];
        if data.is_empty() {
            return None;
        }
        if data.len() < RECORD_HEADER_LEN_PUB {
            // Torn final header: ignore the unacknowledged fragment.
            self.pos = self.data.len();
            return None;
        }

        let key_len = read_u32_le(&data[KEY_LEN_OFFSET..KEY_LEN_OFFSET + 4]) as usize;
        let value_len = read_u32_le(&data[VALUE_LEN_OFFSET..VALUE_LEN_OFFSET + 4]) as usize;

        if key_len > MAX_KEY_SIZE || value_len > MAX_VALUE_SIZE {
            self.pos = self.data.len();
            return Some(Err(Error::CorruptedRecord));
        }

        let record_len = RECORD_HEADER_LEN_PUB + key_len + value_len;
        if data.len() < record_len {
            // Torn final payload: ignore the unacknowledged fragment.
            self.pos = self.data.len();
            return None;
        }

        let record = &data[..record_len];
        self.pos += record_len;
        Some(decode(record))
    }
}

/// Test-only fault injection. Production builds compile these to no-ops.
pub mod failpoints {
    use std::cell::Cell;

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum Kind {
        WalAppend,
        WalSync,
        SsTableWrite,
        SsTableSync,
        ManifestWrite,
        Rename,
    }

    thread_local! {
        static NEXT: Cell<Option<Kind>> = const { Cell::new(None) };
    }

    pub fn inject(kind: Kind) {
        NEXT.with(|c| c.set(Some(kind)));
    }

    pub fn clear() {
        NEXT.with(|c| c.set(None));
    }

    pub fn should_fail(kind: Kind) -> bool {
        NEXT.with(|c| {
            if c.get() == Some(kind) {
                c.set(None);
                true
            } else {
                false
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

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
                "lsm-store-wal-{}-{nanos}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            TestDir(path)
        }

        fn wal_path(&self) -> PathBuf {
            self.0.join("wal.log")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn put(key: &[u8], value: &[u8]) -> Operation {
        Operation::Put {
            key: key.to_vec(),
            value: value.to_vec(),
        }
    }

    #[test]
    fn wal_file_is_created() {
        let dir = TestDir::new();
        let wal = Wal::open(dir.wal_path()).unwrap();
        assert!(wal.path().is_file());
        assert!(wal.is_empty().unwrap());
    }

    #[test]
    fn multiple_records_append_and_replay_in_order() {
        let dir = TestDir::new();
        let mut wal = Wal::open(dir.wal_path()).unwrap();
        wal.append(&put(b"a", b"1")).unwrap();
        wal.append(&put(b"b", b"2")).unwrap();
        wal.append(&Operation::Delete { key: b"a".to_vec() })
            .unwrap();
        wal.sync().unwrap();

        let replayed = wal.replay().unwrap();
        assert_eq!(
            replayed,
            vec![
                put(b"a", b"1"),
                put(b"b", b"2"),
                Operation::Delete { key: b"a".to_vec() },
            ]
        );
    }

    #[test]
    fn wal_survives_reopening() {
        let dir = TestDir::new();
        {
            let mut wal = Wal::open(dir.wal_path()).unwrap();
            wal.append(&put(b"k", b"v")).unwrap();
            wal.sync().unwrap();
        }
        let mut wal = Wal::open(dir.wal_path()).unwrap();
        assert_eq!(wal.replay().unwrap(), vec![put(b"k", b"v")]);
    }

    #[test]
    fn truncate_produces_an_empty_log() {
        let dir = TestDir::new();
        let mut wal = Wal::open(dir.wal_path()).unwrap();
        wal.append(&put(b"k", b"v")).unwrap();
        wal.sync().unwrap();
        assert!(!wal.is_empty().unwrap());

        wal.truncate().unwrap();
        assert!(wal.is_empty().unwrap());
        assert!(wal.replay().unwrap().is_empty());
    }

    #[test]
    fn truncated_final_record_is_ignored() {
        let dir = TestDir::new();
        let good_len = {
            let mut wal = Wal::open(dir.wal_path()).unwrap();
            wal.append(&put(b"a", b"1")).unwrap();
            let len = wal.len().unwrap();
            wal.append(&put(b"b", b"22222")).unwrap();
            wal.sync().unwrap();
            len
        };

        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.wal_path())
            .unwrap();
        file.set_len(good_len + 4).unwrap();
        drop(file);

        let mut wal = Wal::open(dir.wal_path()).unwrap();
        assert_eq!(wal.replay().unwrap(), vec![put(b"a", b"1")]);
    }

    #[test]
    fn corrupted_record_is_detected() {
        let dir = TestDir::new();
        {
            let mut wal = Wal::open(dir.wal_path()).unwrap();
            wal.append(&put(b"a", b"1")).unwrap();
            wal.append(&put(b"b", b"2")).unwrap();
            wal.sync().unwrap();
        }

        let mut bytes = std::fs::read(dir.wal_path()).unwrap();
        bytes[RECORD_HEADER_LEN_PUB] ^= 0xff;
        std::fs::write(dir.wal_path(), &bytes).unwrap();

        let mut wal = Wal::open(dir.wal_path()).unwrap();
        assert!(matches!(
            wal.replay(),
            Err(Error::InvalidChecksum) | Err(Error::CorruptedRecord)
        ));
    }

    #[test]
    fn failed_append_does_not_grow_the_log() {
        let dir = TestDir::new();
        let mut wal = Wal::open(dir.wal_path()).unwrap();
        let bad = Operation::Put {
            key: vec![0u8; MAX_KEY_SIZE + 1],
            value: Vec::new(),
        };
        assert!(wal.append(&bad).is_err());
        assert_eq!(wal.len().unwrap(), 0);
    }

    #[test]
    fn iterator_replays_hundreds_of_records() {
        let dir = TestDir::new();
        let mut wal = Wal::open(dir.wal_path()).unwrap();
        for i in 0..250u32 {
            wal.append(&put(&i.to_le_bytes(), b"v")).unwrap();
        }
        wal.sync().unwrap();
        let replayed = wal.replay().unwrap();
        assert_eq!(replayed.len(), 250);
        match &replayed[249] {
            Operation::Put { key, .. } => assert_eq!(key, &249u32.to_le_bytes()),
            Operation::Delete { .. } => panic!("expected a put"),
        }
    }
}
