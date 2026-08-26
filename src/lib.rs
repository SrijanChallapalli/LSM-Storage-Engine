//! # lsm-store
//!
//! An [LSM-tree](https://en.wikipedia.org/wiki/Log-structured_merge-tree) storage
//! engine written in Rust, built up one stage at a time.
//!
//! ## Build order
//!
//! | Stage | Module                | Status        |
//! |-------|-----------------------|---------------|
//! | 1     | [`memtable`]          | implemented   |
//! | 2     | [`engine`]            | implemented   |
//! | 3     | [`record`]            | implemented   |
//! | 4     | [`wal`]               | implemented   |
//! | 5     | crash recovery        | wired into [`engine`] |
//! | 6–7   | [`sstable`]           | implemented   |
//! | 8     | [`manifest`]          | implemented   |
//! | 9–12  | [`compaction`]        | implemented   |
//! | 13    | [`bloom`]             | implemented   |
//! | 14    | [`cache`]             | implemented   |
//! | 15    | range scans           | wired into [`engine`] |

pub mod bloom;
pub mod cache;
pub mod compaction;
pub mod crc;
pub mod engine;
pub mod error;
pub mod manifest;
pub mod memtable;
pub mod options;
pub mod record;
pub mod sstable;
pub mod wal;

pub use engine::{Engine, EngineIterator, MAX_KEY_SIZE, MAX_VALUE_SIZE};
pub use error::{Error, Result};
pub use memtable::{Entry, MemTable};
pub use options::Options;
pub use record::Operation;
