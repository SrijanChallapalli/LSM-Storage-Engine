//! Stage 2 — the public [`Engine`] API.
//!
//! This is the surface users of the database call. It hides every internal
//! detail — the memtable, WAL, SSTables, manifest, and compaction — behind a
//! small key/value interface.
//!
//! ## Design decisions
//!
//! - **Maximum key size:** [`MAX_KEY_SIZE`] (64 KiB).
//! - **Maximum value size:** [`MAX_VALUE_SIZE`] (64 MiB).
//! - **Empty keys:** not allowed; they return [`Error::EmptyKey`].
//! - **Empty values:** allowed. An empty value is a real, retrievable value and
//!   is distinct from a deletion, so `get` returns `Some(vec![])` for it.
//! - **Deleting a missing key:** succeeds. Internally a tombstone is recorded
//!   so older values on disk stay hidden.
//! - **Durability of `put`:** durable. A `put` (or `delete`) is appended to the
//!   write-ahead log and `fsync`ed before it is applied in memory.
//! - **`get` return type:** owned `Vec<u8>`. Returning borrowed bytes would tie
//!   the value's lifetime to internal state that flushes and compaction mutate.

use std::fs;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::cache::BlockCache;
use crate::compaction::{compact, CompactionWorker};
use crate::error::{Error, Result};
use crate::manifest::Manifest;
use crate::memtable::{Entry, MemTable};
use crate::options::Options;
use crate::record::Operation;
use crate::sstable::{table_file_name, SharedTable, SsTable};
use crate::wal::Wal;

/// Name of the write-ahead log file inside the database directory.
const WAL_FILE_NAME: &str = "wal.log";

/// Maximum allowed key size, in bytes (64 KiB).
pub const MAX_KEY_SIZE: usize = 64 * 1024;

/// Maximum allowed value size, in bytes (64 MiB).
pub const MAX_VALUE_SIZE: usize = 64 * 1024 * 1024;

/// Shared, lock-protected pieces of the engine that compaction also touches.
pub(crate) struct SharedState {
    path: PathBuf,
    options: Options,
    version: RwLock<Version>,
    manifest: Mutex<Manifest>,
    pub cache: Mutex<BlockCache>,
    last_error: Arc<Mutex<Option<String>>>,
    pub bloom_skips: AtomicU64,
    pub sstable_reads: AtomicU64,
}

/// The live set of SSTables, organized by level.
#[derive(Clone, Default)]
pub(crate) struct Version {
    /// `levels[0]` is L0 (newest first, ranges may overlap).
    /// `levels[i > 0]` is non-overlapping and sorted by smallest key.
    pub levels: Vec<Vec<SharedTable>>,
}

impl Version {
    fn with_max_levels(max_levels: usize) -> Self {
        Self {
            levels: vec![Vec::new(); max_levels],
        }
    }

    fn tables_newest_first(&self) -> Vec<SharedTable> {
        let mut out = Vec::new();
        if let Some(l0) = self.levels.first() {
            out.extend(l0.iter().cloned());
        }
        for level in self.levels.iter().skip(1) {
            out.extend(level.iter().cloned());
        }
        out
    }
}

/// An embedded key/value database.
pub struct Engine {
    path: PathBuf,
    options: Options,
    wal: Wal,
    memtable: MemTable,
    immutable: Option<MemTable>,
    shared: Arc<SharedState>,
    worker: CompactionWorker,
}

impl Engine {
    /// Opens (creating if necessary) the database rooted at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, Options::default())
    }

    /// Opens the database with explicit [`Options`].
    pub fn open_with(path: impl AsRef<Path>, options: Options) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        fs::create_dir_all(&path)?;

        let manifest = Manifest::open(&path)?;
        let mut version = Version::with_max_levels(options.max_levels);
        for (id, level) in manifest.active_tables().to_vec() {
            let table_path = path.join(table_file_name(id));
            if !table_path.exists() {
                return Err(Error::InvalidManifest);
            }
            let mut table = SsTable::open(&table_path, id)?;
            table.set_level(level as usize);
            let level = (level as usize).min(options.max_levels.saturating_sub(1));
            version.levels[level].push(Arc::new(table));
        }
        // Manifest stores L0 in add order (oldest first). Search needs newest first.
        if let Some(l0) = version.levels.first_mut() {
            l0.reverse();
        }
        // L1+ must stay sorted by smallest key.
        for level in version.levels.iter_mut().skip(1) {
            level.sort_by(|a, b| a.smallest_key().cmp(b.smallest_key()));
        }

        let mut wal = Wal::open(path.join(WAL_FILE_NAME))?;
        let mut memtable = MemTable::new();
        for operation in wal.iter()? {
            apply(&mut memtable, operation?);
        }

        let shared = Arc::new(SharedState {
            path: path.clone(),
            options: options.clone(),
            version: RwLock::new(version),
            manifest: Mutex::new(manifest),
            cache: Mutex::new(BlockCache::new(options.cache_size_bytes)),
            last_error: Arc::new(Mutex::new(None)),
            bloom_skips: AtomicU64::new(0),
            sstable_reads: AtomicU64::new(0),
        });

        let shared_for_worker = Arc::clone(&shared);
        let last_error = Arc::clone(&shared.last_error);
        let worker = CompactionWorker::start(move || compact_once(&shared_for_worker), last_error);

        Ok(Self {
            path,
            options,
            wal,
            memtable,
            immutable: None,
            shared,
            worker,
        })
    }

    /// The directory this database lives in.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stores `value` under `key`, replacing any existing value.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        Self::validate_key(key)?;
        Self::validate_value(value)?;
        self.write(Operation::Put {
            key: key.to_vec(),
            value: value.to_vec(),
        })
    }

    /// Returns the newest visible value for `key`.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Self::validate_key(key)?;
        match lookup_memtable(&self.memtable, key) {
            Lookup::Value(value) => return Ok(Some(value)),
            Lookup::Tombstone => return Ok(None),
            Lookup::Miss => {}
        }
        if let Some(imm) = &self.immutable {
            match lookup_memtable(imm, key) {
                Lookup::Value(value) => return Ok(Some(value)),
                Lookup::Tombstone => return Ok(None),
                Lookup::Miss => {}
            }
        }
        self.get_from_tables(key)
    }

    /// Deletes `key`. Deleting a key that is not present succeeds.
    pub fn delete(&mut self, key: &[u8]) -> Result<()> {
        Self::validate_key(key)?;
        self.write(Operation::Delete { key: key.to_vec() })
    }

    /// Flushes the memtable into an SSTable and syncs durable state.
    pub fn flush(&mut self) -> Result<()> {
        self.flush_memtable()?;
        self.maybe_request_compaction();
        Ok(())
    }

    /// Runs compaction synchronously on the calling thread.
    pub fn compact(&mut self) -> Result<()> {
        compact_once(&self.shared)
    }

    /// Closes the database, consuming the handle after shutting down the worker.
    pub fn close(mut self) -> Result<()> {
        self.worker.shutdown()?;
        self.wal.sync()
    }

    /// Ordered scan over `[start, end)`-style bounds.
    pub fn scan(&self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<EngineIterator> {
        EngineIterator::new(self, start, end)
    }

    /// Bloom-filter skip count since open. Used by benchmarks.
    pub fn bloom_skips(&self) -> u64 {
        self.shared.bloom_skips.load(Ordering::Relaxed)
    }

    /// SSTable files consulted by `get` since open.
    pub fn sstable_reads(&self) -> u64 {
        self.shared.sstable_reads.load(Ordering::Relaxed)
    }

    /// Last background-compaction error, if any.
    pub fn compaction_error(&self) -> Option<String> {
        self.shared.last_error.lock().ok().and_then(|g| g.clone())
    }

    fn write(&mut self, operation: Operation) -> Result<()> {
        self.wal.append(&operation)?;
        self.wal.sync()?;
        apply(&mut self.memtable, operation);
        if self.memtable.approximate_size() >= self.options.memtable_size_bytes {
            self.flush()?;
        }
        Ok(())
    }

    fn flush_memtable(&mut self) -> Result<()> {
        if self.memtable.is_empty() {
            return self.wal.sync();
        }

        let frozen = std::mem::replace(&mut self.memtable, MemTable::new());
        self.immutable = Some(MemTable::new());
        // Keep a copy of the frozen table for reads during the flush.
        let snapshot: Vec<(Vec<u8>, Entry)> = frozen
            .iter()
            .map(|(k, e)| (k.to_vec(), e.clone()))
            .collect();
        self.immutable = Some({
            let mut imm = MemTable::new();
            for (k, e) in &snapshot {
                match e {
                    Entry::Value(v) => imm.put(k.clone(), v.clone()),
                    Entry::Tombstone => imm.delete(k.clone()),
                }
            }
            imm
        });

        let id = self
            .shared
            .manifest
            .lock()
            .expect("manifest lock")
            .next_table_id()?;
        let table_path = self.path.join(table_file_name(id));
        let table = SsTable::create(
            &table_path,
            id,
            snapshot.into_iter(),
            self.options.bloom_false_positive_rate,
            self.options.block_size,
        )?;

        self.shared
            .manifest
            .lock()
            .expect("manifest lock")
            .add_table(id, 0)?;

        {
            let mut version = self.shared.version.write().expect("version lock");
            version.levels[0].insert(0, Arc::new(table));
        }

        self.wal.truncate()?;
        self.immutable = None;
        Ok(())
    }

    fn maybe_request_compaction(&mut self) {
        let l0_len = self
            .shared
            .version
            .read()
            .map(|v| v.levels.first().map(Vec::len).unwrap_or(0))
            .unwrap_or(0);
        if l0_len >= self.options.l0_compaction_trigger {
            let _ = self.worker.request_compaction();
        }
    }

    fn get_from_tables(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let version = self.shared.version.read().expect("version lock");
        for table in version.tables_newest_first() {
            if !table.contains_range(key) {
                continue;
            }
            if !table.bloom_might_contain(key) {
                self.shared.bloom_skips.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            self.shared.sstable_reads.fetch_add(1, Ordering::Relaxed);
            match table.get(key, Some(&self.shared.cache))? {
                Some(Entry::Value(value)) => return Ok(Some(value)),
                Some(Entry::Tombstone) => return Ok(None),
                None => {}
            }
        }
        Ok(None)
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

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.worker.shutdown();
    }
}

enum Lookup {
    Value(Vec<u8>),
    Tombstone,
    Miss,
}

fn lookup_memtable(table: &MemTable, key: &[u8]) -> Lookup {
    match table.get(key) {
        Some(Entry::Value(value)) => Lookup::Value(value.clone()),
        Some(Entry::Tombstone) => Lookup::Tombstone,
        None => Lookup::Miss,
    }
}

fn apply(memtable: &mut MemTable, operation: Operation) {
    match operation {
        Operation::Put { key, value } => memtable.put(key, value),
        Operation::Delete { key } => memtable.delete(key),
    }
}

fn overlaps(a: &SsTable, b_small: &[u8], b_large: &[u8]) -> bool {
    if a.smallest_key().is_empty() && a.largest_key().is_empty() {
        return false;
    }
    a.smallest_key() <= b_large && b_small <= a.largest_key()
}

fn compact_once(shared: &SharedState) -> Result<()> {
    let options = &shared.options;
    let version = shared.version.read().expect("version lock").clone();

    let Some(job) = pick_compaction(&version, options) else {
        return Ok(());
    };

    let output_id = shared
        .manifest
        .lock()
        .expect("manifest lock")
        .next_table_id()?;
    let output_path = shared.path.join(table_file_name(output_id));

    let lower_overlap = version
        .levels
        .iter()
        .enumerate()
        .filter(|(level, _)| *level > job.output_level)
        .any(|(_, tables)| {
            tables.iter().any(|t| {
                job.inputs
                    .iter()
                    .any(|i| overlaps(t, i.smallest_key(), i.largest_key()))
            })
        });
    let drop_tombstones = job.output_level + 1 >= options.max_levels || !lower_overlap;

    let mut output = compact(
        &job.inputs,
        &output_path,
        output_id,
        options.bloom_false_positive_rate,
        options.block_size,
        drop_tombstones,
    )?;
    output.set_level(job.output_level);
    let output = Arc::new(output);

    let remove_ids: Vec<u64> = job.inputs.iter().map(|t| t.id()).collect();
    {
        let mut manifest = shared.manifest.lock().expect("manifest lock");
        manifest.add_table(output_id, job.output_level as u32)?;
        manifest.remove_tables(&remove_ids)?;
    }

    {
        let mut version = shared.version.write().expect("version lock");
        apply_compaction(&mut version, &job, output);
    }

    for table in job.inputs {
        if Arc::strong_count(&table) == 1 {
            let _ = fs::remove_file(table.path());
        }
    }
    Ok(())
}

struct CompactionJob {
    inputs: Vec<SharedTable>,
    output_level: usize,
}

fn pick_compaction(version: &Version, options: &Options) -> Option<CompactionJob> {
    let l0 = version.levels.first()?;
    if l0.len() >= options.l0_compaction_trigger {
        let mut inputs = l0.clone();
        let (small, large) = span(&inputs);
        if let Some(l1) = version.levels.get(1) {
            for table in l1 {
                if overlaps(table, &small, &large) {
                    inputs.push(Arc::clone(table));
                }
            }
        }
        return Some(CompactionJob {
            inputs,
            output_level: 1.min(options.max_levels.saturating_sub(1)),
        });
    }

    let mut limit = options.memtable_size_bytes as u64 * options.l0_compaction_trigger as u64;
    for level in 1..options.max_levels.saturating_sub(1) {
        let tables = version.levels.get(level)?;
        let size: u64 = tables.iter().map(|t| t.size_bytes()).sum();
        if size > limit {
            // Compact the first table plus overlapping next-level tables.
            let victim = tables.first()?.clone();
            let mut inputs = vec![victim.clone()];
            if let Some(next) = version.levels.get(level + 1) {
                for table in next {
                    if overlaps(table, victim.smallest_key(), victim.largest_key()) {
                        inputs.push(Arc::clone(table));
                    }
                }
            }
            return Some(CompactionJob {
                inputs,
                output_level: level + 1,
            });
        }
        limit = limit.saturating_mul(options.level_size_multiplier as u64);
    }
    None
}

fn span(tables: &[SharedTable]) -> (Vec<u8>, Vec<u8>) {
    let mut small = tables
        .first()
        .map(|t| t.smallest_key().to_vec())
        .unwrap_or_default();
    let mut large = tables
        .first()
        .map(|t| t.largest_key().to_vec())
        .unwrap_or_default();
    for table in tables.iter().skip(1) {
        if table.smallest_key() < small.as_slice() {
            small = table.smallest_key().to_vec();
        }
        if table.largest_key() > large.as_slice() {
            large = table.largest_key().to_vec();
        }
    }
    (small, large)
}

fn apply_compaction(version: &mut Version, job: &CompactionJob, output: SharedTable) {
    let remove: std::collections::HashSet<u64> = job.inputs.iter().map(|t| t.id()).collect();
    for level in &mut version.levels {
        level.retain(|t| !remove.contains(&t.id()));
    }
    let dest = job.output_level.min(version.levels.len().saturating_sub(1));
    version.levels[dest].push(output);
    if dest > 0 {
        version.levels[dest].sort_by(|a, b| a.smallest_key().cmp(b.smallest_key()));
    }
}

/// Merged, newest-wins iterator over every live layer.
pub struct EngineIterator {
    sources: Vec<PeekSource>,
    start: Bound<Vec<u8>>,
    end: Bound<Vec<u8>>,
}

struct PeekSource {
    next: Option<(Vec<u8>, Entry)>,
    rest: SourceRest,
}

enum SourceRest {
    Vec(std::vec::IntoIter<(Vec<u8>, Entry)>),
    Table(crate::sstable::SsTableIterator),
}

impl EngineIterator {
    fn new(engine: &Engine, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<Self> {
        let mut sources = Vec::new();
        sources.push(PeekSource::from_mem(&engine.memtable, start, end));
        if let Some(imm) = &engine.immutable {
            sources.push(PeekSource::from_mem(imm, start, end));
        }
        let tables = engine
            .shared
            .version
            .read()
            .expect("version lock")
            .tables_newest_first();
        for table in tables {
            let iter = table.iter()?;
            sources.push(PeekSource::from_table(iter, start, end)?);
        }
        Ok(Self {
            sources,
            start: clone_bound(start),
            end: clone_bound(end),
        })
    }
}

fn clone_bound(bound: Bound<&[u8]>) -> Bound<Vec<u8>> {
    match bound {
        Bound::Unbounded => Bound::Unbounded,
        Bound::Included(k) => Bound::Included(k.to_vec()),
        Bound::Excluded(k) => Bound::Excluded(k.to_vec()),
    }
}

fn bound_as_ref(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match bound {
        Bound::Unbounded => Bound::Unbounded,
        Bound::Included(k) => Bound::Included(k.as_slice()),
        Bound::Excluded(k) => Bound::Excluded(k.as_slice()),
    }
}

impl PeekSource {
    fn from_mem(table: &MemTable, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Self {
        let items: Vec<(Vec<u8>, Entry)> = table
            .iter()
            .filter(|(k, _)| in_range(k, start, end))
            .map(|(k, e)| (k.to_vec(), e.clone()))
            .collect();
        let mut rest = items.into_iter();
        let next = rest.next();
        Self {
            next,
            rest: SourceRest::Vec(rest),
        }
    }

    fn from_table(
        mut iter: crate::sstable::SsTableIterator,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Result<Self> {
        let next = next_in_range(&mut iter, start, end)?;
        Ok(Self {
            next,
            rest: SourceRest::Table(iter),
        })
    }

    fn advance(&mut self, start: Bound<&[u8]>, end: Bound<&[u8]>) -> Result<()> {
        self.next = match &mut self.rest {
            SourceRest::Vec(iter) => iter.next(),
            SourceRest::Table(iter) => next_in_range(iter, start, end)?,
        };
        Ok(())
    }
}

fn next_in_range(
    iter: &mut crate::sstable::SsTableIterator,
    start: Bound<&[u8]>,
    end: Bound<&[u8]>,
) -> Result<Option<(Vec<u8>, Entry)>> {
    for item in iter {
        let (key, entry) = item?;
        if in_range(&key, start, end) {
            return Ok(Some((key, entry)));
        }
        if past_end(&key, end) {
            return Ok(None);
        }
    }
    Ok(None)
}

fn in_range(key: &[u8], start: Bound<&[u8]>, end: Bound<&[u8]>) -> bool {
    let after_start = match start {
        Bound::Unbounded => true,
        Bound::Included(s) => key >= s,
        Bound::Excluded(s) => key > s,
    };
    let before_end = match end {
        Bound::Unbounded => true,
        Bound::Included(e) => key <= e,
        Bound::Excluded(e) => key < e,
    };
    after_start && before_end
}

fn past_end(key: &[u8], end: Bound<&[u8]>) -> bool {
    match end {
        Bound::Unbounded => false,
        Bound::Included(e) => key > e,
        Bound::Excluded(e) => key >= e,
    }
}

impl Iterator for EngineIterator {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let mut best: Option<(usize, Vec<u8>)> = None;
            for (idx, source) in self.sources.iter().enumerate() {
                if let Some((key, _)) = &source.next {
                    match &best {
                        None => best = Some((idx, key.clone())),
                        Some((_, best_key)) if key < best_key => best = Some((idx, key.clone())),
                        Some((_, best_key)) if key == best_key => {
                            // newer source already recorded; skip
                            let _ = idx;
                        }
                        _ => {}
                    }
                }
            }
            let (winner_idx, key) = best?;
            let entry = self.sources[winner_idx]
                .next
                .as_ref()
                .map(|(_, e)| e.clone());

            // Advance every source sitting on this key.
            for source in &mut self.sources {
                if source.next.as_ref().is_some_and(|(k, _)| k == &key) {
                    if let Err(err) =
                        source.advance(bound_as_ref(&self.start), bound_as_ref(&self.end))
                    {
                        return Some(Err(err));
                    }
                }
            }

            match entry {
                Some(Entry::Value(value)) => return Some(Ok((key, value))),
                Some(Entry::Tombstone) => continue,
                None => continue,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::Bound;

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
                "lsm-store-eng-{}-{nanos}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            TestDir(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_engine(dir: &TestDir) -> Engine {
        Engine::open_with(&dir.0, Options::for_test()).unwrap()
    }

    #[test]
    fn engine_can_be_created() {
        let dir = TestDir::new();
        let engine = test_engine(&dir);
        assert_eq!(engine.path(), dir.0.as_path());
        assert!(dir.0.is_dir());
    }

    #[test]
    fn put_and_get_work() {
        let dir = TestDir::new();
        let mut engine = test_engine(&dir);
        engine.put(b"name", b"Srijan").unwrap();
        assert_eq!(engine.get(b"name").unwrap(), Some(b"Srijan".to_vec()));
        assert_eq!(engine.get(b"missing").unwrap(), None);
    }

    #[test]
    fn delete_works() {
        let dir = TestDir::new();
        let mut engine = test_engine(&dir);
        engine.put(b"name", b"Srijan").unwrap();
        engine.delete(b"name").unwrap();
        assert_eq!(engine.get(b"name").unwrap(), None);
    }

    #[test]
    fn deleting_a_missing_key_succeeds() {
        let dir = TestDir::new();
        let mut engine = test_engine(&dir);
        engine.delete(b"ghost").unwrap();
        assert_eq!(engine.get(b"ghost").unwrap(), None);
    }

    #[test]
    fn empty_values_work() {
        let dir = TestDir::new();
        let mut engine = test_engine(&dir);
        engine.put(b"k", b"").unwrap();
        assert_eq!(engine.get(b"k").unwrap(), Some(Vec::new()));
    }

    #[test]
    fn invalid_inputs_return_errors() {
        let dir = TestDir::new();
        let mut engine = test_engine(&dir);
        assert!(matches!(engine.put(b"", b"v"), Err(Error::EmptyKey)));
        assert!(matches!(engine.get(b""), Err(Error::EmptyKey)));
        assert!(matches!(engine.delete(b""), Err(Error::EmptyKey)));
        let big_key = vec![b'x'; MAX_KEY_SIZE + 1];
        assert!(matches!(
            engine.put(&big_key, b"v"),
            Err(Error::KeyTooLarge)
        ));
        let big_value = vec![b'y'; MAX_VALUE_SIZE + 1];
        assert!(matches!(
            engine.put(b"k", &big_value),
            Err(Error::ValueTooLarge)
        ));
    }

    #[test]
    fn crash_without_clean_shutdown_recovers() {
        let dir = TestDir::new();
        {
            let mut db = test_engine(&dir);
            db.put(b"a", b"1").unwrap();
            db.put(b"b", b"2").unwrap();
            db.delete(b"a").unwrap();
            // Drop without close — same as a process dying.
        }
        let db = test_engine(&dir);
        assert_eq!(db.get(b"a").unwrap(), None);
        assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec()));
    }

    #[test]
    fn overwrites_recover_the_latest_value() {
        let dir = TestDir::new();
        {
            let mut db = test_engine(&dir);
            db.put(b"k", b"old").unwrap();
            db.put(b"k", b"new").unwrap();
        }
        let db = test_engine(&dir);
        assert_eq!(db.get(b"k").unwrap(), Some(b"new".to_vec()));
    }

    #[test]
    fn flush_then_restart_reads_from_sstable() {
        let dir = TestDir::new();
        {
            let mut db = test_engine(&dir);
            for i in 0..50u32 {
                db.put(format!("k{i:04}").as_bytes(), b"v").unwrap();
            }
            db.flush().unwrap();
        }
        let db = test_engine(&dir);
        assert_eq!(db.get(b"k0000").unwrap(), Some(b"v".to_vec()));
        assert_eq!(db.get(b"k0049").unwrap(), Some(b"v".to_vec()));
        assert_eq!(db.get(b"missing").unwrap(), None);
    }

    #[test]
    fn memtable_overrides_sstable() {
        let dir = TestDir::new();
        let mut db = test_engine(&dir);
        db.put(b"k", b"old").unwrap();
        db.flush().unwrap();
        db.put(b"k", b"new").unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(b"new".to_vec()));
    }

    #[test]
    fn memtable_tombstone_hides_sstable_value() {
        let dir = TestDir::new();
        let mut db = test_engine(&dir);
        db.put(b"k", b"old").unwrap();
        db.flush().unwrap();
        db.delete(b"k").unwrap();
        assert_eq!(db.get(b"k").unwrap(), None);
    }

    #[test]
    fn newer_sstable_overrides_older() {
        let dir = TestDir::new();
        let mut db = test_engine(&dir);
        db.put(b"k", b"v1").unwrap();
        db.flush().unwrap();
        db.put(b"k", b"v2").unwrap();
        db.flush().unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(b"v2".to_vec()));
    }

    #[test]
    fn latest_write_wins_after_multiple_flushes_and_reopen() {
        let dir = TestDir::new();
        {
            let mut db = test_engine(&dir);
            db.put(b"k", b"1").unwrap();
            db.flush().unwrap();
            db.put(b"k", b"2").unwrap();
            db.flush().unwrap();
            db.put(b"k", b"3").unwrap();
            db.flush().unwrap();
        }
        let db = test_engine(&dir);
        assert_eq!(db.get(b"k").unwrap(), Some(b"3".to_vec()));
    }

    #[test]
    fn compaction_preserves_visible_state() {
        let dir = TestDir::new();
        let mut db = test_engine(&dir);
        db.put(b"apple", b"red").unwrap();
        db.put(b"dog", b"brown").unwrap();
        db.flush().unwrap();
        db.put(b"apple", b"green").unwrap();
        db.put(b"cat", b"black").unwrap();
        db.flush().unwrap();
        db.delete(b"dog").unwrap();
        db.flush().unwrap();
        db.compact().unwrap();
        assert_eq!(db.get(b"apple").unwrap(), Some(b"green".to_vec()));
        assert_eq!(db.get(b"cat").unwrap(), Some(b"black".to_vec()));
        assert_eq!(db.get(b"dog").unwrap(), None);
    }

    #[test]
    fn scan_hides_tombstones_and_is_sorted() {
        let dir = TestDir::new();
        let mut db = test_engine(&dir);
        db.put(b"a", b"1").unwrap();
        db.put(b"b", b"2").unwrap();
        db.put(b"c", b"3").unwrap();
        db.delete(b"b").unwrap();
        let items: Vec<(Vec<u8>, Vec<u8>)> = db
            .scan(Bound::Unbounded, Bound::Unbounded)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            items,
            vec![
                (b"a".to_vec(), b"1".to_vec()),
                (b"c".to_vec(), b"3".to_vec())
            ]
        );
    }

    #[test]
    fn scan_respects_inclusive_and_exclusive_bounds() {
        let dir = TestDir::new();
        let mut db = test_engine(&dir);
        for key in [b"a", b"b", b"c", b"d"] {
            db.put(key, b"v").unwrap();
        }
        let keys = |s: Bound<&[u8]>, e: Bound<&[u8]>| {
            db.scan(s, e)
                .unwrap()
                .map(|r| r.unwrap().0)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            keys(Bound::Included(b"b"), Bound::Excluded(b"d")),
            vec![b"b".to_vec(), b"c".to_vec()]
        );
        assert_eq!(
            keys(Bound::Excluded(b"a"), Bound::Included(b"b")),
            vec![b"b".to_vec()]
        );
        assert!(keys(Bound::Included(b"x"), Bound::Included(b"z")).is_empty());
    }

    #[test]
    fn reference_map_matches_after_restarts() {
        let dir = TestDir::new();
        let mut reference: std::collections::BTreeMap<Vec<u8>, Option<Vec<u8>>> =
            std::collections::BTreeMap::new();
        {
            let mut db = test_engine(&dir);
            for i in 0..80u32 {
                let key = format!("k{i:03}").into_bytes();
                if i % 7 == 0 {
                    db.delete(&key).unwrap();
                    reference.insert(key, None);
                } else {
                    let value = format!("v{i}").into_bytes();
                    db.put(&key, &value).unwrap();
                    reference.insert(key, Some(value));
                }
                if i % 11 == 0 {
                    db.flush().unwrap();
                }
            }
            let _ = db.compact();
        }
        let db = test_engine(&dir);
        for (key, expected) in &reference {
            let got = db.get(key).unwrap();
            assert_eq!(
                got,
                expected.clone(),
                "mismatch for {}",
                String::from_utf8_lossy(key)
            );
        }
    }
}
