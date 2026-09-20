//! Property tests for the tokenizer, plus end-to-end checks against the
//! fixture corpus.
//!
//! The unit tests in `src/tokenize.rs` pin down specific cases — this one
//! asserts the invariants that have to hold for *every* input, and lets
//! `proptest` go looking for the input that breaks them. Those invariants are
//! not decoration: day 4 stores offsets and positions in the index and can
//! never re-derive them, so a tokenizer that miscounts silently corrupts every
//! phrase query built on top of it.

use std::path::Path;

use boolsearch::{JsonlCorpus, Token, tokenize};
use proptest::prelude::*;

/// Every invariant that must hold for any input whatsoever.
fn assert_invariants(text: &str) {
    let tokens: Vec<Token<'_>> = tokenize(text).collect();

    for (ordinal, token) in tokens.iter().enumerate() {
        // The one that matters most: a token is exactly the slice at its own
        // recorded offset. If this ever fails, highlighting points at the
        // wrong characters and nobody notices until a user complains.
        assert_eq!(
            &text[token.offset..token.end()],
            token.text,
            "token {ordinal} is not the slice at its offset, in {text:?}"
        );

        // Positions are the ordinal, with no gaps. Phrase matching compares
        // `position + 1`, so a gap would silently break adjacency.
        assert_eq!(
            token.position as usize, ordinal,
            "token {ordinal} has position {}, in {text:?}",
            token.position
        );

        assert!(!token.text.is_empty(), "empty token {ordinal} in {text:?}");

        assert!(
            !token.text.contains(char::is_whitespace),
            "token {ordinal} contains whitespace: {:?}, in {text:?}",
            token.text
        );

        // Normalization is exactly Rust's `to_lowercase`, no more and no
        // less. "contains no uppercase" is NOT the invariant: U+1F130 🄰 is
        // uppercase with no lowercase mapping, so no correct implementation
        // could satisfy it. Day 4 looks terms up by their normalized form, so
        // what matters is that this agrees with the standard mapping.
        let normalized = token.normalized();
        assert_eq!(
            normalized,
            token.text.to_lowercase(),
            "normalized token {ordinal} disagrees with to_lowercase, in {text:?}"
        );

        // And normalizing twice changes nothing.
        assert_eq!(
            normalized.to_lowercase(),
            normalized.as_ref(),
            "normalization is not idempotent for token {ordinal}, in {text:?}"
        );
    }

    // Offsets strictly increase and tokens never overlap.
    for pair in tokens.windows(2) {
        assert!(
            pair[0].end() <= pair[1].offset,
            "tokens overlap: {:?} then {:?}, in {text:?}",
            pair[0],
            pair[1]
        );
    }
}

proptest! {
    /// Arbitrary Unicode, which is where the interesting failures live:
    /// multi-byte characters, combining marks, unpaired punctuation.
    #[test]
    fn invariants_hold_for_arbitrary_text(text in ".*") {
        assert_invariants(&text);
    }

    /// Weighted towards the characters this tokenizer makes decisions about,
    /// so proptest spends its budget on boundaries rather than on emoji.
    #[test]
    fn invariants_hold_for_awkward_text(
        text in "[a-zA-Z0-9\u{00e0}-\u{00ff}\u{4e00}-\u{4e20}'\u{2019}.,\\-_ \n\t]{0,200}"
    ) {
        assert_invariants(&text);
    }

    /// Tokenizing is a function of the text alone: splitting the input and
    /// tokenizing the halves must find the same terms, as long as the split
    /// lands on whitespace.
    #[test]
    fn whitespace_splits_do_not_change_the_terms(
        left in "[a-z ]{0,50}",
        right in "[a-z ]{0,50}",
    ) {
        let joined = format!("{left} {right}");

        let from_whole: Vec<&str> = tokenize(&joined).map(|token| token.text).collect();
        let from_parts: Vec<&str> = tokenize(&left)
            .chain(tokenize(&right))
            .map(|token| token.text)
            .collect();

        prop_assert_eq!(from_whole, from_parts);
    }

    /// Leading and trailing whitespace shifts offsets but must not change the
    /// terms or their positions.
    #[test]
    fn padding_shifts_offsets_but_not_positions(text in "[a-z ]{0,80}", pad in 0usize..8) {
        let padded = format!("{}{}", " ".repeat(pad), text);

        let bare: Vec<(String, u32)> = tokenize(&text)
            .map(|token| (token.text.to_owned(), token.position))
            .collect();
        let shifted: Vec<(String, u32)> = tokenize(&padded)
            .map(|token| (token.text.to_owned(), token.position))
            .collect();

        prop_assert_eq!(bare, shifted);
    }
}

#[test]
fn the_invariants_hold_over_every_fixture_document() {
    // Real records, including the hard-wrapped one, the CJK one, and the one
    // that exists purely to be awkward.
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join("tiny.jsonl");

    let corpus = JsonlCorpus::open(path).expect("fixture exists");
    let mut documents = 0;
    let mut tokens = 0;

    for document in corpus.map(|item| item.expect("fixture is well formed")) {
        assert_invariants(&document.title);
        assert_invariants(&document.body);

        documents += 1;
        tokens += tokenize(&document.title).count() + tokenize(&document.body).count();
    }

    assert_eq!(documents, 20);
    assert!(tokens > 400, "expected a few hundred tokens, got {tokens}");
}

#[test]
fn the_awkward_fixture_record_tokenizes_as_intended() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join("tiny.jsonl");

    let document = JsonlCorpus::open(path)
        .expect("fixture exists")
        .map(|item| item.expect("fixture is well formed"))
        .find(|document| document.external_id == "0704.0020")
        .expect("the awkward record is present");

    let terms: Vec<&str> = tokenize(&document.body).map(|token| token.text).collect();

    // "it's full of hyphenated terms, apostrophes, e.g. abbreviations,
    //  and numbers like 3.14 and 1,000."
    assert!(terms.contains(&"it's"), "apostrophe held the word together");
    assert!(terms.contains(&"3.14"), "decimal survived");
    assert!(terms.contains(&"1,000"), "thousands separator survived");
    assert!(
        terms.contains(&"e"),
        "e.g. split, since . only joins digits"
    );
    assert!(terms.contains(&"g"));
    assert!(terms.contains(&"hyphenated"));

    // The title is "State-of-the-art: hyphenated terms and apostrophes".
    let title_terms: Vec<&str> = tokenize(&document.title).map(|token| token.text).collect();
    assert_eq!(&title_terms[..4], ["State", "of", "the", "art"]);
}

#[test]
fn the_cjk_fixture_record_is_indexed_one_character_at_a_time() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join("tiny.jsonl");

    let document = JsonlCorpus::open(path)
        .expect("fixture exists")
        .map(|item| item.expect("fixture is well formed"))
        .find(|document| document.external_id == "0704.0018")
        .expect("the Unicode record is present");

    let terms: Vec<&str> = tokenize(&document.title).map(|token| token.text).collect();

    // 量子誤り訂正符号の構成について — fifteen characters, fifteen tokens.
    assert_eq!(terms.len(), document.title.chars().count());
    assert_eq!(&terms[..2], ["量", "子"]);
}
