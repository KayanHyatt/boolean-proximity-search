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

## Day 4 — the positional inverted index

763 MiB synthetic corpus, 600,000 records, vocabulary shaped with a realistic
long tail (a handful of very common words, a few hundred mid-frequency ones,
and 400,000 rare terms).

| Documents | Terms | Postings | Positions | Index size | Build | Peak RSS |
| --- | --- | --- | --- | --- | --- | --- |
| 100,000 | 398,891 | 6.8M | 15.5M | 124.6 MiB | 4.41 s | 162 MiB |
| 600,000 | 400,051 | 41.0M | 93.0M | 680.9 MiB | 24.34 s | 802 MiB |

### What the layout is worth

The index is 681 MiB for 93M positions and 41M postings — **7.3 bytes per
position**, all in. The shape the plan originally called for, a `Vec<Posting>`
per term with a `Vec<u32>` inside each posting, would have spent 24 bytes of
`Vec` header per posting before storing a single number: **984 MiB of headers
alone**, on top of 372 MiB of positions, in 41 million separate heap
allocations. Compressed sparse row replaces each of those headers with one
4-byte offset, and the whole index is four allocations.

Peak RSS is 802 MiB against a 681 MiB index — the two-pass build allocates each
array exactly once, at exactly the right size, so there is no doubling from
`Vec` growth and nothing to copy. That is the payoff for reading the corpus
twice.

### Build rate, and where it goes

| Index size | Rate |
| --- | --- |
| 8.9 MiB | 4.71M positions/s |
| 68.2 MiB | 3.87M positions/s |
| 235.9 MiB | 3.56M positions/s |
| 680.9 MiB | 3.71M positions/s |

Day 3 tokenized at 25M tokens/s. Indexing runs at roughly 3.7M positions/s —
about seven times slower — and each position is visited twice, once per pass.
The extra work per token is a dictionary lookup and a write into a large array
at an essentially random offset.

Swapping the standard library's default hasher for `rustc-hash`'s FxHash took
the 600k build from 28.40 s to 24.34 s, a **14%** saving for a one-line change.
The default is SipHash, chosen to resist hash-flooding attacks from untrusted
input; an offline index build over a file already on disk is not exposed to
that, so the protection is pure cost here.

The remaining gap is not explained by index size alone: the rate falls only 21%
across a 76-fold growth in the index, and flattens above ~200 MiB. So scattered
writes cost something, but most of the seven-fold gap is simply the per-token
dictionary lookup that tokenization does not have to do. Day 13 is where that
gets attacked properly.

### What this means for the real corpus

436M positions at 7.3 bytes each is roughly **3.2 GB**, and at 3.7M
positions/s roughly **two minutes** to build. So the full arXiv dump wants a
machine with memory to spare; `--limit 500000` builds a representative index in
about 25 seconds and 700 MiB. Making the full corpus comfortable is exactly
what day 13's delta encoding and varint compression are for.

### Day 4 — the real corpus, on real hardware

500,000 arXiv records on the development machine, against the 600,000-record
synthetic corpus measured above:

| | Synthetic (600k) | **Real arXiv (500k)** |
| --- | --- | --- |
| Terms | 400,051 | **291,917** |
| Postings | 41.0M | **41.9M** |
| Positions | 93.0M | **72.6M** |
| Index size | 680.9 MiB | **605.8 MiB** |
| Build time | 24.34 s | **14.81 s** |
| Rate | 3.71M positions/s | **4.90M positions/s** |

Real text indexes **32% faster per position** than the synthetic corpus, and
the reason is the vocabulary. The generator invented 400,000 rare terms drawn
uniformly, so almost every lookup missed cache. Real English has a bounded
vocabulary with a steep frequency curve — 292,000 terms for 500,000 abstracts,
and the common ones dominate — so the hot part of the dictionary stays
resident. The synthetic corpus was harder than reality, which is the right
direction for a benchmark to be wrong in.

The other difference runs the other way: real documents repeat themselves less.
41.9M postings over 72.6M positions is 1.73 occurrences per (term, document)
pair, against the synthetic corpus's 2.27. More postings per position means
more per-posting bookkeeping, so the real index costs **8.75 bytes per
position** rather than 7.3.

### Projection for the full corpus

436M positions at 8.75 bytes is roughly **3.8 GB**, and at 33.8k documents/s
roughly **80 seconds**. One million abstracts — the figure the project set out
to beat sixty seconds on — lands at about **30 seconds**.

### A hand-checkable sanity check

The run reported that `quantum` first occurs in document #0 at position 118.
Document #0 is the diphoton paper, whose title is twelve tokens long, so the
body begins at 12 + `FIELD_GAP` = 112. Its abstract opens *"A fully
differential calculation in perturbative quantum..."* — and `quantum` is the
seventh body token, at 112 + 6 = **118**. The index can be checked by hand, on
paper, against the corpus.
