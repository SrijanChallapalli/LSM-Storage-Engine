//! Crate-wide error type and [`Result`] alias.
//!
//! Introduced in Stage 2 and grown through Stages 3–5 as durability and
//! recovery add new failure modes. The public API surfaces these variants so
//! callers never have to know about memtables, WALs, or SSTables.

use std::fmt;

/// Errors that the storage engine can return.
#[derive(Debug)]
pub enum Error {
    /// An underlying I/O operation failed.
    Io(std::io::Error),
    /// A key was empty, which the engine does not allow.
    EmptyKey,
    /// A record on disk could not be parsed.
    CorruptedRecord,
    /// A record's stored checksum did not match its contents.
    InvalidChecksum,
    /// The manifest could not be parsed or was internally inconsistent.
    InvalidManifest,
    /// A key exceeded the configured maximum size.
    KeyTooLarge,
    /// A value exceeded the configured maximum size.
    ValueTooLarge,
}

/// A [`Result`](std::result::Result) whose error type is this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(source) => write!(f, "i/o error: {source}"),
            Error::EmptyKey => write!(f, "keys must not be empty"),
            Error::CorruptedRecord => write!(f, "corrupted record"),
            Error::InvalidChecksum => write!(f, "invalid checksum"),
            Error::InvalidManifest => write!(f, "invalid manifest"),
            Error::KeyTooLarge => write!(f, "key exceeds the maximum allowed size"),
            Error::ValueTooLarge => write!(f, "value exceeds the maximum allowed size"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(source) => Some(source),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        Error::Io(source)
    }
}
