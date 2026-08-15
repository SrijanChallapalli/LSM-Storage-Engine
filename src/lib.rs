//! # lsm-store
//!
//! An LSM-tree storage engine written in Rust, built up one stage at a time.

pub mod memtable;

pub use memtable::{Entry, MemTable};
