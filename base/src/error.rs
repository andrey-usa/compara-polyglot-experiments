//! The one error type the library returns.

use std::fmt;

/// Anything that stops a comparison: a missing file, a key that is not in both
/// headers, an engine that cannot run.
#[derive(Debug)]
pub struct Error {
    message: String,
    /// Set when a reader is saying *not mine* -- a codec, a page version or a
    /// column type it does not handle -- rather than *this file is wrong*.
    ///
    /// The difference matters because this binary carries two Parquet readers.
    /// The columnar one is the fast path and reads a deliberately narrow slice
    /// of the format; `turbo` decodes pages into rows and reads far more of it.
    /// A refusal from the first is a reason to try the second, and for a while
    /// it was not: a zstd file was refused outright by a binary that could read
    /// it, with `--engine turbo` ignored on that path. [`crate::engine`] reads
    /// this flag to route around that.
    unsupported: bool,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Error {
            message: message.into(),
            unsupported: false,
        }
    }

    /// A reader declining a file it does not handle. See [`Error::unsupported`].
    pub fn unsupported(message: impl Into<String>) -> Self {
        Error {
            message: message.into(),
            unsupported: true,
        }
    }

    /// Whether this is a reader declining rather than a file being wrong.
    pub fn is_unsupported(&self) -> bool {
        self.unsupported
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::new(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::new(e.to_string())
    }
}

impl From<csv::Error> for Error {
    fn from(e: csv::Error) -> Self {
        Error::new(e.to_string())
    }
}

/// The library's result type.
pub type Result<T> = std::result::Result<T, Error>;
