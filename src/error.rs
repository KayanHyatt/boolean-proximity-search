//! The crate's error type.
//!
//! One enum for the whole crate, rather than `Box<dyn Error>`. The difference
//! matters for a library: a caller can `match` on [`Error::Query`] to show a
//! user their mistake while treating [`Error::Io`] as fatal, and they can do it
//! without downcasting or string-matching on a message. The cost is that every
//! new failure mode has to be named here, which is the point — it keeps the
//! list of things that can go wrong short and visible.

use std::path::PathBuf;

use thiserror::Error;

/// A `Result` whose error type defaults to this crate's [`enum@Error`].
///
/// Writing `Result<Index>` instead of `Result<Index, Error>` throughout is
/// tidier, and the default parameter means the alias still works when a caller
/// wants a different error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Anything that can go wrong inside the engine.
///
/// Marked `#[non_exhaustive]` so that later days can add variants without it
/// being a breaking change for anyone matching on it.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// An I/O operation failed.
    ///
    /// The path is carried alongside the underlying error because
    /// [`std::io::Error`] on its own will only tell you *"No such file or
    /// directory"*, never which file.
    #[error("i/o error on {}", path.display())]
    Io {
        /// The file that was being read or written.
        path: PathBuf,
        /// The underlying operating-system error.
        #[source]
        source: std::io::Error,
    },

    /// A query could not be lexed or parsed.
    ///
    /// Carries a byte range rather than a single offset, so the offending text
    /// can be underlined rather than merely pointed at.
    #[error("invalid query at byte {offset}: {message}")]
    Query {
        /// Byte offset into the query string where the trouble starts.
        offset: usize,
        /// How many bytes of the query the problem covers.
        length: usize,
        /// What was found, and what was wanted instead.
        message: String,
    },

    /// An index file was truncated, corrupt, or written by another version.
    #[error("invalid index: {0}")]
    IndexFormat(String),

    /// A line in the corpus was not valid JSON, or was missing a field the
    /// engine requires.
    ///
    /// Carries the line number so the offending record can actually be found
    /// in a four-gigabyte file.
    #[error("{}:{line}: malformed record", path.display())]
    MalformedRecord {
        /// The corpus file.
        path: PathBuf,
        /// One-based line number of the offending record.
        line: usize,
        /// What serde objected to.
        #[source]
        source: serde_json::Error,
    },

    /// The corpus holds more documents than a [`DocId`](crate::DocId) can
    /// address.
    #[error(
        "corpus exceeds {} documents, the most a u32 document id can address",
        u32::MAX
    )]
    CorpusTooLarge,

    /// The index needs more postings or positions than a `u32` offset can
    /// address.
    #[error("index exceeds {} postings or positions", u32::MAX)]
    IndexTooLarge,

    /// The corpus changed between the counting pass and the filling pass.
    #[error(
        "corpus changed between passes: {unexpected_terms} term occurrence(s) the counting pass never saw"
    )]
    CorpusChanged {
        /// How many occurrences of unknown terms the filling pass met.
        unexpected_terms: u64,
    },

    /// A feature that a later day of the build plan will fill in.
    ///
    /// Temporary scaffolding; it should be gone by day 14.
    #[error("{0} is not implemented yet")]
    NotImplemented(&'static str),
}

impl Error {
    /// Attaches a path to a [`std::io::Error`].
    ///
    /// Intended for use with [`Result::map_err`], so that the call site reads
    /// as one line:
    ///
    /// ```
    /// # use std::path::Path;
    /// # use boolsearch::Error;
    /// # fn example(path: &Path) -> Result<std::fs::File, Error> {
    /// std::fs::File::open(path).map_err(|e| Error::io(path, e))
    /// # }
    /// ```
    #[must_use]
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;
    use std::io::{Error as IoError, ErrorKind};

    use super::Error;

    #[test]
    fn io_errors_name_the_path_and_keep_their_cause() {
        let error = Error::io(
            "corpus/arxiv.jsonl",
            IoError::new(ErrorKind::NotFound, "no such file"),
        );

        assert_eq!(error.to_string(), "i/o error on corpus/arxiv.jsonl");

        let cause = error.source().expect("the io error is kept as the source");
        assert_eq!(cause.to_string(), "no such file");
    }

    #[test]
    fn query_errors_point_at_an_offset() {
        let error = Error::Query {
            offset: 7,
            length: 3,
            message: "expected a term after AND".to_owned(),
        };

        assert_eq!(
            error.to_string(),
            "invalid query at byte 7: expected a term after AND"
        );
        assert!(error.source().is_none());
    }
}
