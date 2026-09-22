//! The on-disk index format.
//!
//! One file holds everything a query needs: the term dictionary, the four
//! compressed-sparse-row arrays from [`crate::index`], and the document
//! metadata that turns a [`DocId`] back into a title.
//!
//! ```text
//! ┌────────────────────────────────────────────────────────────┐
//! │ header    magic, version, reserved, and eight counts        │  48 bytes
//! ├────────────────────────────────────────────────────────────┤
//! │ term_offsets            (terms + 1) × u32                  │  ┐
//! │ term_bytes              concatenated UTF-8, sorted         │  │ dictionary
//! │ term_ids                terms × u32                        │  ┘
//! ├────────────────────────────────────────────────────────────┤
//! │ term_postings_start     (terms + 1) × u32                  │  ┐
//! │ posting_docs            postings × u32                     │  │ the index
//! │ posting_positions_start (postings + 1) × u32               │  │
//! │ positions               positions × u32                    │  ┘
//! ├────────────────────────────────────────────────────────────┤
//! │ external_offsets        (documents + 1) × u32              │  ┐
//! │ external_bytes          concatenated arXiv ids             │  │ documents
//! │ title_offsets           (documents + 1) × u32              │  │
//! │ title_bytes             concatenated titles                │  ┘
//! └────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Why not `bincode` or `serde`
//!
//! Because day 13 needs somewhere to put delta encoding and varint
//! compression, and that is only possible in a format nobody else defines. A
//! derived serializer would also write the term dictionary in `HashMap` order,
//! which is *random* — see determinism below.
//!
//! # The dictionary is stored sorted
//!
//! Terms are interned in first-seen order, so [`TermId`] ordering is arbitrary.
//! On disk they are written in lexicographic order, with a parallel array
//! mapping each sorted rank back to its `TermId`. Two reasons:
//!
//! - **Day 11 needs it.** A prefix wildcard like `comp*` is a binary search for
//!   the range `[comp, compz…)`, which requires the terms to be in order. A
//!   hash map cannot answer that question at all.
//! - **Determinism.** Iterating a `HashMap` yields a different order every run,
//!   so a serializer that followed it would produce a different file each time
//!   from identical input. Sorting makes the same corpus produce byte-identical
//!   bytes, which is testable — and `saving_is_deterministic` tests it.
//!
//! # Endianness and integer width
//!
//! Every number is a little-endian `u32`, matching the in-memory layout, so
//! loading is close to a straight read into the arrays the engine already uses.
//! Little-endian is chosen rather than network order because every machine this
//! will realistically run on is little-endian, and byte-swapping 436 million
//! integers to be principled about it would cost more than it is worth.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use rustc_hash::FxHashMap;

use crate::corpus::{DocId, DocMeta, DocStore};
use crate::error::{Error, Result};
use crate::index::{Index, TermId};

/// Identifies the file and the layout it uses.
const MAGIC: [u8; 8] = *b"BOOLIDX\x01";

/// Bumped whenever the layout changes in a way that makes old files unreadable.
const VERSION: u32 = 1;

/// Fixed-size prefix: magic, version, and the counts that size every section.
const HEADER_BYTES: usize = 8 + 4 * 2 + 4 * 4 + 4 * 3 + 4;

/// How many integers to convert at a time when reading or writing.
///
/// Converting one `u32` per `write_all` would mean 436 million syscalls' worth
/// of buffered writes; converting the whole array at once would mean a second
/// copy of a 1.7 GB array. A chunk keeps both bounded.
const CHUNK: usize = 16 * 1024;

/// An index together with the document metadata its results refer to.
///
/// They travel together because a search result is useless without a title, and
/// separating them would mean two files that could get out of step.
#[derive(Debug)]
pub struct SearchIndex {
    /// The positional inverted index.
    pub index: Index,
    /// Metadata for every indexed document.
    pub documents: DocStore,
}

impl SearchIndex {
    /// Bundles an index with its document store.
    #[must_use]
    pub const fn new(index: Index, documents: DocStore) -> Self {
        Self { index, documents }
    }

    /// Writes the index to `path`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file cannot be written, or
    /// [`Error::IndexTooLarge`] if a section exceeds what a `u32` offset can
    /// address.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let file = File::create(path).map_err(|source| Error::io(path, source))?;
        let mut writer = BufWriter::with_capacity(1 << 20, file);

        self.write_to(&mut writer)
            .map_err(|source| Error::io(path, source))?;

        writer
            .into_inner()
            .map_err(|error| Error::io(path, error.into_error()))?
            .sync_all()
            .map_err(|source| Error::io(path, source))?;

        Ok(())
    }

    /// Reads an index written by [`SearchIndex::save`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the file cannot be read, or
    /// [`Error::IndexFormat`] if it is not an index file, was written by an
    /// incompatible version, or is truncated or internally inconsistent.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file = File::open(path).map_err(|source| Error::io(path, source))?;
        let mut reader = BufReader::with_capacity(1 << 20, file);

        Self::read_from(&mut reader).map_err(|error| match error {
            // An unexpected end of file means truncation, which is a format
            // problem rather than a disk problem, and saying so is more useful.
            Error::Io { source, .. } if source.kind() == std::io::ErrorKind::UnexpectedEof => {
                Error::IndexFormat(format!("{} is truncated", path.display()))
            }
            Error::Io { source, .. } => Error::io(path, source),
            other => other,
        })
    }

    fn write_to<W: Write>(&self, writer: &mut W) -> std::io::Result<()> {
        let index = &self.index;
        let documents = &self.documents;

        // Lexicographic order, so day 11 can binary search and so the same
        // corpus always produces the same bytes.
        let mut vocabulary: Vec<(&str, TermId)> = index.vocabulary().collect();
        vocabulary.sort_unstable_by(|left, right| left.0.cmp(right.0));

        let term_bytes: usize = vocabulary.iter().map(|(term, _)| term.len()).sum();
        let external_bytes: usize = documents
            .entries()
            .iter()
            .map(|meta| meta.external_id.len())
            .sum();
        let title_bytes: usize = documents
            .entries()
            .iter()
            .map(|meta| meta.title.len())
            .sum();

        let mut scratch = Vec::with_capacity(CHUNK * 4);

        writer.write_all(&MAGIC)?;
        writer.write_all(&VERSION.to_le_bytes())?;
        writer.write_all(&0u32.to_le_bytes())?; // reserved for flags
        writer.write_all(&u32_of(documents.len())?.to_le_bytes())?;
        writer.write_all(&u32_of(index.terms())?.to_le_bytes())?;
        writer.write_all(&u32_of(index.posting_docs.len())?.to_le_bytes())?;
        writer.write_all(&u32_of(index.positions_len())?.to_le_bytes())?;
        writer.write_all(&u32_of(term_bytes)?.to_le_bytes())?;
        writer.write_all(&u32_of(external_bytes)?.to_le_bytes())?;
        writer.write_all(&u32_of(title_bytes)?.to_le_bytes())?;
        writer.write_all(&u32_of(index.documents())?.to_le_bytes())?;

        // Dictionary.
        write_offsets(
            writer,
            vocabulary.iter().map(|(term, _)| term.len()),
            &mut scratch,
        )?;
        for (term, _) in &vocabulary {
            writer.write_all(term.as_bytes())?;
        }
        write_u32s(
            writer,
            vocabulary.iter().map(|(_, id)| id.get()),
            &mut scratch,
        )?;

        // The index proper.
        write_u32s(
            writer,
            index.term_postings_start.iter().copied(),
            &mut scratch,
        )?;
        write_u32s(
            writer,
            index.posting_docs.iter().map(|id| id.get()),
            &mut scratch,
        )?;
        write_u32s(
            writer,
            index.posting_positions_start.iter().copied(),
            &mut scratch,
        )?;
        write_u32s(writer, index.positions.iter().copied(), &mut scratch)?;

        // Document metadata.
        write_offsets(
            writer,
            documents
                .entries()
                .iter()
                .map(|meta| meta.external_id.len()),
            &mut scratch,
        )?;
        for meta in documents.entries() {
            writer.write_all(meta.external_id.as_bytes())?;
        }
        write_offsets(
            writer,
            documents.entries().iter().map(|meta| meta.title.len()),
            &mut scratch,
        )?;
        for meta in documents.entries() {
            writer.write_all(meta.title.as_bytes())?;
        }

        Ok(())
    }

    fn read_from<R: Read>(reader: &mut R) -> Result<Self> {
        let mut header = [0u8; HEADER_BYTES];
        read_exact(reader, &mut header)?;

        if header[..8] != MAGIC {
            return Err(Error::IndexFormat(
                "not an index file: wrong magic number".to_owned(),
            ));
        }

        let version = u32_at(&header, 8);
        if version != VERSION {
            return Err(Error::IndexFormat(format!(
                "index was written by format version {version}, this build reads {VERSION}"
            )));
        }

        let stored_documents = u32_at(&header, 16) as usize;
        let terms = u32_at(&header, 20) as usize;
        let postings = u32_at(&header, 24) as usize;
        let positions = u32_at(&header, 28) as usize;
        let term_bytes = u32_at(&header, 32) as usize;
        let external_bytes = u32_at(&header, 36) as usize;
        let title_bytes = u32_at(&header, 40) as usize;
        let indexed_documents = u32_at(&header, 44);

        let mut scratch = Vec::with_capacity(CHUNK * 4);

        // Dictionary.
        let term_offsets = read_u32s(reader, terms + 1, &mut scratch)?;
        let mut term_blob = vec![0u8; term_bytes];
        read_exact(reader, &mut term_blob)?;
        let term_ids = read_u32s(reader, terms, &mut scratch)?;

        check(
            term_offsets.last().copied().unwrap_or(0) as usize == term_bytes,
            "term dictionary length does not match its offsets",
        )?;

        let mut dictionary = FxHashMap::with_capacity_and_hasher(terms, rustc_hash::FxBuildHasher);
        for rank in 0..terms {
            let from = term_offsets[rank] as usize;
            let to = term_offsets[rank + 1] as usize;
            check(
                from <= to && to <= term_bytes,
                "term offsets are out of order",
            )?;

            let term = std::str::from_utf8(&term_blob[from..to])
                .map_err(|_| Error::IndexFormat("a term is not valid UTF-8".to_owned()))?;
            dictionary.insert(Box::from(term), TermId::new(term_ids[rank]));
        }
        check(
            dictionary.len() == terms,
            "the term dictionary contains duplicates",
        )?;

        // The index proper.
        let term_postings_start = read_u32s(reader, terms + 1, &mut scratch)?;
        let posting_docs: Vec<DocId> = read_u32s(reader, postings, &mut scratch)?
            .into_iter()
            .map(DocId::new)
            .collect();
        let posting_positions_start = read_u32s(reader, postings + 1, &mut scratch)?;
        let index_positions = read_u32s(reader, positions, &mut scratch)?;

        check(
            term_postings_start.last().copied().unwrap_or(0) as usize == postings,
            "the postings array does not end where the term offsets say it should",
        )?;
        check(
            posting_positions_start.last().copied().unwrap_or(0) as usize == positions,
            "the positions array does not end where the posting offsets say it should",
        )?;

        // Document metadata.
        let external_offsets = read_u32s(reader, stored_documents + 1, &mut scratch)?;
        let mut external_blob = vec![0u8; external_bytes];
        read_exact(reader, &mut external_blob)?;
        let title_offsets = read_u32s(reader, stored_documents + 1, &mut scratch)?;
        let mut title_blob = vec![0u8; title_bytes];
        read_exact(reader, &mut title_blob)?;

        let mut entries = Vec::with_capacity(stored_documents);
        for document in 0..stored_documents {
            let external = slice_of(&external_blob, &external_offsets, document, "document id")?;
            let title = slice_of(&title_blob, &title_offsets, document, "title")?;
            entries.push(DocMeta {
                external_id: external.to_owned(),
                title: title.to_owned(),
            });
        }

        Ok(Self {
            index: Index::from_parts(
                dictionary,
                term_postings_start,
                posting_docs,
                posting_positions_start,
                index_positions,
                indexed_documents,
            ),
            documents: DocStore::from_entries(entries),
        })
    }
}

/// Reads a UTF-8 string out of a blob using a CSR offset array.
fn slice_of<'a>(blob: &'a [u8], offsets: &[u32], nth: usize, what: &str) -> Result<&'a str> {
    let from = offsets[nth] as usize;
    let to = offsets[nth + 1] as usize;
    check(
        from <= to && to <= blob.len(),
        "a string offset is out of range",
    )?;

    std::str::from_utf8(&blob[from..to])
        .map_err(|_| Error::IndexFormat(format!("a {what} is not valid UTF-8")))
}

fn check(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(Error::IndexFormat(message.to_owned()))
    }
}

fn u32_of(value: usize) -> std::io::Result<u32> {
    u32::try_from(value).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a section exceeds what a u32 offset can address",
        )
    })
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

/// Writes a run of integers, converting in bounded chunks.
fn write_u32s<W, I>(writer: &mut W, values: I, scratch: &mut Vec<u8>) -> std::io::Result<()>
where
    W: Write,
    I: IntoIterator<Item = u32>,
{
    scratch.clear();

    for value in values {
        scratch.extend_from_slice(&value.to_le_bytes());
        if scratch.len() >= CHUNK * 4 {
            writer.write_all(scratch)?;
            scratch.clear();
        }
    }

    if !scratch.is_empty() {
        writer.write_all(scratch)?;
        scratch.clear();
    }

    Ok(())
}

/// Turns a run of lengths into the CSR offsets they imply, and writes them.
///
/// `[3, 1, 4]` becomes `[0, 3, 4, 8]` — the same prefix sum the index builder
/// does, one level down.
fn write_offsets<W, I>(writer: &mut W, lengths: I, scratch: &mut Vec<u8>) -> std::io::Result<()>
where
    W: Write,
    I: IntoIterator<Item = usize>,
{
    let mut running = 0usize;
    let mut offsets = Vec::new();
    offsets.push(0u32);

    for length in lengths {
        running += length;
        offsets.push(u32_of(running)?);
    }

    write_u32s(writer, offsets, scratch)
}

fn read_exact<R: Read>(reader: &mut R, buffer: &mut [u8]) -> Result<()> {
    reader
        .read_exact(buffer)
        .map_err(|source| Error::io("<index>", source))
}

/// Reads `count` integers, converting in bounded chunks.
fn read_u32s<R: Read>(reader: &mut R, count: usize, scratch: &mut Vec<u8>) -> Result<Vec<u32>> {
    let mut values = Vec::with_capacity(count);
    let mut remaining = count;

    while remaining > 0 {
        let take = remaining.min(CHUNK);
        scratch.clear();
        scratch.resize(take * 4, 0);
        read_exact(reader, scratch)?;

        values.extend(
            scratch
                .chunks_exact(4)
                .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
        );
        remaining -= take;
    }

    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::{MAGIC, SearchIndex};
    use crate::corpus::{DocId, DocStore, Document};
    use crate::error::Error;
    use crate::index::IndexBuilder;

    fn document(id: u32, title: &str, body: &str) -> Document {
        Document {
            id: DocId::new(id),
            external_id: format!("07{id:02}.0001"),
            title: title.to_owned(),
            body: body.to_owned(),
        }
    }

    fn corpus() -> Vec<Document> {
        vec![
            document(0, "Quantum error correction", "surface code quantum"),
            document(1, "Machine learning", "neural networks learn"),
            document(2, "Quantum computing", "quantum quantum quantum"),
            document(3, "Unicode 量子", "naïve don't 3.14"),
        ]
    }

    fn build(documents: &[Document]) -> SearchIndex {
        let mut builder = IndexBuilder::new();
        for document in documents {
            builder.add_document(document);
        }

        let mut assembler = builder.finish_counting().expect("small");
        let mut store = DocStore::new();
        for document in documents {
            assembler.add_document(document);
            store.push(document);
        }

        SearchIndex::new(assembler.finish().expect("consistent"), store)
    }

    fn round_trip(documents: &[Document]) -> (SearchIndex, SearchIndex) {
        let original = build(documents);
        let mut bytes = Vec::new();
        original.write_to(&mut bytes).expect("writing to memory");
        let loaded = SearchIndex::read_from(&mut bytes.as_slice()).expect("reading back");
        (original, loaded)
    }

    #[test]
    fn a_loaded_index_answers_exactly_as_the_built_one_did() {
        let documents = corpus();
        let (original, loaded) = round_trip(&documents);

        assert_eq!(original.index.stats(), loaded.index.stats());

        for (term, id) in original.index.vocabulary() {
            let loaded_id = loaded
                .index
                .term_id(term)
                .unwrap_or_else(|| panic!("{term:?} was lost"));

            assert_eq!(
                original.index.doc_ids(id),
                loaded.index.doc_ids(loaded_id),
                "{term:?} documents"
            );

            for nth in 0..original.index.document_frequency(id) {
                assert_eq!(
                    original.index.positions(id, nth),
                    loaded.index.positions(loaded_id, nth),
                    "{term:?} positions in posting {nth}"
                );
            }
        }
    }

    #[test]
    fn document_metadata_survives_the_round_trip() {
        let documents = corpus();
        let (original, loaded) = round_trip(&documents);

        assert_eq!(original.documents.len(), loaded.documents.len());
        for (id, meta) in original.documents.iter() {
            let other = loaded.documents.get(id).expect("present");
            assert_eq!(meta.external_id, other.external_id);
            assert_eq!(meta.title, other.title);
        }
    }

    #[test]
    fn non_ascii_text_survives_the_round_trip() {
        let documents = corpus();
        let (_, loaded) = round_trip(&documents);

        assert!(loaded.index.term_id("量").is_some());
        assert!(loaded.index.term_id("naïve").is_some());
        assert!(loaded.index.term_id("don't").is_some());
        assert!(loaded.index.term_id("3.14").is_some());
        assert_eq!(
            loaded.documents.get(DocId::new(3)).unwrap().title,
            "Unicode 量子"
        );
    }

    #[test]
    fn saving_is_deterministic() {
        // Two independently built indexes over the same corpus must serialize
        // to identical bytes. They will not, if the dictionary is written in
        // hash order — which is exactly why it is sorted.
        let documents = corpus();

        let mut first = Vec::new();
        build(&documents).write_to(&mut first).expect("write");

        let mut second = Vec::new();
        build(&documents).write_to(&mut second).expect("write");

        assert_eq!(first, second, "the same corpus produced different bytes");
    }

    #[test]
    fn the_dictionary_is_written_in_sorted_order() {
        let documents = corpus();
        let index = build(&documents);
        let mut bytes = Vec::new();
        index.write_to(&mut bytes).expect("write");

        // Pull the term blob back out and confirm it is sorted, which is what
        // day 11's prefix search will depend on.
        let terms = index.index.terms();
        let header = super::HEADER_BYTES;
        let offsets_at = header;
        let blob_at = offsets_at + (terms + 1) * 4;

        let offsets: Vec<u32> = (0..=terms)
            .map(|i| super::u32_at(&bytes, offsets_at + i * 4))
            .collect();

        let mut previous = String::new();
        for rank in 0..terms {
            let from = blob_at + offsets[rank] as usize;
            let to = blob_at + offsets[rank + 1] as usize;
            let term = std::str::from_utf8(&bytes[from..to]).expect("utf-8");
            assert!(
                previous.as_str() < term,
                "terms out of order: {previous:?} then {term:?}"
            );
            previous = term.to_owned();
        }
    }

    #[test]
    fn an_empty_index_round_trips() {
        let (_, loaded) = round_trip(&[]);

        assert_eq!(loaded.index.terms(), 0);
        assert_eq!(loaded.documents.len(), 0);
        assert!(loaded.index.term_id("quantum").is_none());
    }

    #[test]
    fn a_file_that_is_not_an_index_is_refused() {
        let mut bytes: &[u8] = b"this is not an index file at all, not even close";
        let error = SearchIndex::read_from(&mut bytes).expect_err("should be refused");

        assert!(
            matches!(&error, Error::IndexFormat(message) if message.contains("magic")),
            "got {error}"
        );
    }

    #[test]
    fn a_file_from_another_format_version_is_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&99u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; super::HEADER_BYTES - 12]);

        let error = SearchIndex::read_from(&mut bytes.as_slice()).expect_err("should be refused");
        assert!(
            matches!(&error, Error::IndexFormat(message) if message.contains("version 99")),
            "got {error}"
        );
    }

    #[test]
    fn a_truncated_file_is_refused_rather_than_misread() {
        let documents = corpus();
        let mut bytes = Vec::new();
        build(&documents).write_to(&mut bytes).expect("write");

        // Cut it off anywhere past the header and it must fail, never silently
        // produce a smaller index.
        for cut in [super::HEADER_BYTES + 1, bytes.len() / 2, bytes.len() - 1] {
            let error = SearchIndex::read_from(&mut &bytes[..cut])
                .expect_err("a truncated file should be refused");
            assert!(
                matches!(error, Error::IndexFormat(_) | Error::Io { .. }),
                "cut at {cut} gave {error}"
            );
        }
    }

    #[test]
    fn a_header_promising_more_than_the_file_holds_is_refused() {
        let documents = corpus();
        let mut bytes = Vec::new();
        build(&documents).write_to(&mut bytes).expect("write");

        // Claim a million terms.
        bytes[20..24].copy_from_slice(&1_000_000u32.to_le_bytes());

        let error = SearchIndex::read_from(&mut bytes.as_slice()).expect_err("should be refused");
        assert!(
            matches!(error, Error::IndexFormat(_) | Error::Io { .. }),
            "got {error}"
        );
    }

    #[test]
    fn a_file_round_trips_through_the_filesystem() {
        let documents = corpus();
        let directory =
            std::env::temp_dir().join(format!("boolsearch-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("temp dir");
        let path = directory.join("index.bin");

        build(&documents).save(&path).expect("save");
        let loaded = SearchIndex::load(&path).expect("load");

        assert_eq!(loaded.index.terms(), build(&documents).index.terms());
        assert!(loaded.index.term_id("quantum").is_some());

        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn loading_a_missing_file_names_it() {
        let error = SearchIndex::load("no-such-index.bin").expect_err("absent");
        assert!(
            error.to_string().contains("no-such-index.bin"),
            "got {error}"
        );
    }
}
