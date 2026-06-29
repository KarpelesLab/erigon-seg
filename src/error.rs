//! Error and result types for the crate.

use std::fmt;
use std::path::Path;

/// The crate result type.
pub type Result<T> = std::result::Result<T, Error>;

/// Anything that can go wrong while opening or reading a seg file set.
#[derive(Debug)]
pub enum Error {
    /// An underlying I/O failure (open, stat, mmap), annotated with the path.
    Io {
        /// The file the operation was acting on.
        path: Box<Path>,
        /// The originating I/O error.
        source: std::io::Error,
    },
    /// The file exists and was mapped, but its contents are not a valid / supported
    /// encoding of the format we were trying to read.
    Format(String),
}

impl Error {
    /// Build a [`Error::Format`] from anything string-like.
    pub(crate) fn format(msg: impl Into<String>) -> Error {
        Error::Format(msg.into())
    }

    pub(crate) fn io(path: &Path, source: std::io::Error) -> Error {
        Error::Io { path: path.into(), source }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Error::Format(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            Error::Format(_) => None,
        }
    }
}
