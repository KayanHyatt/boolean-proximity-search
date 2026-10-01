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
//! # Phrases
//!
//! `"exact phrase"` is where the positions the index has been carrying since
//! day 4 finally pay for themselves. Day 9 does it in two stages, and the
//! first stage is day 8's work unchanged: intersect the words' *document*
//! lists, then look at positions only in the documents that survive.
//!
//! ```text
//!   "quantum theory"
//!   quantum  in doc 40 at [ 7, 41, 118 ]
//!   theory   in doc 40 at [ 8, 92 ]
//!                            ▲
//!   7 + 1 = 8, so doc 40 matches
//! ```
//!
//! Inside a document, the check anchors on the word with the fewest
//! occurrences — the planner's rarest-first idea one level down. `the` might
//! occur thirty times in an abstract and `gravity` twice, and anchoring on
//! `gravity` means testing two alignments instead of thirty.
//!
//! # Proximity
//!
//! Day 10 is the design change day 9 saw coming. `("machine learning") NEAR/5
//! medical` asks how far apart two *matches* are, so a `NEAR` operand has to
//! be able to say where it matched — and a phrase's match is not a point but a
//! stretch, so the answer is an *extent*, a first and last position:
//!
//! ```text
//!   ("machine learning") NEAR/5 medical
//!
//!   position  41 42 43 44 45 46 47 48
//!             ├──────┤              │
//!             machine learning      medical
//!                    └── gap = 47 - 42 = 5 ──┘
//! ```
//!
//! `distance` counts the gap between the end of one match and the start of the
//! next, which for two bare terms is the conventional "positions differ by at
//! most k" — and makes `a NEAR/1 b` exactly `"a b" OR "b a"`, a property worth
//! a test. Overlapping matches do not count, so `a NEAR/3 a` needs two
//! occurrences of `a` rather than pairing one with itself.
//!
//! The positional layer sits *alongside* the document one rather than
//! replacing it. [`evaluate`] still answers with documents, because that is
//! what a search result is; the positional layer answers with positions, and only
//! `NEAR` ever asks. So the day-8 and day-9 code paths are untouched, and a
//! query with no proximity in it pays nothing for this.
//!
//! What a `NEAR` operand may be is therefore not a matter of taste.
//! `Term`, `Phrase`, `Prefix`, a nested `Near`, and `Or` of those all have
//! positions. `AND` and `NOT` do not: there is no position at which
//! `quantum AND gravity` occurs, because that is a fact about a document
//! rather than a place in one. [`parse`](crate::parse) rejects
//! `(quantum AND gravity) NEAR/5 loop` at parse time, where the spans still
//! exist and the error can underline the group at fault.
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
use crate::index::{Index, TermId};
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
/// Returns [`Error::NotImplemented`] for the prefix operator, which day 11
/// fills in. The check happens before any work, so a query using one fails the
/// same way whether or not its other clauses match anything.
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
        Expr::Phrase(terms) => Ok(evaluate_phrase(terms, index)),
        Expr::Near {
            left,
            right,
            distance,
            ordered,
        } => evaluate_near(left, right, *distance, *ordered, index),
        Expr::And(..) | Expr::Not(..) => evaluate_conjunction(expression, index),
        Expr::Or(left, right) => {
            let left = evaluate_branch(left, index)?;
            let right = evaluate_branch(right, index)?;
            Ok(union(&left, &right).collect())
        }
        // Unreachable in practice: `evaluate` rejects this up front. Answered
        // rather than panicked because a total function is one fewer thing to
        // be careful about when day 11 rewrites this match.
        Expr::Prefix(_) => Err(Error::NotImplemented(
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

// ---------------------------------------------------------------------------
// Phrases
// ---------------------------------------------------------------------------

/// One term of a phrase, with a cursor into its postings.
///
/// `nth` is the cursor. [`Index::positions`] indexes a term's own postings
/// rather than the corpus, so turning a [`DocId`] into a position list means
/// knowing *which* of this term's postings that document is. The candidate
/// documents arrive in ascending order, so the cursor only ever moves forward
/// and the whole scan costs one pass over each term's postings — rather than a
/// binary search over the full list per candidate, which is what calling
/// `doc_ids(..).binary_search(..)` every time would cost.
#[derive(Debug)]
struct PhraseTerm {
    id: TermId,
    /// Position of this term within the phrase: 0 for the first word.
    offset: u32,
    /// How many of this term's postings the cursor has passed.
    nth: usize,
}

/// The documents where `terms` occur at consecutive positions, in order.
///
/// Two stages, and the first one is day 8's work reused. Intersect the terms'
/// document lists rarest-first to get the documents that hold *all* the words;
/// only then look at positions. Checking positions is far more expensive per
/// document than comparing document ids, so the cheap filter has to run first:
/// `"the quantum theory"` touches three positional lists per surviving
/// document, and there is no point paying that for a document that does not
/// contain `theory` at all.
///
/// The positional check anchors on whichever term has the fewest occurrences
/// *in that document* — the same rarest-first idea as the planner, applied one
/// level down. In `"the quantum theory of gravity"`, `the` may occur thirty
/// times in an abstract and `gravity` twice; anchoring on `gravity` means two
/// candidate alignments to check instead of thirty.
///
/// A phrase cannot straddle the title/body join, because
/// [`FIELD_GAP`](crate::FIELD_GAP) is 100 and no phrase anyone writes is a
/// hundred words long. That is the whole reason the gap exists.
fn evaluate_phrase(terms: &[String], index: &Index) -> Vec<DocId> {
    // `lex_phrase` never produces either of these — an empty phrase is a lex
    // error and a one-word phrase is lexed as a bare term — but `Expr` is
    // public, so the function has to be honest about them anyway.
    match terms {
        [] => return Vec::new(),
        [single] => return term_docs(single, index).to_vec(),
        _ => {}
    }

    // A term the corpus has never seen cannot be part of any phrase in it.
    let mut phrase = Vec::with_capacity(terms.len());
    for (offset, term) in terms.iter().enumerate() {
        let Some(id) = index.term_id(term) else {
            return Vec::new();
        };
        let Ok(offset) = u32::try_from(offset) else {
            return Vec::new();
        };
        phrase.push(PhraseTerm { id, offset, nth: 0 });
    }

    // Stage one: the documents holding every word, rarest list first.
    let mut order: Vec<usize> = (0..phrase.len()).collect();
    order.sort_by_key(|&slot| index.document_frequency(phrase[slot].id));

    let mut candidates = index.doc_ids(phrase[order[0]].id).to_vec();
    let mut already = vec![phrase[order[0]].id];
    for &slot in &order[1..] {
        if candidates.is_empty() {
            return candidates;
        }
        // `"the cat the cat"` names the same term twice. Intersecting a list
        // with itself is a no-op, so skip it rather than walk it.
        let id = phrase[slot].id;
        if already.contains(&id) {
            continue;
        }
        already.push(id);

        let narrowed = intersect(&candidates, index.doc_ids(id)).collect();
        candidates = narrowed;
    }

    // Stage two: of those, the ones where the words are actually adjacent.
    // Allocated once and reused, because this runs per surviving document.
    let mut positions: Vec<&[u32]> = Vec::with_capacity(phrase.len());
    candidates.retain(|&doc| phrase_occurs(&mut phrase, &mut positions, index, doc));

    candidates
}

/// Whether the phrase occurs in `doc`, advancing each term's cursor to it.
///
/// `doc` must be a document that contains every term and must not precede any
/// cursor — both guaranteed by the caller, which walks the intersection of the
/// terms' document lists in ascending order.
fn phrase_occurs<'a>(
    phrase: &mut [PhraseTerm],
    positions: &mut Vec<&'a [u32]>,
    index: &'a Index,
    doc: DocId,
) -> bool {
    positions.clear();
    for term in phrase.iter_mut() {
        // Walk the cursor up to this document. Over the whole candidate list
        // this is one linear pass through the postings, not a search per
        // document.
        let docs = index.doc_ids(term.id);
        term.nth += docs[term.nth..].partition_point(|&seen| seen < doc);
        positions.push(index.positions(term.id, term.nth));
    }

    let offsets: Vec<u32> = phrase.iter().map(|term| term.offset).collect();
    aligned_starts(&offsets, positions).next().is_some()
}

/// Every position at which a phrase begins, given where each of its words
/// occurs in one document.
///
/// `offsets[i]` is word `i`'s place in the phrase and `positions[i]` is where
/// that word occurs in the document, ascending. Returned as an iterator rather
/// than a `Vec` so that one implementation serves both callers: day 9's
/// document filter only needs to know whether there is a first element, and
/// stops there, while day 10's `NEAR` needs all of them.
///
/// Anchors on whichever word has the fewest occurrences *in this document* —
/// the planner's rarest-first idea one level down. A paper about quantum error
/// correction says `correction` eight times and `the` thirty; anchoring on the
/// rarer one means eight alignments to test instead of thirty.
fn aligned_starts<'a>(
    offsets: &'a [u32],
    positions: &'a [&'a [u32]],
) -> impl Iterator<Item = u32> + 'a {
    let anchor = (0..positions.len())
        .min_by_key(|&slot| positions[slot].len())
        .unwrap_or(0);
    let shift = offsets.get(anchor).copied().unwrap_or(0);
    let occurrences: &[u32] = positions.get(anchor).copied().unwrap_or(&[]);

    occurrences.iter().filter_map(move |&occurrence| {
        // If the anchor is the phrase's third word and it sits at position 7,
        // the phrase would have to start at 5. Ascending occurrences give
        // ascending starts, which is what the extent list relies on.
        let start = occurrence.checked_sub(shift)?;

        offsets
            .iter()
            .zip(positions)
            .all(|(offset, list)| list.binary_search(&start.saturating_add(*offset)).is_ok())
            .then_some(start)
    })
}

// ---------------------------------------------------------------------------
// Proximity
// ---------------------------------------------------------------------------

/// The stretch of positions one match occupies, both ends inclusive.
///
/// A term's match is one position wide. A phrase's is as wide as the phrase. A
/// `NEAR` match spans from the start of whichever operand came first to the end
/// of the other — which is what lets `(a NEAR/2 b) NEAR/5 c` mean anything:
/// the inner match has edges, so the outer one can measure from them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Extent {
    /// First position of the match.
    start: u32,
    /// Last position of the match, inclusive.
    end: u32,
}

/// The documents where `left` and `right` match within `distance` positions.
///
/// Day 9's two-stage shape again, and for the same reason: find the documents
/// where both sides match at all, which costs document-id comparisons, and only
/// then read positions, which costs far more.
///
/// # Errors
///
/// Propagates [`Error::NotImplemented`] from an operand.
fn evaluate_near(
    left: &Expr,
    right: &Expr,
    distance: u32,
    ordered: bool,
    index: &Index,
) -> Result<Vec<DocId>> {
    let mut candidates = evaluate_branch(left, index)?;
    if candidates.is_empty() {
        return Ok(candidates);
    }

    let other = evaluate_branch(right, index)?;
    let narrowed: Vec<DocId> = intersect(&candidates, &other).collect();
    candidates = narrowed;

    // Reused across documents rather than allocated per document.
    let mut first = Vec::new();
    let mut second = Vec::new();

    candidates.retain(|&doc| {
        first.clear();
        second.clear();

        extents_in(left, index, doc, &mut first)
            && extents_in(right, index, doc, &mut second)
            && (within(&first, &second, distance)
                || (!ordered && within(&second, &first, distance)))
    });

    Ok(candidates)
}

/// Whether some extent of `first` is followed by one of `second`, no more than
/// `distance` positions later.
///
/// # What `distance` counts
///
/// The gap between the end of one match and the start of the next, so for two
/// bare terms `a NEAR/3 b` means their positions differ by at most 3 — the
/// conventional reading — and `a NEAR/1 b` means adjacent, which makes
/// `a NEAR/1 b` exactly `"a b" OR "b a"`. For a phrase the gap is measured from
/// its nearest edge, since a phrase occupies a stretch rather than a point.
///
/// Overlapping matches do not count: `second` has to *begin after* `first`
/// ends. That is deliberate rather than incidental — without it,
/// `a NEAR/3 a` would match any document containing `a` once, by pairing the
/// occurrence with itself.
fn within(first: &[Extent], second: &[Extent], distance: u32) -> bool {
    first.iter().any(|a| {
        // `second` is sorted by start, so the first extent beginning after `a`
        // ends is the closest one there is. If that one is too far, every later
        // one is further still.
        let next = second.partition_point(|b| b.start <= a.end);
        second
            .get(next)
            .is_some_and(|b| b.start - a.end <= distance)
    })
}

/// Appends, ascending, the extents at which `expression` matches inside `doc`.
///
/// Returns `false` for an expression that has no positions to report, leaving
/// `out` as it found it. [`parse`](crate::parse) rejects those as `NEAR`
/// operands, so in a parsed query this only fires for a prefix — day 11.
fn extents_in(expression: &Expr, index: &Index, doc: DocId, out: &mut Vec<Extent>) -> bool {
    let from = out.len();

    match expression {
        Expr::Term(term) => {
            let Some(id) = index.term_id(term) else {
                return true;
            };
            let Ok(nth) = index.doc_ids(id).binary_search(&doc) else {
                return true;
            };
            out.extend(
                index
                    .positions(id, nth)
                    .iter()
                    .map(|&at| Extent { start: at, end: at }),
            );
            true
        }

        Expr::Phrase(terms) => {
            phrase_extents_in(terms, index, doc, out);
            true
        }

        Expr::Or(left, right) => {
            if !extents_in(left, index, doc, out) || !extents_in(right, index, doc, out) {
                out.truncate(from);
                return false;
            }
            // Two ascending runs, concatenated. Sorting the tail is cheaper to
            // write than a merge and the lists are a handful of entries long.
            out[from..].sort_unstable();
            dedup_from(out, from);
            true
        }

        Expr::Near {
            left,
            right,
            distance,
            ordered,
        } => {
            if !extents_in(left, index, doc, out) {
                out.truncate(from);
                return false;
            }
            let middle = out.len();
            if !extents_in(right, index, doc, out) {
                out.truncate(from);
                return false;
            }

            let mut paired = Vec::new();
            {
                let (head, tail) = out.split_at(middle);
                pair(&head[from..], tail, *distance, &mut paired);
                if !*ordered {
                    pair(tail, &head[from..], *distance, &mut paired);
                }
            }

            out.truncate(from);
            paired.sort_unstable();
            paired.dedup();
            out.append(&mut paired);
            true
        }

        // Day 11 will expand a prefix into its terms and union their positions.
        Expr::Prefix(_) => false,
        // No position exists to report; see `Expr::is_positional`.
        Expr::And(..) | Expr::Not(..) => false,
    }
}

/// Appends the extent of every `first`-then-`second` pair within `distance`.
fn pair(first: &[Extent], second: &[Extent], distance: u32, out: &mut Vec<Extent>) {
    for a in first {
        let next = second.partition_point(|b| b.start <= a.end);
        for b in &second[next..] {
            if b.start - a.end > distance {
                break;
            }
            out.push(Extent {
                start: a.start,
                end: b.end,
            });
        }
    }
}

/// Removes consecutive duplicates from `out[from..]`, leaving the head alone.
fn dedup_from(out: &mut Vec<Extent>, from: usize) {
    let mut write = from;
    for read in from..out.len() {
        if write == from || out[write - 1] != out[read] {
            out[write] = out[read];
            write += 1;
        }
    }
    out.truncate(write);
}

/// Appends, ascending, the extents at which the phrase `terms` occurs in `doc`.
///
/// The cursorless sibling of [`phrase_occurs`]. That one walks a whole
/// candidate list in order and can carry a forward-only cursor per term; this
/// one is handed one document at a time from inside a `NEAR`, so it pays a
/// binary search instead.
fn phrase_extents_in(terms: &[String], index: &Index, doc: DocId, out: &mut Vec<Extent>) {
    if terms.is_empty() {
        return;
    }

    let mut offsets = Vec::with_capacity(terms.len());
    let mut positions: Vec<&[u32]> = Vec::with_capacity(terms.len());

    for (offset, term) in terms.iter().enumerate() {
        let Some(id) = index.term_id(term) else {
            return;
        };
        let Ok(nth) = index.doc_ids(id).binary_search(&doc) else {
            return;
        };
        let Ok(offset) = u32::try_from(offset) else {
            return;
        };
        offsets.push(offset);
        positions.push(index.positions(id, nth));
    }

    let width = u32::try_from(terms.len() - 1).unwrap_or(0);
    out.extend(aligned_starts(&offsets, &positions).map(|start| Extent {
        start,
        end: start.saturating_add(width),
    }));
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
            Expr::Prefix(_) => Some("prefix search"),
            Expr::Term(_)
            | Expr::Phrase(_)
            | Expr::Near { .. }
            | Expr::And(..)
            | Expr::Or(..)
            | Expr::Not(..) => None,
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
        Expr, Index, Strategy, difference, difference_with, estimate, evaluate, evaluate_phrase,
        flatten, gallop, intersect, intersect_with, union,
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

    /// An index over `(title, body)` pairs, for the phrase tests.
    fn index_of(corpus: &[(&str, &str)]) -> Index {
        let documents: Vec<Document> = corpus
            .iter()
            .enumerate()
            .map(|(nth, (title, body))| Document {
                id: DocId::new(u32::try_from(nth).expect("small corpus")),
                external_id: format!("d{nth}"),
                title: (*title).to_owned(),
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

    fn phrase(words: &str) -> Expr {
        Expr::Phrase(words.split(' ').map(str::to_owned).collect())
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
    fn a_phrase_matches_only_consecutive_terms_in_order() {
        let index = index_of(&[
            ("", "the quantum theory of gravity"),
            ("", "quantum gravity without the theory"),
            ("", "a theory of quantum computing"),
            ("", "loop theory quantum corrections"),
        ]);

        let cases = [
            ("quantum theory", vec![0]),
            ("theory of gravity", vec![0]),
            // The near miss the plan asks for: the same two words, and only the
            // document that has them in *this* order comes back. Document 2
            // holds both words two apart and must not match either way.
            ("theory quantum", vec![3]),
            ("the quantum theory of gravity", vec![0]),
            ("quantum gravity", vec![1]),
            ("gravity quantum", vec![]),
            ("quantum theory of gravity", vec![0]),
        ];

        for (words, expected) in cases {
            let found = evaluate(&phrase(words), &index).expect("phrases work from day 9");
            assert_eq!(raw(&found), expected, "{words:?}");
        }
    }

    #[test]
    fn a_phrase_can_repeat_a_word() {
        // Two slots share one `TermId`, each with its own cursor, and the
        // document-list intersection must not walk `the`'s postings twice.
        let index = index_of(&[
            ("", "the more the merrier"),
            ("", "the merrier the more"),
            ("", "more of the merrier"),
        ]);

        assert_eq!(
            raw(&evaluate(&phrase("the more the merrier"), &index).unwrap()),
            [0]
        );
        assert_eq!(
            raw(&evaluate(&phrase("more the merrier"), &index).unwrap()),
            [0]
        );
        assert_eq!(
            raw(&evaluate(&phrase("the the"), &index).unwrap()),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn a_phrase_cannot_straddle_the_title_body_join() {
        // `FIELD_GAP` exists for exactly this: the last word of the title and
        // the first of the body are not adjacent, however they read.
        let index = index_of(&[("prompt diphoton production", "cross sections at Tevatron")]);

        assert_eq!(
            raw(&evaluate(&phrase("diphoton production"), &index).unwrap()),
            [0]
        );
        assert_eq!(
            raw(&evaluate(&phrase("cross sections"), &index).unwrap()),
            [0]
        );
        assert!(
            evaluate(&phrase("production cross"), &index)
                .expect("phrases work")
                .is_empty()
        );
    }

    #[test]
    fn a_phrase_containing_an_unknown_word_matches_nothing() {
        let index = index_of(&[("", "quantum theory of gravity")]);

        assert!(
            evaluate(&phrase("quantum flurbles"), &index)
                .expect("an unknown term is not an error")
                .is_empty()
        );
        assert!(
            evaluate(&phrase("flurbles quantum"), &index)
                .expect("an unknown term is not an error")
                .is_empty()
        );
    }

    #[test]
    fn degenerate_phrases_are_answered_rather_than_panicked() {
        // `lex_phrase` cannot produce either — an empty phrase is a lex error
        // and a one-word phrase is lexed as a bare term — but `Expr` is public.
        let index = index_of(&[("", "quantum theory")]);

        assert!(
            evaluate(&Expr::Phrase(Vec::new()), &index)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            evaluate(&Expr::Phrase(vec!["quantum".to_owned()]), &index).unwrap(),
            evaluate(&term("quantum"), &index).unwrap()
        );
    }

    #[test]
    fn phrases_compose_with_the_boolean_operators() {
        let index = index_of(&[
            ("", "the quantum theory of gravity"),
            ("", "quantum theory without gravity"),
            ("", "a classical theory of gravity"),
        ]);

        let cases = [
            (and(phrase("quantum theory"), term("gravity")), vec![0, 1]),
            (not(phrase("quantum theory"), term("gravity")), vec![]),
            (
                or(phrase("quantum theory"), phrase("classical theory")),
                vec![0, 1, 2],
            ),
            (and(phrase("theory of gravity"), term("quantum")), vec![0]),
        ];

        for (expression, expected) in cases {
            let found = evaluate(&expression, &index).expect("phrases work");
            assert_eq!(raw(&found), expected, "{expression}");
        }
    }

    #[test]
    fn the_planner_treats_a_phrase_as_no_commoner_than_its_rarest_word() {
        let index = index_of(&[
            ("", "the quantum theory"),
            ("", "the classical theory"),
            ("", "the theory"),
        ]);

        // `the` is in all three, `quantum` in one; the phrase cannot beat one.
        assert_eq!(estimate(&phrase("the quantum"), &index), 1);
        // And that bound really does hold for the answer.
        let found = evaluate(&phrase("the quantum"), &index).expect("phrases work");
        assert!(estimate(&phrase("the quantum"), &index) >= found.len());
    }

    #[test]
    fn evaluate_phrase_is_reached_through_the_planner_too() {
        // A phrase nested as an `AND` branch goes through `evaluate_branch`,
        // and one at the top goes straight to `evaluate_phrase`. Same answer.
        let index = index_of(&[("", "quantum theory of gravity")]);
        let words = ["quantum".to_owned(), "theory".to_owned()];

        assert_eq!(
            evaluate(&phrase("quantum theory"), &index).unwrap(),
            evaluate_phrase(&words, &index)
        );
    }

    /// Parses `query` and evaluates it, so the tests read like queries.
    fn hits(query: &str, index: &Index) -> Vec<u32> {
        let expression = crate::query::parse(query).expect("valid query");
        raw(&evaluate(&expression, index).expect("supported"))
    }

    #[test]
    fn near_counts_the_gap_between_the_two_matches() {
        // alpha 0, beta 1, gamma 2, delta 3, epsilon 4.
        let index = index_of(&[("", "alpha beta gamma delta epsilon")]);

        let cases = [
            ("alpha NEAR/1 beta", vec![0]),
            ("alpha NEAR/1 gamma", vec![]),
            ("alpha NEAR/2 gamma", vec![0]),
            ("alpha NEAR/3 epsilon", vec![]),
            ("alpha NEAR/4 epsilon", vec![0]),
            // Two bare terms: the gap is just the difference of their
            // positions, which is the conventional reading of NEAR/k.
            ("beta NEAR/2 delta", vec![0]),
            ("beta NEAR/1 delta", vec![]),
        ];

        for (query, expected) in cases {
            assert_eq!(hits(query, &index), expected, "{query}");
        }
    }

    #[test]
    fn near_is_symmetric_and_onear_is_not() {
        let index = index_of(&[("", "alpha beta gamma delta epsilon")]);

        assert_eq!(hits("epsilon NEAR/4 alpha", &index), [0]);
        assert_eq!(hits("alpha ONEAR/4 epsilon", &index), [0]);
        assert!(hits("epsilon ONEAR/4 alpha", &index).is_empty());
        // ONEAR/9 cannot rescue it: the order is wrong, not the distance.
        assert!(hits("epsilon ONEAR/9 alpha", &index).is_empty());
    }

    #[test]
    fn near_one_is_exactly_a_phrase_or_its_reverse() {
        // The property that makes the chosen definition of `distance` the
        // right one. If `NEAR/1` were off by one in either direction this
        // would fail, and so would any user's intuition.
        let index = index_of(&[
            ("", "alpha beta gamma"),
            ("", "beta alpha gamma"),
            ("", "alpha gamma beta"),
            ("", "alpha only"),
        ]);

        assert_eq!(
            hits("alpha NEAR/1 beta", &index),
            hits("'alpha beta' OR 'beta alpha'", &index)
        );
        assert_eq!(hits("alpha NEAR/1 beta", &index), [0, 1]);
        // And the ordered form is exactly the phrase.
        assert_eq!(
            hits("alpha ONEAR/1 beta", &index),
            hits("'alpha beta'", &index)
        );
    }

    #[test]
    fn a_term_is_not_near_itself_unless_it_occurs_twice() {
        let index = index_of(&[("", "alpha beta alpha"), ("", "alpha beta gamma")]);

        // Document 0 has alpha at 0 and 2; document 1 has it once, and must
        // not match by pairing that occurrence with itself.
        assert_eq!(hits("alpha NEAR/2 alpha", &index), [0]);
        assert!(hits("alpha NEAR/1 alpha", &index).is_empty());
        assert!(hits("gamma NEAR/9 gamma", &index).is_empty());
    }

    #[test]
    fn a_phrase_operand_is_measured_from_its_edge() {
        // machine 0, learning 1, for 2, medical 3, imaging 4.
        let index = index_of(&[("", "machine learning for medical imaging")]);

        // From the phrase's *end*: 3 - 1 = 2. From its start it would be 3,
        // and NEAR/2 would wrongly fail.
        assert_eq!(hits("'machine learning' NEAR/2 medical", &index), [0]);
        assert!(hits("'machine learning' NEAR/1 medical", &index).is_empty());
        // Reversed, the gap is measured to the phrase's start.
        assert_eq!(hits("medical NEAR/2 'machine learning'", &index), [0]);
    }

    #[test]
    fn or_inside_near_takes_whichever_side_matched() {
        let index = index_of(&[("", "alpha beta gamma"), ("", "zeta beta gamma")]);

        assert_eq!(hits("(alpha OR zeta) NEAR/2 gamma", &index), [0, 1]);
        assert_eq!(hits("(alpha OR flurbles) NEAR/2 gamma", &index), [0]);
        assert!(hits("(flurbles OR others) NEAR/9 gamma", &index).is_empty());
    }

    #[test]
    fn nested_near_measures_from_the_inner_match_span() {
        // alpha 0, beta 1, gamma 2, delta 3, epsilon 4.
        let index = index_of(&[("", "alpha beta gamma delta epsilon")]);

        // The inner match spans 0..1, so gamma at 2 is one away.
        assert_eq!(hits("(alpha NEAR/1 beta) NEAR/1 gamma", &index), [0]);
        // delta at 3 is two away from the end of that span.
        assert!(hits("(alpha NEAR/1 beta) NEAR/1 delta", &index).is_empty());
        assert_eq!(hits("(alpha NEAR/1 beta) NEAR/2 delta", &index), [0]);
    }

    #[test]
    fn proximity_does_not_reach_across_the_title_body_join() {
        // Title ends at position 1; the body starts at 1 + 1 + FIELD_GAP.
        let index = index_of(&[("alpha beta", "gamma delta")]);

        assert_eq!(hits("alpha NEAR/1 beta", &index), [0]);
        assert_eq!(hits("gamma NEAR/1 delta", &index), [0]);
        // The real gap is 101, so anything smaller must not reach.
        assert!(hits("beta NEAR/50 gamma", &index).is_empty());
        assert!(hits("beta NEAR/100 gamma", &index).is_empty());
        // And at exactly 101 it does — which pins FIELD_GAP behaviourally
        // rather than by reading the constant.
        assert_eq!(hits("beta NEAR/101 gamma", &index), [0]);
    }

    #[test]
    fn proximity_composes_with_the_boolean_operators() {
        let index = index_of(&[
            ("", "alpha beta gamma"),
            ("", "alpha beta delta"),
            ("", "gamma delta"),
        ]);

        let cases = [
            ("(alpha NEAR/1 beta) AND gamma", vec![0]),
            ("(alpha NEAR/1 beta) NOT gamma", vec![1]),
            ("(alpha NEAR/1 beta) OR (gamma NEAR/1 delta)", vec![0, 1, 2]),
            ("alpha AND (beta NEAR/1 gamma)", vec![0]),
        ];

        for (query, expected) in cases {
            assert_eq!(hits(query, &index), expected, "{query}");
        }
    }

    #[test]
    fn a_distance_wider_than_the_document_is_still_bounded_by_the_document() {
        let index = index_of(&[("", "alpha beta"), ("", "alpha"), ("", "beta")]);

        // NEAR cannot reach into another document however large k is.
        assert_eq!(hits("alpha NEAR/9999 beta", &index), [0]);
    }

    #[test]
    fn later_days_operators_say_so_instead_of_answering_wrongly() {
        let index = index();

        let message = evaluate(&Expr::Prefix("alph".to_owned()), &index)
            .expect_err("not implemented until day 11")
            .to_string();
        assert!(message.contains("prefix search"), "{message}");
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
