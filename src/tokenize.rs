//! Splitting text into positioned terms.
//!
//! Day 3 of `docs/PLAN.md`, and the day the borrow checker earns its keep. The
//! signature to aim for hands back slices of the caller's string rather than a
//! `String` per token:
//!
//! ```text
//! pub fn tokenize(text: &str) -> impl Iterator<Item = Token<'_>> + '_
//!
//! pub struct Token<'a> {
//!     pub text: &'a str,   // borrowed from `text`, never allocated
//!     pub offset: usize,   // byte offset, for highlighting
//!     pub position: u32,   // token ordinal, for phrase and NEAR queries
//! }
//! ```
//!
//! Tokenizing 2.7 million abstracts allocates roughly 300 million `String`s if
//! done the obvious way. Borrowing instead makes indexing a memory-bandwidth
//! problem rather than an allocator problem.
//!
//! Two decisions, both recorded here so they can be defended later:
//!
//! - **Stop words are kept.** Dropping "the" makes an index smaller, but
//!   `"the cat"` and `a NEAR/2 b` both depend on positions being contiguous and
//!   truthful. Correctness beats a few percent of disk.
//! - **No stemming.** Stemming would make `optimise` match `optimising`, but it
//!   also makes `"organic"` match `"organ"`, and there is no way to ask for the
//!   exact word back. Prefix wildcards on day 11 cover most of the want.
