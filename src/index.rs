//! The positional inverted index.
//!
//! An inverted index maps each term to the documents containing it. A
//! *positional* index also records where in each document, which is what turns
//! a Boolean engine into one that can answer `"error correction"` and
//! `quantum NEAR/5 surface`:
//!
//! ```text
//! "quantum" ──▶ [ (doc 3, [12, 87]), (doc 17, [4]), (doc 22, [9, 31, 44]) ]
//!                  ▲       ▲
//!                  │       └── every position the term occurs at
//!                  └────────── documents, ascending, so lists can be merged
//! ```
//!
//! Postings are kept sorted by document id. That single invariant is what makes
//! intersection a linear merge instead of a hash join, and it is what day 8's
//! galloping search relies on.
//!
//! # Why this is not a `Vec<Vec<...>>`
//!
//! The obvious shape is the one the build plan originally called for:
//!
//! ```text
//! postings: Vec<Vec<Posting>>              // one Vec per term
//! struct Posting { doc_id: DocId, positions: Vec<u32> }
//! ```
//!
//! Day 3 measured the corpus at 436 million tokens across 2.7 million
//! documents, which works out at roughly 280 million distinct (term, document)
//! pairs. Every `Vec` in Rust is a pointer, a length and a capacity — 24 bytes
//! — *before* it holds anything. So that design spends **6.7 GB on `Vec`
//! headers alone**, plus a separate heap allocation per posting, to carry 1.7 GB
//! of actual positions. The data structure would be four times the size of the
//! data.
//!
//! # What it is instead
//!
//! Four flat arrays, in the shape known as compressed sparse row. Each level
//! stores *where the next level's slice begins*, so a slice costs 4 bytes of
//! bookkeeping instead of 24, and the whole index is four allocations rather
//! than 280 million:
//!
//! ```text
//! term_postings_start   [0, 2, 5, ...]        one entry per term, plus a sentinel
//!                        │  └──────────┐
//!                        ▼             ▼
//! posting_docs          [3, 17,   22, 40, 41, ...]      one entry per posting
//! posting_positions_start[0,  2,    3,  6, ...]         one entry per posting, plus a sentinel
//!                        │   └─┐
//!                        ▼     ▼
//! positions             [12, 87, 4, 9, 31, 44, ...]     one entry per occurrence
//! ```
//!
//! Term *t*'s postings are `posting_docs[term_postings_start[t]..
//! term_postings_start[t + 1]]` — a contiguous slice, which day 8 can binary
//! search directly. The positions of its *n*th posting are found the same way
//! one level down.
//!
//! # Two passes
//!
//! Flat arrays need their sizes up front, and a single pass cannot know them.
//! So the build reads the corpus twice: [`IndexBuilder`] counts, and
//! [`IndexAssembler`] fills. Between them, a prefix sum turns "how many" into
//! "starting where".
//!
//! The two are separate types on purpose. `IndexBuilder::finish_counting`
//! consumes the builder and hands back an assembler, so calling them out of
//! order is not a mistake you can make — the method you would be misusing does
//! not exist on the type you are holding. Rust calls this typestate, and it is
//! cheaper than a runtime check and more reliable than a comment.

use std::fmt;

use rustc_hash::FxHashMap;

use crate::corpus::{DocId, Document};
use crate::error::{Error, Result};
use crate::tokenize::tokenize;

/// How far positions jump between a document's title and its body.
///
/// Without a gap, the last word of the title and the first word of the abstract
/// would sit at consecutive positions, and the phrase query
/// `"energies A fully"` would match a document where those words merely happen
/// to straddle the join. A gap larger than any proximity operator anyone would
/// write keeps the two fields from bleeding into each other, while still
/// letting one index serve both.
pub const FIELD_GAP: u32 = 100;

/// An interned term.
///
/// Terms are interned once during indexing and referred to by number
/// thereafter: a `u32` compares in one instruction and a string does not, and
/// day 8 compares them in a loop over hundreds of millions of postings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TermId(u32);

impl TermId {
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

    /// The identifier as a `usize`, for indexing into the term tables.
    #[must_use]
    pub const fn as_usize(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for TermId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "t{}", self.0)
    }
}

/// One term's occurrences within one document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Posting<'a> {
    /// The document containing the term.
    pub doc_id: DocId,
    /// Every position the term occurs at, ascending.
    pub positions: &'a [u32],
}

impl Posting<'_> {
    /// How many times the term occurs in this document.
    #[must_use]
    pub const fn frequency(&self) -> usize {
        self.positions.len()
    }
}

/// Sentinel for "this term has not been seen in any document yet".
///
/// [`DocId`] tops out at `u32::MAX - 1` — the corpus reader refuses to go
/// further — so `u32::MAX` cannot collide with a real document.
const NO_DOCUMENT: u32 = u32::MAX;

/// Visits every term of a document, in position order, title before body.
///
/// Both passes go through here, so they cannot disagree about what a document
/// contains. If they did, the counts from pass one would not match what pass
/// two tried to write, and the index would be quietly wrong.
fn for_each_term<F>(document: &Document, mut visit: F)
where
    F: FnMut(&str, u32),
{
    let mut after_title = 0;

    for token in tokenize(&document.title) {
        visit(&token.normalized(), token.position);
        after_title = token.position + 1;
    }

    let body_offset = after_title + FIELD_GAP;
    for token in tokenize(&document.body) {
        visit(&token.normalized(), token.position + body_offset);
    }
}

/// The counting pass.
///
/// Learns the vocabulary and how much room each term needs. Produces no
/// postings — that is [`IndexAssembler`]'s job, and it cannot start until this
/// one has finished.
#[derive(Debug, Default)]
pub struct IndexBuilder {
    dictionary: FxHashMap<Box<str>, TermId>,
    /// Per term: how many distinct documents contain it.
    document_frequency: Vec<u32>,
    /// Per term: how many times it occurs across the whole corpus.
    occurrences: Vec<u32>,
    /// Per term: the last document counted, so that a term appearing five times
    /// in one document still only counts once towards its document frequency.
    last_document: Vec<u32>,
    documents: u32,
}

impl IndexBuilder {
    /// A builder with an empty vocabulary.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A builder expecting roughly `terms` distinct terms.
    #[must_use]
    pub fn with_capacity(terms: usize) -> Self {
        Self {
            dictionary: FxHashMap::with_capacity_and_hasher(terms, rustc_hash::FxBuildHasher),
            document_frequency: Vec::with_capacity(terms),
            occurrences: Vec::with_capacity(terms),
            last_document: Vec::with_capacity(terms),
            documents: 0,
        }
    }

    /// Counts one document. Call once per document, in corpus order.
    pub fn add_document(&mut self, document: &Document) {
        let Self {
            dictionary,
            document_frequency,
            occurrences,
            last_document,
            ..
        } = self;

        let raw_doc_id = document.id.get();

        for_each_term(document, |term, _position| {
            let id = match dictionary.get(term) {
                Some(&id) => id,
                None => {
                    let id = TermId::new(
                        u32::try_from(dictionary.len()).expect("term count checked on finish"),
                    );
                    dictionary.insert(term.into(), id);
                    document_frequency.push(0);
                    occurrences.push(0);
                    last_document.push(NO_DOCUMENT);
                    id
                }
            };

            let slot = id.as_usize();
            occurrences[slot] += 1;

            if last_document[slot] != raw_doc_id {
                last_document[slot] = raw_doc_id;
                document_frequency[slot] += 1;
            }
        });

        self.documents += 1;
    }

    /// How many distinct terms have been seen so far.
    #[must_use]
    pub fn terms(&self) -> usize {
        self.dictionary.len()
    }

    /// Turns the counts into starting offsets and hands back the filling pass.
    ///
    /// This is the prefix sum: `[3, 1, 4]` occurrences per term becomes
    /// `[0, 3, 4, 8]` — term 0's positions start at 0, term 1's at 3, term 2's
    /// at 4, and the final entry is the total. Every slice boundary in the
    /// finished index is one of these numbers.
    ///
    /// # Errors
    ///
    /// Returns [`Error::IndexTooLarge`] if the corpus needs more than
    /// [`u32::MAX`] postings or positions, which the offsets cannot address.
    pub fn finish_counting(self) -> Result<IndexAssembler> {
        let terms = self.dictionary.len();

        let mut term_postings_start = Vec::with_capacity(terms + 1);
        let mut position_cursor = Vec::with_capacity(terms);

        let mut postings_total: u64 = 0;
        let mut positions_total: u64 = 0;

        for slot in 0..terms {
            term_postings_start.push(u32::try_from(postings_total).map_err(|_| too_large())?);
            position_cursor.push(u32::try_from(positions_total).map_err(|_| too_large())?);

            postings_total += u64::from(self.document_frequency[slot]);
            positions_total += u64::from(self.occurrences[slot]);
        }

        let postings = usize::try_from(postings_total).map_err(|_| too_large())?;
        let positions = usize::try_from(positions_total).map_err(|_| too_large())?;
        u32::try_from(postings_total).map_err(|_| too_large())?;
        u32::try_from(positions_total).map_err(|_| too_large())?;

        term_postings_start.push(u32::try_from(postings_total).map_err(|_| too_large())?);

        Ok(IndexAssembler {
            dictionary: self.dictionary,
            term_postings_start,
            posting_docs: vec![DocId::FIRST; postings],
            // One extra for the sentinel, so the last posting's positions have
            // an end to point at.
            posting_positions_start: vec![0; postings + 1],
            positions: vec![0; positions],
            posting_cursor: Vec::new(),
            position_cursor,
            last_document: vec![NO_DOCUMENT; terms],
            documents: self.documents,
            unexpected_terms: 0,
        }
        .with_posting_cursor())
    }
}

fn too_large() -> Error {
    Error::IndexTooLarge
}

/// The filling pass.
///
/// Writes postings and positions into arrays already sized by the counting
/// pass. Every write lands in a slot that is known to exist, so there is no
/// reallocation and no copying during the whole second pass.
#[derive(Debug)]
pub struct IndexAssembler {
    dictionary: FxHashMap<Box<str>, TermId>,
    term_postings_start: Vec<u32>,
    posting_docs: Vec<DocId>,
    posting_positions_start: Vec<u32>,
    positions: Vec<u32>,
    /// Per term: the next posting slot to write.
    posting_cursor: Vec<u32>,
    /// Per term: the next position slot to write.
    position_cursor: Vec<u32>,
    /// Per term: the document the cursor is currently inside.
    last_document: Vec<u32>,
    documents: u32,
    /// Terms seen in the second pass that the first pass never saw, which can
    /// only mean the corpus changed underneath us.
    unexpected_terms: u64,
}

impl IndexAssembler {
    fn with_posting_cursor(mut self) -> Self {
        // Each term's first posting goes where its run starts.
        self.posting_cursor =
            self.term_postings_start[..self.term_postings_start.len() - 1].to_vec();
        self
    }

    /// Fills in one document. Call with the same documents, in the same order,
    /// as the counting pass.
    pub fn add_document(&mut self, document: &Document) {
        let Self {
            dictionary,
            posting_docs,
            posting_positions_start,
            positions,
            posting_cursor,
            position_cursor,
            last_document,
            unexpected_terms,
            ..
        } = self;

        let raw_doc_id = document.id.get();

        for_each_term(document, |term, position| {
            let Some(&id) = dictionary.get(term) else {
                *unexpected_terms += 1;
                return;
            };

            let slot = id.as_usize();

            if last_document[slot] != raw_doc_id {
                last_document[slot] = raw_doc_id;

                let posting = posting_cursor[slot] as usize;
                posting_cursor[slot] += 1;
                posting_docs[posting] = document.id;
                // This posting's positions begin wherever the term's cursor has
                // reached, which is exactly where the previous posting stopped.
                posting_positions_start[posting] = position_cursor[slot];
            }

            positions[position_cursor[slot] as usize] = position;
            position_cursor[slot] += 1;
        });
    }

    /// Seals the index.
    ///
    /// # Errors
    ///
    /// Returns [`Error::IndexTooLarge`] wrapped as an inconsistency if the
    /// second pass saw terms the first did not, which means the corpus was
    /// modified between the passes.
    pub fn finish(mut self) -> Result<Index> {
        if self.unexpected_terms > 0 {
            return Err(Error::CorpusChanged {
                unexpected_terms: self.unexpected_terms,
            });
        }

        // The sentinel: the last posting's positions end where the array does.
        let total_positions =
            u32::try_from(self.positions.len()).expect("checked when the arrays were sized");
        if let Some(last) = self.posting_positions_start.last_mut() {
            *last = total_positions;
        }

        Ok(Index {
            dictionary: self.dictionary,
            term_postings_start: self.term_postings_start,
            posting_docs: self.posting_docs,
            posting_positions_start: self.posting_positions_start,
            positions: self.positions,
            documents: self.documents,
        })
    }
}

/// A finished positional inverted index.
#[derive(Debug)]
pub struct Index {
    dictionary: FxHashMap<Box<str>, TermId>,
    term_postings_start: Vec<u32>,
    posting_docs: Vec<DocId>,
    posting_positions_start: Vec<u32>,
    positions: Vec<u32>,
    documents: u32,
}

impl Index {
    /// Looks a term up by its normalized text.
    ///
    /// The term must already be lowercased the way [`crate::Token::normalized`]
    /// would: the index stores normalized forms and does not normalize on
    /// lookup, because day 7's parser normalizes query terms once instead.
    #[must_use]
    pub fn term_id(&self, term: &str) -> Option<TermId> {
        self.dictionary.get(term).copied()
    }

    /// How many documents were indexed.
    #[must_use]
    pub const fn documents(&self) -> usize {
        self.documents as usize
    }

    /// How many distinct terms the index holds.
    #[must_use]
    pub fn terms(&self) -> usize {
        self.dictionary.len()
    }

    /// The documents containing `term`, ascending — a contiguous slice.
    ///
    /// Contiguous and sorted is the whole point: day 8 intersects two of these
    /// with a two-pointer merge, and binary searches one when it is far shorter
    /// than the other.
    #[must_use]
    pub fn doc_ids(&self, term: TermId) -> &[DocId] {
        let slot = term.as_usize();
        let Some(&start) = self.term_postings_start.get(slot) else {
            return &[];
        };
        let end = self.term_postings_start[slot + 1];
        &self.posting_docs[start as usize..end as usize]
    }

    /// In how many documents `term` occurs.
    #[must_use]
    pub fn document_frequency(&self, term: TermId) -> usize {
        self.doc_ids(term).len()
    }

    /// The positions of `term` in the `nth` document that contains it.
    ///
    /// `nth` indexes the term's own postings, not the corpus — it lines up with
    /// [`Index::doc_ids`].
    #[must_use]
    pub fn positions(&self, term: TermId, nth: usize) -> &[u32] {
        let slot = term.as_usize();
        let Some(&start) = self.term_postings_start.get(slot) else {
            return &[];
        };
        let end = self.term_postings_start[slot + 1];

        let posting = start as usize + nth;
        if posting >= end as usize {
            return &[];
        }

        let from = self.posting_positions_start[posting] as usize;
        let to = self.posting_positions_start[posting + 1] as usize;
        &self.positions[from..to]
    }

    /// Every posting for `term`.
    #[must_use]
    pub fn postings(&self, term: TermId) -> Postings<'_> {
        Postings {
            index: self,
            term,
            next: 0,
            count: self.document_frequency(term),
        }
    }

    /// Every posting for a term given by its normalized text.
    #[must_use]
    pub fn postings_for(&self, term: &str) -> Option<Postings<'_>> {
        self.term_id(term).map(|id| self.postings(id))
    }

    /// Every term in the index, with its identifier, in arbitrary order.
    pub fn vocabulary(&self) -> impl Iterator<Item = (&str, TermId)> {
        self.dictionary.iter().map(|(term, &id)| (&**term, id))
    }

    /// What the index holds and roughly what it costs.
    #[must_use]
    pub fn stats(&self) -> IndexStats {
        let dictionary_bytes: usize = self
            .dictionary
            .keys()
            // The string's own bytes, the `Box<str>` fat pointer, the `TermId`,
            // and hashbrown's one control byte per slot.
            .map(|term| term.len() + size_of::<Box<str>>() + size_of::<TermId>() + 1)
            .sum();

        let array_bytes = self.term_postings_start.len() * size_of::<u32>()
            + self.posting_docs.len() * size_of::<DocId>()
            + self.posting_positions_start.len() * size_of::<u32>()
            + self.positions.len() * size_of::<u32>();

        IndexStats {
            documents: self.documents(),
            terms: self.terms(),
            postings: self.posting_docs.len(),
            positions: self.positions.len(),
            bytes: array_bytes + dictionary_bytes,
        }
    }
}

/// An iterator over one term's postings.
#[derive(Debug, Clone)]
pub struct Postings<'a> {
    index: &'a Index,
    term: TermId,
    next: usize,
    count: usize,
}

impl<'a> Iterator for Postings<'a> {
    type Item = Posting<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.count {
            return None;
        }

        let nth = self.next;
        self.next += 1;

        Some(Posting {
            doc_id: self.index.doc_ids(self.term)[nth],
            positions: self.index.positions(self.term, nth),
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.count - self.next;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for Postings<'_> {}

/// A summary of an index's contents and cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStats {
    /// Documents indexed.
    pub documents: usize,
    /// Distinct terms.
    pub terms: usize,
    /// (term, document) pairs.
    pub postings: usize,
    /// Individual term occurrences.
    pub positions: usize,
    /// Approximate heap bytes held by the index.
    pub bytes: usize,
}

/// Builds an index from documents that can be iterated twice.
///
/// The closure is called once per pass and must yield the same documents, in
/// the same order, both times.
///
/// # Errors
///
/// Propagates whatever the corpus iterator produces, and returns
/// [`Error::CorpusChanged`] if the two passes disagree.
pub fn build<F, I>(mut corpus: F) -> Result<Index>
where
    F: FnMut() -> Result<I>,
    I: Iterator<Item = Result<Document>>,
{
    let mut builder = IndexBuilder::new();
    for document in corpus()? {
        builder.add_document(&document?);
    }

    let mut assembler = builder.finish_counting()?;
    for document in corpus()? {
        assembler.add_document(&document?);
    }

    assembler.finish()
}

#[cfg(test)]
mod tests {
    use super::{FIELD_GAP, Index, IndexBuilder, Posting};
    use crate::corpus::{DocId, Document};

    fn document(id: u32, title: &str, body: &str) -> Document {
        Document {
            id: DocId::new(id),
            external_id: format!("doc{id}"),
            title: title.to_owned(),
            body: body.to_owned(),
        }
    }

    /// Runs both passes over the same documents.
    fn index_of(documents: &[Document]) -> Index {
        let mut builder = IndexBuilder::new();
        for document in documents {
            builder.add_document(document);
        }

        let mut assembler = builder.finish_counting().expect("corpus is small");
        for document in documents {
            assembler.add_document(document);
        }

        assembler.finish().expect("both passes saw the same corpus")
    }

    fn corpus() -> Vec<Document> {
        vec![
            document(0, "Quantum error correction", "surface code quantum"),
            document(1, "Machine learning", "neural networks learn"),
            document(2, "Quantum computing", "quantum quantum quantum"),
        ]
    }

    #[test]
    fn an_empty_corpus_produces_an_empty_index() {
        let index = index_of(&[]);
        let stats = index.stats();

        assert_eq!(stats.documents, 0);
        assert_eq!(stats.terms, 0);
        assert_eq!(stats.postings, 0);
        assert_eq!(stats.positions, 0);
        assert!(index.term_id("anything").is_none());
    }

    #[test]
    fn terms_are_found_by_their_normalized_form() {
        let index = index_of(&corpus());

        assert!(index.term_id("quantum").is_some());
        // The corpus says "Quantum" with a capital Q; the index stores it
        // lowercased, and does not normalize on lookup.
        assert!(index.term_id("Quantum").is_none());
        assert!(index.term_id("nonexistent").is_none());
    }

    #[test]
    fn a_term_lists_exactly_the_documents_containing_it() {
        let index = index_of(&corpus());

        let quantum = index.term_id("quantum").expect("indexed");
        assert_eq!(
            index.doc_ids(quantum),
            [DocId::new(0), DocId::new(1 + 1)] // documents 0 and 2
        );

        let learning = index.term_id("learning").expect("indexed");
        assert_eq!(index.doc_ids(learning), [DocId::new(1)]);
    }

    #[test]
    fn document_ids_within_a_term_are_ascending() {
        let index = index_of(&corpus());

        for (_, id) in index.vocabulary() {
            let docs = index.doc_ids(id);
            assert!(
                docs.windows(2).all(|pair| pair[0] < pair[1]),
                "postings for {id} are not sorted: {docs:?}"
            );
        }
    }

    #[test]
    fn positions_record_every_occurrence_in_order() {
        let index = index_of(&corpus());
        let quantum = index.term_id("quantum").expect("indexed");

        // Document 2: title "Quantum computing", body "quantum quantum quantum".
        // Title positions 0..1, body starts at 2 + FIELD_GAP.
        let body_start = 2 + FIELD_GAP;
        assert_eq!(
            index.positions(quantum, 1),
            [0, body_start, body_start + 1, body_start + 2]
        );
    }

    #[test]
    fn the_field_gap_separates_the_title_from_the_body() {
        let index = index_of(&[document(0, "alpha beta", "gamma")]);

        let beta = index.term_id("beta").expect("indexed");
        let gamma = index.term_id("gamma").expect("indexed");

        assert_eq!(index.positions(beta, 0), [1]);
        // "gamma" is the first body token, so it lands a whole gap later rather
        // than at position 2 — no phrase can match across the join.
        assert_eq!(index.positions(gamma, 0), [2 + FIELD_GAP]);
    }

    #[test]
    fn document_frequency_counts_documents_not_occurrences() {
        let index = index_of(&corpus());
        let quantum = index.term_id("quantum").expect("indexed");

        // Four occurrences in document 2 alone, but two documents.
        assert_eq!(index.document_frequency(quantum), 2);
        assert_eq!(index.positions(quantum, 1).len(), 4);
    }

    #[test]
    fn postings_pair_each_document_with_its_positions() {
        let index = index_of(&corpus());
        let quantum = index.term_id("quantum").expect("indexed");

        let postings: Vec<Posting<'_>> = index.postings(quantum).collect();
        assert_eq!(postings.len(), 2);
        assert_eq!(postings[0].doc_id, DocId::new(0));
        assert_eq!(postings[1].doc_id, DocId::new(2));
        assert_eq!(postings[1].frequency(), 4);

        for posting in &postings {
            assert_eq!(
                posting.positions,
                index.positions(
                    quantum,
                    if posting.doc_id == DocId::new(0) {
                        0
                    } else {
                        1
                    }
                )
            );
        }
    }

    #[test]
    fn postings_report_their_length_without_being_counted() {
        let index = index_of(&corpus());
        let quantum = index.term_id("quantum").expect("indexed");

        assert_eq!(index.postings(quantum).len(), 2);
    }

    #[test]
    fn an_unknown_term_id_yields_nothing_rather_than_panicking() {
        let index = index_of(&corpus());
        let beyond = super::TermId::new(9_999);

        assert!(index.doc_ids(beyond).is_empty());
        assert!(index.positions(beyond, 0).is_empty());
        assert_eq!(index.document_frequency(beyond), 0);
    }

    #[test]
    fn asking_for_a_posting_past_the_end_yields_nothing() {
        let index = index_of(&corpus());
        let learning = index.term_id("learning").expect("indexed");

        assert_eq!(index.positions(learning, 0).len(), 1);
        assert!(index.positions(learning, 1).is_empty());
    }

    #[test]
    fn a_second_pass_over_a_different_corpus_is_refused() {
        let mut builder = IndexBuilder::new();
        builder.add_document(&document(0, "alpha", "beta"));

        let mut assembler = builder.finish_counting().expect("small");
        // A term the counting pass never saw.
        assembler.add_document(&document(0, "alpha", "surprise"));

        assert!(matches!(
            assembler.finish(),
            Err(crate::Error::CorpusChanged { .. })
        ));
    }

    #[test]
    fn stats_add_up() {
        let index = index_of(&corpus());
        let stats = index.stats();

        assert_eq!(stats.documents, 3);

        let postings: usize = index
            .vocabulary()
            .map(|(_, id)| index.document_frequency(id))
            .sum();
        assert_eq!(stats.postings, postings);

        let positions: usize = index
            .vocabulary()
            .flat_map(|(_, id)| index.postings(id))
            .map(|posting| posting.frequency())
            .sum();
        assert_eq!(stats.positions, positions);

        assert!(stats.bytes > stats.positions * 4);
    }

    #[test]
    fn every_position_slice_is_sorted_and_within_the_document() {
        let index = index_of(&corpus());

        for (_, id) in index.vocabulary() {
            for posting in index.postings(id) {
                assert!(!posting.positions.is_empty());
                assert!(
                    posting.positions.windows(2).all(|pair| pair[0] < pair[1]),
                    "positions out of order: {:?}",
                    posting.positions
                );
            }
        }
    }
}
