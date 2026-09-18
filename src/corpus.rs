//! Loading documents and handing out dense internal identifiers.
//!
//! Day 2 of `docs/PLAN.md` fills in the streaming JSONL reader: a
//! [`BufReader`](std::io::BufReader) over the arXiv metadata dump, one
//! `serde_json` parse per line, yielding documents lazily so that a 4 GB corpus
//! never has to fit in memory.

use std::fmt;

/// A dense, internal document identifier.
///
/// Documents are numbered in the order they were indexed, starting at zero, so
/// a `DocId` doubles as an index into the document-metadata table — no hash
/// lookup needed to go from a search hit back to a title.
///
/// It is a `u32`, not a `usize`, deliberately. A posting is a document id plus
/// a position list, and there will be hundreds of millions of them; halving the
/// id halves that part of the index and buys back cache lines during
/// intersection, which is the hot loop of the entire engine. Four billion
/// documents is a ceiling this project will not reach.
///
/// It is a newtype, not a bare `u32`, equally deliberately: positions are also
/// `u32`, term ids are also `u32`, and the type system should be the thing that
/// stops them being mixed up rather than a careful reading of argument order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocId(u32);

impl DocId {
    /// The first document in a corpus.
    pub const FIRST: Self = Self(0);

    /// Wraps a raw identifier.
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw identifier.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The identifier as a `usize`, for indexing into the document table.
    #[must_use]
    pub const fn as_usize(self) -> usize {
        self.0 as usize
    }

    /// The next identifier, for assigning ids while ingesting a corpus.
    ///
    /// Returns `None` past [`u32::MAX`] rather than wrapping silently, because
    /// a wrapped id would corrupt an index rather than fail a build.
    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(raw) => Some(Self(raw)),
            None => None,
        }
    }
}

impl fmt::Display for DocId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::DocId;

    #[test]
    fn ids_sort_by_their_raw_value() {
        let mut ids = [DocId::new(9), DocId::FIRST, DocId::new(3)];
        ids.sort_unstable();
        assert_eq!(ids, [DocId::FIRST, DocId::new(3), DocId::new(9)]);
    }

    #[test]
    fn ids_round_trip_through_their_raw_value() {
        assert_eq!(DocId::new(42).get(), 42);
        assert_eq!(DocId::new(42).as_usize(), 42);
        assert_eq!(DocId::FIRST.get(), 0);
    }

    #[test]
    fn ids_display_distinguishably_from_a_plain_number() {
        assert_eq!(DocId::new(42).to_string(), "#42");
    }

    #[test]
    fn the_last_id_has_no_successor() {
        assert_eq!(DocId::FIRST.checked_next(), Some(DocId::new(1)));
        assert_eq!(DocId::new(u32::MAX).checked_next(), None);
    }
}
