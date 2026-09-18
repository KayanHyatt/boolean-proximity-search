//! The positional inverted index.
//!
//! Days 4 and 5 of `docs/PLAN.md`: first in memory, then on disk.
//!
//! An inverted index maps each term to the documents containing it. A
//! *positional* index also records where in each document, which is what turns
//! a Boolean engine into one that can answer `"error correction"` and
//! `quantum NEAR/5 surface`:
//!
//! ```text
//! "quantum" ──▶ [ (doc 3, [12, 87]), (doc 17, [4]), (doc 22, [9, 31, 44]) ]
//!                  ▲       ▲
//!                  │       └── every position the term occurs at
//!                  └────────── documents, ascending, so lists can be merged
//! ```
//!
//! Postings lists are kept sorted by document id. That single invariant is what
//! makes intersection a linear merge instead of a hash join, and it is what day
//! 8's galloping search relies on.
//!
//! Day 5 replaces the in-memory structure with a hand-rolled binary file rather
//! than reaching for `bincode`. Owning the byte layout is the whole point:
//! day 13 wants to delta-encode document ids and varint-compress positions, and
//! that is only possible in a format nobody else defines.
