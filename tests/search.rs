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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use boolsearch::{
    DocId, Document, Expr, Index, IndexBuilder, JsonlCorpus, evaluate, parse, tokenize,
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
fn brute_force(expression: &Expr, reference: &Reference) -> BTreeSet<DocId> {
    match expression {
        Expr::Term(term) => reference.get(term).cloned().unwrap_or_default(),
        Expr::And(left, right) => brute_force(left, reference)
            .intersection(&brute_force(right, reference))
            .copied()
            .collect(),
        Expr::Or(left, right) => brute_force(left, reference)
            .union(&brute_force(right, reference))
            .copied()
            .collect(),
        Expr::Not(left, right) => brute_force(left, reference)
            .difference(&brute_force(right, reference))
            .copied()
            .collect(),
        Expr::Phrase(_) | Expr::Near { .. } | Expr::Prefix(_) => {
            unreachable!("days 9 to 11")
        }
    }
}

struct Fixture {
    index: Index,
    reference: Reference,
    vocabulary: Vec<String>,
}

/// Built once, because building it twenty-thousand times for proptest would
/// make the property test the slowest thing in the suite.
static FIXTURE: LazyLock<Fixture> = LazyLock::new(|| {
    let documents = documents();
    let reference = reference_of(&documents);
    let vocabulary = reference.keys().cloned().collect();

    Fixture {
        index: index_of(&documents),
        reference,
        vocabulary,
    }
});

/// Asserts that the engine and the reference agree, and says how they differ
/// when they do not.
fn agree(expression: &Expr) {
    let found = evaluate(expression, &FIXTURE.index).expect("no unsupported operators");
    let expected: Vec<DocId> = brute_force(expression, &FIXTURE.reference)
        .into_iter()
        .collect();

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

/// A random query tree over terms the fixture actually contains.
///
/// Drawing from the real vocabulary rather than random strings is the point:
/// random strings would make almost every clause empty, and an evaluator that
/// returns nothing for everything would pass.
fn any_expression() -> impl Strategy<Value = Expr> {
    let terms = FIXTURE.vocabulary.clone();

    prop::sample::select(terms)
        .prop_map(Expr::Term)
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
        let expected: Vec<DocId> =
            brute_force(&expression, &FIXTURE.reference).into_iter().collect();

        prop_assert_eq!(&found, &expected, "{}", expression);
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
