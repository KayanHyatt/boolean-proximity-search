//! Evaluating a parsed query against an index.
//!
//! Days 8 through 11 of `docs/PLAN.md`: Boolean evaluation, then phrases, then
//! proximity, then prefix wildcards.
//!
//! Evaluation walks the AST and combines postings lists. Because both lists are
//! sorted by document id, `AND` is a two-pointer merge and `OR` is the merge
//! step of a merge sort — no hashing, no allocation beyond the output:
//!
//! ```text
//! left   [ 3,  17,  22,  40 ]
//! right  [ 1,  17,  40 ]
//!               ▲    ▲
//! AND    [ 17, 40 ]
//! ```
//!
//! Two refinements that day 8 measures rather than assumes: evaluate the
//! smallest list first, since intersection can only shrink a result, and use
//! galloping (exponential) search when one list is far shorter than the other,
//! so a 10-element list meeting a 10-million-element list costs ten binary
//! searches rather than ten million steps.
//!
//! `NOT` is only supported as the right operand of `AND` — `a NOT b`, never a
//! bare `NOT b`. Complementing a postings list against a 2.7-million-document
//! corpus would materialise nearly the whole corpus to answer a question nobody
//! meant to ask.
//!
//! Day 10 forces a design change worth anticipating: `("machine learning")
//! NEAR/5 medical` means a `NEAR` operand must expose *positions*, not just
//! documents, so evaluation returns positions throughout and discards them only
//! at the top level.
