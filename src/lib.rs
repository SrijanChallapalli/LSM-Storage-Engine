//! # lsm-store
//!
//! An [LSM-tree](https://en.wikipedia.org/wiki/Log-structured_merge-tree) storage
//! engine written in Rust, built up one stage at a time.
//!
//! The crate is organized into the modules an LSM engine needs. Only the pieces
//! that have been implemented so far are wired up; the remaining modules are
//! placeholders that will be filled in as the project progresses.
//!
//! ## Build order
//!
//! | Stage | Module                | Status        |
//! |-------|-----------------------|---------------|
//! | 1     | [`memtable`]          | implemented   |
//! | 2     | [`engine`]            | implemented   |
//! | 3–5   | [`error`], [`wal`]    | error type in progress |
//! | 6–7   | [`sstable`]           | planned       |
//! | 8     | [`manifest`]          | planned       |
//! | 9–12  | [`compaction`]        | planned       |

pub mod compaction;
pub mod engine;
pub mod error;
pub mod manifest;
pub mod memtable;
pub mod sstable;
pub mod wal;

pub use engine::Engine;
pub use error::{Error, Result};
pub use memtable::{Entry, MemTable};
