//! Stages 6–7 and 14 — sorted string tables.
//!
//! An SSTable is a sorted, immutable file produced by flushing a memtable.
//! The on-disk layout is block oriented:
//!
//! ```text
//! data block 0
//! data block 1
//! ...
//! index block
//! bloom filter
//! footer (48 bytes)
//! ```
//!
//! Each data block stores a run of key/entry pairs plus a CRC32. The index
//! maps the first and last key of a block to its file offset so `get` can
//! binary-search without scanning the file. Temporary files use a `.tmp`
//! suffix and are never treated as completed tables.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::bloom::BloomFilter;
use crate::cache::{BlockCache, BlockData, BlockKey};
use crate::crc::{crc32, read_u32_le, read_u64_le};
use crate::error::{Error, Result};
use crate::memtable::Entry;
use crate::wal::failpoints;

/// On-disk magic number (`LSM1`) stored in the footer.
const MAGIC: u32 = 0x4C53_4D31;
const FOOTER_LEN: usize = 48;
const FORMAT_VERSION: u32 = 1;
const TYPE_VALUE: u8 = 1;
const TYPE_TOMBSTONE: u8 = 2;

/// One index row: the key range of a data block and where that block lives.
#[derive(Debug, Clone)]
struct IndexEntry {
    first_key: Vec<u8>,
    last_key: Vec<u8>,
    offset: u64,
    length: u32,
}

/// A sorted, immutable on-disk table.
#[derive(Debug)]
pub struct SsTable {
    id: u64,
    path: PathBuf,
    index: Vec<IndexEntry>,
    bloom: BloomFilter,
    smallest: Vec<u8>,
    largest: Vec<u8>,
    size_bytes: u64,
    level: usize,
}

impl SsTable {
    /// Writes `entries` (already sorted) into a new table at `path`.
    ///
    /// The file is created under a `.tmp` name, synced, then renamed into
    /// place so a crash never leaves a half-written table visible.
    pub fn create(
        path: impl AsRef<Path>,
        id: u64,
        entries: impl Iterator<Item = (Vec<u8>, Entry)>,
        bloom_fp: f64,
        block_size: usize,
    ) -> Result<Self> {
        let final_path = path.as_ref().to_path_buf();
        let tmp_path = tmp_path(&final_path);

        if failpoints::should_fail(failpoints::Kind::SsTableWrite) {
            return Err(Error::Io(std::io::Error::other(
                "injected SSTable write failure",
            )));
        }

        let mut collected: Vec<(Vec<u8>, Entry)> = entries.collect();
        collected.sort_by(|a, b| a.0.cmp(&b.0));
        let mut bloom = BloomFilter::new(collected.len().max(1), bloom_fp);
        for (key, _) in &collected {
            bloom.insert(key);
        }

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)?;

        let mut index = Vec::new();
        let mut block_buf = Vec::new();
        let mut block_first: Option<Vec<u8>> = None;
        let mut block_last = Vec::new();
        let mut block_count: u32 = 0;
        let mut offset: u64 = 0;

        for (key, entry) in &collected {
            if block_first.is_none() {
                block_first = Some(key.clone());
            }
            encode_entry(&mut block_buf, key, entry);
            block_count += 1;
            block_last = key.clone();

            if block_buf.len() >= block_size && block_count > 0 {
                let length = flush_data_block(&mut file, &mut block_buf, block_count)?;
                index.push(IndexEntry {
                    first_key: block_first.take().unwrap_or_default(),
                    last_key: std::mem::take(&mut block_last),
                    offset,
                    length,
                });
                offset += u64::from(length);
                block_count = 0;
            }
        }

        if block_count > 0 || index.is_empty() {
            let length = flush_data_block(&mut file, &mut block_buf, block_count)?;
            index.push(IndexEntry {
                first_key: block_first.unwrap_or_default(),
                last_key: block_last,
                offset,
                length,
            });
            offset += u64::from(length);
        }

        let index_offset = offset;
        let index_bytes = encode_index(&index);
        file.write_all(&index_bytes)?;
        let index_len = index_bytes.len() as u64;

        let bloom_offset = index_offset + index_len;
        let bloom_bytes = bloom.encode();
        file.write_all(&bloom_bytes)?;
        let bloom_len = bloom_bytes.len() as u64;

        let mut footer = Vec::with_capacity(FOOTER_LEN);
        footer.extend_from_slice(&index_offset.to_le_bytes());
        footer.extend_from_slice(&index_len.to_le_bytes());
        footer.extend_from_slice(&bloom_offset.to_le_bytes());
        footer.extend_from_slice(&bloom_len.to_le_bytes());
        footer.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        footer.extend_from_slice(&0u32.to_le_bytes());
        let footer_crc = crc32(&footer);
        footer.extend_from_slice(&footer_crc.to_le_bytes());
        footer.extend_from_slice(&MAGIC.to_le_bytes());
        debug_assert_eq!(footer.len(), FOOTER_LEN);
        file.write_all(&footer)?;

        if failpoints::should_fail(failpoints::Kind::SsTableSync) {
            return Err(Error::Io(std::io::Error::other(
                "injected SSTable sync failure",
            )));
        }
        file.flush()?;
        file.sync_all()?;
        drop(file);

        if failpoints::should_fail(failpoints::Kind::Rename) {
            return Err(Error::Io(std::io::Error::other("injected rename failure")));
        }
        std::fs::rename(&tmp_path, &final_path)?;
        sync_dir(final_path.parent().unwrap_or_else(|| Path::new(".")))?;

        let size_bytes = std::fs::metadata(&final_path)?.len();
        let smallest = collected
            .first()
            .map(|(k, _)| k.clone())
            .unwrap_or_default();
        let largest = collected.last().map(|(k, _)| k.clone()).unwrap_or_default();

        Ok(Self {
            id,
            path: final_path,
            index,
            bloom,
            smallest,
            largest,
            size_bytes,
            level: 0,
        })
    }

    /// Opens an existing completed table.
    pub fn open(path: impl AsRef<Path>, id: u64) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".tmp"))
        {
            return Err(Error::CorruptedRecord);
        }

        let mut file = File::open(&path)?;
        let size_bytes = file.metadata()?.len();
        if size_bytes < FOOTER_LEN as u64 {
            return Err(Error::CorruptedRecord);
        }

        file.seek(SeekFrom::End(-(FOOTER_LEN as i64)))?;
        let mut footer = [0u8; FOOTER_LEN];
        file.read_exact(&mut footer)?;

        let magic = read_u32_le(&footer[44..48]);
        if magic != MAGIC {
            return Err(Error::CorruptedRecord);
        }
        let footer_crc = read_u32_le(&footer[40..44]);
        if crc32(&footer[..40]) != footer_crc {
            return Err(Error::InvalidChecksum);
        }
        let version = read_u32_le(&footer[32..36]);
        if version != FORMAT_VERSION {
            return Err(Error::CorruptedRecord);
        }

        let index_offset = read_u64_le(&footer[0..8]);
        let index_len = read_u64_le(&footer[8..16]) as usize;
        let bloom_offset = read_u64_le(&footer[16..24]);
        let bloom_len = read_u64_le(&footer[24..32]) as usize;

        if index_offset
            .checked_add(index_len as u64)
            .is_none_or(|end| end > size_bytes)
            || bloom_offset
                .checked_add(bloom_len as u64)
                .is_none_or(|end| end > size_bytes)
        {
            return Err(Error::CorruptedRecord);
        }

        file.seek(SeekFrom::Start(index_offset))?;
        let mut index_bytes = vec![0u8; index_len];
        file.read_exact(&mut index_bytes)?;
        let index = decode_index(&index_bytes)?;

        file.seek(SeekFrom::Start(bloom_offset))?;
        let mut bloom_bytes = vec![0u8; bloom_len];
        file.read_exact(&mut bloom_bytes)?;
        let bloom = BloomFilter::decode(&bloom_bytes)?;

        let smallest = index
            .first()
            .map(|e| e.first_key.clone())
            .unwrap_or_default();
        let largest = index.last().map(|e| e.last_key.clone()).unwrap_or_default();

        Ok(Self {
            id,
            path,
            index,
            bloom,
            smallest,
            largest,
            size_bytes,
            level: 0,
        })
    }

    /// Looks up `key`, using `cache` when provided.
    pub fn get(&self, key: &[u8], cache: Option<&Mutex<BlockCache>>) -> Result<Option<Entry>> {
        if !self.contains_range(key) {
            return Ok(None);
        }
        if !self.bloom.might_contain(key) {
            return Ok(None);
        }
        let Some(block_idx) = self.block_for_key(key) else {
            return Ok(None);
        };
        let block = self.load_block(block_idx, cache)?;
        Ok(search_block(&block, key).cloned())
    }

    /// Returns `true` when `key` falls inside this table's key range.
    pub fn contains_range(&self, key: &[u8]) -> bool {
        if self.smallest.is_empty() && self.largest.is_empty() && self.index.len() == 1 {
            // Empty table: nothing is in range.
            return false;
        }
        key >= self.smallest.as_slice() && key <= self.largest.as_slice()
    }

    /// Whether the Bloom filter thinks `key` might be present.
    pub fn bloom_might_contain(&self, key: &[u8]) -> bool {
        self.bloom.might_contain(key)
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn smallest_key(&self) -> &[u8] {
        &self.smallest
    }

    pub fn largest_key(&self) -> &[u8] {
        &self.largest
    }

    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    pub fn level(&self) -> usize {
        self.level
    }

    pub fn set_level(&mut self, level: usize) {
        self.level = level;
    }

    /// Streams every entry in key order without loading the whole file.
    pub fn iter(&self) -> Result<SsTableIterator> {
        SsTableIterator::new(self)
    }

    fn block_for_key(&self, key: &[u8]) -> Option<usize> {
        self.index
            .binary_search_by(|entry| {
                if key < entry.first_key.as_slice() {
                    std::cmp::Ordering::Greater
                } else if key > entry.last_key.as_slice() {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .ok()
    }

    fn load_block(&self, index: usize, cache: Option<&Mutex<BlockCache>>) -> Result<BlockData> {
        let entry = &self.index[index];
        let key = BlockKey {
            table_id: self.id,
            offset: entry.offset,
        };
        if let Some(cache) = cache {
            if let Some(block) = cache.lock().expect("block cache lock").get(key) {
                return Ok(block);
            }
        }
        let block = Arc::new(read_data_block(&self.path, entry.offset, entry.length)?);
        if let Some(cache) = cache {
            cache
                .lock()
                .expect("block cache lock")
                .insert(key, block.clone());
        }
        Ok(block)
    }
}

/// Sequential iterator over an SSTable's entries.
pub struct SsTableIterator {
    path: PathBuf,
    index: Vec<IndexEntry>,
    block_idx: usize,
    entry_idx: usize,
    current: Vec<(Vec<u8>, Entry)>,
}

impl SsTableIterator {
    fn new(table: &SsTable) -> Result<Self> {
        let mut iter = Self {
            path: table.path.clone(),
            index: table.index.clone(),
            block_idx: 0,
            entry_idx: 0,
            current: Vec::new(),
        };
        iter.load_next_block()?;
        Ok(iter)
    }

    fn load_next_block(&mut self) -> Result<()> {
        self.current.clear();
        self.entry_idx = 0;
        if self.block_idx >= self.index.len() {
            return Ok(());
        }
        let entry = &self.index[self.block_idx];
        self.current = read_data_block(&self.path, entry.offset, entry.length)?;
        self.block_idx += 1;
        Ok(())
    }
}

impl Iterator for SsTableIterator {
    type Item = Result<(Vec<u8>, Entry)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.entry_idx < self.current.len() {
                let item = self.current[self.entry_idx].clone();
                self.entry_idx += 1;
                return Some(Ok(item));
            }
            if self.block_idx >= self.index.len() {
                return None;
            }
            if let Err(err) = self.load_next_block() {
                return Some(Err(err));
            }
        }
    }
}

fn tmp_path(final_path: &Path) -> PathBuf {
    let mut tmp = final_path.as_os_str().to_os_string();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

fn encode_entry(buf: &mut Vec<u8>, key: &[u8], entry: &Entry) {
    match entry {
        Entry::Value(value) => {
            buf.push(TYPE_VALUE);
            buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
            buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
            buf.extend_from_slice(key);
            buf.extend_from_slice(value);
        }
        Entry::Tombstone => {
            buf.push(TYPE_TOMBSTONE);
            buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
            buf.extend_from_slice(&0u32.to_le_bytes());
            buf.extend_from_slice(key);
        }
    }
}

fn flush_data_block(file: &mut File, body: &mut Vec<u8>, count: u32) -> Result<u32> {
    let mut block = Vec::with_capacity(4 + body.len() + 4);
    block.extend_from_slice(&count.to_le_bytes());
    block.append(body);
    let checksum = crc32(&block);
    block.extend_from_slice(&checksum.to_le_bytes());
    file.write_all(&block)?;
    Ok(block.len() as u32)
}

fn read_data_block(path: &Path, offset: u64, length: u32) -> Result<Vec<(Vec<u8>, Entry)>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0u8; length as usize];
    file.read_exact(&mut bytes)?;
    if bytes.len() < 8 {
        return Err(Error::CorruptedRecord);
    }
    let checksum = read_u32_le(&bytes[bytes.len() - 4..]);
    let body = &bytes[..bytes.len() - 4];
    if crc32(body) != checksum {
        return Err(Error::InvalidChecksum);
    }
    let count = read_u32_le(&body[0..4]) as usize;
    let mut rest = &body[4..];
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        if rest.is_empty() {
            return Err(Error::CorruptedRecord);
        }
        let kind = rest[0];
        if rest.len() < 9 {
            return Err(Error::CorruptedRecord);
        }
        let key_len = read_u32_le(&rest[1..5]) as usize;
        let value_len = read_u32_le(&rest[5..9]) as usize;
        rest = &rest[9..];
        if rest.len() < key_len + value_len {
            return Err(Error::CorruptedRecord);
        }
        let key = rest[..key_len].to_vec();
        rest = &rest[key_len..];
        let entry = match kind {
            TYPE_VALUE => {
                let value = rest[..value_len].to_vec();
                rest = &rest[value_len..];
                Entry::Value(value)
            }
            TYPE_TOMBSTONE => {
                rest = &rest[value_len..];
                Entry::Tombstone
            }
            _ => return Err(Error::CorruptedRecord),
        };
        out.push((key, entry));
    }
    Ok(out)
}

fn encode_index(index: &[IndexEntry]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&(index.len() as u32).to_le_bytes());
    for entry in index {
        buf.extend_from_slice(&(entry.first_key.len() as u32).to_le_bytes());
        buf.extend_from_slice(&entry.first_key);
        buf.extend_from_slice(&(entry.last_key.len() as u32).to_le_bytes());
        buf.extend_from_slice(&entry.last_key);
        buf.extend_from_slice(&entry.offset.to_le_bytes());
        buf.extend_from_slice(&entry.length.to_le_bytes());
    }
    let checksum = crc32(&buf);
    buf.extend_from_slice(&checksum.to_le_bytes());
    buf
}

fn decode_index(bytes: &[u8]) -> Result<Vec<IndexEntry>> {
    if bytes.len() < 8 {
        return Err(Error::CorruptedRecord);
    }
    let checksum = read_u32_le(&bytes[bytes.len() - 4..]);
    let body = &bytes[..bytes.len() - 4];
    if crc32(body) != checksum {
        return Err(Error::InvalidChecksum);
    }
    let count = read_u32_le(&body[0..4]) as usize;
    let mut rest = &body[4..];
    let mut index = Vec::with_capacity(count);
    for _ in 0..count {
        if rest.len() < 4 {
            return Err(Error::CorruptedRecord);
        }
        let first_len = read_u32_le(&rest[0..4]) as usize;
        rest = &rest[4..];
        if rest.len() < first_len + 4 {
            return Err(Error::CorruptedRecord);
        }
        let first_key = rest[..first_len].to_vec();
        rest = &rest[first_len..];
        let last_len = read_u32_le(&rest[0..4]) as usize;
        rest = &rest[4..];
        if rest.len() < last_len + 12 {
            return Err(Error::CorruptedRecord);
        }
        let last_key = rest[..last_len].to_vec();
        rest = &rest[last_len..];
        let offset = read_u64_le(&rest[0..8]);
        let length = read_u32_le(&rest[8..12]);
        rest = &rest[12..];
        index.push(IndexEntry {
            first_key,
            last_key,
            offset,
            length,
        });
    }
    Ok(index)
}

fn search_block<'a>(block: &'a [(Vec<u8>, Entry)], key: &[u8]) -> Option<&'a Entry> {
    block
        .binary_search_by(|(k, _)| k.as_slice().cmp(key))
        .ok()
        .map(|i| &block[i].1)
}

/// Best-effort directory sync. On Windows opening a directory as a file is
/// not always permitted, so a failure here is ignored.
pub(crate) fn sync_dir(path: &Path) -> Result<()> {
    if let Ok(file) = File::open(path) {
        let _ = file.sync_all();
    }
    Ok(())
}

/// Table file name for `id`.
pub fn table_file_name(id: u64) -> String {
    format!("{id:06}.sst")
}

/// Shared handle used by the engine and compaction.
pub type SharedTable = Arc<SsTable>;

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
                "lsm-store-sst-{}-{nanos}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            TestDir(path)
        }

        fn sst(&self, id: u64) -> PathBuf {
            self.0.join(table_file_name(id))
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn entries(pairs: &[(&[u8], Option<&[u8]>)]) -> Vec<(Vec<u8>, Entry)> {
        pairs
            .iter()
            .map(|(k, v)| {
                (
                    k.to_vec(),
                    match v {
                        Some(value) => Entry::Value(value.to_vec()),
                        None => Entry::Tombstone,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn entries_are_written_in_sorted_order() {
        let dir = TestDir::new();
        let table = SsTable::create(
            dir.sst(1),
            1,
            entries(&[(b"c", Some(b"3")), (b"a", Some(b"1")), (b"b", Some(b"2"))])
                .into_iter()
                // caller is expected to pass sorted data; sort here to be safe
                .collect::<Vec<_>>()
                .into_iter(),
            0.01,
            64,
        )
        .unwrap();
        let keys: Vec<Vec<u8>> = table.iter().unwrap().map(|r| r.unwrap().0).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn sstable_can_be_reopened() {
        let dir = TestDir::new();
        SsTable::create(
            dir.sst(1),
            1,
            entries(&[(b"name", Some(b"Srijan"))]).into_iter(),
            0.01,
            256,
        )
        .unwrap();
        let table = SsTable::open(dir.sst(1), 1).unwrap();
        assert_eq!(
            table.get(b"name", None).unwrap(),
            Some(Entry::Value(b"Srijan".to_vec()))
        );
    }

    #[test]
    fn existing_key_can_be_found_and_missing_returns_none() {
        let dir = TestDir::new();
        let table = SsTable::create(
            dir.sst(1),
            1,
            entries(&[(b"a", Some(b"1")), (b"b", Some(b"2"))]).into_iter(),
            0.01,
            256,
        )
        .unwrap();
        assert_eq!(
            table.get(b"a", None).unwrap(),
            Some(Entry::Value(b"1".to_vec()))
        );
        assert_eq!(table.get(b"z", None).unwrap(), None);
    }

    #[test]
    fn tombstone_survives_flush() {
        let dir = TestDir::new();
        let table = SsTable::create(
            dir.sst(1),
            1,
            entries(&[(b"gone", None)]).into_iter(),
            0.01,
            256,
        )
        .unwrap();
        assert_eq!(table.get(b"gone", None).unwrap(), Some(Entry::Tombstone));
    }

    #[test]
    fn smallest_and_largest_keys_are_correct() {
        let dir = TestDir::new();
        let table = SsTable::create(
            dir.sst(1),
            1,
            entries(&[(b"apple", Some(b"1")), (b"zebra", Some(b"2"))]).into_iter(),
            0.01,
            256,
        )
        .unwrap();
        assert_eq!(table.smallest_key(), b"apple");
        assert_eq!(table.largest_key(), b"zebra");
    }

    #[test]
    fn empty_table_get_returns_none() {
        let dir = TestDir::new();
        let table = SsTable::create(dir.sst(1), 1, std::iter::empty(), 0.01, 256).unwrap();
        assert_eq!(table.get(b"anything", None).unwrap(), None);
        assert!(table.iter().unwrap().next().is_none());
    }

    #[test]
    fn truncated_sstable_is_rejected() {
        let dir = TestDir::new();
        SsTable::create(
            dir.sst(1),
            1,
            entries(&[(b"a", Some(b"1"))]).into_iter(),
            0.01,
            256,
        )
        .unwrap();
        let path = dir.sst(1);
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
        assert!(SsTable::open(&path, 1).is_err());
    }

    #[test]
    fn invalid_footer_is_rejected() {
        let dir = TestDir::new();
        SsTable::create(
            dir.sst(1),
            1,
            entries(&[(b"a", Some(b"1"))]).into_iter(),
            0.01,
            256,
        )
        .unwrap();
        let path = dir.sst(1);
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();
        assert!(SsTable::open(&path, 1).is_err());
    }

    #[test]
    fn temporary_file_is_not_treated_as_completed() {
        let dir = TestDir::new();
        let tmp = dir.0.join("000001.sst.tmp");
        std::fs::write(&tmp, b"not a table").unwrap();
        assert!(SsTable::open(&tmp, 1).is_err());
    }

    #[test]
    fn reads_spanning_different_blocks_work() {
        let dir = TestDir::new();
        let pairs: Vec<(Vec<u8>, Entry)> = (0..40u32)
            .map(|i| {
                (
                    format!("k{i:04}").into_bytes(),
                    Entry::Value(format!("v{i}").into_bytes()),
                )
            })
            .collect();
        let table = SsTable::create(dir.sst(1), 1, pairs.into_iter(), 0.01, 64).unwrap();
        assert!(table.index.len() > 1, "expected multiple data blocks");
        assert_eq!(
            table.get(b"k0000", None).unwrap(),
            Some(Entry::Value(b"v0".to_vec()))
        );
        assert_eq!(
            table.get(b"k0039", None).unwrap(),
            Some(Entry::Value(b"v39".to_vec()))
        );
    }

    #[test]
    fn cache_serves_a_second_lookup() {
        let dir = TestDir::new();
        let table = SsTable::create(
            dir.sst(1),
            1,
            entries(&[(b"a", Some(b"1"))]).into_iter(),
            0.01,
            256,
        )
        .unwrap();
        let cache = Mutex::new(BlockCache::new(4096));
        table.get(b"a", Some(&cache)).unwrap();
        table.get(b"a", Some(&cache)).unwrap();
        let cache = cache.lock().unwrap();
        assert!(cache.hits >= 1);
        assert!(cache.misses >= 1);
    }
}
