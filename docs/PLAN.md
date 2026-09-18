# 14-Day Build Plan

A Boolean + proximity search engine in Rust: positional inverted index, a real
query language with a recursive-descent parser, and a benchmark against naive
linear scan.

**Corpus:** arXiv metadata dump (~2.7M abstracts, one JSON object per line).
Scales from a 10k slice for tests to the full set for benchmarks.

**How to use this plan:** one step per day. Each day ends with a green
`cargo test`, a commit, and a push. Nothing carries over half-finished.

**Success criteria for the finished project:**

- Index 1M+ abstracts in under 60s on a laptop
- Median query latency under 1ms; p99 under 10ms
- 50-500x faster than linear scan on multi-term Boolean queries
- Index size under 40% of raw corpus size
- Query language: `AND OR NOT ( ) "exact phrase" a NEAR/3 b prefix*`

---

## Week 1 — Index and parser

### Day 1 — Skeleton and CI

Cargo binary + library split (`src/lib.rs` + `src/main.rs`), module stubs
(`tokenize`, `index`, `query`, `search`, `corpus`), `thiserror` error type,
`clap` CLI with `index` / `search` / `bench` subcommands that only print for
now. `rustfmt.toml`, `clippy.toml`, and a GitHub Actions workflow running
fmt + clippy (`-D warnings`) + test on push.

**Rust you learn:** module system and visibility, the lib/bin split, why
`Result<T, E>` with a custom error enum beats `Box<dyn Error>` in a library.

**Done when:** `cargo clippy -- -D warnings` and `cargo test` both pass; CI is
green on GitHub.

### Day 2 — Corpus ingestion

Download a slice of the arXiv dump. `Corpus` trait over a streaming JSONL
reader (`BufReader` + `serde_json`), yielding `Document { id, title, abstract }`.
Dense internal `DocId(u32)` assigned on ingest, with a side table mapping back
to the arXiv id. A `--limit N` flag so every later step can work on 10k docs
while developing and 1M when benchmarking. Tiny 20-doc fixture committed to
`tests/data/` so tests never need the real dump.

**Rust you learn:** `Iterator` implementation by hand, `serde` derive with
renamed/borrowed fields, why streaming beats `read_to_string` on a 4GB file.

**Done when:** `cargo run -- index --limit 1000` prints a document count and a
timing, and the fixture corpus loads in a test.

### Day 3 — Tokenizer

Unicode-aware tokenizer producing `(&str, position)` pairs borrowed from the
input — no allocation per token. Lowercasing, punctuation splitting, a decision
on stop words (keep them; positional queries need them) and on stemming (skip
it; document why). Position counter is per-document and monotonic.

**Rust you learn:** lifetimes that actually matter — `fn tokenize<'a>(&'a str)
-> impl Iterator<Item = Token<'a>> + 'a`. This is the day the borrow checker
teaches you something.

**Done when:** property test asserts every token is a substring of the input at
its recorded byte offset, and a table-driven test covers hyphens, apostrophes,
accented characters and CJK.

### Day 4 — Positional inverted index (in memory)

`IndexBuilder` that consumes the corpus and produces:
term dictionary (`HashMap<Box<str>, TermId>`), postings as
`Vec<Posting { doc_id: DocId, positions: Vec<u32> }>` sorted by `doc_id`, plus
per-term document frequency. `stats()` reporting term count, posting count,
total positions, and estimated bytes.

**Rust you learn:** ownership when building large structures, `entry()` API,
why `Box<str>` beats `String` for a dictionary you never mutate.

**Done when:** a test builds the fixture index and asserts exact postings for a
handful of terms; `cargo run -- index --limit 100000` prints real stats.

### Day 5 — On-disk index format

Serialize the index to a single file and load it back. Hand-rolled binary
layout (header, sorted term dictionary block, postings block, offset table)
rather than `bincode`, so the format is yours and Day 13's compression has
somewhere to go. `index` writes `index.bin`; `search` loads it.

**Rust you learn:** byte-level I/O, `u32`/`u64` endianness discipline,
round-trip testing.

**Done when:** build → save → load → query returns identical results to the
in-memory index, and you can report index size on disk vs corpus size.

### Day 6 — Query lexer

Lex the query language into a `Token` enum with byte spans: identifiers,
quoted strings, `AND`/`OR`/`NOT`, `(`/`)`, `NEAR/k`, trailing `*`. Errors carry
a span so they can be pointed at.

**Rust you learn:** enums as data, `Peekable<Chars>`, designing errors that
carry location.

**Done when:** tests cover every token kind plus three malformed inputs whose
error messages name the right byte offset.

### Day 7 — Recursive-descent parser

`Expr` AST enum: `Term`, `Prefix`, `Phrase(Vec<String>)`, `Near { left, right,
k }`, `And`, `Or`, `Not`. Precedence: `NOT` > `NEAR` > `AND` > `OR`, with
parentheses and implicit AND between adjacent terms. One function per
precedence level.

**Rust you learn:** recursive enums with `Box`, exhaustive `match`, how an AST
makes the evaluator on Day 8 almost write itself.

**Done when:** 20+ parser tests including precedence cases
(`a OR b AND c` parses as `a OR (b AND c)`) and a round-trip `Display` impl
that re-prints the AST unambiguously.

---

## Week 2 — Evaluation, speed, and the write-up

### Day 8 — Boolean evaluation

Walk the AST over postings lists. Sorted-merge intersection for `AND`, merge
for `OR`, and `NOT` only as the right operand of `AND` (document why a bare
`NOT` over 2.7M docs is a trap). Galloping/exponential search when one list is
far shorter than the other, and evaluate `AND` children smallest-list-first.

**Rust you learn:** iterator combinators over slices, writing a custom iterator
adapter, and measuring that galloping actually helps.

**Done when:** `cargo run -- search 'quantum AND entanglement NOT classical'`
returns correct hits, verified against a brute-force checker in tests.

### Day 9 — Phrase queries

`"exact phrase"` via positional intersection: for each shared document,
advance position lists in lockstep looking for consecutive offsets. Generalize
to n terms by folding pairwise.

**Rust you learn:** two-pointer algorithms over `&[u32]`, and the difference
between a clean recursive fold and a fast one.

**Done when:** phrases of 2, 3 and 5 terms return correct documents, and a test
asserts a near-miss (right words, wrong order) is excluded.

### Day 10 — NEAR/k proximity

Unordered `a NEAR/k b` (within k tokens, either order) and ordered
`a ONEAR/k b`. Must compose: `("machine learning") NEAR/5 medical` has to work,
which means `NEAR` operands need position lists, not just doc lists — so the
evaluator returns positions, not booleans, and drops them only at the top.

**Rust you learn:** refactoring an evaluator's return type after the fact, and
why the first design was wrong. This is the most interesting day.

**Done when:** nested phrase-inside-NEAR queries work and the k boundary is
tested at k-1, k, k+1.

### Day 11 — Prefix wildcards

Store the term dictionary sorted so `comp*` is a binary search for the range
`[comp, comq)`. Expand to a bounded union of terms (cap at, say, 1024 matches
with a clear error past that), then OR their postings.

**Rust you learn:** `slice::partition_point`, designing a limit that fails
loudly instead of hanging.

**Done when:** `comp*` matches computation/computer/compiler, `z*` returns few
or none without a full scan, and the term cap is tested.

### Day 12 — Benchmark harness

Naive baseline: linear scan over raw documents doing substring/regex matching
per query. A workload file of ~50 representative queries across all operators.
`criterion` benches plus a `bench` subcommand reporting p50/p90/p99/p999
latency, index build time, index size, and the speedup factor over naive.
Output as a markdown table you can paste straight into the README.

**Done when:** you have a real results table with real numbers on at least
500k documents.

### Day 13 — Performance pass

Two changes, each measured before and after: delta-encode + varint-compress
postings and positions (target: index under 40% of raw corpus), and build the
index in parallel with `rayon` (shard by document range, merge dictionaries).
Keep the Day 12 numbers as the baseline column.

**Rust you learn:** `rayon`'s parallel iterators, why merging shards is the
hard part, and that compression can make queries *faster* by shrinking cache
misses.

**Done when:** the results table has before/after columns and you can explain
every row.

### Day 14 — Report and polish

README with: what it is, the query language grammar, architecture diagram,
benchmark results, and a design-decisions section (why positional postings, why
no stemming, why `NOT` is restricted, what you'd do next). Doc comments on the
public API, `cargo doc` clean. Tag `v1.0.0`.

**Done when:** someone who has never seen the repo can read the README, clone
it, run one command, and search a corpus.

---

## Stretch goals (only after Day 14)

- Memory-mapped index so startup is O(1) instead of O(index size)
- BM25 ranking on top of the Boolean filter
- Skip lists inside long postings lists
- A small TUI with live query-as-you-type
