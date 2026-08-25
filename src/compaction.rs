//! Stages 9–12 — compaction and the background worker.
//!
//! Compaction merges overlapping SSTables into one. The newest version of each
//! key wins, the output stays sorted, and tombstones are kept unless it is
//! safe to drop them (the merge includes every older overlapping table, or
//! the output is going into the last level).
//!
//! Source files are not deleted until the new table is in the manifest.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::error::{Error, Result};
use crate::memtable::Entry;
use crate::sstable::{SharedTable, SsTable};

/// Messages accepted by the compaction worker.
#[derive(Debug)]
pub enum CompactionMessage {
    Compact,
    Shutdown,
}

/// Background worker that merges SSTables off the calling thread.
pub struct CompactionWorker {
    sender: Sender<CompactionMessage>,
    handle: Option<JoinHandle<()>>,
}

impl CompactionWorker {
    /// Starts a worker. `on_compact` is invoked whenever a `Compact` message
    /// arrives; errors it returns are stored in `last_error`.
    pub fn start<F>(mut on_compact: F, last_error: Arc<Mutex<Option<String>>>) -> Self
    where
        F: FnMut() -> Result<()> + Send + 'static,
    {
        let (sender, receiver): (Sender<CompactionMessage>, Receiver<CompactionMessage>) =
            mpsc::channel();
        let handle = thread::spawn(move || worker_loop(receiver, &mut on_compact, last_error));
        Self {
            sender,
            handle: Some(handle),
        }
    }

    /// Asks the worker to run one compaction pass.
    pub fn request_compaction(&self) -> Result<()> {
        self.sender
            .send(CompactionMessage::Compact)
            .map_err(|err| Error::Compaction(err.to_string()))
    }

    /// Signals shutdown and waits for the worker to finish.
    pub fn shutdown(&mut self) -> Result<()> {
        let _ = self.sender.send(CompactionMessage::Shutdown);
        if let Some(handle) = self.handle.take() {
            handle
                .join()
                .map_err(|_| Error::Compaction("compaction worker panicked".into()))?;
        }
        Ok(())
    }
}

impl Drop for CompactionWorker {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn worker_loop<F>(
    receiver: Receiver<CompactionMessage>,
    on_compact: &mut F,
    last_error: Arc<Mutex<Option<String>>>,
) where
    F: FnMut() -> Result<()>,
{
    while let Ok(message) = receiver.recv() {
        match message {
            CompactionMessage::Shutdown => break,
            CompactionMessage::Compact => {
                if let Err(err) = on_compact() {
                    if let Ok(mut slot) = last_error.lock() {
                        *slot = Some(err.to_string());
                    }
                }
            }
        }
    }
}

/// Merges `input_tables` (newest first) into a new table at `output_path`.
///
/// `drop_tombstones` is set when every older overlapping value is included
/// in this merge, or when the output lives in the last level.
pub fn compact(
    input_tables: &[SharedTable],
    output_path: &Path,
    output_id: u64,
    bloom_fp: f64,
    block_size: usize,
    drop_tombstones: bool,
) -> Result<SsTable> {
    let merged = merge_tables(input_tables, drop_tombstones)?;
    SsTable::create(
        output_path,
        output_id,
        merged.into_iter(),
        bloom_fp,
        block_size,
    )
}

fn merge_tables(
    input_tables: &[SharedTable],
    drop_tombstones: bool,
) -> Result<Vec<(Vec<u8>, Entry)>> {
    // Heap of (key, table_index, entry_index-is-implicit via iterators).
    // Newest table has the smallest index and therefore wins ties.
    let mut iters: Vec<crate::sstable::SsTableIterator> = Vec::new();
    for table in input_tables {
        iters.push(table.iter()?);
    }

    let mut heads: Vec<Option<(Vec<u8>, Entry)>> = Vec::with_capacity(iters.len());
    for iter in &mut iters {
        heads.push(match iter.next() {
            Some(Ok(item)) => Some(item),
            Some(Err(err)) => return Err(err),
            None => None,
        });
    }

    // Min-heap by key; for equal keys the newest table (lowest index) wins
    // by popping all of them and keeping the first we see while scanning
    // from index 0.
    let mut heap: BinaryHeap<Reverse<(Vec<u8>, usize)>> = BinaryHeap::new();
    for (idx, head) in heads.iter().enumerate() {
        if let Some((key, _)) = head {
            heap.push(Reverse((key.clone(), idx)));
        }
    }

    let mut out = Vec::new();
    while let Some(Reverse((key, _))) = heap.pop() {
        let mut winner: Option<(usize, Entry)> = None;
        // Collect every table that currently sits on `key`.
        let mut same = vec![key.clone()];
        while let Some(Reverse((next_key, _))) = heap.peek() {
            if next_key == &key {
                if let Some(Reverse((k, _))) = heap.pop() {
                    same.push(k);
                }
            } else {
                break;
            }
        }
        let _ = same;

        for (idx, head) in heads.iter().enumerate() {
            if let Some((k, entry)) = head {
                if k == &key && winner.is_none() {
                    winner = Some((idx, entry.clone()));
                }
            }
        }

        // Advance every iterator that contributed this key.
        for (idx, head) in heads.iter_mut().enumerate() {
            if head.as_ref().is_some_and(|(k, _)| k == &key) {
                *head = match iters[idx].next() {
                    Some(Ok(item)) => {
                        heap.push(Reverse((item.0.clone(), idx)));
                        Some(item)
                    }
                    Some(Err(err)) => return Err(err),
                    None => None,
                };
            }
        }

        if let Some((_, entry)) = winner {
            match entry {
                Entry::Tombstone if drop_tombstones => {}
                other => out.push((key, other)),
            }
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memtable::Entry;
    use crate::sstable::table_file_name;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "lsm-store-cmp-{}-{nanos}-{unique}",
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

    fn table(dir: &std::path::Path, id: u64, pairs: &[(&[u8], Option<&[u8]>)]) -> SharedTable {
        let entries = pairs.iter().map(|(k, v)| {
            (
                k.to_vec(),
                match v {
                    Some(value) => Entry::Value(value.to_vec()),
                    None => Entry::Tombstone,
                },
            )
        });
        let created =
            SsTable::create(dir.join(table_file_name(id)), id, entries, 0.01, 256).unwrap();
        Arc::new(created)
    }

    #[test]
    fn two_tables_merge_correctly() {
        let dir = TestDir::new();
        // Newest first.
        let newest = table(&dir.0, 2, &[(b"apple", Some(b"red")), (b"dog", None)]);
        let oldest = table(
            &dir.0,
            1,
            &[
                (b"apple", Some(b"yellow")),
                (b"bird", Some(b"blue")),
                (b"dog", Some(b"brown")),
            ],
        );
        let out = compact(
            &[newest, oldest],
            &dir.0.join(table_file_name(3)),
            3,
            0.01,
            256,
            false,
        )
        .unwrap();
        assert_eq!(
            out.get(b"apple", None).unwrap(),
            Some(Entry::Value(b"red".to_vec()))
        );
        assert_eq!(
            out.get(b"bird", None).unwrap(),
            Some(Entry::Value(b"blue".to_vec()))
        );
        assert_eq!(out.get(b"dog", None).unwrap(), Some(Entry::Tombstone));
    }

    #[test]
    fn output_is_sorted_and_unique() {
        let dir = TestDir::new();
        let a = table(&dir.0, 2, &[(b"b", Some(b"2")), (b"d", Some(b"4"))]);
        let b = table(
            &dir.0,
            1,
            &[(b"a", Some(b"1")), (b"b", Some(b"old")), (b"c", Some(b"3"))],
        );
        let out = compact(
            &[a, b],
            &dir.0.join(table_file_name(3)),
            3,
            0.01,
            256,
            false,
        )
        .unwrap();
        let keys: Vec<Vec<u8>> = out.iter().unwrap().map(|r| r.unwrap().0).collect();
        assert_eq!(
            keys,
            vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
        );
    }

    #[test]
    fn tombstones_drop_when_requested() {
        let dir = TestDir::new();
        let newest = table(&dir.0, 2, &[(b"dog", None)]);
        let oldest = table(&dir.0, 1, &[(b"dog", Some(b"brown"))]);
        let out = compact(
            &[newest, oldest],
            &dir.0.join(table_file_name(3)),
            3,
            0.01,
            256,
            true,
        )
        .unwrap();
        assert_eq!(out.get(b"dog", None).unwrap(), None);
    }

    #[test]
    fn empty_input_tables_work() {
        let dir = TestDir::new();
        let empty = table(&dir.0, 1, &[]);
        let out = compact(
            &[empty],
            &dir.0.join(table_file_name(2)),
            2,
            0.01,
            256,
            true,
        )
        .unwrap();
        assert_eq!(out.get(b"x", None).unwrap(), None);
    }

    #[test]
    fn worker_shuts_down_cleanly() {
        let last_error = Arc::new(Mutex::new(None));
        let mut worker = CompactionWorker::start(|| Ok(()), last_error);
        worker.request_compaction().unwrap();
        worker.shutdown().unwrap();
    }
}
