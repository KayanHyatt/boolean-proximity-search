//! End-to-end tests against the committed fixture corpora.
//!
//! These read real files from `tests/data/`, which the unit tests in
//! `src/corpus.rs` deliberately do not — those work on in-memory strings so
//! they stay fast and independent of the filesystem. What is being checked here
//! is the part only a real file can exercise: opening it, reading it through a
//! `BufReader`, and surviving the shape of an actual arXiv record.

use std::path::{Path, PathBuf};

use boolsearch::{DocId, DocStore, Document, Error, JsonlCorpus};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join(name)
}

fn load(name: &str) -> Vec<Document> {
    JsonlCorpus::open(fixture(name))
        .expect("fixture exists")
        .map(|item| item.expect("fixture is well formed"))
        .collect()
}

#[test]
fn the_tiny_corpus_loads_every_record() {
    let documents = load("tiny.jsonl");
    assert_eq!(documents.len(), 20);
}

#[test]
fn document_ids_are_dense_and_start_at_zero() {
    let documents = load("tiny.jsonl");

    for (position, document) in documents.iter().enumerate() {
        assert_eq!(
            document.id,
            DocId::new(u32::try_from(position).unwrap()),
            "document {position} has the wrong id"
        );
    }
}

#[test]
fn arxiv_ids_survive_the_round_trip() {
    let documents = load("tiny.jsonl");

    assert_eq!(documents[0].external_id, "0704.0001");
    assert_eq!(documents[19].external_id, "0704.0020");
}

#[test]
fn the_fields_the_engine_ignores_do_not_break_parsing() {
    // Every fixture record carries submitter, authors, journal-ref, report-no,
    // versions and authors_parsed — including nulls and nested arrays.
    let documents = load("tiny.jsonl");

    assert_eq!(
        documents[0].title,
        "Calculation of prompt diphoton production cross sections at Tevatron and LHC energies"
    );
    assert!(
        documents[0]
            .body
            .starts_with("A fully differential calculation")
    );
}

#[test]
fn no_document_keeps_the_typesetting_whitespace_arxiv_stores() {
    // The real dump pads abstracts with leading spaces and hard-wraps both
    // titles and abstracts at roughly eighty columns. None of that should
    // survive into a Document.
    let documents = load("tiny.jsonl");

    for document in &documents {
        for (field, text) in [("title", &document.title), ("body", &document.body)] {
            assert!(
                !text.starts_with(char::is_whitespace) && !text.ends_with(char::is_whitespace),
                "{} {field} has whitespace at an end: {text:?}",
                document.external_id
            );
            assert!(
                !text.contains('\n') && !text.contains('\r') && !text.contains('\t'),
                "{} {field} still contains a line break: {text:?}",
                document.external_id
            );
            assert!(
                !text.contains("  "),
                "{} {field} still contains a double space: {text:?}",
                document.external_id
            );
        }
    }
}

#[test]
fn a_hard_wrapped_title_is_rejoined_into_one_line() {
    // Fixture record 0704.0001 is stored exactly as the real dump stores it,
    // with the title broken across two lines.
    let documents = load("tiny.jsonl");

    assert_eq!(
        documents[0].title,
        "Calculation of prompt diphoton production cross sections at Tevatron and LHC energies"
    );
    assert_eq!(
        documents[0].body,
        "A fully differential calculation in perturbative quantum chromodynamics is \
         presented for the production of photon pairs at hadron colliders."
    );
}

#[test]
fn non_ascii_records_survive_intact() {
    let documents = load("tiny.jsonl");
    let japanese = documents
        .iter()
        .find(|document| document.external_id == "0704.0018")
        .expect("the Unicode fixture record is present");

    assert_eq!(japanese.title, "量子誤り訂正符号の構成について");
    assert!(japanese.title.chars().count() < japanese.title.len());
}

#[test]
fn a_doc_store_built_from_the_corpus_maps_ids_back_to_titles() {
    let documents = load("tiny.jsonl");
    let mut store = DocStore::with_capacity(documents.len());
    for document in &documents {
        store.push(document);
    }

    assert_eq!(store.len(), 20);

    let meta = store.get(DocId::new(2)).expect("third document exists");
    assert_eq!(meta.external_id, "0704.0003");
    assert_eq!(
        meta.title,
        "Surface code quantum error correction at threshold"
    );

    assert!(store.get(DocId::new(20)).is_none());
}

#[test]
fn the_damaged_corpus_yields_errors_without_losing_the_good_records() {
    let items: Vec<_> = JsonlCorpus::open(fixture("malformed.jsonl"))
        .expect("fixture exists")
        .collect();

    let good: Vec<_> = items.iter().filter(|item| item.is_ok()).collect();
    let bad: Vec<_> = items.iter().filter(|item| item.is_err()).collect();

    // Three intact records, one truncated line, one record missing `abstract`.
    // The blank line is skipped silently and is neither.
    assert_eq!(good.len(), 3, "good records");
    assert_eq!(bad.len(), 2, "bad records");
}

#[test]
fn malformed_records_name_the_file_and_the_line() {
    let error = JsonlCorpus::open(fixture("malformed.jsonl"))
        .expect("fixture exists")
        .find_map(std::result::Result::err)
        .expect("the fixture contains a malformed record");

    let Error::MalformedRecord { path, line, .. } = &error else {
        panic!("expected a malformed-record error, got {error:?}");
    };

    assert_eq!(*line, 3, "the truncated JSON is on line 3");
    assert!(path.ends_with("malformed.jsonl"));

    let rendered = error.to_string();
    assert!(rendered.contains("malformed.jsonl:3"), "got {rendered}");
}

#[test]
fn skipping_bad_records_keeps_ids_dense() {
    let documents: Vec<_> = JsonlCorpus::open(fixture("malformed.jsonl"))
        .expect("fixture exists")
        .filter_map(std::result::Result::ok)
        .collect();

    let ids: Vec<DocId> = documents.iter().map(|document| document.id).collect();
    assert_eq!(ids, vec![DocId::FIRST, DocId::new(1), DocId::new(2)]);
}

#[test]
fn byte_counts_match_the_file_on_disk() {
    let path = fixture("tiny.jsonl");
    let expected = std::fs::metadata(&path).expect("fixture exists").len();

    let mut corpus = JsonlCorpus::open(&path).expect("fixture exists");
    let count = corpus.by_ref().count();

    assert_eq!(count, 20);
    assert_eq!(corpus.bytes_read(), expected);
}

#[test]
fn opening_a_missing_corpus_names_the_file() {
    let error = JsonlCorpus::open("does-not-exist.jsonl").expect_err("the file is absent");

    let Error::Io { path, .. } = &error else {
        panic!("expected an io error, got {error:?}");
    };
    assert_eq!(path, Path::new("does-not-exist.jsonl"));

    // The point of carrying the path: the message says which file.
    assert!(error.to_string().contains("does-not-exist.jsonl"));
}

#[test]
fn a_limit_stops_the_stream_early_without_reading_the_rest() {
    let mut corpus = JsonlCorpus::open(fixture("tiny.jsonl")).expect("fixture exists");
    let first_five: Vec<_> = corpus.by_ref().take(5).collect();

    assert_eq!(first_five.len(), 5);
    assert!(
        corpus.lines_read() <= 5,
        "taking five documents should not have read the whole file, read {} lines",
        corpus.lines_read()
    );
}
