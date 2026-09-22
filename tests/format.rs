//! The on-disk format, exercised against the real fixture corpus and a real
//! file on disk.
//!
//! `src/format.rs` covers the layout in isolation, on a four-document corpus
//! written for the purpose. This file uses the twenty fixture records — hard
//! wrapping, CJK, apostrophes, decimals — and checks that what comes back off
//! disk still agrees with the brute-force reference from `tests/index.rs`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use boolsearch::{DocId, DocStore, Document, FIELD_GAP, JsonlCorpus, SearchIndex, tokenize};

fn documents() -> Vec<Document> {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join("tiny.jsonl");

    JsonlCorpus::open(fixture)
        .expect("fixture exists")
        .map(|item| item.expect("fixture is well formed"))
        .collect()
}

fn build(documents: &[Document]) -> SearchIndex {
    let mut builder = boolsearch::IndexBuilder::new();
    for document in documents {
        builder.add_document(document);
    }

    let mut assembler = builder.finish_counting().expect("fixture is small");
    let mut store = DocStore::with_capacity(documents.len());
    for document in documents {
        assembler.add_document(document);
        store.push(document);
    }

    SearchIndex::new(assembler.finish().expect("consistent"), store)
}

/// A scratch directory unique to this test binary.
fn scratch(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("boolsearch-format-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("scratch directory");
    directory.join("index.bin")
}

#[test]
fn the_fixture_index_survives_a_trip_through_a_file() {
    let documents = documents();
    let path = scratch("roundtrip");

    build(&documents).save(&path).expect("save");
    let loaded = SearchIndex::load(&path).expect("load");

    // The same brute-force reference tests/index.rs uses, rebuilt here so the
    // check is against the corpus rather than against the in-memory index.
    let mut reference: BTreeMap<String, BTreeMap<DocId, Vec<u32>>> = BTreeMap::new();
    for document in &documents {
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

    assert_eq!(loaded.index.terms(), reference.len());

    for (term, expected) in &reference {
        let id = loaded
            .index
            .term_id(term)
            .unwrap_or_else(|| panic!("{term:?} was lost in the round trip"));

        let actual: Vec<_> = loaded.index.postings(id).collect();
        assert_eq!(actual.len(), expected.len(), "{term:?} document frequency");

        for (posting, (document, positions)) in actual.iter().zip(expected) {
            assert_eq!(posting.doc_id, *document, "{term:?}");
            assert_eq!(posting.positions, &positions[..], "{term:?} in {document}");
        }
    }

    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn titles_come_back_intact_including_the_awkward_ones() {
    let documents = documents();
    let path = scratch("titles");

    build(&documents).save(&path).expect("save");
    let loaded = SearchIndex::load(&path).expect("load");

    for document in &documents {
        let meta = loaded
            .documents
            .get(document.id)
            .unwrap_or_else(|| panic!("{} is missing", document.external_id));

        assert_eq!(meta.external_id, document.external_id);
        assert_eq!(meta.title, document.title);
    }

    // The two records that exist to be difficult.
    let cjk = loaded.documents.get(DocId::new(17)).expect("present");
    assert_eq!(cjk.title, "量子誤り訂正符号の構成について");

    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn the_file_is_exactly_the_size_its_sections_imply() {
    // Stronger, and more useful, than "smaller than memory" — which would be
    // comparing different things anyway, since the file carries the document
    // store and `IndexStats::bytes` does not.
    //
    // Every byte in the file belongs to a section whose size is known, so the
    // total is predictable to the byte. If this ever drifts, the format has
    // grown padding or overhead that nobody decided to add.
    let documents = documents();
    let path = scratch("size");

    let index = build(&documents);
    index.save(&path).expect("save");

    let stats = index.index.stats();
    let terms = stats.terms;
    let postings = stats.postings;
    let positions = stats.positions;
    let count = documents.len();

    let term_bytes: usize = index.index.vocabulary().map(|(term, _)| term.len()).sum();
    let external_bytes: usize = documents.iter().map(|d| d.external_id.len()).sum();
    let title_bytes: usize = documents.iter().map(|d| d.title.len()).sum();

    let expected = 48                                   // header (magic, version, reserved, 8 counts)
        + (terms + 1) * 4 + term_bytes + terms * 4      // dictionary
        + (terms + 1) * 4                               // term_postings_start
        + postings * 4                                  // posting_docs
        + (postings + 1) * 4                            // posting_positions_start
        + positions * 4                                 // positions
        + (count + 1) * 4 + external_bytes              // arXiv ids
        + (count + 1) * 4 + title_bytes; // titles

    let actual = std::fs::metadata(&path).expect("written").len() as usize;
    assert_eq!(actual, expected, "the file has unaccounted-for bytes");

    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
fn a_corrupted_byte_in_the_middle_does_not_go_unnoticed() {
    let documents = documents();
    let path = scratch("corrupt");

    build(&documents).save(&path).expect("save");
    let mut bytes = std::fs::read(&path).expect("read back");

    // Scribble over one of the count fields in the header.
    bytes[24] = bytes[24].wrapping_add(17);
    std::fs::write(&path, &bytes).expect("write back");

    assert!(
        SearchIndex::load(&path).is_err(),
        "a corrupted header was accepted"
    );

    std::fs::remove_dir_all(path.parent().unwrap()).ok();
}
