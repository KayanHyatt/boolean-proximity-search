//! Evaluating a parsed query against an index.
//!
//! Days 8 through 11 of `docs/PLAN.md`: Boolean evaluation, then phrases, then
//! proximity, then prefix wildcards.
//!
//! Evaluation walks the AST and combines postings lists. Because both lists are
//! sorted by document id, `AND` is a two-pointer merge and `OR` is the merge
//! step of a merge sort — no hashing, no allocation beyond the output:
//!
//! ```text
//! left   [ 3,  17,  22,  40 ]
//! right  [ 1,  17,  40 ]
//!               ▲    ▲
//! AND    [ 17, 40 ]
//! ```
//!
//! Two refinements that day 8 measures rather than assumes: evaluate the
//! smallest list first, since intersection can only shrink a result, and use
//! galloping (exponential) search when one list is far shorter than the other,
//! so a 10-element list meeting a 10-million-element list costs ten binary
//! searches rather than ten million steps. Measuring the second one turned out
//! to matter: galloping is 426 times faster at a ratio of 28,000:1 and 27%
//! *slower* at 1:1, so it is applied on a measured threshold
//! ([`GALLOP_RATIO`]) rather than always.
//!
//! `NOT` is only supported as the right operand of `AND` — `a NOT b`, never a
//! bare `NOT b`. Complementing a postings list against a 2.7-million-document
//! corpus would materialise nearly the whole corpus to answer a question nobody
//! meant to ask.
//!
//! Day 10 forces a design change worth anticipating: `("machine learning")
//! NEAR/5 medical` means a `NEAR` operand must expose *positions*, not just
//! documents, so evaluation returns positions throughout and discards them only
//! at the top level.
//!
//! # Set operations as iterators
//!
//! [`intersect`], [`union`] and [`difference`] return iterators rather than
//! `Vec`s. That is not decoration. `a AND b AND c` intersects three lists, and
//! an iterator means the intermediate `a ∩ b` never has to exist as a
//! heap allocation if a caller does not want it — and when a caller only wants
//! the first ten hits, `.take(10)` stops the work rather than filtering a
//! finished answer. Each one borrows its two input slices for `'a` and advances
//! by re-slicing, so the iterator itself is four words on the stack.
//!
//! # The planner
//!
//! [`evaluate`] does not walk the `AND` spine in the order it was written. It
//! flattens the spine, estimates how many documents each branch can possibly
//! match, and intersects cheapest-first. On `quantum AND the`, where `the`
//! appears in nearly every document and `quantum` in a few thousand, the
//! difference is between galloping through `the`'s postings a few thousand
//! times and walking all 2.7 million of them.

use std::cmp::Ordering;

use crate::corpus::DocId;
use crate::error::{Error, Result};
use crate::index::Index;
use crate::query::Expr;

// ---------------------------------------------------------------------------
// Galloping search
// ---------------------------------------------------------------------------

/// The first index in `haystack` whose document id is at least `needle`, or
/// `haystack.len()` if there is none.
///
/// This is `partition_point` with a different cost profile. A binary search
/// over a ten-million-element list always costs its 24 comparisons; galloping
/// probes 1, 2, 4, 8 … elements ahead first, so an answer that is three
/// elements away costs four comparisons instead of twenty-four, and one that is
/// genuinely far away costs `2 log d` where `d` is the distance travelled
/// rather than `log n` over the whole list.
///
/// That asymmetry is the whole reason it is here — and also the reason it is
/// not used unconditionally. Intersecting a 10-element list with a
/// 10-million-element one wants long jumps; intersecting two
/// 10-million-element ones wants single steps, and measurement says galloping
/// loses 27% in that case. [`Strategy::Adaptive`] picks between them by length
/// ratio.
fn gallop(haystack: &[DocId], needle: DocId) -> usize {
    // Widen a window whose left edge is known to hold only ids below `needle`.
    // After the loop, `step / 2 - 1` was the last index confirmed too small,
    // so the answer lies in `step / 2 .. min(step, len)`.
    let mut step = 1;
    while step <= haystack.len() && haystack[step - 1] < needle {
        step *= 2;
    }

    let low = step / 2;
    let high = step.min(haystack.len());
    low + haystack[low..high].partition_point(|&doc| doc < needle)
}

// ---------------------------------------------------------------------------
// Set operations
// ---------------------------------------------------------------------------

/// How much longer one list must be than the other before galloping pays.
///
/// Measured, not guessed. On a million-document index, galloping is *27%
/// slower* than stepping when both lists are the same length — the window
/// arithmetic costs more than `&slice[1..]` and it defeats the branch
/// predictor — and 426 times faster at 28,000:1. The crossover sits between
/// 3:1 and 10:1, so 8 takes the win without risking the loss.
/// `docs/RESULTS.md` has the whole curve.
pub const GALLOP_RATIO: usize = 8;

/// How a set operation should skip over documents it does not need.
///
/// Exposed so that day 12's benchmark can measure the choice rather than trust
/// it, and so that this file's tests can assert the two strategies agree on
/// every input. Ordinary callers want [`Strategy::Adaptive`], which is what
/// [`intersect`] and [`difference`] use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strategy {
    /// Gallop when the lists differ in length by at least [`GALLOP_RATIO`],
    /// step otherwise.
    #[default]
    Adaptive,
    /// Always skip ahead by exponential search.
    Gallop,
    /// Always advance one document at a time.
    Linear,
}

impl Strategy {
    /// Whether to gallop over these two lists.
    fn gallops(self, left: &[DocId], right: &[DocId]) -> bool {
        match self {
            Self::Gallop => true,
            Self::Linear => false,
            // Decided once, from the initial lengths. The ratio drifts as the
            // lists are consumed, but re-deciding every step would cost more than
            // the better answer is worth.
            Self::Adaptive => {
                let short = left.len().min(right.len());
                let long = left.len().max(right.len());
                long > short.saturating_mul(GALLOP_RATIO)
            }
        }
    }
}

/// The documents in both `left` and `right`, ascending.
///
/// Both inputs must be sorted and free of duplicates, which is exactly what
/// [`Index::doc_ids`] hands out.
#[must_use]
pub fn intersect<'a>(left: &'a [DocId], right: &'a [DocId]) -> Intersection<'a> {
    intersect_with(left, right, Strategy::Adaptive)
}

/// The documents in both `left` and `right`, with the skipping strategy named.
#[must_use]
pub fn intersect_with<'a>(
    left: &'a [DocId],
    right: &'a [DocId],
    strategy: Strategy,
) -> Intersection<'a> {
    Intersection {
        gallop: strategy.gallops(left, right),
        left,
        right,
    }
}

/// The documents in either `left` or `right`, ascending, without duplicates.
#[must_use]
pub fn union<'a>(left: &'a [DocId], right: &'a [DocId]) -> Union<'a> {
    Union { left, right }
}

/// The documents in `left` that are not in `right`, ascending.
#[must_use]
pub fn difference<'a>(left: &'a [DocId], right: &'a [DocId]) -> Difference<'a> {
    difference_with(left, right, Strategy::Adaptive)
}

/// The documents in `left` but not `right`, with the skipping strategy named.
#[must_use]
pub fn difference_with<'a>(
    left: &'a [DocId],
    right: &'a [DocId],
    strategy: Strategy,
) -> Difference<'a> {
    Difference {
        gallop: strategy.gallops(left, right),
        left,
        right,
    }
}

/// The documents in both of two sorted lists.
///
/// Returned by [`intersect`]. Advances the list that is behind either one
/// document at a time or by exponential search, depending on the [`Strategy`]
/// it was built with.
#[derive(Debug, Clone)]
pub struct Intersection<'a> {
    left: &'a [DocId],
    right: &'a [DocId],
    gallop: bool,
}

impl Iterator for Intersection<'_> {
    type Item = DocId;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (&left, &right) = match (self.left.first(), self.right.first()) {
                (Some(left), Some(right)) => (left, right),
                // One list ran out; nothing further can be in both.
                _ => return None,
            };

            match left.cmp(&right) {
                Ordering::Equal => {
                    self.left = &self.left[1..];
                    self.right = &self.right[1..];
                    return Some(left);
                }
                // Move the behind list up to where the other one is. A gallop
                // returns at least 1 here, since the head is strictly smaller,
                // so the loop always makes progress.
                Ordering::Less => {
                    let step = if self.gallop {
                        gallop(self.left, right)
                    } else {
                        1
                    };
                    self.left = &self.left[step..];
                }
                Ordering::Greater => {
                    let step = if self.gallop {
                        gallop(self.right, left)
                    } else {
                        1
                    };
                    self.right = &self.right[step..];
                }
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // Nothing is guaranteed, but the answer cannot be bigger than the
        // shorter input — which is enough for `collect` to size its allocation
        // once instead of doubling it.
        (0, Some(self.left.len().min(self.right.len())))
    }
}

/// The documents in either of two sorted lists.
///
/// Returned by [`union`]. No strategy to choose here, and none possible: every
/// element of both inputs ends up in the output, so there is nothing to skip.
#[derive(Debug, Clone)]
pub struct Union<'a> {
    left: &'a [DocId],
    right: &'a [DocId],
}

impl Iterator for Union<'_> {
    type Item = DocId;

    fn next(&mut self) -> Option<Self::Item> {
        match (self.left.first(), self.right.first()) {
            (Some(&left), Some(&right)) => {
                // A document in both lists is taken from both, so it is
                // emitted once.
                if left <= right {
                    self.left = &self.left[1..];
                }
                if right <= left {
                    self.right = &self.right[1..];
                }
                Some(left.min(right))
            }
            (Some(&left), None) => {
                self.left = &self.left[1..];
                Some(left)
            }
            (None, Some(&right)) => {
                self.right = &self.right[1..];
                Some(right)
            }
            (None, None) => None,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.left.len();
        let right = self.right.len();
        (left.max(right), Some(left + right))
    }
}

/// The documents in the first of two sorted lists but not the second.
///
/// Returned by [`difference`]. This is what `a NOT b` compiles to, and the
/// reason `NOT` is binary: the left list bounds the work, whereas a unary
/// `NOT b` would have to walk the entire corpus.
#[derive(Debug, Clone)]
pub struct Difference<'a> {
    left: &'a [DocId],
    right: &'a [DocId],
    gallop: bool,
}

impl Iterator for Difference<'_> {
    type Item = DocId;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let &candidate = self.left.first()?;
            self.left = &self.left[1..];

            // Discard everything in the exclusion list below the candidate;
            // later candidates are larger still, so it can never be needed
            // again. Once `right` is empty the skip is a no-op and every
            // remaining candidate passes straight through.
            let step = if self.gallop {
                gallop(self.right, candidate)
            } else {
                self.right
                    .iter()
                    .take_while(|&&doc| doc < candidate)
                    .count()
            };
            self.right = &self.right[step..];

            if self.right.first() != Some(&candidate) {
                return Some(candidate);
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.left.len()))
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// The documents satisfying `expression`, ascending by document id.
///
/// # Errors
///
/// Returns [`Error::NotImplemented`] for phrase, `NEAR` and prefix operators,
/// which days 9 to 11 fill in. The check happens before any work, so a query
/// using one fails the same way whether or not its other clauses match
/// anything.
pub fn evaluate(expression: &Expr, index: &Index) -> Result<Vec<DocId>> {
    if let Some(feature) = unsupported(expression) {
        return Err(Error::NotImplemented(feature));
    }

    evaluate_conjunction(expression, index)
}

/// Evaluates one `AND`/`NOT` spine, cheapest branch first.
fn evaluate_conjunction(expression: &Expr, index: &Index) -> Result<Vec<DocId>> {
    let mut required = Vec::new();
    let mut excluded = Vec::new();
    flatten(expression, &mut required, &mut excluded);

    // The one decision the planner makes. Intersection can only remove
    // documents, so whichever branch matches fewest documents bounds every
    // later step; starting anywhere else means galloping through a long list
    // more times than necessary. `sort_by_key` is stable, so branches with
    // equal estimates keep the order they were written in and the plan stays
    // deterministic — which is what makes the tests reproducible.
    required.sort_by_key(|branch| estimate(branch, index));

    let (&first, rest) = required
        .split_first()
        .expect("flatten always pushes at least one branch");
    let mut matches = evaluate_branch(first, index)?;

    for &branch in rest {
        // An empty running result cannot grow again, so the remaining branches
        // need never be looked at. On `quantum AND flurbles AND the`, this is
        // the difference between one dictionary miss and a walk through every
        // posting of `the`.
        if matches.is_empty() {
            break;
        }

        // A bare term's postings already live in the index as a contiguous
        // sorted slice, so borrow it. The alternative — `evaluate_branch`
        // returning an owned `Vec` — would copy two million document ids in
        // order to read them once.
        let narrowed = match branch {
            Expr::Term(term) => intersect(&matches, term_docs(term, index)).collect(),
            other => {
                let other = evaluate_branch(other, index)?;
                intersect(&matches, &other).collect()
            }
        };
        matches = narrowed;
    }

    // Exclusions last, and in the order they were written. Subtraction costs
    // one gallop per surviving document, so it is cheapest once the required
    // branches have made the result as small as it is going to get.
    for &branch in &excluded {
        if matches.is_empty() {
            break;
        }

        let narrowed = match branch {
            Expr::Term(term) => difference(&matches, term_docs(term, index)).collect(),
            other => {
                let other = evaluate_branch(other, index)?;
                difference(&matches, &other).collect()
            }
        };
        matches = narrowed;
    }

    Ok(matches)
}

/// Evaluates a branch that is not part of the enclosing `AND` spine.
fn evaluate_branch(expression: &Expr, index: &Index) -> Result<Vec<DocId>> {
    match expression {
        Expr::Term(term) => Ok(term_docs(term, index).to_vec()),
        Expr::And(..) | Expr::Not(..) => evaluate_conjunction(expression, index),
        Expr::Or(left, right) => {
            let left = evaluate_branch(left, index)?;
            let right = evaluate_branch(right, index)?;
            Ok(union(&left, &right).collect())
        }
        // Unreachable in practice: `evaluate` rejects these up front. Answered
        // rather than panicked because a total function is one fewer thing to
        // be careful about when days 9 to 11 rewrite this match.
        Expr::Phrase(_) | Expr::Near { .. } | Expr::Prefix(_) => Err(Error::NotImplemented(
            unsupported(expression).unwrap_or("this operator"),
        )),
    }
}

/// Splits an `AND`/`NOT` spine into branches that must match and branches that
/// must not.
///
/// `a AND b NOT c` parses as `Not(And(a, b), c)`, so the required and excluded
/// branches are interleaved down the left edge of the tree. Pulling them into
/// two flat lists is what lets the planner reorder them, and it is sound
/// because `∧` is associative and `x ∧ ¬y` commutes with any further `∧`: the
/// answer to `(a NOT b) AND c` and to `(a AND c) NOT b` is the same set.
fn flatten<'a>(expression: &'a Expr, required: &mut Vec<&'a Expr>, excluded: &mut Vec<&'a Expr>) {
    match expression {
        Expr::And(left, right) => {
            flatten(left, required, excluded);
            flatten(right, required, excluded);
        }
        Expr::Not(left, right) => {
            flatten(left, required, excluded);
            // Deliberately not flattened: `a NOT (b AND c)` excludes documents
            // holding both, which is not the same as excluding each.
            excluded.push(right);
        }
        other => required.push(other),
    }
}

/// An upper bound on how many documents `expression` can match.
///
/// Cheap by construction — every case is a dictionary lookup or arithmetic, so
/// planning an eight-term query costs a handful of hash lookups rather than a
/// trial evaluation. It only has to be good enough to sort by.
fn estimate(expression: &Expr, index: &Index) -> usize {
    match expression {
        Expr::Term(term) => term_docs(term, index).len(),
        // A phrase cannot occur in more documents than its rarest term does.
        Expr::Phrase(terms) => terms
            .iter()
            .map(|term| term_docs(term, index).len())
            .min()
            .unwrap_or(0),
        // Day 11 will know; until then, assume the worst so a prefix is
        // planned last rather than first.
        Expr::Prefix(_) => index.documents(),
        Expr::Near { left, right, .. } | Expr::And(left, right) => {
            estimate(left, index).min(estimate(right, index))
        }
        Expr::Or(left, right) => estimate(left, index)
            .saturating_add(estimate(right, index))
            .min(index.documents()),
        Expr::Not(left, _) => estimate(left, index),
    }
}

/// The documents holding `term`, or an empty slice if the corpus never saw it.
///
/// A term absent from the dictionary is not an error. `quantum AND flurbles`
/// has a perfectly good answer — no documents — and reporting it as a failure
/// would make every typo look like a broken index.
fn term_docs<'a>(term: &str, index: &'a Index) -> &'a [DocId] {
    index
        .term_id(term)
        .map_or_else(|| &[][..], |id| index.doc_ids(id))
}

/// Names the first operator in `expression` that is not implemented yet.
fn unsupported(expression: &Expr) -> Option<&'static str> {
    let mut found = None;

    expression.walk(&mut |node| {
        let feature = match node {
            Expr::Phrase(_) => Some("the phrase operator"),
            Expr::Near { .. } => Some("the NEAR operator"),
            Expr::Prefix(_) => Some("prefix search"),
            Expr::Term(_) | Expr::And(..) | Expr::Or(..) | Expr::Not(..) => None,
        };

        // `walk` visits parents before children, so the first hit is the
        // outermost offending operator — the one a user is likeliest to
        // recognise in their own query.
        if found.is_none() {
            found = feature;
        }
    });

    found
}

#[cfg(test)]
mod tests {
    use super::{
        Expr, Index, Strategy, difference, difference_with, estimate, evaluate, flatten, gallop,
        intersect, intersect_with, union,
    };
    use crate::corpus::{DocId, Document};
    use crate::index::IndexBuilder;

    fn docs(ids: &[u32]) -> Vec<DocId> {
        ids.iter().copied().map(DocId::new).collect()
    }

    fn raw(ids: &[DocId]) -> Vec<u32> {
        ids.iter().map(|id| id.get()).collect()
    }

    /// A three-document index whose term frequencies are easy to reason about:
    /// `alpha` in all three, `beta` in two, `gamma` in one.
    fn index() -> Index {
        let corpus = [
            ("a", "alpha beta gamma"),
            ("b", "alpha beta"),
            ("c", "alpha"),
        ];
        let documents: Vec<Document> = corpus
            .iter()
            .enumerate()
            .map(|(nth, (external, body))| Document {
                id: DocId::new(u32::try_from(nth).expect("three documents")),
                external_id: (*external).to_owned(),
                title: String::new(),
                body: (*body).to_owned(),
            })
            .collect();

        let mut builder = IndexBuilder::new();
        for document in &documents {
            builder.add_document(document);
        }
        let mut assembler = builder.finish_counting().expect("tiny corpus");
        for document in &documents {
            assembler.add_document(document);
        }
        assembler.finish().expect("same corpus twice")
    }

    fn term(name: &str) -> Expr {
        Expr::Term(name.to_owned())
    }

    fn and(left: Expr, right: Expr) -> Expr {
        Expr::And(Box::new(left), Box::new(right))
    }

    fn not(left: Expr, right: Expr) -> Expr {
        Expr::Not(Box::new(left), Box::new(right))
    }

    fn or(left: Expr, right: Expr) -> Expr {
        Expr::Or(Box::new(left), Box::new(right))
    }

    #[test]
    fn gallop_finds_the_first_id_at_least_as_large() {
        let haystack = docs(&[1, 3, 5, 7, 9]);

        assert_eq!(gallop(&haystack, DocId::new(0)), 0);
        assert_eq!(gallop(&haystack, DocId::new(1)), 0);
        assert_eq!(gallop(&haystack, DocId::new(2)), 1);
        assert_eq!(gallop(&haystack, DocId::new(7)), 3);
        assert_eq!(gallop(&haystack, DocId::new(9)), 4);
        // Nothing large enough: one past the end, like `partition_point`.
        assert_eq!(gallop(&haystack, DocId::new(10)), 5);
    }

    #[test]
    fn gallop_handles_an_empty_haystack() {
        assert_eq!(gallop(&[], DocId::new(7)), 0);
    }

    #[test]
    fn gallop_agrees_with_partition_point_everywhere() {
        // Exhaustive over a small range, which is worth more than a handful of
        // chosen cases: the power-of-two window arithmetic is exactly the kind
        // of code that is correct for 8 elements and off by one for 9.
        for length in 0..40_u32 {
            let haystack = docs(&(0..length).map(|n| n * 2).collect::<Vec<_>>());
            for needle in 0..=(length * 2 + 2) {
                let needle = DocId::new(needle);
                assert_eq!(
                    gallop(&haystack, needle),
                    haystack.partition_point(|&doc| doc < needle),
                    "length {length}, needle {needle}",
                );
            }
        }
    }

    #[test]
    fn intersection_keeps_only_shared_documents() {
        let left = docs(&[3, 17, 22, 40]);
        let right = docs(&[1, 17, 40]);

        assert_eq!(raw(&intersect(&left, &right).collect::<Vec<_>>()), [17, 40]);
    }

    #[test]
    fn every_strategy_gives_the_same_answer() {
        // The strategies differ only in how they skip, so any disagreement is a
        // bug in the skipping — which is exactly the arithmetic worth doubting.
        // A short list against a long one is also the case galloping exists
        // for, so it is the case worth pinning down.
        let sparse = docs(&[0, 511, 512, 4095]);
        let dense = docs(&(0..4096).collect::<Vec<_>>());

        for (left, right) in [(&sparse, &dense), (&dense, &sparse)] {
            let strategies = [Strategy::Adaptive, Strategy::Gallop, Strategy::Linear];

            let intersections: Vec<Vec<_>> = strategies
                .iter()
                .map(|&strategy| intersect_with(left, right, strategy).collect())
                .collect();
            assert_eq!(intersections[0], intersections[1]);
            assert_eq!(intersections[0], intersections[2]);
            assert_eq!(intersections[0], sparse);

            let differences: Vec<Vec<_>> = strategies
                .iter()
                .map(|&strategy| difference_with(left, right, strategy).collect())
                .collect();
            assert_eq!(differences[0], differences[1]);
            assert_eq!(differences[0], differences[2]);
        }
    }

    #[test]
    fn the_adaptive_strategy_gallops_only_on_lopsided_lists() {
        let short = docs(&[1, 2, 3, 4]);
        let same = docs(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let long = docs(&(0..80).collect::<Vec<_>>());

        // 8:1 is not *more* than the 8:1 threshold; 20:1 is.
        assert!(!Strategy::Adaptive.gallops(&short, &same));
        assert!(Strategy::Adaptive.gallops(&short, &long));
        // Symmetric: which side is longer is not the question.
        assert!(Strategy::Adaptive.gallops(&long, &short));
        assert!(Strategy::Gallop.gallops(&long, &long));
        assert!(!Strategy::Linear.gallops(&short, &long));
    }

    #[test]
    fn intersection_with_an_empty_list_is_empty() {
        let left = docs(&[1, 2, 3]);

        assert!(intersect(&left, &[]).next().is_none());
        assert!(intersect(&[], &left).next().is_none());
    }

    #[test]
    fn union_merges_without_duplicating_shared_documents() {
        let left = docs(&[1, 4, 9]);
        let right = docs(&[4, 5]);

        assert_eq!(raw(&union(&left, &right).collect::<Vec<_>>()), [1, 4, 5, 9]);
    }

    #[test]
    fn union_with_an_empty_list_is_the_other_list() {
        let left = docs(&[2, 4]);

        assert_eq!(union(&left, &[]).collect::<Vec<_>>(), left);
        assert_eq!(union(&[], &left).collect::<Vec<_>>(), left);
    }

    #[test]
    fn difference_removes_the_right_hand_documents() {
        let left = docs(&[1, 2, 3, 4, 5]);
        let right = docs(&[2, 4, 99]);

        assert_eq!(
            raw(&difference(&left, &right).collect::<Vec<_>>()),
            [1, 3, 5]
        );
    }

    #[test]
    fn difference_by_nothing_is_everything() {
        let left = docs(&[1, 2, 3]);

        assert_eq!(difference(&left, &[]).collect::<Vec<_>>(), left);
        assert!(difference(&[], &left).next().is_none());
    }

    #[test]
    fn size_hints_bound_the_real_output() {
        let left = docs(&[1, 2, 3, 4]);
        let right = docs(&[2, 4]);

        for (lower, upper, actual) in [
            {
                let hint = intersect(&left, &right).size_hint();
                (hint.0, hint.1, intersect(&left, &right).count())
            },
            {
                let hint = union(&left, &right).size_hint();
                (hint.0, hint.1, union(&left, &right).count())
            },
            {
                let hint = difference(&left, &right).size_hint();
                (hint.0, hint.1, difference(&left, &right).count())
            },
        ] {
            assert!(lower <= actual, "lower bound {lower} exceeded {actual}");
            assert_eq!(upper.map(|upper| upper >= actual), Some(true));
        }
    }

    #[test]
    fn evaluating_a_term_returns_its_documents() {
        let index = index();

        assert_eq!(
            raw(&evaluate(&term("beta"), &index).expect("no unsupported operators")),
            [0, 1]
        );
    }

    #[test]
    fn a_term_the_corpus_never_saw_matches_nothing_rather_than_failing() {
        let index = index();

        assert!(
            evaluate(&term("flurbles"), &index)
                .expect("an unknown term is not an error")
                .is_empty()
        );
        assert!(
            evaluate(&and(term("alpha"), term("flurbles")), &index)
                .expect("an unknown term is not an error")
                .is_empty()
        );
    }

    #[test]
    fn and_intersects_or_unions_and_not_subtracts() {
        let index = index();

        let cases = [
            (and(term("alpha"), term("gamma")), vec![0]),
            (or(term("gamma"), term("beta")), vec![0, 1]),
            (not(term("alpha"), term("beta")), vec![2]),
            (
                not(and(term("alpha"), term("beta")), term("gamma")),
                vec![1],
            ),
        ];

        for (expression, expected) in cases {
            let found = evaluate(&expression, &index).expect("no unsupported operators");
            assert_eq!(raw(&found), expected, "{expression}");
        }
    }

    #[test]
    fn exclusions_commute_with_the_rest_of_the_conjunction() {
        let index = index();

        // `(alpha NOT gamma) AND beta` and `(alpha AND beta) NOT gamma` are the
        // same set, which is what licenses the planner to hoist exclusions to
        // the end of the spine.
        let hoisted = evaluate(
            &and(not(term("alpha"), term("gamma")), term("beta")),
            &index,
        )
        .expect("no unsupported operators");
        let written = evaluate(
            &not(and(term("alpha"), term("beta")), term("gamma")),
            &index,
        )
        .expect("no unsupported operators");

        assert_eq!(hoisted, written);
        assert_eq!(raw(&hoisted), [1]);
    }

    #[test]
    fn the_planner_sorts_the_and_spine_by_estimated_selectivity() {
        let index = index();
        let expression = and(and(term("alpha"), term("beta")), term("gamma"));

        let mut required = Vec::new();
        let mut excluded = Vec::new();
        flatten(&expression, &mut required, &mut excluded);
        required.sort_by_key(|branch| estimate(branch, &index));

        // Written widest-first; planned narrowest-first.
        let planned: Vec<String> = required.iter().map(|branch| branch.to_string()).collect();
        assert_eq!(planned, ["gamma", "beta", "alpha"]);
        assert!(excluded.is_empty());
    }

    #[test]
    fn flatten_separates_exclusions_without_descending_into_them() {
        // `a NOT (b AND c)` excludes documents holding both b and c. Flattening
        // the right operand would wrongly exclude documents holding either.
        let expression = not(term("alpha"), and(term("beta"), term("gamma")));

        let mut required = Vec::new();
        let mut excluded = Vec::new();
        flatten(&expression, &mut required, &mut excluded);

        assert_eq!(required.len(), 1);
        assert_eq!(excluded.len(), 1);
        assert_eq!(excluded[0].to_string(), "(beta AND gamma)");
    }

    #[test]
    fn estimates_bound_the_answer_they_are_estimating() {
        let index = index();

        for expression in [
            term("alpha"),
            term("flurbles"),
            and(term("alpha"), term("gamma")),
            or(term("gamma"), term("beta")),
            not(term("alpha"), term("beta")),
        ] {
            let found = evaluate(&expression, &index).expect("no unsupported operators");
            assert!(
                estimate(&expression, &index) >= found.len(),
                "{expression} estimated below its {} hits",
                found.len(),
            );
        }
    }

    #[test]
    fn later_days_operators_say_so_instead_of_answering_wrongly() {
        let index = index();

        let cases = [
            (Expr::Phrase(vec!["alpha".to_owned()]), "phrase operator"),
            (Expr::Prefix("alph".to_owned()), "prefix search"),
            (
                Expr::Near {
                    left: Box::new(term("alpha")),
                    right: Box::new(term("beta")),
                    distance: 3,
                    ordered: false,
                },
                "NEAR operator",
            ),
        ];

        for (expression, expected) in cases {
            let message = evaluate(&expression, &index)
                .expect_err("not implemented until days 9 to 11")
                .to_string();
            assert!(message.contains(expected), "{message}");
        }
    }

    #[test]
    fn an_unsupported_operator_is_reported_even_when_nothing_else_matches() {
        let index = index();

        // The empty-result short circuit must not swallow the error, or the
        // same query would succeed or fail depending on the corpus.
        let expression = and(term("flurbles"), Expr::Prefix("alph".to_owned()));
        assert!(evaluate(&expression, &index).is_err());
    }
}
