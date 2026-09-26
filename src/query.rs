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

#[cfg(test)]
mod tests {
    use super::{Lexeme, LexemeKind, Span, lex, point_at};
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
}
