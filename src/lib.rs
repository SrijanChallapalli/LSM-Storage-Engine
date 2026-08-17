//! # lsm-store
//!
//! An LSM-tree storage engine written in Rust, built up one stage at a time.

pub mod error;
pub mod memtable;
pub mod options;

pub use error::{Error, Result};
pub use memtable::{Entry, MemTable};
pub use options::Options;
