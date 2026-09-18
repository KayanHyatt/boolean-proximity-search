//! The query language: lexer, parser, and abstract syntax tree.
//!
//! Days 6 and 7 of `docs/PLAN.md`.
//!
//! ```text
//! quantum AND ("error correction" NEAR/5 surface) NOT class*
//! ```
//!
//! Lexing produces tokens carrying byte spans, so an error can point at the
//! offending character instead of shrugging. Parsing is recursive descent, one
//! function per precedence level, producing a tree:
//!
//! ```text
//! pub enum Expr {
//!     Term(String),
//!     Prefix(String),
//!     Phrase(Vec<String>),
//!     Near { left: Box<Expr>, right: Box<Expr>, k: u32, ordered: bool },
//!     And(Box<Expr>, Box<Expr>),
//!     Or(Box<Expr>, Box<Expr>),
//!     Not(Box<Expr>, Box<Expr>),
//! }
//! ```
//!
//! Precedence, tightest first: `NOT`, `NEAR`, `AND`, `OR`. So `a OR b AND c`
//! parses as `a OR (b AND c)`, the same way `+` and `*` behave in arithmetic.
//!
//! The recursion is why `Box` appears: an `Expr` containing an `Expr` by value
//! would have no finite size. Boxing puts the child on the heap and gives the
//! parent a known size — the standard shape for an AST in Rust, and the reason
//! day 8's evaluator is a single `match` over seven cases.
