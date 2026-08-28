//! Stage 19 — random operation sequences must match a reference BTreeMap
//! after flushes, compaction, and restarts.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use lsm_store::{Engine, Options};

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
            "lsm-store-inv-{}-{nanos}-{unique}",
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

fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

#[test]
fn random_ops_match_reference_after_restarts() {
    let dir = TestDir::new();
    let mut rng = 0xC0FFEE_u64;
    let mut reference: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();

    {
        let mut db = Engine::open_with(&dir.0, Options::for_test()).unwrap();
        for step in 0..200u32 {
            let key_n = (xorshift(&mut rng) % 40) as u32;
            let key = format!("k{key_n:02}").into_bytes();
            match xorshift(&mut rng) % 10 {
                0 => {
                    db.delete(&key).unwrap();
                    reference.insert(key, None);
                }
                1..=2 => {
                    db.flush().unwrap();
                    if step % 20 == 0 {
                        let _ = db.compact();
                    }
                }
                _ => {
                    let value = format!("v{step}").into_bytes();
                    db.put(&key, &value).unwrap();
                    reference.insert(key, Some(value));
                }
            }
        }
    }

    let db = Engine::open_with(&dir.0, Options::for_test()).unwrap();
    for (key, expected) in &reference {
        assert_eq!(
            db.get(key).unwrap(),
            expected.clone(),
            "mismatch for {}",
            String::from_utf8_lossy(key)
        );
    }
}
