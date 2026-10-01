//! Boolean evaluation checked against brute force.
//!
//! The unit tests in `src/search.rs` pin down the pieces: that `gallop` agrees
//! with `partition_point`, that the planner reorders an `AND` spine, that an
//! exclusion can be hoisted. This file asks the only question that really
//! matters — does the answer come out right — and asks it of a reference
//! implementation that shares no code with the engine.
//!
//! The reference is `BTreeMap<String, BTreeSet<DocId>>` and plain set algebra:
//! `intersection`, `union`, `difference` from the standard library. No
//! galloping, no compressed-sparse-row arrays, no query planning, no
//! reordering — so when the two agree, they agree for a reason. A bug in the
//! gallop window, or a planner that reordered an exclusion it should not have,
//! would show up as a disagreement rather than as a plausible-looking list of
//! documents.
//!
//! Phrases get a second, even blunter reference (day 9): each document's terms
//! as one flat `Vec<String>` with a sentinel between title and body, searched
//! with `windows(k)`. It knows nothing of positions, postings or `FIELD_GAP` —
//! it just looks for the words next to each other, the way a person reading the
//! document would.
//!
//! Proximity needs positions, so day 10 adds a third reference: a
//! `BTreeMap<String, Vec<u32>>` per document, with the title/body offset
//! re-derived here rather than imported, and extents computed by nested loops
//! over every pair. No sorting, no `partition_point`, no early exit, no anchor
//! heuristic. It is the definition of `NEAR` typed out, and it disagrees with
//! the engine if the engine is wrong about an edge.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use boolsearch::{
    DocId, Document, Expr, FIELD_GAP, Index, IndexBuilder, JsonlCorpus, evaluate, parse, tokenize,
};
use proptest::prelude::*;

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

/// Which documents hold each term, built the obvious way.
type Reference = BTreeMap<String, BTreeSet<DocId>>;

fn reference_of(documents: &[Document]) -> Reference {
    let mut reference = Reference::new();

    for document in documents {
        for text in [&document.title, &document.body] {
            for token in tokenize(text) {
                reference
                    .entry(token.normalized().into_owned())
                    .or_default()
                    .insert(document.id);
            }
        }
    }

    reference
}

/// Evaluates a query with standard-library set operations.
///
/// Recursive, allocating, and quadratic in places. All three are fine: it only
/// ever sees twenty documents, and being obviously right is its entire job.
fn brute_force(expression: &Expr, fixture: &Fixture) -> BTreeSet<DocId> {
    match expression {
        Expr::Term(term) => fixture.reference.get(term).cloned().unwrap_or_default(),
        Expr::Phrase(terms) => brute_force_phrase(terms, &fixture.words),
        Expr::And(left, right) => brute_force(left, fixture)
            .intersection(&brute_force(right, fixture))
            .copied()
            .collect(),
        Expr::Or(left, right) => brute_force(left, fixture)
            .union(&brute_force(right, fixture))
            .copied()
            .collect(),
        Expr::Not(left, right) => brute_force(left, fixture)
            .difference(&brute_force(right, fixture))
            .copied()
            .collect(),
        Expr::Near { .. } => fixture
            .positions
            .iter()
            .enumerate()
            .filter(|(_, at)| !brute_extents(expression, at).is_empty())
            .map(|(nth, _)| DocId::new(u32::try_from(nth).expect("small fixture")))
            .collect(),
        Expr::Prefix(_) => unreachable!("day 11"),
    }
}

/// Every document's terms in order, with `None` marking the title/body join.
///
/// The join has to be represented, not elided: `FIELD_GAP` exists so that the
/// last word of a title and the first of a body are not adjacent, and a
/// reference that ran them together would disagree with the engine on exactly
/// the case the gap was introduced for. A sentinel no real term can equal is
/// the simplest way to say "these two are not neighbours".
type Words = Vec<Vec<Option<String>>>;

fn words_of(documents: &[Document]) -> Words {
    documents
        .iter()
        .map(|document| {
            let title = tokenize(&document.title).map(|t| Some(t.normalized().into_owned()));
            let body = tokenize(&document.body).map(|t| Some(t.normalized().into_owned()));
            title.chain(std::iter::once(None)).chain(body).collect()
        })
        .collect()
}

/// The documents where `phrase` appears as consecutive words.
///
/// `windows` over a flat list. Quadratic, allocating, and incapable of being
/// subtly wrong about a prefix sum.
fn brute_force_phrase(phrase: &[String], words: &Words) -> BTreeSet<DocId> {
    let wanted: Vec<Option<String>> = phrase.iter().cloned().map(Some).collect();

    words
        .iter()
        .enumerate()
        .filter(|(_, document)| {
            !wanted.is_empty()
                && document.len() >= wanted.len()
                && document
                    .windows(wanted.len())
                    .any(|window| window == &wanted[..])
        })
        .map(|(nth, _)| DocId::new(u32::try_from(nth).expect("small fixture")))
        .collect()
}

/// Where every term occurs in one document, ascending.
type DocPositions = BTreeMap<String, Vec<u32>>;

/// Positions per document, with the field offset worked out from scratch.
///
/// Deliberately re-derives `FIELD_GAP` arithmetic instead of calling the
/// index's own helper. If both used the same function, agreeing about the
/// title/body join would prove nothing about it.
fn positions_of(documents: &[Document]) -> Vec<DocPositions> {
    documents
        .iter()
        .map(|document| {
            let mut at: DocPositions = BTreeMap::new();
            let mut after_title = 0;

            for token in tokenize(&document.title) {
                at.entry(token.normalized().into_owned())
                    .or_default()
                    .push(token.position);
                after_title = token.position + 1;
            }

            let offset = after_title + FIELD_GAP;
            for token in tokenize(&document.body) {
                at.entry(token.normalized().into_owned())
                    .or_default()
                    .push(token.position + offset);
            }

            at
        })
        .collect()
}

/// The extents at which `expression` matches in one document, the slow way.
///
/// Every case is the definition written out: a phrase tries every position of
/// its first word, a `NEAR` tries every pair of extents. Quadratic and
/// allocating, over twenty documents, and incapable of an off-by-one in a
/// window bound because there are no window bounds.
fn brute_extents(expression: &Expr, at: &DocPositions) -> Vec<(u32, u32)> {
    let mut extents = match expression {
        Expr::Term(term) => at
            .get(term)
            .map(|positions| positions.iter().map(|&p| (p, p)).collect())
            .unwrap_or_default(),

        Expr::Phrase(terms) => {
            let Some(first) = terms.first().and_then(|term| at.get(term)) else {
                return Vec::new();
            };
            let width = u32::try_from(terms.len() - 1).expect("short phrase");

            first
                .iter()
                .filter(|&&start| {
                    terms.iter().enumerate().all(|(offset, term)| {
                        let offset = u32::try_from(offset).expect("short phrase");
                        at.get(term)
                            .is_some_and(|positions| positions.contains(&(start + offset)))
                    })
                })
                .map(|&start| (start, start + width))
                .collect()
        }

        Expr::Or(left, right) => {
            let mut both = brute_extents(left, at);
            both.extend(brute_extents(right, at));
            both
        }

        Expr::Near {
            left,
            right,
            distance,
            ordered,
        } => {
            let first = brute_extents(left, at);
            let second = brute_extents(right, at);
            let mut paired = Vec::new();

            for &(a_start, a_end) in &first {
                for &(b_start, b_end) in &second {
                    if b_start > a_end && b_start - a_end <= *distance {
                        paired.push((a_start, b_end));
                    }
                }
            }
            if !*ordered {
                for &(b_start, b_end) in &second {
                    for &(a_start, a_end) in &first {
                        if a_start > b_end && a_start - b_end <= *distance {
                            paired.push((b_start, a_end));
                        }
                    }
                }
            }

            paired
        }

        Expr::And(..) | Expr::Not(..) | Expr::Prefix(_) => {
            unreachable!("not positional, or day 11")
        }
    };

    extents.sort_unstable();
    extents.dedup();
    extents
}

struct Fixture {
    index: Index,
    reference: Reference,
    words: Words,
    positions: Vec<DocPositions>,
    vocabulary: Vec<String>,
    /// Word sequences that really occur in the fixture, 2 to 5 words long.
    ///
    /// A phrase strategy built only from random vocabulary words would match
    /// nothing, and an evaluator that always returned nothing would pass. These
    /// are drawn out of the documents themselves, so roughly half the generated
    /// phrases have an answer to get wrong.
    ngrams: Vec<Vec<String>>,
}

fn ngrams_of(words: &Words) -> Vec<Vec<String>> {
    let mut ngrams = Vec::new();

    for document in words {
        for length in 2..=5 {
            for window in document.windows(length) {
                // A window containing the title/body sentinel is not a phrase
                // that occurs — which is exactly the case worth generating.
                if let Some(phrase) = window.iter().cloned().collect::<Option<Vec<String>>>() {
                    ngrams.push(phrase);
                }
            }
        }
    }

    ngrams.sort();
    ngrams.dedup();
    ngrams
}

/// Built once, because building it twenty-thousand times for proptest would
/// make the property test the slowest thing in the suite.
static FIXTURE: LazyLock<Fixture> = LazyLock::new(|| {
    let documents = documents();
    let reference = reference_of(&documents);
    let vocabulary = reference.keys().cloned().collect();

    let words = words_of(&documents);
    let ngrams = ngrams_of(&words);

    Fixture {
        index: index_of(&documents),
        reference,
        words,
        positions: positions_of(&documents),
        vocabulary,
        ngrams,
    }
});

/// Asserts that the engine and the reference agree, and says how they differ
/// when they do not.
fn agree(expression: &Expr) {
    let found = evaluate(expression, &FIXTURE.index).expect("no unsupported operators");
    let expected: Vec<DocId> = brute_force(expression, &FIXTURE).into_iter().collect();

    assert_eq!(found, expected, "{expression}");
}

#[test]
fn the_day_eight_acceptance_query_returns_the_right_documents() {
    // `docs/PLAN.md` names this one. The fixture has `quantum` and `classical`
    // but no `entanglement`, so the answer is empty — and that is worth
    // asserting too: an unknown term has to narrow the result to nothing
    // rather than being quietly ignored.
    let expression = parse("quantum AND entanglement NOT classical").expect("valid query");
    agree(&expression);
    assert!(
        evaluate(&expression, &FIXTURE.index)
            .expect("supported")
            .is_empty()
    );

    // The same query with a term the fixture does have.
    let expression = parse("quantum AND production NOT classical").expect("valid query");
    agree(&expression);
    assert!(
        !evaluate(&expression, &FIXTURE.index)
            .expect("supported")
            .is_empty()
    );
}

#[test]
fn hand_written_queries_agree_with_brute_force() {
    for query in [
        "quantum",
        "quantum AND production",
        "quantum OR classical",
        "quantum NOT classical",
        "quantum production",
        "(quantum OR classical) AND model",
        "quantum AND (production OR model)",
        "quantum AND production AND model",
        "quantum OR production OR model",
        "quantum NOT classical NOT model",
        "quantum AND (classical NOT model)",
        "(quantum NOT classical) AND model",
        "quantum AND flurbles",
        "quantum OR flurbles",
        "flurbles NOT quantum",
    ] {
        agree(&parse(query).expect("valid query"));
    }
}

#[test]
fn results_are_sorted_and_free_of_duplicates() {
    // Every operator has to preserve this, because every operator's input is
    // another operator's output. One unsorted intermediate and the galloping
    // search silently starts missing documents.
    for query in [
        "quantum OR classical OR model OR production",
        "(quantum OR model) AND (classical OR production)",
        "(quantum OR model) NOT (classical OR production)",
    ] {
        let found = evaluate(&parse(query).expect("valid query"), &FIXTURE.index)
            .expect("no unsupported operators");

        assert!(found.windows(2).all(|pair| pair[0] < pair[1]), "{query}");
    }
}

#[test]
fn every_hit_really_contains_every_required_term() {
    // Independent of the reference index: goes back to the documents. If both
    // the engine and the reference tokenized something the same wrong way,
    // this is the test that would still notice the hit is not a hit.
    let documents = documents();
    let found = evaluate(
        &parse("quantum AND production").expect("valid query"),
        &FIXTURE.index,
    )
    .expect("no unsupported operators");

    assert!(!found.is_empty());
    for id in found {
        let document = &documents[id.as_usize()];
        let text = format!("{} {}", document.title, document.body).to_lowercase();
        assert!(text.contains("quantum"), "{}", document.external_id);
        assert!(text.contains("production"), "{}", document.external_id);
    }
}

#[test]
fn the_day_nine_acceptance_phrases_return_the_right_documents() {
    // `docs/PLAN.md` asks for phrases of 2, 3 and 5 terms and a near miss.
    for query in [
        "'quantum chromodynamics'",
        "'error correction'",
        "'surface code quantum error correction'",
        "'perturbative quantum chromodynamics'",
        "'edge-disjoint spanning trees'",
        "'quantum error correction at threshold'",
        // Right words, wrong order.
        "'chromodynamics quantum'",
        "'correction error'",
        // Words that all occur, never adjacent.
        "'quantum trees'",
        // A word the fixture has never seen.
        "'quantum flurbles'",
    ] {
        agree(&parse(query).expect("valid query"));
    }

    // Two of those must actually find something, or the test proves nothing.
    let hits = |query: &str| {
        evaluate(&parse(query).expect("valid query"), &FIXTURE.index).expect("supported")
    };
    assert!(!hits("'quantum chromodynamics'").is_empty());
    assert!(!hits("'surface code quantum error correction'").is_empty());
    assert!(hits("'chromodynamics quantum'").is_empty());
    assert!(hits("'quantum trees'").is_empty());
}

#[test]
fn a_phrase_is_narrower_than_the_conjunction_of_its_words() {
    // Not a tautology worth skipping: it is the property that would break if
    // the positional stage were accidentally a no-op, and it would break
    // silently, because every count would still look plausible.
    for words in [
        "quantum chromodynamics",
        "error correction",
        "spanning trees",
    ] {
        let phrase = parse(&format!("'{words}'")).expect("valid query");
        let conjunction = parse(words).expect("valid query");

        let phrase_hits = evaluate(&phrase, &FIXTURE.index).expect("supported");
        let conjunction_hits = evaluate(&conjunction, &FIXTURE.index).expect("supported");

        assert!(!phrase_hits.is_empty(), "{words}");
        assert!(phrase_hits.len() <= conjunction_hits.len(), "{words}");
        assert!(
            phrase_hits.iter().all(|doc| conjunction_hits.contains(doc)),
            "{words}"
        );
    }
}

#[test]
fn a_phrase_never_spans_the_title_body_join() {
    // Derived from the fixture rather than hard-coded: for every document, the
    // last word of the title followed by the first word of the body is a phrase
    // that must not match that document. `FIELD_GAP` is the only thing stopping
    // it, and nothing else in the suite would notice if the gap were removed.
    let mut checked = 0;

    for (nth, document) in FIXTURE.words.iter().enumerate() {
        let join = document.iter().position(Option::is_none).expect("sentinel");
        let (Some(Some(last)), Some(Some(first))) =
            (document.get(join.wrapping_sub(1)), document.get(join + 1))
        else {
            continue;
        };

        let straddling = Expr::Phrase(vec![last.clone(), first.clone()]);
        let found = evaluate(&straddling, &FIXTURE.index).expect("supported");
        let doc = DocId::new(u32::try_from(nth).expect("small fixture"));

        assert!(!found.contains(&doc), "doc {nth}: {last:?} {first:?}");
        checked += 1;
    }

    assert!(checked >= 15, "only checked {checked} documents");
}

#[test]
fn the_day_ten_acceptance_queries_return_the_right_documents() {
    for query in [
        "quantum NEAR/3 error",
        "quantum NEAR/1 error",
        "quantum ONEAR/3 error",
        "error ONEAR/3 quantum",
        "'error correction' NEAR/5 surface",
        "surface NEAR/5 'error correction'",
        "(quantum OR optical) NEAR/4 code",
        "(quantum NEAR/2 error) NEAR/4 threshold",
        "quantum NEAR/9999 trees",
        "quantum NEAR/3 flurbles",
        "quantum AND (surface NEAR/6 code) NOT classical",
    ] {
        agree(&parse(query).expect("valid query"));
    }

    let hits = |query: &str| {
        evaluate(&parse(query).expect("valid query"), &FIXTURE.index).expect("supported")
    };
    assert!(!hits("quantum NEAR/3 error").is_empty());
    assert!(hits("quantum NEAR/3 flurbles").is_empty());
}

#[test]
fn near_one_is_the_phrase_in_either_order() {
    // Checked against the engine's *own* phrase search rather than the
    // reference, because the point is the relationship between two operators,
    // not whether either is right.
    for (left, right) in [
        ("quantum", "error"),
        ("error", "correction"),
        ("surface", "code"),
        ("the", "quantum"),
    ] {
        let near = evaluate(
            &parse(&format!("{left} NEAR/1 {right}")).expect("valid"),
            &FIXTURE.index,
        )
        .expect("supported");
        let phrases = evaluate(
            &parse(&format!("'{left} {right}' OR '{right} {left}'")).expect("valid"),
            &FIXTURE.index,
        )
        .expect("supported");

        assert_eq!(near, phrases, "{left} NEAR/1 {right}");

        let ordered = evaluate(
            &parse(&format!("{left} ONEAR/1 {right}")).expect("valid"),
            &FIXTURE.index,
        )
        .expect("supported");
        let phrase = evaluate(
            &parse(&format!("'{left} {right}'")).expect("valid"),
            &FIXTURE.index,
        )
        .expect("supported");

        assert_eq!(ordered, phrase, "{left} ONEAR/1 {right}");
    }
}

#[test]
fn widening_the_distance_can_only_add_documents() {
    // Monotonicity. Cheap to check, and it would catch a window bound that
    // was right at k=3 and wrong at k=4.
    for (left, right) in [("quantum", "error"), ("surface", "threshold")] {
        let mut previous = Vec::new();

        for distance in 1..=12 {
            let found = evaluate(
                &parse(&format!("{left} NEAR/{distance} {right}")).expect("valid"),
                &FIXTURE.index,
            )
            .expect("supported");

            assert!(
                previous.iter().all(|doc| found.contains(doc)),
                "{left} NEAR/{distance} {right} lost a document the narrower query found"
            );
            previous = found;
        }
    }
}

#[test]
fn an_ordered_match_is_always_also_an_unordered_one() {
    for query in [
        ("quantum", "error", 4),
        ("error", "correction", 2),
        ("the", "surface", 6),
    ] {
        let (left, right, distance) = query;
        let ordered = evaluate(
            &parse(&format!("{left} ONEAR/{distance} {right}")).expect("valid"),
            &FIXTURE.index,
        )
        .expect("supported");
        let unordered = evaluate(
            &parse(&format!("{left} NEAR/{distance} {right}")).expect("valid"),
            &FIXTURE.index,
        )
        .expect("supported");

        assert!(
            ordered.iter().all(|doc| unordered.contains(doc)),
            "{left}/{right}"
        );
    }
}

#[test]
fn the_two_phrase_references_agree_with_each_other() {
    // One scans a flat word list with `windows`; the other works from a
    // position map. Both are independent of the engine, and if they disagreed
    // the proptests below would be checking against a coin flip.
    for ngram in FIXTURE.ngrams.iter().take(400) {
        let expression = Expr::Phrase(ngram.clone());

        let windowed = brute_force_phrase(ngram, &FIXTURE.words);
        let positional: BTreeSet<DocId> = FIXTURE
            .positions
            .iter()
            .enumerate()
            .filter(|(_, at)| !brute_extents(&expression, at).is_empty())
            .map(|(nth, _)| DocId::new(u32::try_from(nth).expect("small fixture")))
            .collect();

        assert_eq!(windowed, positional, "{expression}");
    }
}

/// A random query tree over terms the fixture actually contains.
///
/// Drawing from the real vocabulary rather than random strings is the point:
/// random strings would make almost every clause empty, and an evaluator that
/// returns nothing for everything would pass.
fn any_expression() -> impl Strategy<Value = Expr> {
    let terms = FIXTURE.vocabulary.clone();

    prop_oneof![
        4 => prop::sample::select(terms).prop_map(Expr::Term),
        2 => any_phrase(),
        2 => any_near(),
    ]
    .prop_recursive(4, 24, 2, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone())
                .prop_map(|(left, right)| Expr::And(Box::new(left), Box::new(right))),
            (inner.clone(), inner.clone())
                .prop_map(|(left, right)| Expr::Or(Box::new(left), Box::new(right))),
            (inner.clone(), inner)
                .prop_map(|(left, right)| Expr::Not(Box::new(left), Box::new(right))),
        ]
    })
}

/// A random positional expression: anything `NEAR` is allowed to take.
///
/// Terms, phrases, `OR` of those, and nested `NEAR` — the set `is_positional`
/// admits. `AND` and `NOT` are deliberately absent, because the parser rejects
/// them here and the generator should not be testing a path that cannot exist.
fn any_positional() -> impl Strategy<Value = Expr> {
    let terms = FIXTURE.vocabulary.clone();

    prop_oneof![
        3 => prop::sample::select(terms).prop_map(Expr::Term),
        2 => any_phrase(),
    ]
    .prop_recursive(2, 6, 2, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone())
                .prop_map(|(left, right)| Expr::Or(Box::new(left), Box::new(right))),
            (inner.clone(), inner, 1_u32..=8, any::<bool>()).prop_map(
                |(left, right, distance, ordered)| Expr::Near {
                    left: Box::new(left),
                    right: Box::new(right),
                    distance,
                    ordered,
                }
            ),
        ]
    })
}

/// A random `NEAR` over two positional operands.
fn any_near() -> impl Strategy<Value = Expr> {
    (any_positional(), any_positional(), 1_u32..=8, any::<bool>()).prop_map(
        |(left, right, distance, ordered)| Expr::Near {
            left: Box::new(left),
            right: Box::new(right),
            distance,
            ordered,
        },
    )
}

/// A random phrase, half of them real and half of them made up.
///
/// The real ones come out of the documents, so they have hits and the engine
/// has something to get wrong. The shuffled and assembled ones mostly have no
/// hits — which is the other half of the job, since a phrase search that
/// matched its words in any order would sail through the first kind.
fn any_phrase() -> impl Strategy<Value = Expr> {
    let ngrams = FIXTURE.ngrams.clone();
    let terms = FIXTURE.vocabulary.clone();

    prop_oneof![
        // Really occurs somewhere.
        2 => prop::sample::select(ngrams.clone()).prop_map(Expr::Phrase),
        // Really occurs, reversed: usually a near miss, occasionally a
        // palindrome that still matches, and the reference decides which.
        1 => prop::sample::select(ngrams).prop_map(|mut words| {
            words.reverse();
            Expr::Phrase(words)
        }),
        // Assembled from real words that were probably never neighbours.
        1 => prop::collection::vec(prop::sample::select(terms), 2..=4).prop_map(Expr::Phrase),
    ]
}

proptest! {
    /// The load-bearing test of the day.
    ///
    /// Hand-written cases check the shapes someone thought of. This checks the
    /// ones nobody did: deeply nested exclusions, `OR` inside `NOT` inside
    /// `AND`, the same term on both sides of a difference. Any of those could
    /// break the planner's right to reorder a spine, and none of them is
    /// obvious enough to have written down.
    #[test]
    fn any_boolean_query_agrees_with_brute_force(expression in any_expression()) {
        let found = evaluate(&expression, &FIXTURE.index).expect("no unsupported operators");
        let expected: Vec<DocId> = brute_force(&expression, &FIXTURE).into_iter().collect();

        prop_assert_eq!(&found, &expected, "{}", expression);
    }

    /// Phrases specifically, against the flat-`windows` reference.
    ///
    /// Separate from the tree property above so that a failure says whether the
    /// positional stage or the Boolean one is wrong, and so the generator can
    /// lean hard on phrases that really occur.
    #[test]
    fn any_phrase_agrees_with_a_flat_window_scan(expression in any_phrase()) {
        let found = evaluate(&expression, &FIXTURE.index).expect("phrases work");
        let expected: Vec<DocId> = brute_force(&expression, &FIXTURE).into_iter().collect();

        prop_assert_eq!(&found, &expected, "{}", expression);
    }

    /// Proximity specifically, against the nested-loop reference.
    ///
    /// The generator nests `NEAR` inside `NEAR` and puts phrases and `OR` on
    /// either side, which is where the extent arithmetic can go wrong in ways
    /// no hand-written case would reach: an inner match whose span is wider
    /// than either operand, two candidate extents whose nearest pair is not
    /// the first pair, a phrase whose own occurrences overlap.
    #[test]
    fn any_proximity_query_agrees_with_a_nested_loop_reference(expression in any_near()) {
        let found = evaluate(&expression, &FIXTURE.index).expect("proximity works");
        let expected: Vec<DocId> = brute_force(&expression, &FIXTURE).into_iter().collect();

        prop_assert_eq!(&found, &expected, "{}", expression);
    }

    /// Everything the parser accepts as a `NEAR` operand, it must evaluate.
    #[test]
    fn every_positional_expression_is_evaluable(expression in any_positional()) {
        prop_assert!(expression.is_positional(), "{}", expression);
        prop_assert!(evaluate(&expression, &FIXTURE.index).is_ok(), "{}", expression);
    }

    /// Re-parsing a printed tree must evaluate identically.
    ///
    /// Day 7 asserted the two parse to equal trees. This asserts the trees
    /// answer the same question, which is the part a user would notice.
    #[test]
    fn printing_and_re_parsing_preserves_the_answer(expression in any_expression()) {
        let printed = expression.to_string();
        let reparsed = parse(&printed).expect("the printer emits valid queries");

        prop_assert_eq!(
            evaluate(&expression, &FIXTURE.index).expect("supported"),
            evaluate(&reparsed, &FIXTURE.index).expect("supported"),
            "{}", printed
        );
    }
}
