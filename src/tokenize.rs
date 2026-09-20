//! Splitting text into positioned terms.
//!
//! ```text
//! "Surface code quantum error correction"
//!   │       │     │       │      └─ position 4, offset 27
//!   │       │     │       └──────── position 3, offset 21
//!   │       │     └──────────────── position 2, offset 13
//!   │       └────────────────────── position 1, offset 8
//!   └────────────────────────────── position 0, offset 0
//! ```
//!
//! Two numbers per token, and they are not the same number. The **offset** is
//! where the token starts in bytes, which is what highlighting a match in the
//! original text needs. The **position** is the token's ordinal, which is what
//! phrase and proximity queries need — `"error correction"` matches because
//! those two terms sit at consecutive positions, and `a NEAR/3 b` because their
//! positions differ by at most three. Neither can be derived from the other.
//!
//! # Borrowing, not allocating
//!
//! [`Token`] holds a `&'a str` pointing into the text it came from. Tokenizing
//! 2.7 million abstracts yields on the order of 300 million tokens; giving each
//! one its own `String` would mean 300 million allocations to produce data that
//! already exists, verbatim, in memory. The lifetime is what makes this safe:
//! a `Token<'a>` cannot outlive the text it points into, and the compiler will
//! not let you try.
//!
//! This is also why [`crate::Document`] owns its strings. The corpus reader
//! recycles one line buffer across the whole file, so a token borrowed from
//! *that* would be invalidated by the next call to `next()`. Borrowing from a
//! `Document`, which lives as long as the indexer needs it, is safe.
//!
//! # Normalization is separate, and on demand
//!
//! A token keeps the text exactly as it was written, and [`Token::normalized`]
//! lowercases on request — returning [`Cow::Borrowed`] when the source was
//! already lowercase, which in academic abstracts it usually is. Lowercasing
//! eagerly would force an allocation per token and throw away the original
//! casing that snippet highlighting will want.
//!
//! # Decisions, and why
//!
//! - **Stop words are kept.** Dropping "the" would shrink the index, but
//!   `"the cat"` and `a NEAR/2 b` both depend on positions being contiguous and
//!   truthful. A removed stop word leaves a hole that silently corrupts every
//!   distance measured across it.
//! - **No stemming.** It would make `optimise` match `optimising`, but it also
//!   makes `organic` match `organ`, and once the index holds only stems there
//!   is no way to ask for the exact word back. Day 11's prefix wildcards cover
//!   most of what stemming is wanted for, and leave the choice with the user.
//! - **No accent folding.** `naïve` and `naive` stay distinct. Folding is a
//!   language-specific judgement — in French it is usually right, in German
//!   `schön`/`schon` are different words — and the corpus is multilingual.
//! - **Hyphens split.** `state-of-the-art` becomes four tokens, so the phrase
//!   query `"state of the art"` finds it. Keeping it as one token would mean
//!   only the exact hyphenated spelling ever matched.
//! - **Apostrophes join.** `don't` is one token, not `don` and `t`, because
//!   the two halves are not words anyone would search for.
//! - **Decimals and thousands separators join.** `3.14` and `1,000` survive
//!   intact, and so, usefully, do arXiv ids like `0704.0001`.
//! - **CJK characters are indexed one per token.** Chinese and Japanese are
//!   written without spaces, so treating a run of them as one token would index
//!   an entire clause as a single unsearchable term. Proper segmentation needs
//!   a dictionary; single-character indexing needs nothing, and phrase queries
//!   recover multi-character words for free — `量子` is a two-token phrase.

use std::borrow::Cow;

/// A single term, borrowed from the text it was found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'a> {
    /// The term exactly as it appears in the source, with its original casing.
    ///
    /// Always equal to `&source_text[offset..offset + text.len()]`.
    pub text: &'a str,
    /// Byte offset of the term within the text it was tokenized from.
    ///
    /// Bytes, not characters, so it can index the original string directly.
    pub offset: usize,
    /// The term's ordinal within the text, counting from zero.
    ///
    /// This is what phrase and proximity queries compare. Consecutive tokens
    /// always differ by exactly one, whatever punctuation lay between them.
    pub position: u32,
}

impl<'a> Token<'a> {
    /// The term lowercased, borrowing when no change is needed.
    ///
    /// Most terms in a corpus of academic abstracts are already lowercase, so
    /// this usually hands back the original slice and allocates nothing. The
    /// [`Cow`] is what lets one function do both without the caller caring.
    ///
    /// Guaranteed equal to `self.text.to_lowercase()`, always.
    #[must_use]
    pub fn normalized(&self) -> Cow<'a, str> {
        if is_already_lowercase(self.text) {
            Cow::Borrowed(self.text)
        } else {
            Cow::Owned(self.text.to_lowercase())
        }
    }

    /// The byte offset one past the end of the term.
    #[must_use]
    pub const fn end(&self) -> usize {
        self.offset + self.text.len()
    }
}

/// Whether lowercasing `text` would leave it unchanged.
///
/// The tempting test is `!text.chars().any(char::is_uppercase)`, and it is
/// wrong in both directions. `ǅ` (U+01C5) is *titlecase*, so `is_uppercase` is
/// false while `to_lowercase` still maps it to `ǆ` — that version silently
/// failed to normalize it. And `🄰` (U+1F130) is uppercase with no lowercase
/// mapping at all, so that version allocated a copy identical to the original.
///
/// Asking each character whether lowercasing is the identity is exact, and
/// still allocates nothing. Found by a property test on the 170th input.
fn is_already_lowercase(text: &str) -> bool {
    if text.is_ascii() {
        // Almost every term in an English corpus takes this path.
        return !text.bytes().any(|byte| byte.is_ascii_uppercase());
    }

    text.chars().all(|character| {
        let mut lowered = character.to_lowercase();
        lowered.next() == Some(character) && lowered.next().is_none()
    })
}

/// Splits `text` into [`Token`]s.
///
/// Lazy: nothing is scanned until the iterator is advanced, so
/// `tokenize(text).take(5)` reads only as far as the fifth term.
///
/// Returns the concrete [`Tokens`] rather than `impl Iterator` so that callers
/// can name the type — day 4's index builder needs to store one.
#[must_use]
pub fn tokenize(text: &str) -> Tokens<'_> {
    Tokens {
        text,
        offset: 0,
        position: 0,
    }
}

/// The iterator returned by [`tokenize`].
#[derive(Debug, Clone)]
pub struct Tokens<'a> {
    text: &'a str,
    offset: usize,
    position: u32,
}

impl<'a> Tokens<'a> {
    /// The text still to be scanned.
    #[must_use]
    pub fn remainder(&self) -> &'a str {
        &self.text[self.offset..]
    }
}

impl<'a> Iterator for Tokens<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let start = self.skip_to_token_start()?;
        let first = self.text[start..]
            .chars()
            .next()
            .expect("skip_to_token_start returned the offset of a real character");

        let end = if is_cjk(first) {
            // One character, one token. See the module documentation.
            start + first.len_utf8()
        } else {
            scan_word_end(self.text, start)
        };

        let token = Token {
            text: &self.text[start..end],
            offset: start,
            position: self.position,
        };

        self.position += 1;
        self.offset = end;

        Some(token)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // At most one token per remaining byte, and possibly none at all.
        (0, Some(self.text.len() - self.offset))
    }
}

impl Tokens<'_> {
    /// Advances past anything that cannot begin a token, returning where one
    /// does, or `None` at the end of the text.
    fn skip_to_token_start(&mut self) -> Option<usize> {
        let bytes = self.text.as_bytes();

        while self.offset < bytes.len() {
            let byte = bytes[self.offset];

            // ASCII fast path. Most of a corpus is ASCII, and deciding a single
            // byte avoids decoding a character to learn it is a space.
            if byte.is_ascii() {
                if byte.is_ascii_alphanumeric() {
                    return Some(self.offset);
                }
                self.offset += 1;
                continue;
            }

            let character = self.text[self.offset..]
                .chars()
                .next()
                .expect("offset is on a character boundary");

            if character.is_alphanumeric() {
                return Some(self.offset);
            }

            self.offset += character.len_utf8();
        }

        None
    }
}

/// Finds where the word starting at `start` ends.
///
/// Two implementations of one rule. Corpus text is overwhelmingly ASCII, and
/// for ASCII a byte *is* a character, so the common path never decodes UTF-8 at
/// all. The moment a non-ASCII byte appears the whole word is re-scanned by the
/// character-aware version — wasteful for that word, and irrelevant, because
/// almost no word takes that path.
fn scan_word_end(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut index = start;
    let mut end = start;

    while index < bytes.len() {
        let byte = bytes[index];

        if !byte.is_ascii() {
            return scan_word_end_unicode(text, start);
        }

        if byte.is_ascii_alphanumeric() {
            index += 1;
            end = index;
            continue;
        }

        let next = bytes.get(index + 1).copied();
        if next.is_none_or(|byte| !byte.is_ascii()) {
            // Whatever follows needs a real character to judge it.
            return scan_word_end_unicode(text, start);
        }

        // `scan_word_end` is only ever called with an alphanumeric at `start`,
        // so by the time a connector is reached there is always a byte behind.
        let previous = bytes[index - 1];
        if joins_a_word(
            byte as char,
            Some(previous as char),
            next.map(|byte| byte as char),
        ) {
            index += 1;
            continue;
        }

        break;
    }

    end
}

/// The character-aware word scan, for words containing anything but ASCII.
fn scan_word_end_unicode(text: &str, start: usize) -> usize {
    let mut end = start;
    let mut previous = None;
    let mut characters = text[start..].char_indices().peekable();

    while let Some((index, character)) = characters.next() {
        if is_cjk(character) {
            break; // a CJK character always begins a token of its own
        }

        if character.is_alphanumeric() {
            end = start + index + character.len_utf8();
            previous = Some(character);
            continue;
        }

        let next = characters.peek().map(|&(_, character)| character);
        if joins_a_word(character, previous, next) {
            // Deliberately does not move `end`: a trailing connector with no
            // word after it is not part of the token, so `dogs'` ends at the s.
            previous = Some(character);
            continue;
        }

        break;
    }

    end
}

/// Whether `character` holds a word together rather than ending it.
///
/// Requires the right sort of neighbour on *both* sides, which is what makes
/// `don't` one token while `dogs'` is just `dogs` and `'quoted'` is `quoted`.
fn joins_a_word(character: char, previous: Option<char>, next: Option<char>) -> bool {
    let (Some(previous), Some(next)) = (previous, next) else {
        return false;
    };

    match character {
        // Straight and typographic apostrophes, between letters.
        '\'' | '\u{2019}' => previous.is_alphabetic() && next.is_alphabetic(),
        // Decimal points and thousands separators, between digits only — so
        // `3.14` survives but `e.g.` is still two tokens.
        '.' | ',' => previous.is_ascii_digit() && next.is_ascii_digit(),
        _ => false,
    }
}

/// Whether a character belongs to a script written without spaces between
/// words, and so should be indexed one character per token.
///
/// A range check rather than a Unicode property lookup: the relevant blocks are
/// contiguous, and this runs once per character of a multi-gigabyte corpus.
const fn is_cjk(character: char) -> bool {
    matches!(
        character as u32,
        0x3040..=0x30FF      // Hiragana and Katakana
        | 0x3400..=0x4DBF    // CJK Unified Ideographs Extension A
        | 0x4E00..=0x9FFF    // CJK Unified Ideographs
        | 0xAC00..=0xD7AF    // Hangul syllables
        | 0xF900..=0xFAFF    // CJK Compatibility Ideographs
        | 0x20000..=0x2FA1F  // Extensions B onwards, and the compatibility supplement
    )
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::{Token, tokenize};

    /// The token texts, discarding offsets and positions.
    fn terms(text: &str) -> Vec<&str> {
        tokenize(text).map(|token| token.text).collect()
    }

    /// The normalized token texts.
    fn normalized(text: &str) -> Vec<String> {
        tokenize(text)
            .map(|token| token.normalized().into_owned())
            .collect()
    }

    #[test]
    fn a_plain_sentence_splits_on_spaces() {
        assert_eq!(
            terms("Surface code quantum error correction"),
            ["Surface", "code", "quantum", "error", "correction"]
        );
    }

    #[test]
    fn positions_count_tokens_and_offsets_count_bytes() {
        let tokens: Vec<Token<'_>> = tokenize("Surface code quantum").collect();

        assert_eq!(tokens[0].position, 0);
        assert_eq!(tokens[1].position, 1);
        assert_eq!(tokens[2].position, 2);

        assert_eq!(tokens[0].offset, 0);
        assert_eq!(tokens[1].offset, 8);
        assert_eq!(tokens[2].offset, 13);
    }

    #[test]
    fn positions_stay_consecutive_across_punctuation() {
        // However much punctuation sits between two words, they are adjacent
        // positions — which is what makes `"error correction"` match here.
        let tokens: Vec<Token<'_>> = tokenize("error --- ,,, correction").collect();

        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].position, 0);
        assert_eq!(tokens[1].position, 1);
    }

    #[test]
    fn every_token_is_the_slice_at_its_own_offset() {
        let text = "Naïve  Bayes, 3.14 — 量子 don't";
        for token in tokenize(text) {
            assert_eq!(&text[token.offset..token.end()], token.text);
        }
    }

    #[test]
    fn empty_and_punctuation_only_input_yields_nothing() {
        assert!(terms("").is_empty());
        assert!(terms("   \n\t  ").is_empty());
        assert!(terms("--- ,,, !!! ...").is_empty());
    }

    #[test]
    fn hyphens_split_so_phrase_queries_can_find_the_parts() {
        assert_eq!(terms("state-of-the-art"), ["state", "of", "the", "art"]);
        assert_eq!(terms("well-known"), ["well", "known"]);
        assert_eq!(
            terms("--leading and trailing--"),
            ["leading", "and", "trailing"]
        );
    }

    #[test]
    fn apostrophes_hold_a_word_together() {
        assert_eq!(terms("don't"), ["don't"]);
        assert_eq!(terms("it's Kayan's"), ["it's", "Kayan's"]);

        // The typographic apostrophe behaves the same way.
        assert_eq!(terms("don\u{2019}t"), ["don\u{2019}t"]);
    }

    #[test]
    fn an_apostrophe_without_a_word_on_both_sides_is_a_boundary() {
        assert_eq!(terms("dogs'"), ["dogs"]);
        assert_eq!(terms("'quoted'"), ["quoted"]);
        assert_eq!(terms("rock 'n' roll"), ["rock", "n", "roll"]);
    }

    #[test]
    fn digits_keep_their_decimal_points_and_separators() {
        assert_eq!(terms("3.14"), ["3.14"]);
        assert_eq!(terms("1,000,000"), ["1,000,000"]);
        assert_eq!(terms("0704.0001"), ["0704.0001"]);

        // Only between digits, so an abbreviation still splits.
        assert_eq!(terms("e.g."), ["e", "g"]);
        assert_eq!(terms("end. Next"), ["end", "Next"]);
    }

    #[test]
    fn accented_letters_are_ordinary_letters() {
        assert_eq!(terms("naïve Bayes"), ["naïve", "Bayes"]);
        assert_eq!(
            terms("Schrödinger's équation"),
            ["Schrödinger's", "équation"]
        );

        // Deliberately not folded: naïve and naive are different terms.
        assert_ne!(normalized("naïve"), normalized("naive"));
    }

    #[test]
    fn cjk_characters_are_one_token_each() {
        assert_eq!(terms("量子誤り"), ["量", "子", "誤", "り"]);

        // So a two-character word is found as a two-token phrase.
        let tokens: Vec<Token<'_>> = tokenize("量子誤り").collect();
        assert_eq!(tokens[0].position, 0);
        assert_eq!(tokens[1].position, 1);
    }

    #[test]
    fn cjk_and_latin_can_be_mixed() {
        assert_eq!(
            terms("量子 quantum 誤り"),
            ["量", "子", "quantum", "誤", "り"]
        );
        assert_eq!(terms("ABC量子"), ["ABC", "量", "子"]);
        assert_eq!(terms("量子ABC"), ["量", "子", "ABC"]);
    }

    #[test]
    fn normalization_lowercases_without_touching_the_original() {
        let tokens: Vec<Token<'_>> = tokenize("Surface CODE").collect();

        assert_eq!(tokens[0].text, "Surface");
        assert_eq!(tokens[0].normalized(), "surface");
        assert_eq!(tokens[1].normalized(), "code");
    }

    #[test]
    fn already_lowercase_terms_are_not_allocated() {
        // The point of Cow here: the common case hands back the original slice.
        let token = tokenize("surface").next().expect("one token");
        assert!(matches!(token.normalized(), Cow::Borrowed(_)));

        let token = tokenize("Surface").next().expect("one token");
        assert!(matches!(token.normalized(), Cow::Owned(_)));
    }

    #[test]
    fn titlecase_characters_are_lowercased() {
        // U+01C5 is category Lt, so `is_uppercase()` is false even though it
        // has a lowercase mapping. Checking `is_uppercase` missed this.
        assert_eq!(normalized("\u{01C5}"), ["\u{01C6}"]);
    }

    #[test]
    fn uppercase_characters_without_a_lowercase_mapping_are_left_alone() {
        // U+1F130 SQUARED LATIN CAPITAL LETTER A is uppercase, but lowercasing
        // it is the identity, so there is nothing to allocate.
        let token = tokenize("\u{1F130}").next().expect("one token");
        assert_eq!(token.normalized(), "\u{1F130}");
        assert!(matches!(token.normalized(), Cow::Borrowed(_)));
    }

    #[test]
    fn normalization_always_agrees_with_to_lowercase() {
        for text in [
            "surface",
            "Surface",
            "\u{01C5}",
            "\u{1F130}",
            "\u{1E9E}",
            "\u{0130}",
            "\u{03A3}",
        ] {
            let token = tokenize(text).next().expect("one token");
            assert_eq!(
                token.normalized(),
                text.to_lowercase(),
                "disagreed on {text:?}"
            );
        }
    }

    #[test]
    fn normalization_handles_non_ascii_case() {
        assert_eq!(normalized("ÉQUATION Straße"), ["équation", "straße"]);
    }

    #[test]
    fn tokens_borrow_from_the_input_rather_than_copying_it() {
        let text = String::from("borrowed slice");
        let token = tokenize(&text).next().expect("one token");

        // Same memory, not a copy: the token's bytes are the input's bytes.
        assert!(std::ptr::eq(token.text.as_ptr(), text.as_ptr()));
    }

    #[test]
    fn newlines_inside_a_body_are_just_separators() {
        // Corpus bodies keep their hard wrapping, so this is the normal case.
        assert_eq!(
            terms("quantum chromodynamics is\npresented for the production"),
            [
                "quantum",
                "chromodynamics",
                "is",
                "presented",
                "for",
                "the",
                "production"
            ]
        );
    }

    #[test]
    fn stop_words_are_kept_so_positions_stay_truthful() {
        let tokens: Vec<Token<'_>> = tokenize("the cat sat on the mat").collect();

        assert_eq!(tokens.len(), 6);
        assert_eq!(tokens[1].text, "cat");
        // `cat` and `sat` are adjacent, so `"cat sat"` is a phrase match.
        assert_eq!(tokens[2].position - tokens[1].position, 1);
    }

    #[test]
    fn the_iterator_is_lazy() {
        let mut tokens = tokenize("one two three four five");
        let first = tokens.next().expect("a token");

        assert_eq!(first.text, "one");
        assert_eq!(tokens.remainder(), " two three four five");
    }
}
