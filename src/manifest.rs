//! Stage 8 — the manifest.
//!
//! The manifest is a crash-consistent log of which SSTables make up the
//! database. It records the next table id, the active tables, the level each
//! table belongs to, and (implicitly) their newest-to-oldest order.
//!
//! Records are appended and the file is synced after every change. Recovery
//! replays the log from the start. A torn final record is ignored; a complete
//! but corrupt record is an error.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::crc::{crc32, read_u32_le, read_u64_le};
use crate::error::{Error, Result};
use crate::sstable::sync_dir;
use crate::wal::failpoints;

const TYPE_ADD: u8 = 1;
const TYPE_REMOVE: u8 = 2;
const TYPE_NEXT_ID: u8 = 3;
const RECORD_LEN: usize = 4 + 1 + 8 + 4; // checksum + type + id + level

/// A single manifest mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestRecord {
    AddTable { id: u64, level: u32 },
    RemoveTable { id: u64 },
    SetNextTableId { id: u64 },
}

/// Persistent metadata for the set of live SSTables.
pub struct Manifest {
    file: File,
    directory: PathBuf,
    next_table_id: u64,
    /// Active tables newest-first within each level, stored as `(id, level)`.
    tables: Vec<(u64, u32)>,
}

impl Manifest {
    /// Opens (or creates) the manifest in `directory`.
    pub fn open(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = directory.as_ref().to_path_buf();
        std::fs::create_dir_all(&directory)?;
        let path = directory.join("MANIFEST");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let mut manifest = Self {
            file,
            directory,
            next_table_id: 1,
            tables: Vec::new(),
        };
        manifest.recover()?;
        Ok(manifest)
    }

    /// Replays the log and rebuilds in-memory state.
    pub fn recover(&mut self) -> Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        self.file.read_to_end(&mut bytes)?;

        self.next_table_id = 1;
        self.tables.clear();

        let mut pos = 0;
        while pos + RECORD_LEN <= bytes.len() {
            let record = &bytes[pos..pos + RECORD_LEN];
            pos += RECORD_LEN;
            let checksum = read_u32_le(&record[0..4]);
            if crc32(&record[4..]) != checksum {
                return Err(Error::InvalidManifest);
            }
            match decode_body(&record[4..])? {
                ManifestRecord::AddTable { id, level } => {
                    if self.tables.iter().any(|(existing, _)| *existing == id) {
                        return Err(Error::InvalidManifest);
                    }
                    self.tables.push((id, level));
                }
                ManifestRecord::RemoveTable { id } => {
                    self.tables.retain(|(existing, _)| *existing != id);
                }
                ManifestRecord::SetNextTableId { id } => {
                    self.next_table_id = id;
                }
            }
        }
        // A trailing fragment shorter than a full record is a torn write.
        Ok(())
    }

    /// Records that `id` now belongs to `level`.
    pub fn add_table(&mut self, id: u64, level: u32) -> Result<()> {
        if self.tables.iter().any(|(existing, _)| *existing == id) {
            return Err(Error::InvalidManifest);
        }
        self.append(&ManifestRecord::AddTable { id, level })?;
        self.tables.push((id, level));
        Ok(())
    }

    /// Drops `ids` from the live set.
    pub fn remove_tables(&mut self, ids: &[u64]) -> Result<()> {
        for id in ids {
            self.append(&ManifestRecord::RemoveTable { id: *id })?;
            self.tables.retain(|(existing, _)| existing != id);
        }
        Ok(())
    }

    /// Allocates and persists the next table id.
    pub fn next_table_id(&mut self) -> Result<u64> {
        let id = self.next_table_id;
        self.next_table_id += 1;
        self.append(&ManifestRecord::SetNextTableId {
            id: self.next_table_id,
        })?;
        Ok(id)
    }

    /// Live table ids in the order they were added (newest last for L0 flushes).
    pub fn active_tables(&self) -> &[(u64, u32)] {
        &self.tables
    }

    pub fn current_next_id(&self) -> u64 {
        self.next_table_id
    }

    fn append(&mut self, record: &ManifestRecord) -> Result<()> {
        if failpoints::should_fail(failpoints::Kind::ManifestWrite) {
            return Err(Error::Io(std::io::Error::other(
                "injected manifest write failure",
            )));
        }
        let bytes = encode_record(record);
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&bytes)?;
        self.file.flush()?;
        self.file.sync_all()?;
        sync_dir(&self.directory)?;
        Ok(())
    }
}

fn encode_record(record: &ManifestRecord) -> Vec<u8> {
    let (kind, id, level) = match record {
        ManifestRecord::AddTable { id, level } => (TYPE_ADD, *id, *level),
        ManifestRecord::RemoveTable { id } => (TYPE_REMOVE, *id, 0),
        ManifestRecord::SetNextTableId { id } => (TYPE_NEXT_ID, *id, 0),
    };
    let mut body = Vec::with_capacity(13);
    body.push(kind);
    body.extend_from_slice(&id.to_le_bytes());
    body.extend_from_slice(&level.to_le_bytes());
    let mut out = crc32(&body).to_le_bytes().to_vec();
    out.extend_from_slice(&body);
    out
}

fn decode_body(body: &[u8]) -> Result<ManifestRecord> {
    if body.len() != 13 {
        return Err(Error::InvalidManifest);
    }
    let kind = body[0];
    let id = read_u64_le(&body[1..9]);
    let level = read_u32_le(&body[9..13]);
    match kind {
        TYPE_ADD => Ok(ManifestRecord::AddTable { id, level }),
        TYPE_REMOVE => Ok(ManifestRecord::RemoveTable { id }),
        TYPE_NEXT_ID => Ok(ManifestRecord::SetNextTableId { id }),
        _ => Err(Error::InvalidManifest),
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
                "lsm-store-man-{}-{nanos}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            TestDir(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn add_and_remove_tables_round_trip() {
        let dir = TestDir::new();
        {
            let mut manifest = Manifest::open(&dir.0).unwrap();
            let id = manifest.next_table_id().unwrap();
            manifest.add_table(id, 0).unwrap();
            assert_eq!(manifest.active_tables(), &[(id, 0)]);
        }
        let manifest = Manifest::open(&dir.0).unwrap();
        assert_eq!(manifest.active_tables(), &[(1, 0)]);
        assert_eq!(manifest.current_next_id(), 2);
    }

    #[test]
    fn truncated_final_record_is_ignored() {
        let dir = TestDir::new();
        {
            let mut manifest = Manifest::open(&dir.0).unwrap();
            let id = manifest.next_table_id().unwrap();
            manifest.add_table(id, 0).unwrap();
        }
        let path = dir.0.join("MANIFEST");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        std::fs::write(&path, bytes).unwrap();

        let manifest = Manifest::open(&dir.0).unwrap();
        assert_eq!(manifest.active_tables(), &[(1, 0)]);
    }

    #[test]
    fn duplicate_table_ids_are_rejected() {
        let dir = TestDir::new();
        let mut manifest = Manifest::open(&dir.0).unwrap();
        manifest.add_table(7, 0).unwrap();
        assert!(manifest.add_table(7, 0).is_err());
    }
}
