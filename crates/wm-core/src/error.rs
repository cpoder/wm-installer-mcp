//! Crate-wide error type.

use std::path::PathBuf;

/// Anything that can go wrong reading or driving a webMethods installation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A path that should exist does not, or is the wrong kind of thing.
    #[error("{what} not found at {}", path.display())]
    NotFound {
        /// What was being looked for, e.g. "product catalog".
        what: &'static str,
        /// Where it was looked for.
        path: PathBuf,
    },

    /// A file exists but could not be read or written.
    #[error("cannot access {}: {source}", path.display())]
    Io {
        /// The file involved.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// A `requiresRegexp` entry in a `.prop` file is not a valid regex.
    #[error("invalid dependency pattern {pattern:?} declared by {required_by}: {source}")]
    BadPattern {
        /// The offending pattern.
        pattern: String,
        /// The product that declared it.
        required_by: String,
        /// The regex compilation failure.
        #[source]
        source: regex::Error,
    },

    /// A script or catalogue file was structurally wrong.
    #[error("{0}")]
    Malformed(String),

    /// Running the installer or Update Manager failed before it could report.
    #[error("{0}")]
    Exec(String),
}

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, Error>;
