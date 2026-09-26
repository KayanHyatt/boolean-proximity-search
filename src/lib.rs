//! A Boolean + proximity search engine over a text corpus.
//!
//! The engine does one thing: given a query in a small but real query language,
//! return exactly the documents that satisfy it — no ranking, no fuzziness, no
//! guessing. Speed comes from never looking at a document that cannot match.
//!
//! # Shape of the thing
//!
//! ```text
//! build time:   corpus ──▶ tokenize ──▶ index ──▶ index.bin
//! query time:   query  ──▶ query::parse ──▶ Expr ──▶ search ──▶ [DocId]
//! ```
//!
//! [`corpus`] streams documents off disk and hands out dense [`DocId`]s.
//! [`mod@tokenize`] splits text into positioned terms. [`index`] inverts that into
//! a term → postings map, where a posting is a document plus every position the
//! term occurs at — positions are what make phrase and proximity queries
//! possible at all. [`query`] lexes and parses the query language into an AST,
//! and [`search`] evaluates that AST by intersecting postings lists.
//!
//! # Status
//!
//! Under construction, one module per day. See `docs/PLAN.md` in the repository
//! for the schedule; each module's documentation names the day that fills it in.

pub mod corpus;
pub mod error;
pub mod format;
pub mod index;
pub mod query;
pub mod search;
pub mod tokenize;

pub use crate::corpus::{DocId, DocMeta, DocStore, Document, JsonlCorpus, normalize_whitespace};
pub use crate::error::{Error, Result};
pub use crate::format::SearchIndex;
pub use crate::index::{
    FIELD_GAP, Index, IndexAssembler, IndexBuilder, IndexStats, Posting, Postings, TermId,
};
pub use crate::query::{Lexeme, LexemeKind, Span, lex, point_at};
pub use crate::tokenize::{Token, Tokens, tokenize};
