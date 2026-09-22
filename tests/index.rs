//! Index tests against the fixture corpus, checked by a brute-force reference.
//!
//! The unit tests in `src/index.rs` verify chosen facts about a three-document
//! corpus. This file does something different and more valuable: it builds the
//! index the fast way and the obvious way, and asserts they agree.
//!
//! The reference implementation is a `BTreeMap<String, BTreeMap<DocId,
//! Vec<u32>>>` — the shape the build plan originally called for, and the shape
//! that does not survive 436 million tokens. Here, over twenty documents, it is
//! perfect: it is slow and fat and obviously correct, it shares no code with
//! the compressed-sparse-row layout, and so it cannot share its bugs. An
//! off-by-one in a prefix sum would show up immediately.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use boolsearch::{DocId, Document, FIELD_GAP, Index, IndexBuilder, JsonlCorpus, Posting, tokenize};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join("tiny.jsonl")
}

fn documents() -> Vec<Document> {
    JsonlCorpus::open(fixture())
        .expect("fixture exists")
        .map(|item| item.expect("fixture is well formed"))
        .collect()
}

fn index_of(documents: &[Document]) -> Index {
    let mut builder = IndexBuilder::new();
    for document in documents {
        builder.add_document(document);
    }

    let mut assembler = builder.finish_counting().expect("fixture is small");
    for document in documents {
        assembler.add_document(document);
    }

    assembler.finish().expect("both passes saw the same corpus")
}

/// The whole index, built the slow obvious way.
type Reference = BTreeMap<String, BTreeMap<DocId, Vec<u32>>>;

fn reference_of(documents: &[Document]) -> Reference {
    let mut reference = Reference::new();

    for document in documents {
        // Deliberately re-derives the title/body position arithmetic rather
        // than calling into the index's own helper. If both used the same
        // function, agreeing would prove nothing about it.
        let mut after_title = 0;
        for token in tokenize(&document.title) {
            reference
                .entry(token.normalized().into_owned())
                .or_default()
                .entry(document.id)
                .or_default()
                .push(token.position);
            after_title = token.position + 1;
        }

        let offset = after_title + FIELD_GAP;
        for token in tokenize(&document.body) {
            reference
                .entry(token.normalized().into_owned())
                .or_default()
                .entry(document.id)
                .or_default()
                .push(token.position + offset);
        }
    }

    reference
}

#[test]
fn the_index_agrees_with_a_brute_force_scan_on_every_term() {
    let documents = documents();
    let index = index_of(&documents);
    let reference = reference_of(&documents);

    assert_eq!(index.terms(), reference.len(), "vocabulary size");

    for (term, expected_postings) in &reference {
        let id = index
            .term_id(term)
            .unwrap_or_else(|| panic!("{term:?} is missing from the index"));

        let actual: Vec<Posting<'_>> = index.postings(id).collect();

        assert_eq!(
            actual.len(),
            expected_postings.len(),
            "{term:?} has the wrong document frequency"
        );

        for (posting, (expected_doc, expected_positions)) in actual.iter().zip(expected_postings) {
            assert_eq!(posting.doc_id, *expected_doc, "{term:?} document mismatch");
            assert_eq!(
                posting.positions,
                &expected_positions[..],
                "{term:?} positions in {expected_doc} mismatch"
            );
        }
    }
}

#[test]
fn the_index_holds_no_terms_the_corpus_does_not_contain() {
    let documents = documents();
    let index = index_of(&documents);
    let reference = reference_of(&documents);

    for (term, _) in index.vocabulary() {
        assert!(
            reference.contains_key(term),
            "{term:?} is in the index but not in the corpus"
        );
    }
}

#[test]
fn totals_match_the_reference() {
    let documents = documents();
    let index = index_of(&documents);
    let reference = reference_of(&documents);
    let stats = index.stats();

    let postings: usize = reference.values().map(BTreeMap::len).sum();
    let positions: usize = reference
        .values()
        .flat_map(BTreeMap::values)
        .map(Vec::len)
        .sum();

    assert_eq!(stats.documents, documents.len());
    assert_eq!(stats.terms, reference.len());
    assert_eq!(stats.postings, postings);
    assert_eq!(stats.positions, positions);
}

#[test]
fn postings_are_sorted_by_document_throughout() {
    let index = index_of(&documents());

    for (term, id) in index.vocabulary() {
        let docs = index.doc_ids(id);
        assert!(
            docs.windows(2).all(|pair| pair[0] < pair[1]),
            "{term:?} postings are not ascending: {docs:?}"
        );
    }
}

#[test]
fn positions_are_sorted_within_every_posting() {
    let index = index_of(&documents());

    for (term, id) in index.vocabulary() {
        for posting in index.postings(id) {
            assert!(
                !posting.positions.is_empty(),
                "{term:?} has an empty posting in {}",
                posting.doc_id
            );
            assert!(
                posting.positions.windows(2).all(|pair| pair[0] < pair[1]),
                "{term:?} positions in {} are not ascending: {:?}",
                posting.doc_id,
                posting.positions
            );
        }
    }
}

#[test]
fn a_known_term_lands_in_the_documents_it_should() {
    let documents = documents();
    let index = index_of(&documents);

    // Four fixture records are about quantum error correction.
    let quantum = index.term_id("quantum").expect("indexed");
    let titles: Vec<&str> = index
        .doc_ids(quantum)
        .iter()
        .map(|&id| documents[id.as_usize()].title.as_str())
        .collect();

    assert!(titles.len() >= 4, "expected several, got {titles:?}");
    assert!(
        titles
            .iter()
            .any(|title| title.contains("Surface code quantum error correction")),
        "got {titles:?}"
    );
}

#[test]
fn the_cjk_record_is_indexed_one_character_per_term() {
    let index = index_of(&documents());

    // Record 0704.0018's title is 量子誤り訂正符号の構成について.
    let first = index.term_id("量").expect("量 is a term of its own");
    let second = index.term_id("子").expect("子 is a term of its own");

    let positions = index.positions(first, 0);
    let next = index.positions(second, 0);

    assert_eq!(positions, [0]);
    assert_eq!(next, [1]);
    // Adjacent positions, so day 9 can match 量子 as a two-term phrase.
}

#[test]
fn nothing_is_indexed_inside_the_gap_between_title_and_body() {
    // A stronger claim than "the join is wide": the positions between the last
    // title token and the first body token hold nothing at all, so no phrase or
    // proximity match can bridge them.
    //
    // (An earlier version of this test compared the last title *term* with the
    // first body *term* and failed on record 0704.0003, where "threshold"
    // appears in both fields. The term was right; the occurrence was not.)
    let documents = documents();
    let index = index_of(&documents);

    for document in &documents {
        let title_tokens = tokenize(&document.title).count() as u32;
        if title_tokens == 0 || tokenize(&document.body).next().is_none() {
            continue;
        }

        let gap = title_tokens..title_tokens + FIELD_GAP;

        for (term, id) in index.vocabulary() {
            let Some(posting) = index
                .postings(id)
                .find(|posting| posting.doc_id == document.id)
            else {
                continue;
            };

            for &position in posting.positions {
                assert!(
                    !gap.contains(&position),
                    "{}: {term:?} sits at {position}, inside the empty gap {gap:?}",
                    document.external_id
                );
            }
        }
    }
}

#[test]
fn the_body_starts_exactly_one_gap_after_the_title() {
    let documents = documents();
    let index = index_of(&documents);

    for document in &documents {
        let title_tokens = tokenize(&document.title).count() as u32;
        let Some(first_body_token) = tokenize(&document.body).next() else {
            continue;
        };

        let term = index
            .term_id(&first_body_token.normalized())
            .expect("indexed");
        let positions = positions_in(&index, term, document.id);

        assert!(
            positions.contains(&(title_tokens + FIELD_GAP)),
            "{}: the first body term {:?} is not at {}, positions are {positions:?}",
            document.external_id,
            first_body_token.text,
            title_tokens + FIELD_GAP
        );
    }
}

#[test]
fn a_term_appearing_many_times_in_one_document_is_one_posting() {
    let documents = documents();
    let index = index_of(&documents);

    for (term, id) in index.vocabulary() {
        let docs = index.doc_ids(id);
        let distinct: std::collections::BTreeSet<DocId> = docs.iter().copied().collect();
        assert_eq!(
            docs.len(),
            distinct.len(),
            "{term:?} has a document listed twice"
        );
    }
}

#[test]
fn an_index_of_nothing_is_valid_and_empty() {
    let index = index_of(&[]);
    let stats = index.stats();

    assert_eq!(stats.documents, 0);
    assert_eq!(stats.terms, 0);
    assert_eq!(stats.postings, 0);
    assert_eq!(stats.positions, 0);
    assert!(index.postings_for("quantum").is_none());
}

/// The positions of `term` within one document, or empty if it is absent.
fn positions_in(index: &Index, term: boolsearch::TermId, document: DocId) -> Vec<u32> {
    index
        .postings(term)
        .find(|posting| posting.doc_id == document)
        .map(|posting| posting.positions.to_vec())
        .unwrap_or_default()
}
