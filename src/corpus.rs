//! Loading documents and handing out dense internal identifiers.
//!
//! The corpus is a JSON Lines file: one complete JSON object per line, no
//! enclosing array. That shape is what makes a 4.6 GB file tractable — the
//! reader never holds more than a single line in memory, so indexing two
//! million abstracts uses roughly as much memory as indexing ten.
//!
//! ```text
//! arxiv-metadata-oai-snapshot.json
//! ├─ {"id":"0704.0001","title":"Calculation of prompt ...","abstract":"  A fully ..."}
//! ├─ {"id":"0704.0002","title":"Sparsity-certifying Graph ...","abstract":"  We describe ..."}
//! └─ ...
//! ```
//!
//! Reading it produces [`Document`]s carrying a dense [`DocId`]; [`DocStore`]
//! keeps the metadata needed to turn a search hit back into something a human
//! recognises.

use std::fmt;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

/// A dense, internal document identifier.
///
/// Documents are numbered in the order they were indexed, starting at zero, so
/// a `DocId` doubles as an index into the document-metadata table — no hash
/// lookup needed to go from a search hit back to a title.
///
/// It is a `u32`, not a `usize`, deliberately. A posting is a document id plus
/// a position list, and there will be hundreds of millions of them; halving the
/// id halves that part of the index and buys back cache lines during
/// intersection, which is the hot loop of the entire engine. Four billion
/// documents is a ceiling this project will not reach.
///
/// It is a newtype, not a bare `u32`, equally deliberately: positions are also
/// `u32`, term ids are also `u32`, and the type system should be the thing that
/// stops them being mixed up rather than a careful reading of argument order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocId(u32);

impl DocId {
    /// The first document in a corpus.
    pub const FIRST: Self = Self(0);

    /// Wraps a raw identifier.
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw identifier.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The identifier as a `usize`, for indexing into the document table.
    #[must_use]
    pub const fn as_usize(self) -> usize {
        self.0 as usize
    }

    /// The next identifier, for assigning ids while ingesting a corpus.
    ///
    /// Returns `None` past [`u32::MAX`] rather than wrapping silently, because
    /// a wrapped id would corrupt an index rather than fail a build.
    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(raw) => Some(Self(raw)),
            None => None,
        }
    }
}

impl fmt::Display for DocId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// One document, as the indexer sees it.
///
/// Deliberately owns its text rather than borrowing. The reader recycles a
/// single line buffer, so borrowed text would be invalidated by the very next
/// call to [`Iterator::next`]. Day 3's tokenizer borrows *from* a `Document`,
/// which lives long enough for that to be safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    /// Dense internal identifier, assigned in corpus order.
    pub id: DocId,
    /// The corpus's own identifier, e.g. the arXiv id `0704.0001`.
    pub external_id: String,
    /// The document's title.
    pub title: String,
    /// The body text. For arXiv, the abstract.
    pub body: String,
}

impl Document {
    /// Total characters of indexable text, title and body together.
    ///
    /// Useful for reporting corpus size without holding the corpus.
    #[must_use]
    pub fn text_len(&self) -> usize {
        self.title.len() + self.body.len()
    }
}

/// The fields of an arXiv record that the engine actually uses.
///
/// Every other key in the record — `submitter`, `authors`, `categories`,
/// `versions`, `authors_parsed` and the rest — is skipped by serde without
/// being allocated, because it is not named here. Deserialising into
/// `serde_json::Value` instead would build a `HashMap` of every field of every
/// one of 2.7 million records and then throw almost all of it away.
///
/// `abstract` is a reserved word in Rust, so the field is named `body` and
/// serde is told the JSON key with `rename`.
#[derive(Debug, Deserialize)]
struct ArxivRecord {
    id: String,
    title: String,
    #[serde(rename = "abstract")]
    body: String,
}

/// A streaming reader over a JSON Lines corpus.
///
/// Implements [`Iterator`], yielding `Result<Document>` so that the caller
/// decides what a malformed record means: the `index` command counts and skips
/// them, but a stricter caller could stop at the first one. Swallowing them
/// inside the reader would take that choice away.
#[derive(Debug)]
pub struct JsonlCorpus<R> {
    reader: R,
    /// Reused across lines. One allocation for the whole corpus rather than
    /// one per record, which is what [`BufRead::lines`] would cost.
    buffer: String,
    path: PathBuf,
    next_id: DocId,
    line: usize,
    bytes_read: u64,
}

impl JsonlCorpus<BufReader<File>> {
    /// Opens a JSON Lines corpus file.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file cannot be opened.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file = File::open(path).map_err(|source| Error::io(path, source))?;

        // A 1 MiB buffer rather than the default 8 KiB: at ~1.7 KiB per arXiv
        // record that is roughly 600 records per read syscall instead of five.
        let reader = BufReader::with_capacity(1 << 20, file);

        Ok(Self::from_reader(reader, path))
    }
}

impl<R: BufRead> JsonlCorpus<R> {
    /// Wraps any buffered reader. The path is used only for error messages.
    pub fn from_reader(reader: R, path: impl Into<PathBuf>) -> Self {
        Self {
            reader,
            buffer: String::with_capacity(8 * 1024),
            path: path.into(),
            next_id: DocId::FIRST,
            line: 0,
            bytes_read: 0,
        }
    }

    /// Bytes consumed so far, for throughput reporting.
    #[must_use]
    pub const fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Lines consumed so far, including blank and malformed ones.
    #[must_use]
    pub const fn lines_read(&self) -> usize {
        self.line
    }
}

impl<R: BufRead> Iterator for JsonlCorpus<R> {
    type Item = Result<Document>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            self.buffer.clear();
            self.line += 1;

            match self.reader.read_line(&mut self.buffer) {
                Ok(0) => return None, // clean end of file
                Ok(bytes) => self.bytes_read += bytes as u64,
                Err(source) => return Some(Err(Error::io(&self.path, source))),
            }

            let line = self.buffer.trim();
            if line.is_empty() {
                continue; // a blank line is not a malformed record
            }

            let record: ArxivRecord = match serde_json::from_str(line) {
                Ok(record) => record,
                Err(source) => {
                    return Some(Err(Error::MalformedRecord {
                        path: self.path.clone(),
                        line: self.line,
                        source,
                    }));
                }
            };

            let id = self.next_id;
            let Some(next) = id.checked_next() else {
                return Some(Err(Error::CorpusTooLarge));
            };
            self.next_id = next;

            return Some(Ok(Document {
                id,
                external_id: record.id,
                title: normalize_whitespace(&record.title),
                body: normalize_whitespace(&record.body),
            }));
        }
    }
}

/// Collapses every run of whitespace to a single space and trims the ends.
///
/// The arXiv dump stores text as it was typeset: abstracts are padded with
/// leading spaces and both titles and abstracts are hard-wrapped, so a title
/// arrives as `"Calculation of prompt diphoton production\n  cross sections"`.
/// A newline in the middle of a title is not a tokenization problem — any
/// whitespace separates tokens — but it is a display problem, and it means two
/// documents whose titles differ only in where the typesetter broke the line
/// would not compare equal.
///
/// Builds the result in one allocation rather than `split_whitespace().collect::<Vec<_>>().join(" ")`,
/// which allocates a `Vec` of slices first and throws it away.
fn normalize_whitespace(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());

    for word in text.split_whitespace() {
        if !normalized.is_empty() {
            normalized.push(' ');
        }
        normalized.push_str(word);
    }

    normalized
}

/// Everything a search result needs in order to name a document, indexed by
/// [`DocId`].
///
/// A `Vec` rather than a `HashMap`: ids are dense and assigned in order, so
/// the id *is* the index. Lookup is a bounds check and a pointer offset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocStore {
    entries: Vec<DocMeta>,
}

/// Metadata for a single document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocMeta {
    /// The corpus's own identifier, e.g. the arXiv id `0704.0001`.
    pub external_id: String,
    /// The document's title.
    pub title: String,
}

impl DocStore {
    /// An empty store.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// An empty store with room for `capacity` documents.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
        }
    }

    /// Records a document's metadata.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if documents are not pushed in corpus order.
    /// The whole point of the `Vec` layout is that `DocId(n)` lives at index
    /// `n`; pushing out of order would silently mislabel every later result.
    pub fn push(&mut self, document: &Document) {
        debug_assert_eq!(
            document.id.as_usize(),
            self.entries.len(),
            "documents must be pushed in corpus order so a DocId indexes the table directly"
        );

        self.entries.push(DocMeta {
            external_id: document.external_id.clone(),
            title: document.title.clone(),
        });
    }

    /// The metadata for `id`, or `None` if it was never indexed.
    #[must_use]
    pub fn get(&self, id: DocId) -> Option<&DocMeta> {
        self.entries.get(id.as_usize())
    }

    /// How many documents are stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether any documents are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every document's metadata, in corpus order.
    pub fn iter(&self) -> impl Iterator<Item = (DocId, &DocMeta)> {
        self.entries
            .iter()
            .enumerate()
            .map(|(index, meta)| (DocId::new(index as u32), meta))
    }
}

#[cfg(test)]
mod tests {
    use super::{DocId, DocStore, Document, JsonlCorpus};
    use crate::error::Error;

    const SAMPLE: &str = concat!(
        r#"{"id":"0704.0001","title":"Prompt diphoton production","abstract":"  A calculation.  ","categories":"hep-ph"}"#,
        "\n",
        r#"{"id":"0704.0002","title":"Sparsity-certifying graphs","abstract":"We describe a decomposition.","submitter":"Ileana Streinu"}"#,
        "\n",
    );

    fn read_all(input: &str) -> Vec<Document> {
        JsonlCorpus::from_reader(input.as_bytes(), "test.jsonl")
            .map(|item| item.expect("every record in this fixture is well formed"))
            .collect()
    }

    #[test]
    fn ids_sort_by_their_raw_value() {
        let mut ids = [DocId::new(9), DocId::FIRST, DocId::new(3)];
        ids.sort_unstable();
        assert_eq!(ids, [DocId::FIRST, DocId::new(3), DocId::new(9)]);
    }

    #[test]
    fn ids_round_trip_through_their_raw_value() {
        assert_eq!(DocId::new(42).get(), 42);
        assert_eq!(DocId::new(42).as_usize(), 42);
        assert_eq!(DocId::FIRST.get(), 0);
    }

    #[test]
    fn ids_display_distinguishably_from_a_plain_number() {
        assert_eq!(DocId::new(42).to_string(), "#42");
    }

    #[test]
    fn the_last_id_has_no_successor() {
        assert_eq!(DocId::FIRST.checked_next(), Some(DocId::new(1)));
        assert_eq!(DocId::new(u32::MAX).checked_next(), None);
    }

    #[test]
    fn records_become_documents_numbered_from_zero() {
        let documents = read_all(SAMPLE);

        assert_eq!(documents.len(), 2);
        assert_eq!(documents[0].id, DocId::FIRST);
        assert_eq!(documents[1].id, DocId::new(1));
        assert_eq!(documents[0].external_id, "0704.0001");
        assert_eq!(documents[1].external_id, "0704.0002");
    }

    #[test]
    fn unused_json_fields_are_ignored_rather_than_rejected() {
        // `categories` and `submitter` appear in the fixture and in the real
        // dump; naming only three fields must not make the rest an error.
        let documents = read_all(SAMPLE);
        assert_eq!(documents[0].title, "Prompt diphoton production");
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_from_text() {
        // The real arXiv dump pads abstracts with leading spaces and newlines.
        let documents = read_all(SAMPLE);
        assert_eq!(documents[0].body, "A calculation.");
    }

    #[test]
    fn hard_wrapped_text_is_unwrapped() {
        // The real dump stores text as it was typeset, so a title arrives
        // broken across lines. Found by running against the actual 4.3 GB file.
        let input = concat!(
            r#"{"id":"0704.0001","title":"Calculation of prompt diphoton production\n  cross sections","abstract":"  A fully differential calculation\nis presented.\n"}"#,
            "\n",
        );

        let documents = read_all(input);

        assert_eq!(
            documents[0].title,
            "Calculation of prompt diphoton production cross sections"
        );
        assert_eq!(
            documents[0].body,
            "A fully differential calculation is presented."
        );
    }

    #[test]
    fn whitespace_normalization_handles_every_kind_of_gap() {
        assert_eq!(super::normalize_whitespace(""), "");
        assert_eq!(super::normalize_whitespace("   "), "");
        assert_eq!(super::normalize_whitespace("one"), "one");
        assert_eq!(super::normalize_whitespace("  one  "), "one");
        assert_eq!(super::normalize_whitespace("one\ntwo"), "one two");
        assert_eq!(super::normalize_whitespace("one \t\r\n two"), "one two");
        assert_eq!(
            super::normalize_whitespace("one   two    three"),
            "one two three"
        );
        // Non-breaking space is whitespace to Rust, and should collapse too.
        assert_eq!(super::normalize_whitespace("one\u{a0}two"), "one two");
    }

    #[test]
    fn blank_lines_are_skipped_without_consuming_an_id() {
        let input = format!("\n{SAMPLE}\n   \n");
        let documents = read_all(&input);

        assert_eq!(documents.len(), 2);
        assert_eq!(documents[0].id, DocId::FIRST);
        assert_eq!(documents[1].id, DocId::new(1));
    }

    #[test]
    fn a_malformed_line_reports_its_line_number_and_does_not_stop_the_stream() {
        let input = concat!(
            r#"{"id":"a","title":"First","abstract":"one"}"#,
            "\n",
            "{ this is not json\n",
            r#"{"id":"b","title":"Second","abstract":"two"}"#,
            "\n",
        );

        let items: Vec<_> = JsonlCorpus::from_reader(input.as_bytes(), "test.jsonl").collect();

        assert_eq!(items.len(), 3);
        assert!(items[0].is_ok());
        assert!(items[2].is_ok());

        let error = items[1].as_ref().expect_err("line 2 is not valid JSON");
        let Error::MalformedRecord { line, .. } = error else {
            panic!("expected a malformed-record error, got {error:?}");
        };
        assert_eq!(*line, 2);
    }

    #[test]
    fn a_record_missing_a_required_field_is_malformed() {
        let input = "{\"id\":\"a\",\"title\":\"No abstract here\"}\n";
        let items: Vec<_> = JsonlCorpus::from_reader(input.as_bytes(), "test.jsonl").collect();

        assert_eq!(items.len(), 1);
        assert!(matches!(items[0], Err(Error::MalformedRecord { .. })));
    }

    #[test]
    fn ids_stay_dense_when_malformed_records_are_skipped() {
        // A skipped record must not leave a hole, or the DocStore's Vec layout
        // would mislabel every document after it.
        let input = concat!(
            r#"{"id":"a","title":"First","abstract":"one"}"#,
            "\n",
            "nonsense\n",
            r#"{"id":"b","title":"Second","abstract":"two"}"#,
            "\n",
        );

        let documents: Vec<_> = JsonlCorpus::from_reader(input.as_bytes(), "test.jsonl")
            .filter_map(std::result::Result::ok)
            .collect();

        assert_eq!(documents.len(), 2);
        assert_eq!(documents[0].id, DocId::FIRST);
        assert_eq!(documents[1].id, DocId::new(1));
    }

    #[test]
    fn byte_and_line_counters_track_the_whole_file() {
        let mut corpus = JsonlCorpus::from_reader(SAMPLE.as_bytes(), "test.jsonl");
        let count = corpus.by_ref().count();

        assert_eq!(count, 2);
        assert_eq!(corpus.bytes_read(), SAMPLE.len() as u64);
        assert_eq!(corpus.lines_read(), 3); // two records plus the EOF probe
    }

    #[test]
    fn a_doc_store_is_indexed_by_doc_id() {
        let documents = read_all(SAMPLE);
        let mut store = DocStore::new();
        for document in &documents {
            store.push(document);
        }

        assert_eq!(store.len(), 2);
        assert!(!store.is_empty());
        assert_eq!(store.get(DocId::FIRST).unwrap().external_id, "0704.0001");
        assert_eq!(
            store.get(DocId::new(1)).unwrap().title,
            "Sparsity-certifying graphs"
        );
        assert!(store.get(DocId::new(2)).is_none());
    }

    #[test]
    fn an_empty_doc_store_says_so() {
        let store = DocStore::new();
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert!(store.get(DocId::FIRST).is_none());
    }

    #[test]
    fn iterating_a_doc_store_yields_ids_in_corpus_order() {
        let documents = read_all(SAMPLE);
        let mut store = DocStore::with_capacity(documents.len());
        for document in &documents {
            store.push(document);
        }

        let ids: Vec<DocId> = store.iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec![DocId::FIRST, DocId::new(1)]);
    }

    #[test]
    fn text_len_counts_title_and_body() {
        let documents = read_all(SAMPLE);
        assert_eq!(
            documents[0].text_len(),
            "Prompt diphoton production".len() + "A calculation.".len()
        );
    }
}
