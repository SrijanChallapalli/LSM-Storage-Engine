//! Stage 19 — fault injection around durability boundaries.

use std::sync::atomic::{AtomicU64, Ordering};

use lsm_store::wal::failpoints::{self, Kind};
use lsm_store::{Engine, Error, Options};

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
            "lsm-store-fault-{}-{nanos}-{unique}",
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
fn wal_append_failure_does_not_publish_the_write() {
    let dir = TestDir::new();
    let mut db = Engine::open_with(&dir.0, Options::for_test()).unwrap();
    db.put(b"ok", b"1").unwrap();

    failpoints::inject(Kind::WalAppend);
    let err = db.put(b"fail", b"2").unwrap_err();
    failpoints::clear();
    assert!(matches!(err, Error::Io(_)));
    assert_eq!(db.get(b"fail").unwrap(), None);
    assert_eq!(db.get(b"ok").unwrap(), Some(b"1".to_vec()));
}

#[test]
fn wal_sync_failure_does_not_publish_the_write() {
    let dir = TestDir::new();
    let mut db = Engine::open_with(&dir.0, Options::for_test()).unwrap();
    failpoints::inject(Kind::WalSync);
    assert!(db.put(b"k", b"v").is_err());
    failpoints::clear();
    assert_eq!(db.get(b"k").unwrap(), None);
}

#[test]
fn failed_write_is_not_visible_after_reopen() {
    let dir = TestDir::new();
    {
        let mut db = Engine::open_with(&dir.0, Options::for_test()).unwrap();
        db.put(b"stable", b"yes").unwrap();
        failpoints::inject(Kind::WalAppend);
        let _ = db.put(b"unstable", b"no");
        failpoints::clear();
    }
    let db = Engine::open_with(&dir.0, Options::for_test()).unwrap();
    assert_eq!(db.get(b"stable").unwrap(), Some(b"yes".to_vec()));
    assert_eq!(db.get(b"unstable").unwrap(), None);
}

#[test]
fn truncated_wal_tail_is_ignored_on_recovery() {
    let dir = TestDir::new();
    {
        let mut db = Engine::open_with(&dir.0, Options::for_test()).unwrap();
        db.put(b"a", b"1").unwrap();
        db.put(b"b", b"2").unwrap();
    }
    let wal_path = dir.0.join("wal.log");
    let bytes = std::fs::read(&wal_path).unwrap();
    std::fs::write(&wal_path, &bytes[..bytes.len().saturating_sub(3)]).unwrap();

    let db = Engine::open_with(&dir.0, Options::for_test()).unwrap();
    assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
}
