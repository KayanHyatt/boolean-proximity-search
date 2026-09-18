# Boolean + Proximity Search Engine

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

## Planned query language

| Syntax | Meaning |
| --- | --- |
| `a AND b` | both terms present (implicit between adjacent terms) |
| `a OR b` | either term present |
| `a NOT b` | `a` present, `b` absent |
| `(a OR b) AND c` | grouping |
| `"exact phrase"` | terms adjacent, in order |
| `a NEAR/3 b` | within 3 tokens, either order |
| `a ONEAR/3 b` | within 3 tokens, `a` first |
| `comp*` | prefix wildcard |

Precedence, tightest first: `NOT` → `NEAR` → `AND` → `OR`.

## Build

```bash
cargo build --release
cargo run --release -- index --input arxiv.jsonl --limit 100000
cargo run --release -- search 'quantum AND "error correction"'
cargo run --release -- bench
```

## License

MIT
