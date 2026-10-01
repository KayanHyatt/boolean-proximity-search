//! The query language: lexer, parser, and abstract syntax tree.
//!
//! ```text
//! quantum AND ("error correction" NEAR/5 surface) NOT class*
//! ```
//!
//! Day 6 is the lexer — turning that line into a flat sequence of [`Lexeme`]s.
//! Day 7 turns the sequence into a tree.
//!
//! # The grammar, in one table
//!
//! | Written | Means |
//! | --- | --- |
//! | `quantum` | a term |
//! | `comp*` | every term starting with `comp` |
//! | `"error correction"` | those terms, adjacent, in order |
//! | `'error correction'` | exactly the same — see below |
//! | `a NEAR/3 b` | within three positions, either order |
//! | `a ONEAR/3 b` | within three positions, `a` first |
//! | `AND` `OR` `NOT` | Boolean operators, uppercase only |
//! | `(` `)` | grouping |
//!
//! # Both quote characters delimit a phrase
//!
//! `"..."` and `'...'` mean exactly the same thing. That is not decoration: on
//! Windows, `search "quantum AND \"error correction\""` is mangled by the shell
//! before the program ever sees it, and getting it through PowerShell 5.1
//! requires backslashes that PowerShell 7 then treats differently. Accepting
//! single quotes means `search "quantum AND 'error correction'"` works in every
//! shell, with no escaping at all.
//!
//! # Operators are uppercase only
//!
//! `AND` is an operator; `and` is a term. Lucene makes the same choice, for the
//! same reason: someone searching for `cats and dogs` means the word, and
//! silently reinterpreting it as an operator would quietly change their
//! results. The cost is having to shout, which is a small price for never being
//! surprised.
//!
//! # Every lexeme carries a span
//!
//! Errors are useless if they cannot point. Each [`Lexeme`] records the byte
//! range it came from, so a bad query can be reported with a caret underneath
//! the exact characters that caused it rather than a shrug.

use std::fmt;

use crate::error::{Error, Result};
use crate::tokenize::tokenize;

/// A byte range within the query text.
///
/// Half-open, like every other range in Rust: `start..end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Byte offset of the first character.
    pub start: usize,
    /// Byte offset one past the last character.
    pub end: usize,
}

impl Span {
    /// A span covering `start..end`.
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// How many bytes the span covers.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.end - self.start
    }

    /// Whether the span covers nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.start >= self.end
    }

    /// The text this span refers to.
    ///
    /// # Panics
    ///
    /// If the span does not lie on character boundaries of `query`, which
    /// cannot happen for spans the lexer produced from that same query.
    #[must_use]
    pub fn of<'a>(&self, query: &'a str) -> &'a str {
        &query[self.start..self.end]
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}

/// One unit of a query, with the text it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lexeme {
    /// What the unit is.
    pub kind: LexemeKind,
    /// Where it was written.
    pub span: Span,
}

impl fmt::Display for Lexeme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.kind)
    }
}

/// The kinds of thing a query can be made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LexemeKind {
    /// A single term, already normalized the way the index stores it.
    Term(String),
    /// A prefix wildcard: the stem of `comp*`, normalized.
    Prefix(String),
    /// A sequence of terms that must appear adjacent and in order.
    ///
    /// Produced by quoting, and also by writing a word the tokenizer splits —
    /// `state-of-the-art` becomes the four-term phrase it has to become, since
    /// that is how the index stored it.
    Phrase(Vec<String>),
    /// `AND`
    And,
    /// `OR`
    Or,
    /// `NOT`
    Not,
    /// `NEAR/k` or `ONEAR/k`.
    Near {
        /// How many positions apart the two operands may be, at most.
        distance: u32,
        /// Whether the left operand must come first.
        ordered: bool,
    },
    /// `(`
    LeftParen,
    /// `)`
    RightParen,
}

impl fmt::Display for LexemeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Term(term) => write!(f, "{term}"),
            Self::Prefix(stem) => write!(f, "{stem}*"),
            Self::Phrase(terms) => write!(f, "\"{}\"", terms.join(" ")),
            Self::And => f.write_str("AND"),
            Self::Or => f.write_str("OR"),
            Self::Not => f.write_str("NOT"),
            Self::Near {
                distance,
                ordered: false,
            } => write!(f, "NEAR/{distance}"),
            Self::Near {
                distance,
                ordered: true,
            } => write!(f, "ONEAR/{distance}"),
            Self::LeftParen => f.write_str("("),
            Self::RightParen => f.write_str(")"),
        }
    }
}

impl LexemeKind {
    /// A short name for the kind, for error messages.
    #[must_use]
    pub const fn describe(&self) -> &'static str {
        match self {
            Self::Term(_) => "a term",
            Self::Prefix(_) => "a prefix wildcard",
            Self::Phrase(_) => "a phrase",
            Self::And | Self::Or | Self::Not => "an operator",
            Self::Near { .. } => "a proximity operator",
            Self::LeftParen => "an opening parenthesis",
            Self::RightParen => "a closing parenthesis",
        }
    }
}

/// Characters that end a bare word.
///
/// Apostrophes are deliberately absent. They are phrase delimiters *and* they
/// sit inside words — `don't` is one indexed term. The two are told apart by
/// position, not by the character: an apostrophe that begins a lexeme opens a
/// phrase, and one inside a word belongs to the word. So `'a phrase'` quotes
/// and `don't` does not, which is how a person reads them.
///
/// The cost is that an archaic contraction like `'tis` reads as an unterminated
/// phrase. That is a fair trade for `don't` working.
///
/// Everything else is handed to the tokenizer, which already knows how to split
/// `state-of-the-art` — and must, because that is how the index stored it.
const DELIMITERS: [char; 3] = ['(', ')', '"'];

/// Splits a query into [`Lexeme`]s.
///
/// # Errors
///
/// Returns [`Error::Query`] with the byte range of the offending text for an
/// unterminated phrase, a misplaced wildcard, a `NEAR` without a distance, or
/// anything else that cannot be made sense of.
pub fn lex(query: &str) -> Result<Vec<Lexeme>> {
    let mut lexemes = Vec::new();
    let mut offset = 0;

    while offset < query.len() {
        let rest = &query[offset..];

        let Some(character) = rest.chars().next() else {
            break;
        };

        if character.is_whitespace() {
            offset += character.len_utf8();
            continue;
        }

        let lexeme = match character {
            '(' => {
                offset += 1;
                Lexeme {
                    kind: LexemeKind::LeftParen,
                    span: Span::new(offset - 1, offset),
                }
            }
            ')' => {
                offset += 1;
                Lexeme {
                    kind: LexemeKind::RightParen,
                    span: Span::new(offset - 1, offset),
                }
            }
            '"' | '\'' | '\u{2019}' => lex_phrase(query, &mut offset, character)?,
            _ => lex_word(query, &mut offset)?,
        };

        lexemes.push(lexeme);
    }

    Ok(lexemes)
}

/// Reads a quoted phrase, starting at the opening quote.
fn lex_phrase(query: &str, offset: &mut usize, quote: char) -> Result<Lexeme> {
    let start = *offset;
    let after_quote = start + quote.len_utf8();

    // A typographic opening quote is closed by its typographic partner, which
    // is what a word processor will have produced if the query was pasted.
    let closing = if quote == '\u{2019}' {
        '\u{2019}'
    } else {
        quote
    };

    let Some(relative) = query[after_quote..].find(closing) else {
        return Err(query_error(
            Span::new(start, query.len()),
            format!("this phrase is never closed — add a matching {closing}"),
        ));
    };

    let end_quote = after_quote + relative;
    let contents = &query[after_quote..end_quote];
    *offset = end_quote + closing.len_utf8();

    let terms: Vec<String> = tokenize(contents)
        .map(|token| token.normalized().into_owned())
        .collect();

    let span = Span::new(start, *offset);

    match terms.len() {
        0 => Err(query_error(
            span,
            "this phrase has no searchable terms in it".to_owned(),
        )),
        // A one-word phrase is just that word; keeping it as a phrase would
        // make day 9 do positional work for no reason.
        1 => Ok(Lexeme {
            kind: LexemeKind::Term(terms.into_iter().next().expect("length checked")),
            span,
        }),
        _ => Ok(Lexeme {
            kind: LexemeKind::Phrase(terms),
            span,
        }),
    }
}

/// Reads a bare word: an operator, a wildcard, or a term.
fn lex_word(query: &str, offset: &mut usize) -> Result<Lexeme> {
    let start = *offset;
    let rest = &query[start..];

    let length = rest
        .find(|character: char| character.is_whitespace() || DELIMITERS.contains(&character))
        .unwrap_or(rest.len());

    let word = &rest[..length];
    *offset = start + length;
    let span = Span::new(start, *offset);

    // Operators are uppercase only, so `and` stays a searchable word.
    let kind = match word {
        "AND" => LexemeKind::And,
        "OR" => LexemeKind::Or,
        "NOT" => LexemeKind::Not,
        "NEAR" | "ONEAR" => {
            return Err(query_error(
                span,
                format!("{word} needs a distance, as in {word}/3"),
            ));
        }
        _ if word.starts_with("NEAR/") || word.starts_with("ONEAR/") => {
            return lex_near(word, span);
        }
        _ => return lex_term(word, span),
    };

    Ok(Lexeme { kind, span })
}

/// Reads `NEAR/k` or `ONEAR/k`.
fn lex_near(word: &str, span: Span) -> Result<Lexeme> {
    let ordered = word.starts_with('O');
    let digits = word
        .split_once('/')
        .expect("the caller checked for a slash")
        .1;

    let distance: u32 = digits.parse().map_err(|_| {
        query_error(
            span,
            format!("{digits:?} is not a distance — write a whole number, as in NEAR/3"),
        )
    })?;

    if distance == 0 {
        return Err(query_error(
            span,
            "a distance of 0 can never match, since two terms cannot share a position".to_owned(),
        ));
    }

    Ok(Lexeme {
        kind: LexemeKind::Near { distance, ordered },
        span,
    })
}

/// Reads a term or a prefix wildcard.
fn lex_term(word: &str, span: Span) -> Result<Lexeme> {
    let (stem, wildcard) = match word.strip_suffix('*') {
        Some(stem) => (stem, true),
        None => (word, false),
    };

    if let Some(inner) = stem.find('*') {
        return Err(query_error(
            Span::new(span.start + inner, span.start + inner + 1),
            "a wildcard is only supported at the end of a term".to_owned(),
        ));
    }

    // The same tokenizer the corpus went through, so a query term is split the
    // way the indexed term was. Skipping this would mean `state-of-the-art`
    // never matched anything, because the index holds four terms, not one.
    let terms: Vec<String> = tokenize(stem)
        .map(|token| token.normalized().into_owned())
        .collect();

    match (terms.len(), wildcard) {
        (0, _) => Err(query_error(
            span,
            format!("{word:?} has no searchable characters in it"),
        )),
        (1, true) => Ok(Lexeme {
            kind: LexemeKind::Prefix(terms.into_iter().next().expect("length checked")),
            span,
        }),
        (1, false) => Ok(Lexeme {
            kind: LexemeKind::Term(terms.into_iter().next().expect("length checked")),
            span,
        }),
        (_, true) => Err(query_error(
            span,
            format!("{word:?} splits into several terms, so the wildcard has nothing to attach to"),
        )),
        // `state-of-the-art` is four terms in the index, so it is a phrase here.
        (_, false) => Ok(Lexeme {
            kind: LexemeKind::Phrase(terms),
            span,
        }),
    }
}

fn query_error(span: Span, message: String) -> Error {
    Error::Query {
        offset: span.start,
        length: span.len(),
        message,
    }
}

/// Renders a query error as the offending text with a caret under it.
///
/// ```text
/// quantum AND NEAR/3 surface
///             ^^^^^^
/// ```
#[must_use]
pub fn point_at(query: &str, offset: usize, length: usize) -> String {
    // Count characters rather than bytes, so the caret lines up under
    // multi-byte text instead of drifting right.
    let leading = query[..offset.min(query.len())].chars().count();
    let width = query
        .get(offset..(offset + length).min(query.len()))
        .map_or(1, |text| text.chars().count().max(1));

    format!("{query}\n{}{}", " ".repeat(leading), "^".repeat(width))
}

// ---------------------------------------------------------------------------
// Day 7: the parser
// ---------------------------------------------------------------------------

/// A parsed query.
///
/// The recursion is why [`Box`] appears: an `Expr` containing an `Expr` by
/// value would have no finite size, since the compiler would have to add up an
/// infinite chain of them. Boxing puts the child on the heap and gives the
/// parent a known size — the standard shape for a tree in Rust, and the reason
/// day 8's evaluator is one `match` over seven cases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// A single term.
    Term(String),
    /// Every term beginning with this stem.
    Prefix(String),
    /// Terms that must appear adjacent and in order.
    Phrase(Vec<String>),
    /// Two expressions occurring within `distance` positions of one another.
    Near {
        /// The left operand.
        left: Box<Expr>,
        /// The right operand.
        right: Box<Expr>,
        /// How many positions apart they may be, at most.
        distance: u32,
        /// Whether `left` must come first.
        ordered: bool,
    },
    /// Both sides must match.
    And(Box<Expr>, Box<Expr>),
    /// Either side must match.
    Or(Box<Expr>, Box<Expr>),
    /// The left side must match and the right must not.
    ///
    /// Binary, never unary. `NOT classical` on its own would mean "every
    /// document except those" — two and a half million results nobody asked
    /// for. Requiring something on the left keeps the answer bounded by
    /// something the user actually wanted.
    Not(Box<Expr>, Box<Expr>),
}

impl fmt::Display for Expr {
    /// Prints the tree fully parenthesized.
    ///
    /// Deliberately not "prettily": the point is that re-parsing the output
    /// gives back the same tree, which makes the shape of a parse visible and
    /// gives `parsing_is_idempotent` something exact to assert.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Term(term) => write!(f, "{term}"),
            Self::Prefix(stem) => write!(f, "{stem}*"),
            Self::Phrase(terms) => write!(f, "\"{}\"", terms.join(" ")),
            Self::And(left, right) => write!(f, "({left} AND {right})"),
            Self::Or(left, right) => write!(f, "({left} OR {right})"),
            Self::Not(left, right) => write!(f, "({left} NOT {right})"),
            Self::Near {
                left,
                right,
                distance,
                ordered,
            } => {
                let operator = if *ordered { "ONEAR" } else { "NEAR" };
                write!(f, "({left} {operator}/{distance} {right})")
            }
        }
    }
}

impl Expr {
    /// Visits every node, parents before children.
    pub fn walk(&self, visit: &mut impl FnMut(&Self)) {
        visit(self);

        match self {
            Self::Term(_) | Self::Prefix(_) | Self::Phrase(_) => {}
            Self::And(left, right) | Self::Or(left, right) | Self::Not(left, right) => {
                left.walk(visit);
                right.walk(visit);
            }
            Self::Near { left, right, .. } => {
                left.walk(visit);
                right.walk(visit);
            }
        }
    }

    /// Whether this expression can say *where* in a document it matched.
    ///
    /// `NEAR` is a question about distance, so both of its operands have to
    /// answer it. A term can — the index stores its positions. A phrase can:
    /// its match occupies a known stretch of them. `OR` can, if both sides
    /// can, by taking whichever matched. A nested `NEAR` can, by taking the
    /// stretch spanning the pair it found.
    ///
    /// `AND` and `NOT` cannot, and not because of a missing feature. There is
    /// no position at which `quantum AND gravity` occurs: it is a fact about a
    /// whole document, not about a place in one. So
    /// `(quantum AND gravity) NEAR/5 loop` has no meaning to implement, and
    /// [`parse`] rejects it rather than inventing one.
    #[must_use]
    pub fn is_positional(&self) -> bool {
        match self {
            Self::Term(_) | Self::Prefix(_) | Self::Phrase(_) => true,
            Self::Or(left, right) | Self::Near { left, right, .. } => {
                left.is_positional() && right.is_positional()
            }
            Self::And(..) | Self::Not(..) => false,
        }
    }

    /// How many nodes the tree holds.
    #[must_use]
    pub fn size(&self) -> usize {
        let mut count = 0;
        self.walk(&mut |_| count += 1);
        count
    }
}

/// Rejects a `NEAR` operand that cannot say where it matched.
fn require_positional(expression: &Expr, span: Span, operator: &str) -> Result<()> {
    if expression.is_positional() {
        return Ok(());
    }

    let culprit = match expression {
        Expr::And(..) => "AND",
        Expr::Not(..) => "NOT",
        // An `OR` whose own operand is non-positional; name the operator the
        // user can actually see at fault.
        _ => "AND or NOT",
    };

    Err(query_error(
        span,
        // Deliberately one line: `cargo fmt` will collapse a backslash
        // continuation inside a string literal and leave its indentation in
        // the message, which is how this one first read "does not have one
        // —              it answers".
        format!("{operator} needs to know where its operands matched, and {culprit} cannot say"),
    ))
}

/// Parses a query into an [`Expr`].
///
/// # Precedence
///
/// Loosest to tightest: `OR`, then `AND`/`NOT`, then `NEAR`, then terms and
/// parenthesized groups. So `a OR b AND c` is `a OR (b AND c)`, the same way
/// `+` and `*` behave in arithmetic, and `a AND b NEAR/3 c` is
/// `a AND (b NEAR/3 c)` — proximity binds its operands before anything else
/// gets to them.
///
/// `AND` and `NOT` share a level and associate left, because `NOT` here is the
/// binary difference `a NOT b` rather than a unary negation. The build plan
/// listed `NOT` as the tightest operator, which is right for a unary `NOT`;
/// with a binary one it belongs beside `AND`, and `a NOT b NOT c` reads
/// left to right as it should.
///
/// Adjacent operands with no operator between them are an implicit `AND`, so
/// `quantum error` finds documents containing both.
///
/// # Errors
///
/// Returns [`Error::Query`] with the byte range of the offending text, or a
/// zero-width range at the end of the query when something is missing rather
/// than wrong.
pub fn parse(query: &str) -> Result<Expr> {
    let lexemes = lex(query)?;
    Parser {
        lexemes: &lexemes,
        position: 0,
        end: query.len(),
    }
    .parse()
}

/// One function per precedence level, walking the lexemes left to right.
struct Parser<'a> {
    lexemes: &'a [Lexeme],
    position: usize,
    /// Byte length of the query, for pointing at "something is missing here".
    end: usize,
}

impl Parser<'_> {
    fn parse(&mut self) -> Result<Expr> {
        if self.lexemes.is_empty() {
            return Err(query_error(
                Span::new(0, 0),
                "the query is empty".to_owned(),
            ));
        }

        let expression = self.parse_or("the query")?;

        // Anything left over means the query did not hang together — most
        // often a stray closing parenthesis.
        if let Some(extra) = self.peek() {
            let message = match &extra.kind {
                LexemeKind::RightParen => {
                    "this closing parenthesis has no opening one to match".to_owned()
                }
                kind => format!("{kind} was not expected here"),
            };
            return Err(query_error(extra.span, message));
        }

        Ok(expression)
    }

    /// `or := and ( OR and )*`
    fn parse_or(&mut self, context: &str) -> Result<Expr> {
        let mut left = self.parse_and(context)?;

        while self.eat(&LexemeKind::Or) {
            let right = self.parse_and("OR")?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }

        Ok(left)
    }

    /// `and := near ( (AND | NOT | nothing at all) near )*`
    fn parse_and(&mut self, context: &str) -> Result<Expr> {
        let mut left = self.parse_near(context)?;

        loop {
            if self.eat(&LexemeKind::And) {
                let right = self.parse_near("AND")?;
                left = Expr::And(Box::new(left), Box::new(right));
            } else if self.eat(&LexemeKind::Not) {
                let right = self.parse_near("NOT")?;
                left = Expr::Not(Box::new(left), Box::new(right));
            } else if self.at_operand() {
                // Two operands side by side: an implicit AND, so that
                // `quantum error` means what anyone would expect.
                let right = self.parse_near(context)?;
                left = Expr::And(Box::new(left), Box::new(right));
            } else {
                break;
            }
        }

        Ok(left)
    }

    /// `near := primary ( NEAR/k primary )*`
    ///
    /// The one rule the grammar alone cannot express: a `NEAR` operand has to
    /// be positional. `primary` allows a parenthesized group, and a group can
    /// hold `AND` or `NOT`, so `(a AND b) NEAR/3 c` parses perfectly well and
    /// means nothing. Catching it here rather than at evaluation is worth the
    /// extra bookkeeping, because here the spans still exist and the error can
    /// underline the offending group.
    fn parse_near(&mut self, context: &str) -> Result<Expr> {
        let opened = self.position;
        let mut left = self.parse_primary(context)?;
        let mut left_span = self.span_since(opened);

        while let Some((distance, ordered)) = self.eat_near() {
            let operator = if ordered { "ONEAR" } else { "NEAR" };
            require_positional(&left, left_span, operator)?;

            let opened = self.position;
            let right = self.parse_primary(operator)?;
            require_positional(&right, self.span_since(opened), operator)?;

            left_span = Span::new(left_span.start, self.span_since(opened).end);
            left = Expr::Near {
                left: Box::new(left),
                right: Box::new(right),
                distance,
                ordered,
            };
        }

        Ok(left)
    }

    /// `primary := term | prefix | phrase | ( or )`
    fn parse_primary(&mut self, context: &str) -> Result<Expr> {
        let Some(lexeme) = self.peek() else {
            return Err(query_error(
                Span::new(self.end, self.end),
                format!("{context} stops here, but something has to come after it"),
            ));
        };

        let span = lexeme.span;

        match &lexeme.kind {
            LexemeKind::Term(term) => {
                let term = term.clone();
                self.position += 1;
                Ok(Expr::Term(term))
            }
            LexemeKind::Prefix(stem) => {
                let stem = stem.clone();
                self.position += 1;
                Ok(Expr::Prefix(stem))
            }
            LexemeKind::Phrase(terms) => {
                let terms = terms.clone();
                self.position += 1;
                Ok(Expr::Phrase(terms))
            }
            LexemeKind::LeftParen => {
                self.position += 1;

                if self.at(&LexemeKind::RightParen) {
                    let close = self.peek().expect("just checked").span;
                    return Err(query_error(
                        Span::new(span.start, close.end),
                        "this group is empty".to_owned(),
                    ));
                }

                let inner = self.parse_or("this group")?;

                if !self.eat(&LexemeKind::RightParen) {
                    return Err(query_error(
                        span,
                        "this group is never closed — add a matching )".to_owned(),
                    ));
                }

                Ok(inner)
            }
            kind => Err(query_error(
                span,
                format!("expected a term after {context}, found {}", kind.describe()),
            )),
        }
    }

    fn peek(&self) -> Option<&Lexeme> {
        self.lexemes.get(self.position)
    }

    /// The span covering every lexeme consumed since position `opened`.
    ///
    /// The AST carries no spans — it does not need them, and threading them
    /// through every node to serve one error message would be a poor trade.
    /// The lexemes do carry them, and the parser knows which ones it ate.
    fn span_since(&self, opened: usize) -> Span {
        let start = self
            .lexemes
            .get(opened)
            .map_or(self.end, |lexeme| lexeme.span.start);
        let end = self
            .position
            .checked_sub(1)
            .and_then(|last| self.lexemes.get(last))
            .map_or(start, |lexeme| lexeme.span.end);

        Span::new(start, end.max(start))
    }

    fn at(&self, kind: &LexemeKind) -> bool {
        self.peek().is_some_and(|lexeme| &lexeme.kind == kind)
    }

    fn eat(&mut self, kind: &LexemeKind) -> bool {
        if self.at(kind) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn eat_near(&mut self) -> Option<(u32, bool)> {
        let LexemeKind::Near { distance, ordered } = self.peek()?.kind else {
            return None;
        };
        self.position += 1;
        Some((distance, ordered))
    }

    /// Whether the next lexeme could begin an operand, which is what makes an
    /// implicit `AND` an implicit `AND` rather than the end of the query.
    fn at_operand(&self) -> bool {
        matches!(
            self.peek().map(|lexeme| &lexeme.kind),
            Some(
                LexemeKind::Term(_)
                    | LexemeKind::Prefix(_)
                    | LexemeKind::Phrase(_)
                    | LexemeKind::LeftParen
            )
        )
    }
}
#[cfg(test)]
mod tests {
    use super::{Lexeme, LexemeKind, Span, lex, parse, point_at};
    use crate::error::Error;

    /// The lexeme kinds, discarding spans.
    fn kinds(query: &str) -> Vec<LexemeKind> {
        lex(query)
            .expect("query should lex")
            .into_iter()
            .map(|lexeme| lexeme.kind)
            .collect()
    }

    fn term(text: &str) -> LexemeKind {
        LexemeKind::Term(text.to_owned())
    }

    fn phrase(terms: &[&str]) -> LexemeKind {
        LexemeKind::Phrase(terms.iter().map(|&t| t.to_owned()).collect())
    }

    /// The error's byte offset and length.
    fn error_span(query: &str) -> (usize, usize) {
        match lex(query).expect_err("query should be rejected") {
            Error::Query { offset, length, .. } => (offset, length),
            other => panic!("expected a query error, got {other}"),
        }
    }

    #[test]
    fn an_empty_query_lexes_to_nothing() {
        assert!(kinds("").is_empty());
        assert!(kinds("   \t\n  ").is_empty());
    }

    #[test]
    fn a_bare_word_is_a_term() {
        assert_eq!(kinds("quantum"), [term("quantum")]);
    }

    #[test]
    fn terms_are_normalized_the_way_the_index_stores_them() {
        assert_eq!(kinds("QUANTUM"), [term("quantum")]);
        assert_eq!(kinds("Naïve"), [term("naïve")]);
    }

    #[test]
    fn adjacent_words_are_separate_terms() {
        assert_eq!(
            kinds("quantum error correction"),
            [term("quantum"), term("error"), term("correction")]
        );
    }

    #[test]
    fn operators_are_recognised_in_uppercase() {
        assert_eq!(
            kinds("a AND b OR c NOT d"),
            [
                term("a"),
                LexemeKind::And,
                term("b"),
                LexemeKind::Or,
                term("c"),
                LexemeKind::Not,
                term("d"),
            ]
        );
    }

    #[test]
    fn lowercase_operators_are_ordinary_words() {
        // Someone searching for `cats and dogs` means the word.
        assert_eq!(
            kinds("cats and dogs"),
            [term("cats"), term("and"), term("dogs")]
        );
        assert_eq!(kinds("not"), [term("not")]);
        assert_eq!(kinds("Or"), [term("or")]);
    }

    #[test]
    fn parentheses_are_their_own_lexemes() {
        assert_eq!(
            kinds("(a OR b)"),
            [
                LexemeKind::LeftParen,
                term("a"),
                LexemeKind::Or,
                term("b"),
                LexemeKind::RightParen,
            ]
        );
    }

    #[test]
    fn parentheses_do_not_need_spaces_around_them() {
        assert_eq!(
            kinds("(a)"),
            [LexemeKind::LeftParen, term("a"), LexemeKind::RightParen]
        );
    }

    #[test]
    fn double_quotes_make_a_phrase() {
        assert_eq!(
            kinds("\"error correction\""),
            [phrase(&["error", "correction"])]
        );
    }

    #[test]
    fn single_quotes_make_exactly_the_same_phrase() {
        // The whole point: this is what survives a Windows shell.
        assert_eq!(kinds("'error correction'"), kinds("\"error correction\""));
    }

    #[test]
    fn typographic_quotes_work_too() {
        // What a word processor produces if the query was pasted.
        assert_eq!(
            kinds("\u{2019}error correction\u{2019}"),
            [phrase(&["error", "correction"])]
        );
    }

    #[test]
    fn a_one_word_phrase_is_just_that_word() {
        // No point making day 9 do positional work for a single term.
        assert_eq!(kinds("\"quantum\""), [term("quantum")]);
    }

    #[test]
    fn a_phrase_can_sit_inside_a_larger_query() {
        assert_eq!(
            kinds("quantum AND \"error correction\""),
            [
                term("quantum"),
                LexemeKind::And,
                phrase(&["error", "correction"]),
            ]
        );
    }

    #[test]
    fn a_hyphenated_word_becomes_the_phrase_the_index_stored() {
        // The tokenizer split `state-of-the-art` into four terms when indexing,
        // so the query has to ask for those four terms, adjacent.
        assert_eq!(
            kinds("state-of-the-art"),
            [phrase(&["state", "of", "the", "art"])]
        );
    }

    #[test]
    fn an_apostrophe_inside_a_word_does_not_open_a_phrase() {
        // `don't` is one indexed term, and the apostrophe is part of it. What
        // tells the two uses apart is position: a quote that *begins* a lexeme
        // opens a phrase, one inside a word belongs to the word.
        assert_eq!(kinds("don't"), [term("don't")]);
        assert_eq!(
            kinds("it's 'a phrase'"),
            [term("it's"), phrase(&["a", "phrase"])]
        );
        assert_eq!(kinds("dogs'"), [term("dogs")]);
    }

    #[test]
    fn a_trailing_star_makes_a_prefix() {
        assert_eq!(kinds("comp*"), [LexemeKind::Prefix("comp".to_owned())]);
        assert_eq!(
            kinds("quantum AND comp*"),
            [
                term("quantum"),
                LexemeKind::And,
                LexemeKind::Prefix("comp".to_owned()),
            ]
        );
    }

    #[test]
    fn near_carries_its_distance_and_ordering() {
        assert_eq!(
            kinds("a NEAR/3 b"),
            [
                term("a"),
                LexemeKind::Near {
                    distance: 3,
                    ordered: false
                },
                term("b"),
            ]
        );
        assert_eq!(
            kinds("a ONEAR/12 b"),
            [
                term("a"),
                LexemeKind::Near {
                    distance: 12,
                    ordered: true
                },
                term("b"),
            ]
        );
    }

    #[test]
    fn a_whole_query_lexes() {
        assert_eq!(
            kinds("quantum AND (\"error correction\" NEAR/5 surface) NOT class*"),
            [
                term("quantum"),
                LexemeKind::And,
                LexemeKind::LeftParen,
                phrase(&["error", "correction"]),
                LexemeKind::Near {
                    distance: 5,
                    ordered: false
                },
                term("surface"),
                LexemeKind::RightParen,
                LexemeKind::Not,
                LexemeKind::Prefix("class".to_owned()),
            ]
        );
    }

    #[test]
    fn spans_point_at_the_text_they_came_from() {
        let query = "quantum AND \"error correction\"";
        let lexemes = lex(query).expect("lexes");

        assert_eq!(lexemes[0].span.of(query), "quantum");
        assert_eq!(lexemes[1].span.of(query), "AND");
        assert_eq!(lexemes[2].span.of(query), "\"error correction\"");
    }

    #[test]
    fn spans_survive_multi_byte_characters() {
        let query = "naïve AND 量子";
        let lexemes = lex(query).expect("lexes");

        for lexeme in &lexemes {
            // The span must land on character boundaries, or this panics.
            let _ = lexeme.span.of(query);
        }
        assert_eq!(lexemes[0].span.of(query), "naïve");
        assert_eq!(lexemes[1].span.of(query), "AND");
    }

    #[test]
    fn an_unterminated_phrase_points_at_the_opening_quote() {
        let query = "quantum AND \"error correction";
        let (offset, length) = error_span(query);

        assert_eq!(offset, 12, "should point at the quote");
        assert_eq!(&query[offset..offset + 1], "\"");
        assert_eq!(length, query.len() - 12);
    }

    #[test]
    fn near_without_a_distance_points_at_the_operator() {
        let query = "quantum NEAR surface";
        let (offset, length) = error_span(query);

        assert_eq!(&query[offset..offset + length], "NEAR");
    }

    #[test]
    fn a_non_numeric_distance_points_at_the_operator() {
        let query = "quantum NEAR/many surface";
        let (offset, length) = error_span(query);

        assert_eq!(&query[offset..offset + length], "NEAR/many");
    }

    #[test]
    fn a_zero_distance_is_refused() {
        let query = "a NEAR/0 b";
        let (offset, length) = error_span(query);

        assert_eq!(&query[offset..offset + length], "NEAR/0");
    }

    #[test]
    fn a_wildcard_in_the_middle_points_at_the_star() {
        let query = "quantum comp*uter";
        let (offset, length) = error_span(query);

        assert_eq!(&query[offset..offset + length], "*");
        assert_eq!(offset, 12);
    }

    #[test]
    fn a_word_with_nothing_searchable_in_it_is_refused() {
        let (offset, length) = error_span("---");
        assert_eq!((offset, length), (0, 3));
    }

    #[test]
    fn an_empty_phrase_is_refused() {
        let query = "\"   \"";
        let (offset, length) = error_span(query);
        assert_eq!((offset, length), (0, query.len()));
    }

    #[test]
    fn the_caret_lines_up_under_the_offending_text() {
        let rendered = point_at("quantum NEAR surface", 8, 4);
        let lines: Vec<&str> = rendered.lines().collect();

        assert_eq!(lines[0], "quantum NEAR surface");
        assert_eq!(lines[1], "        ^^^^");
        // The caret starts directly under the N.
        assert_eq!(lines[1].find('^'), lines[0].find("NEAR"));
    }

    #[test]
    fn the_caret_counts_characters_not_bytes() {
        // Four bytes of `naïve` precede the operator, but only three
        // characters, so a byte-counted caret would sit one column too far.
        let query = "naïve AND x";
        let lexemes = lex(query).expect("lexes");
        let and = &lexemes[1];

        let rendered = point_at(query, and.span.start, and.span.len());
        let lines: Vec<&str> = rendered.lines().collect();

        assert_eq!(lines[1].find('^'), Some("naïve ".chars().count()));
    }

    #[test]
    fn lexemes_display_as_something_close_to_what_was_written() {
        let query = "quantum AND (\"error correction\" NEAR/5 surface) NOT class*";
        let rendered: Vec<String> = lex(query)
            .expect("lexes")
            .iter()
            .map(Lexeme::to_string)
            .collect();

        assert_eq!(
            rendered.join(" "),
            "quantum AND ( \"error correction\" NEAR/5 surface ) NOT class*"
        );
    }

    #[test]
    fn spans_are_ordered_and_never_overlap() {
        let query = "quantum AND (\"error correction\" NEAR/5 surface) NOT class*";
        let lexemes = lex(query).expect("lexes");

        for pair in lexemes.windows(2) {
            assert!(
                pair[0].span.end <= pair[1].span.start,
                "{:?} overlaps {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn a_span_knows_its_own_length() {
        let span = Span::new(4, 9);
        assert_eq!(span.len(), 5);
        assert!(!span.is_empty());
        assert!(Span::new(3, 3).is_empty());
    }

    // -----------------------------------------------------------------------
    // Day 7: the parser
    // -----------------------------------------------------------------------

    /// The parsed tree, printed fully parenthesized.
    fn tree(query: &str) -> String {
        super::parse(query).expect("query should parse").to_string()
    }

    /// The byte range a parse error points at.
    fn parse_error_span(query: &str) -> (usize, usize) {
        match super::parse(query).expect_err("query should be rejected") {
            Error::Query { offset, length, .. } => (offset, length),
            other => panic!("expected a query error, got {other}"),
        }
    }

    fn parse_message(query: &str) -> String {
        match super::parse(query).expect_err("query should be rejected") {
            Error::Query { message, .. } => message,
            other => panic!("expected a query error, got {other}"),
        }
    }

    #[test]
    fn a_single_term_parses_to_itself() {
        assert_eq!(tree("quantum"), "quantum");
    }

    #[test]
    fn a_prefix_and_a_phrase_parse_to_themselves() {
        assert_eq!(tree("comp*"), "comp*");
        assert_eq!(tree("\"error correction\""), "\"error correction\"");
    }

    #[test]
    fn and_binds_two_terms() {
        assert_eq!(tree("a AND b"), "(a AND b)");
    }

    #[test]
    fn adjacent_terms_are_an_implicit_and() {
        assert_eq!(tree("quantum error"), "(quantum AND error)");
        assert_eq!(tree("a b c"), "((a AND b) AND c)");
    }

    #[test]
    fn or_binds_looser_than_and() {
        // The case the build plan named: `a OR b AND c` must be `a OR (b AND c)`,
        // the same way `+` is looser than `*`.
        assert_eq!(tree("a OR b AND c"), "(a OR (b AND c))");
        assert_eq!(tree("a AND b OR c"), "((a AND b) OR c)");
    }

    #[test]
    fn near_binds_tighter_than_and() {
        assert_eq!(tree("a AND b NEAR/3 c"), "(a AND (b NEAR/3 c))");
        assert_eq!(tree("a NEAR/3 b AND c"), "((a NEAR/3 b) AND c)");
    }

    #[test]
    fn near_binds_tighter_than_or_too() {
        assert_eq!(tree("a OR b NEAR/2 c"), "(a OR (b NEAR/2 c))");
    }

    #[test]
    fn ordered_near_is_a_distinct_operator() {
        assert_eq!(tree("a ONEAR/4 b"), "(a ONEAR/4 b)");
        assert_ne!(tree("a ONEAR/4 b"), tree("a NEAR/4 b"));
    }

    #[test]
    fn not_is_binary_and_sits_with_and() {
        assert_eq!(tree("a NOT b"), "(a NOT b)");
        assert_eq!(tree("a AND b NOT c"), "((a AND b) NOT c)");
        assert_eq!(tree("a NOT b AND c"), "((a NOT b) AND c)");
    }

    #[test]
    fn a_near_operand_that_has_no_position_is_rejected() {
        // `primary` allows a parenthesized group and a group can hold `AND`,
        // so the grammar admits this and the semantics do not.
        for query in [
            "(a AND b) NEAR/3 c",
            "c NEAR/3 (a AND b)",
            "(a NOT b) NEAR/3 c",
            "a ONEAR/2 (b AND c)",
            "((a OR b) AND c) NEAR/1 d",
            "(a AND b) NEAR/3 (c AND d)",
        ] {
            let message = parse_message(query);
            // Asserts the message names the operator that cannot be satisfied
            // and the one at fault, rather than pinning its exact wording.
            assert!(message.contains("NEAR"), "{query}: {message}");
            assert!(
                message.contains("AND") || message.contains("NOT"),
                "{query}: {message}"
            );
        }
    }

    #[test]
    fn the_rejection_underlines_the_group_at_fault() {
        let query = "quantum AND (alpha AND beta) NEAR/3 gamma";
        let Err(Error::Query { offset, length, .. }) = parse(query) else {
            panic!("expected a query error");
        };

        assert_eq!(&query[offset..offset + length], "(alpha AND beta)");
    }

    #[test]
    fn a_near_operand_that_does_have_a_position_is_accepted() {
        // Everything positional, including the shapes that look suspicious.
        for query in [
            "(a OR b) NEAR/3 c",
            "(\"a b\") NEAR/3 c",
            "(a NEAR/1 b) NEAR/3 c",
            "a NEAR/3 b NEAR/3 c",
            "((a OR b) OR \"c d\") ONEAR/2 e",
            "a* NEAR/3 b",
        ] {
            assert!(parse(query).is_ok(), "{query}: {}", parse_message(query));
        }
    }

    #[test]
    fn only_and_and_not_are_non_positional() {
        assert!(parse("a").expect("valid").is_positional());
        assert!(parse("\"a b\"").expect("valid").is_positional());
        assert!(parse("a*").expect("valid").is_positional());
        assert!(parse("a OR b").expect("valid").is_positional());
        assert!(parse("a NEAR/2 b").expect("valid").is_positional());
        assert!(!parse("a AND b").expect("valid").is_positional());
        assert!(!parse("a NOT b").expect("valid").is_positional());
        // An `OR` is only as positional as its operands.
        assert!(!parse("a OR (b AND c)").expect("valid").is_positional());
    }

    #[test]
    fn operators_at_one_level_associate_left() {
        assert_eq!(tree("a AND b AND c"), "((a AND b) AND c)");
        assert_eq!(tree("a OR b OR c"), "((a OR b) OR c)");
        assert_eq!(tree("a NOT b NOT c"), "((a NOT b) NOT c)");
    }

    #[test]
    fn parentheses_override_precedence() {
        assert_eq!(tree("(a OR b) AND c"), "((a OR b) AND c)");
        assert_eq!(tree("a AND (b OR c)"), "(a AND (b OR c))");
        assert_ne!(tree("(a OR b) AND c"), tree("a OR b AND c"));
    }

    #[test]
    fn parentheses_nest() {
        assert_eq!(tree("((a OR b) AND (c OR d))"), "((a OR b) AND (c OR d))");
        assert_eq!(tree("(((a)))"), "a");
    }

    #[test]
    fn a_group_can_be_a_near_operand() {
        assert_eq!(
            tree("(\"error correction\") NEAR/5 surface"),
            "(\"error correction\" NEAR/5 surface)"
        );
    }

    #[test]
    fn a_phrase_can_be_a_near_operand_without_parentheses() {
        // Day 10 has to make this work, so day 7 has to produce it.
        assert_eq!(
            tree("\"machine learning\" NEAR/5 medical"),
            "(\"machine learning\" NEAR/5 medical)"
        );
    }

    #[test]
    fn the_worked_example_from_the_readme_parses() {
        assert_eq!(
            tree("quantum AND (\"error correction\" NEAR/5 surface) NOT class*"),
            "((quantum AND (\"error correction\" NEAR/5 surface)) NOT class*)"
        );
    }

    #[test]
    fn an_implicit_and_mixes_with_explicit_operators() {
        assert_eq!(tree("a b OR c"), "((a AND b) OR c)");
        assert_eq!(tree("a OR b c"), "(a OR (b AND c))");
    }

    #[test]
    fn a_hyphenated_word_parses_as_the_phrase_it_became() {
        assert_eq!(tree("state-of-the-art"), "\"state of the art\"");
    }

    #[test]
    fn parsing_is_idempotent() {
        // Printing a tree and re-parsing it must give the same tree. This is
        // what makes the Display impl trustworthy as evidence of a parse.
        let queries = [
            "quantum",
            "a AND b",
            "a OR b AND c",
            "a NEAR/3 b AND c",
            "(a OR b) AND (c NOT d)",
            "quantum AND (\"error correction\" NEAR/5 surface) NOT class*",
            "a b c d",
            "a ONEAR/9 b OR c*",
        ];

        for query in queries {
            let once = super::parse(query).expect("parses");
            let printed = once.to_string();
            let twice = super::parse(&printed).expect("the printed tree should re-parse");

            assert_eq!(once, twice, "{query:?} printed as {printed:?}");
        }
    }

    #[test]
    fn walking_a_tree_visits_every_node() {
        let expression = super::parse("a AND (b OR c)").expect("parses");

        let mut terms = Vec::new();
        expression.walk(&mut |node| {
            if let super::Expr::Term(term) = node {
                terms.push(term.clone());
            }
        });

        assert_eq!(terms, ["a", "b", "c"]);
        // Three terms plus an AND plus an OR.
        assert_eq!(expression.size(), 5);
    }

    #[test]
    fn an_empty_query_is_refused() {
        assert_eq!(parse_error_span(""), (0, 0));
        assert!(parse_message("   ").contains("empty"));
    }

    #[test]
    fn a_dangling_operator_points_past_the_end() {
        let query = "quantum AND";
        let (offset, length) = parse_error_span(query);

        assert_eq!(offset, query.len(), "should point at the end of the query");
        assert_eq!(length, 0);
        assert!(
            parse_message(query).contains("AND"),
            "{}",
            parse_message(query)
        );
    }

    #[test]
    fn a_dangling_near_points_past_the_end() {
        let query = "quantum NEAR/3";
        assert_eq!(parse_error_span(query), (query.len(), 0));
        assert!(parse_message(query).contains("NEAR"));
    }

    #[test]
    fn a_leading_operator_points_at_the_operator() {
        let query = "AND quantum";
        let (offset, length) = parse_error_span(query);

        assert_eq!(&query[offset..offset + length], "AND");
    }

    #[test]
    fn two_operators_in_a_row_point_at_the_second() {
        let query = "a AND OR b";
        let (offset, length) = parse_error_span(query);

        assert_eq!(&query[offset..offset + length], "OR");
    }

    #[test]
    fn an_unclosed_group_points_at_the_opening_parenthesis() {
        let query = "quantum AND (a OR b";
        let (offset, length) = parse_error_span(query);

        assert_eq!(&query[offset..offset + length], "(");
        assert!(parse_message(query).contains("never closed"));
    }

    #[test]
    fn a_stray_closing_parenthesis_points_at_itself() {
        let query = "quantum) AND b";
        let (offset, length) = parse_error_span(query);

        assert_eq!(&query[offset..offset + length], ")");
        assert!(parse_message(query).contains("no opening one"));
    }

    #[test]
    fn an_empty_group_is_refused() {
        let query = "a AND ()";
        let (offset, length) = parse_error_span(query);

        assert_eq!(&query[offset..offset + length], "()");
        assert!(parse_message(query).contains("empty"));
    }

    #[test]
    fn a_parse_error_span_lands_on_character_boundaries() {
        // If the span were byte-naive, slicing a multi-byte query would panic.
        let query = "naïve AND OR 量子";
        let (offset, length) = parse_error_span(query);

        assert_eq!(&query[offset..offset + length], "OR");
        let _ = point_at(query, offset, length);
    }
}
