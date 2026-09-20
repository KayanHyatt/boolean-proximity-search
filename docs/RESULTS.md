# Measurements

Numbers are re-measured as the engine grows, so a row is only comparable
within its own section. Machine: Linux container, 4.4 GiB synthetic corpus of
2.7M arXiv-shaped records unless stated otherwise.

## Day 2 — corpus ingestion

Streaming read, JSON parse, and document-metadata collection. No index yet.

| Documents | Elapsed | Throughput | Peak RSS |
| --- | --- | --- | --- |
| 100,000 | 186 ms | 538k docs/s · 896 MiB/s | 18 MiB |
| 500,000 | 946 ms | 529k docs/s · 881 MiB/s | 86 MiB |
| 2,700,000 | 4.85 s | 557k docs/s · 927 MiB/s | 462 MiB |

Two things worth reading off this table.

**Throughput is flat.** ~900 MiB/s whether the run touches 170 MiB of the file
or all 4.4 GiB of it. That is what "streaming" has to mean: cost per document
does not depend on how many documents came before.

**Peak memory tracks documents kept, not bytes read.** 462 MiB while reading a
4.4 GiB file — the file is never resident. The memory that *is* used is the
`DocStore`: 2.7M titles and arXiv ids, deliberately retained so a search hit
can be turned back into something a human recognises. It scales linearly with
document count (18 → 86 → 462 MiB for 100k → 500k → 2.7M), exactly as a `Vec`
of metadata should.

### Day 2 addendum — the cost of unwrapping text

The first run against the *real* dump exposed something the synthetic corpus
never could: arXiv stores text as it was typeset, hard-wrapped at ~80 columns,
so titles arrived with newlines in the middle of them. Fixing that by
normalizing whitespace on both the title and the body cost more than the fix
was worth.

Measured on a 429 MiB corpus of 300k hard-wrapped records:

| Variant | Throughput | vs baseline |
| --- | --- | --- |
| `trim()` only — the original, buggy | 460k docs/s · 655 MiB/s | baseline |
| Normalize title + body, word by word | 195k docs/s · 279 MiB/s | **0.42×** |
| Normalize title + body, ASCII byte scan | 214k docs/s · 306 MiB/s | 0.47× |
| Normalize title + body, span copy | 232k docs/s · 332 MiB/s | 0.50× |
| **Normalize title only, span copy** | **433k docs/s · 618 MiB/s** | **0.94×** |

The first three rows are the wrong question. Making the scan cleverer —
switching from `split_whitespace` to raw bytes, then copying in spans instead
of word by word — bought 19%, because the cost was never the *scanning
strategy*. It was that normalizing requires looking at every byte at all, and
`trim().to_owned()` looks at almost none: it checks the ends and memcpys the
middle at tens of gigabytes a second.

So the fix was to stop doing the work. The body is only ever fed to the
tokenizer, which treats every kind of whitespace alike — a newline inside an
abstract changes nothing. Only the title is displayed and compared, and it is
about 3% of the corpus. Normalizing that alone fixes the bug for 6% of the
throughput rather than 58%.

The general shape of this, worth remembering: when an optimization gets you
19% and you wanted 100%, the problem is usually the work itself, not how you
are doing it.

## Day 3 — tokenization

687 MiB synthetic corpus, 500,000 hard-wrapped records, **69,093,279 tokens**.
Tokenization is measured as the difference between an ingest-only run and one
that also tokenizes every title and abstract.

| | Total elapsed | Tokenizing alone | Rate |
| --- | --- | --- | --- |
| Ingest only (day 2) | 1.20 s | — | — |
| \+ tokenize, character by character | 4.06 s | 2.86 s | 24.2M tokens/s |
| **\+ tokenize, ASCII fast path** | **3.20 s** | **2.00 s** | **34.5M tokens/s** |

The fast path is the same rule written twice. Scanning a word with
`char_indices()` decodes UTF-8 for every character; in ASCII text, a byte *is*
a character, so that decoding is pure overhead. The byte loop handles ASCII and
hands the whole word to the character-aware version the moment a non-ASCII byte
appears — wasteful for that word, and irrelevant, because almost no word takes
that path.

Note the contrast with day 2, where two rounds of exactly this kind of cleverness
bought 19% and the real answer was to delete the work. Here the work is not
optional — the tokens are the product — so making the scan cheaper is the only
lever, and it pays.

### Allocations

Zero per token. A `Token<'a>` is a `&'a str` into the document's own text plus
two integers, and `Token::normalized()` hands back that same slice unless
lowercasing would actually change it. At 69M tokens, the naive
`String`-per-token design would allocate 69 million times to produce bytes that
already exist in memory.
