# Boolean + Proximity Search Engine

[![CI](https://github.com/KayanHyatt/boolean-proximity-search/actions/workflows/ci.yml/badge.svg)](https://github.com/KayanHyatt/boolean-proximity-search/actions/workflows/ci.yml)

A search engine in Rust over a large text corpus, built around a **positional
inverted index** and a **real query language** — not a `grep` wrapper.

```
quantum AND ("error correction" NEAR/5 surface) NOT class*
```

Boolean operators with precedence and parentheses, exact phrases, bounded
proximity, and prefix wildcards — parsed by a hand-written recursive-descent
parser into an AST, then evaluated by intersecting postings lists.

Benchmarked against a naive linear scan, reporting index build time, index size,
and query latency percentiles.

## Status

🚧 In progress. See [`docs/PLAN.md`](docs/PLAN.md) — a 14-day build plan, one
commit-sized step per day.

## Why Rust

This is a workload where the language earns its place. The whole project is
measured in index build time, index size on disk, and p99 query latency —
numbers that a GC pause or a boxed-everything data model would quietly ruin.
Zero-copy tokenization over borrowed `&str`, postings lists laid out as flat
`Vec`s, and `rayon` for parallel index construction are all load-bearing, not
decoration.

## Corpus

arXiv metadata dump — ~2.7M paper abstracts, one JSON object per line. Every
stage takes a `--limit` so development runs on 10k documents and benchmarks run
on the full set.

## Query language

| Syntax | Meaning |
| --- | --- |
| `a AND b` | both terms present (implicit between adjacent terms) |
| `a OR b` | either term present |
| `a NOT b` | `a` present, `b` absent |
| `(a OR b) AND c` | grouping |
| `"exact phrase"` | terms adjacent, in order |
| `'exact phrase'` | identical — single quotes work too, see below |
| `a NEAR/3 b` | within 3 positions, either order |
| `a ONEAR/3 b` | within 3 positions, `a` first |
| `comp*` | prefix wildcard |

Everything except prefix wildcards **works today**; `comp*` parses correctly and
reports `not implemented yet` — day 11 of the plan fills it in.

`NEAR` measures the gap between two *matches*, so a phrase operand is measured
from its nearest edge and `a NEAR/1 b` is exactly `"a b" OR "b a"`. Both
operands must be able to say where they matched, which `AND` and `NOT` cannot —
there is no position at which `quantum AND gravity` occurs. `parse` rejects
`(quantum AND gravity) NEAR/5 loop` and underlines the group at fault.

Operators are **uppercase only**, so `cats and dogs` searches for the word
"and" rather than silently becoming an operator.

Both quote characters delimit a phrase, which matters more than it sounds:
on Windows, `search "a AND \"b c\""` is mangled by the shell before the
program sees it. Single quotes mean `search "a AND 'b c'"` works in every
shell with no escaping. An apostrophe inside a word — `don't` — is part of
the word; only a quote that *begins* a lexeme opens a phrase.

A word the tokenizer splits becomes the phrase it has to become:
`state-of-the-art` is four adjacent terms, because that is how the index
stored it.

Precedence, tightest first: `NEAR` → `AND`/`NOT` → `OR`. `AND` and `NOT`
share a level and associate left, because `NOT` here is the binary difference
`a NOT b` rather than a unary negation — so `a NOT b NOT c` reads left to
right, and `a OR b AND c` means `a OR (b AND c)` the way `+` and `*` do.

## Build

Requires Rust 1.85 or newer (edition 2024).

```bash
cargo build --release
cargo run --release -- index --input arxiv.jsonl --limit 100000
cargo run --release -- search 'quantum AND entanglement NOT classical'
cargo run --release -- bench
```

`search` prints the parsed tree, each term's document frequency in the order
the planner intersects them, and the hits:

```
  ((quantum AND entanglement) NOT classical)

  terms, rarest first — the order the planner intersects in:
    entanglement                  543,439
    classical                     591,505
    quantum                       681,275

  151,914 document(s), 15.1914% of the corpus, in 17 ms
```

Phrases and proximity run in two stages: intersect the words' document lists,
then read positions only in the documents that survive. Inside a document the
phrase check anchors on whichever word occurs fewest times there.

Intersecting the rarest list first is worth 935× on a three-clause query,
skipping ahead by exponential search instead of stepping is worth up to 800×
when one list is far shorter than the other, anchoring a phrase on its rarest
word is worth 1.4× on `'the quantum'`, and widening a `NEAR` window costs
nothing at all. All measured in [`docs/RESULTS.md`](docs/RESULTS.md).

## License

MIT
