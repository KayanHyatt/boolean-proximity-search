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

## Day 5 — the index on disk

509 MiB synthetic corpus, 400,000 records, 62.0M positions.

| | Value |
| --- | --- |
| Build from corpus | 18.47 s |
| **Write to disk** | **1.14 s** (430 MiB/s) |
| File size | 490.4 MiB |
| **Load from disk** | **~0.43 s** |
| Single-term lookup | ~1.1 µs |

**Loading is 43x cheaper than rebuilding** — 0.43 s against 18.47 s. That is
the entire point of the day: an index built once is now a file that a query can
pick up in under half a second, instead of eighteen seconds of re-reading and
re-tokenizing the corpus for every search.

Writing runs at 430 MiB/s, which is close to sequential disk speed, because the
in-memory layout is already four flat `u32` arrays. Serialization is a
little-endian copy, not a traversal — there is no tree to walk and no per-node
bookkeeping to emit.

### The file has no slack in it

`the_file_is_exactly_the_size_its_sections_imply` asserts the file's length
equals 48 bytes of header plus the exact size of every section, to the byte. It
is a sharper test than "smaller than memory", which would have compared
different things anyway: the file carries the document store and
`IndexStats::bytes` does not. If the format ever grows padding nobody decided
to add, that test fails.

### The dictionary is stored sorted, for two reasons

Terms are interned in first-seen order, so `TermId` ordering is arbitrary. On
disk they go in lexicographic order with a parallel rank-to-`TermId` array.

- **Day 11 needs it.** `comp*` is a binary search over a sorted term list. A
  hash map cannot answer that question at all.
- **Determinism.** `HashMap` iteration order is randomized per run, so a
  serializer that followed it would write different bytes each time from
  identical input. Sorting makes the same corpus produce byte-identical files —
  and `saving_is_deterministic` builds two indexes independently and asserts
  their bytes match.

## Day 6 — the query lexer

No throughput numbers worth reporting: lexing a query is microseconds against
a 358 ms index load, and the query is a line of text rather than a corpus. What
day 6 produces is a token stream with byte spans, and errors that can point.

```
$ boolsearch search 'quantum NEAR surface'
error: invalid query at byte 8: NEAR needs a distance, as in NEAR/3

  quantum NEAR surface
          ^^^^
```

Every lexeme records the byte range it came from, which is what makes the
caret possible. The caret counts *characters*, not bytes, so it still lines up
under `naïve AND x` — a byte-counted caret would sit one column too far right.

Lexing happens **before** the index is opened. A syntax error should not cost a
third of a second of disk read to discover, and
`a_malformed_query_is_refused_before_the_index_is_even_opened` points at a
nonexistent index file to prove the ordering.

## Day 7 — the parser

No throughput to report: parsing a query is a walk over a handful of lexemes.
What day 7 produces is a tree, and evidence that the tree is the right one.

```
$ boolsearch search "quantum AND ('error correction' NEAR/5 surface) NOT class*"

  parsed, 7 node(s):

    ((quantum AND ("error correction" NEAR/5 surface)) NOT class*)
```

### Precedence

Loosest to tightest: `OR`, then `AND`/`NOT`, then `NEAR`, then terms and
groups. So `a OR b AND c` parses as `(a OR (b AND c))`, the same way `+` is
looser than `*`, and `a AND b NEAR/3 c` as `(a AND (b NEAR/3 c))` — proximity
binds its operands before anything else can take them.

The build plan listed `NOT` as the tightest operator. That is right for a
*unary* `NOT`; this language has the binary difference `a NOT b`, so it belongs
beside `AND`, where `a NOT b NOT c` reads left to right as it should. A unary
`NOT classical` would mean "every document except those" — two and a half
million results nobody asked for.

### The Display impl is a test, not a convenience

`Expr` prints fully parenthesized rather than prettily, which makes
`parsing_is_idempotent` possible: print a tree, re-parse the printed form, and
assert the two trees are equal. Eight queries go through that round trip. A
precedence bug that happened to print plausibly would still fail it.

### Errors still point

```
$ boolsearch search 'quantum AND'
error: invalid query at byte 11: AND stops here, but something has to come after it

  quantum AND
             ^
```

Something *missing* gets a zero-width span at the end of the query; something
*wrong* gets the span of the offending lexeme. Both survive multi-byte text,
because the caret counts characters.

## Day 8 — Boolean evaluation

Machine for this section: 2-core Linux container, 1,000,000 synthetic records
with Zipfian term frequencies (200,000 terms, 126M postings, 160M positions,
1.5 GiB index). Zipf is the point — a uniform vocabulary would never produce
the rare-term-meets-common-term pair that the whole day is about.

### Query latency

Cold, one run per query, timing `evaluate` only. `%` is of the corpus.

| Query | Hits | % | Latency |
| --- | --- | --- | --- |
| `kephopyr23` (a rare term) | 60 | 0.006% | 3 µs |
| `quantum` | 681,275 | 68% | 2.0 ms |
| `quantum AND kephopyr23` | 40 | 0.004% | 85 µs |
| `the AND quantum AND kephopyr23` | 40 | 0.004% | 0.4–1.7 ms |
| `quantum AND entanglement` | 369,310 | 37% | 12 ms |
| `quantum AND entanglement NOT classical` | 151,914 | 15% | 17 ms |
| `quantum OR entanglement OR classical` | 941,306 | 94% | 15 ms |
| `(quantum OR classical) AND decay NOT the` | 1 | 0.0001% | 22 ms |
| `quantum AND flurbles AND entanglement` | 0 | 0% | 3 µs |

Four things to read off it.

**Cost tracks the answer, not the corpus.** A rare term costs 3 µs and a term
in two thirds of the corpus costs 2 ms — a factor of 600 on a corpus of fixed
size. The 2 ms is not searching; it is `memcpy` of 681,275 document ids.

**A term nobody indexed costs nothing.** `quantum AND flurbles AND
entanglement` is 3 µs because the planner puts the empty list first and the
loop stops. The same query written widest-first would walk two lists of
hundreds of thousands of documents to reach the same answer.

**The expensive queries are the ones with big answers.** 12–22 ms for queries
matching 15–94% of a million documents. There is no skipping to be had when
both operands are everywhere; the work is proportional to the output.

**The last row is honest about what `NOT` does not buy you.**
`(quantum OR classical) AND decay NOT the` returns one document and still
costs 22 ms, because the `OR` has to be materialised in full before anything
can narrow it. Day 13 revisits this.

### Galloping search: measured, then made conditional

The plan said to use galloping search "when one list is far shorter than the
other". The measurement is what defines *far*. Intersecting one term's postings
against `the` (999,997 documents), best of 25–200 runs, cache-warm:

| Shorter list | Ratio | Adaptive | Always gallop | Always step |
| --- | --- | --- | --- | --- |
| 35 | 28571:1 | **2.9 µs** | 2.9 µs | 2,371 µs |
| 60 | 16667:1 | **5.1 µs** | 5.1 µs | 2,326 µs |
| 200 | 5000:1 | **14 µs** | 14 µs | 2,419 µs |
| 700 | 1429:1 | **40 µs** | 40 µs | 2,415 µs |
| 2,500 | 400:1 | **208 µs** | 209 µs | 2,445 µs |
| 9,008 | 111:1 | **390 µs** | 389 µs | 2,544 µs |
| 30,020 | 33:1 | **856 µs** | 859 µs | 2,819 µs |
| 100,955 | 10:1 | **2.35 ms** | 2.36 ms | 3.94 ms |
| 300,524 | 3:1 | 6.56 ms | **5.69 ms** | 6.65 ms |
| 681,275 | 1:1 | **7.83 ms** | 8.32 ms | 8.25 ms |

**Galloping is 800× faster at one end and slower at the other.** At 28571:1 it
is 2.9 µs against 2.4 ms. At 1:1 an earlier run of the same benchmark had it
27% *slower* than stepping, and this run 1% slower. The window arithmetic costs
more than `&slice[1..]`, and the unpredictable branch costs more again.

That is why `intersect` does not simply gallop. `Strategy::Adaptive` compares
the two lengths once and gallops only past `GALLOP_RATIO` (8:1). The 3:1 row is
the price of that choice — 15% off the best available — and it is deliberate:
across runs the crossover moved between 2:1 and 10:1, so the threshold sits on
the far side of the only ratio measured as a clear loss. Chasing the true
crossover would be fitting a constant to noise.

**Cache-cold costs ten to twenty times more than cache-warm.** The CLI reports
85 µs for `quantum AND kephopyr23`; the table's equivalent row is 5 µs. Both
are correct. 60 galloped probes into a 2.7 MB postings array is 60 × ~14 cache
misses on the first pass and nearly free on the two-hundredth. The CLI number
is the one a user gets.

### The planner earns more than the search algorithm does

Three clauses, `the` (999,997 docs) AND `quantum` (681,275) AND `kephopyr23`
(60), folded left to right:

| Order | Time |
| --- | --- |
| As written — widest first | 8.05 ms |
| As planned — rarest first | 8.6 µs |
| | **935× faster** |

Same operator, same data, same galloping. The only difference is which
intersection happens first. Written order builds a 369,000-element intermediate
and then throws away all but 40 of it; planned order is down to 60 candidates
before it touches the long lists at all.

This is the day's real lesson. Galloping is the clever part and it bought 800×
in its best case; deciding *what order to do the work in* bought 935× on an
ordinary three-word query. The planner is 40 lines: flatten the `AND` spine,
look up each term's document frequency, `sort_by_key`.

### `NOT` is binary, and this is the arithmetic

A unary `NOT classical` over this corpus would return 408,495 documents — and
over the real 2.7M arXiv dump, roughly a million. `a NOT b` is bounded by `a`:
`difference` walks the left list once and skips through the right, so
`quantum AND entanglement NOT classical` costs 5 ms more than
`quantum AND entanglement`, not 400,000 documents' worth.

Exclusions are also hoisted to the end of the conjunction, which is sound
because `x ∧ ¬y` commutes with any further `∧`: `(a NOT b) AND c` and
`(a AND c) NOT b` are the same set. Doing them last means subtracting from the
smallest list the query will ever produce.

### Correctness

209 tests. The load-bearing one is a property test: random `AND`/`OR`/`NOT`
trees up to four levels deep over the fixture's real vocabulary, evaluated by
the engine and by `BTreeMap<String, BTreeSet<DocId>>` with standard-library set
algebra, asserted equal. The reference shares no code with the engine — no
galloping, no compressed-sparse-row arrays, no planner — so agreement is
evidence rather than coincidence.

`gallop` is also checked exhaustively against `partition_point` for every
haystack length from 0 to 39 and every needle in range: 33,000 assertions
against the standard library, because power-of-two window arithmetic is exactly
the kind of code that is right for eight elements and off by one for nine.

### Day 8 addendum — real corpus, real hardware

Kayan's laptop, 500,000 arXiv records (291,917 terms, 41.9M postings, 72.6M
positions, 605.8 MiB index; load 386–763 ms).

| Query | Hits | % | Latency |
| --- | --- | --- | --- |
| `quantum AND entanglement NOT classical` | 3,874 | 0.77% | **220 µs** |
| `quantum AND flurbles AND entanglement` | 0 | 0% | **8 µs** |

The same query took **17 ms** on the container's synthetic corpus — 77 times
slower on half as many documents again. The synthetic corpus was not wrong; it
was the worst case, and it took real data to notice.

| Term | Synthetic (1M docs) | Real arXiv (500k docs) |
| --- | --- | --- |
| `entanglement` | 543,439 (54%) | 7,072 (1.4%) |
| `classical` | 591,505 (59%) | 27,519 (5.5%) |
| `quantum` | 681,275 (68%) | 56,630 (11.3%) |

Zipf's law put the synthetic vocabulary's anchor words at ranks 11–16 of
200,000, so every one of them landed in most documents and no intersection
could skip anything. Real English over real abstracts is far steeper in the
tail: the rarest clause is 77× smaller, so the planner starts from 7,072
candidates instead of 543,439 and galloping has somewhere to gallop to.

**The lesson is about the benchmark, not the engine.** Two thirds of the
corpus matching a term is not a realistic query; it is a stress test. Day 12's
harness needs its query set drawn from the real corpus's own frequency
distribution, or the percentiles it reports will describe a corpus nobody has.
Both numbers stay in this file: 220 µs is what a user gets, 17 ms is the
ceiling when every clause is common.

## Day 9 — Phrase queries

**The corpus changed for this section, and day 8's numbers do not carry over.**
Day 8's addendum found the synthetic corpus 77× too common at the tail; it also
drew every token independently, so no two-word sequence ever recurred and a
phrase query had nothing to find. Both are fixed:

| | v1 (day 8) | v2 (day 9) | Real arXiv |
| --- | --- | --- | --- |
| `quantum` | 68.1% | **11.03%** | 11.33% |
| `entanglement` | 54.3% | **1.05%** | 1.41% |
| Recurring two-word sequences | none | yes | yes |

v2 assembles each document from four-word chunks drawn from their own Zipf
distribution, so a common phrase is common and a rare one is rare. Word ranks
are now solved for a target document frequency rather than picked by hand.

1,000,000 documents, 46,884 terms, 112.2M postings, 160M positions, 1.4 GiB
index, 56.0 s to build.

### Getting the chunk ranks wrong is instructive

The first run of the v2 generator gave `quantum` a document frequency of 66%
even though its word rank had been solved for 11%. The cause: a planted chunk at
Zipf rank 3. A chunk at rank *r* lands in roughly `40/(r·H)` of documents, so
rank 3 puts all four of its words in 69% of the corpus, and `quantum` inherited
that instead of its own rank. A planted phrase has to be as rare as the phrase
it stands for.

### Phrase latency, and what the positional stage costs

| Query | Hits | Latency |
| --- | --- | --- |
| `'quantum chromodynamics'` | 4,029 | 954 µs |
| `quantum AND chromodynamics` | 4,717 | 849 µs |
| `'quantum entanglement'` | 5,067 | 1.15 ms |
| `quantum AND entanglement` | 5,608 | 450 µs |
| `'error correction'` | 19,683 | 1.93 ms |
| `error AND correction` | 19,716 | 452 µs |
| `'surface code quantum error'` | 1,005 | 1.97 ms |
| `surface AND code AND quantum AND error` | 1,036 | 1.24 ms |
| `'chromodynamics quantum'` (reversed) | 0 | 948 µs |
| `'the of and in we'` | 0 | 352 ms |

**A phrase costs 1.1–4× the `AND` of the same words.** Stage one *is* that
`AND`; the extra is stage two reading position lists in the documents that
survived it. `'error correction'` is the worst ratio at 4.3× and the smallest
filter — 19,683 of 19,716 co-occurrences are adjacent — because in this corpus
`correction` only ever appears in a chunk where `error` precedes it. Real text
is not that obliging; `'quantum chromodynamics'` drops 688 of 4,717 (15%).

**The reversed phrase costs the same and returns nothing.**
`'chromodynamics quantum'` is 948 µs against 954 µs for the forward phrase. It
finds the same 4,717 co-occurring documents and rejects every one. That is the
price of correctness, and it is not avoidable: nothing in a document list says
which order the words are in.

**Five stop words in a row is the worst case, at 352 ms.**
`'the of and in we'` survives stage one in ~800,000 documents and has to read
five position lists in each. It is 370× slower than a phrase of content words
and returns nothing. A stop-word list would make it disappear, and this engine
deliberately has none — dropping `the` would break `"to be or not to be"`.
Day 13 can revisit it; the honest number belongs here either way.

### Why the anchor is the rarest word *in the document*

Inside a document, the positional check tests one alignment per occurrence of
the anchor word. Choosing which word to anchor on therefore depends on how
often each repeats — not in the corpus, but in that one document:

| Term | Documents | Mean occurrences per document | p50 | p95 | max |
| --- | --- | --- | --- | --- | --- |
| `the` | 980,436 | 4.25 | 4 | 8 | 18 |
| `of` | 864,072 | 2.30 | 2 | 5 | 12 |
| `we` | 653,157 | 1.61 | 1 | 3 | 8 |
| `quantum` | 110,257 | 1.06 | 1 | 2 | 4 |
| `theory` | 35,911 | 1.02 | 1 | 1 | 3 |
| `entanglement` | 10,482 | 1.01 | 1 | 1 | 3 |
| `chromodynamics` | 10,242 | 1.00 | 1 | 1 | 2 |

Content words occur once. Stop words occur four to eighteen times. So on *this*
corpus the anchor choice can only matter for a phrase that mixes the two —
which is exactly what the measurement below shows. Real arXiv does not agree,
and the day 9 addendum says why it matters. Best of five runs, against the same binary
with `min_by_key` replaced by "always anchor on the first word":

| Query | Rarest-word anchor | First-word anchor | |
| --- | --- | --- | --- |
| `'the quantum'` | **17 ms** | 24 ms | 1.41× |
| `'the quantum theory'` | **11 ms** | 12 ms | 1.12× |
| `'quantum chromodynamics'` | 984 µs | 987 µs | 1.00× |
| `'the of and in we'` | 320 ms | 328 ms | 1.03× |

It pays where one word repeats and another does not, it is a wash where every
word repeats equally, and it never loses. One line — `min_by_key` instead of
`[0]` — for 1.4× on the queries people actually write, since a phrase in prose
usually starts with an article.

### Correctness

221 tests. Phrases get a second reference implementation, blunter than day 8's:
each document's terms flattened into one `Vec<Option<String>>` with `None`
marking the title/body join, searched with `windows(k)`. It knows nothing about
positions, postings or `FIELD_GAP` — it looks for the words next to each other
the way a reader would.

The property test generates phrases three ways: sequences lifted out of the
documents (so roughly half have hits), those same sequences reversed (near
misses, and the occasional palindrome the reference has to adjudicate), and
sequences assembled from real words that were probably never neighbours. A
phrase search that ignored order would pass on the first kind alone.

One test is derived rather than written: for every document, the last word of
the title followed by the first word of the body is a phrase that must not
match that document. `FIELD_GAP` is the only thing preventing it, and nothing
else in the suite would notice if the gap were removed.

### Day 9 addendum — real corpus, real hardware

Kayan's laptop, 500,000 arXiv records.

| Query | Hits | % | Latency |
| --- | --- | --- | --- |
| `'error correction'` | 674 | 0.135% | **174 µs** |
| `'surface code quantum error correction'` | 2 | 0.0004% | **187 µs** |
| `'correction error'` (reversed) | 3 | 0.0006% | **141 µs** |
| `quantum AND 'error correction' NOT classical` | 331 | 0.066% | **199 µs** |

Eleven times faster than the synthetic corpus again, and for the same reason as
day 8: real vocabulary is far steeper at the tail, so stage one hands stage two
hundreds of documents rather than twenty thousand. The five-word phrase is *no
slower* than the two-word one — 187 µs against 174 µs — because each extra word
narrows stage one more than it adds to stage two.

**The reversed phrase was supposed to return nothing, and it returned three
documents. It is right and the prediction was wrong.** One of them is
0806.2782, *"Another Correction. Error estimates for Binomial approximation…"*.
The tokenizer splits on the full stop, so `correction` and `error` are adjacent
positions with a sentence boundary between them that the index does not record.

That is a real limitation, not a bug in phrase search.
[`FIELD_GAP`](../src/index.rs) keeps a phrase from spanning the title/body join;
nothing keeps one from spanning a sentence. Fixing it would mean emitting a gap
at sentence boundaries during tokenization, which changes every position in the
index and therefore the on-disk format. Noting it here rather than doing it:
plenty of production engines behave exactly this way, the cost of the fix is a
full reindex, and day 10's `NEAR/k` deliberately crosses sentence boundaries
anyway — `a NEAR/5 b` is a question about a window of text, not a sentence.

### Output: showing the plan, not just the terms

Kayan's first run exposed a display bug worth the fix. The per-hit occurrence
line was built from `Expr::Term` nodes only, so a phrase-only query printed an
empty line under every hit — a phrase contains no bare terms. The plan summary
had the same blind spot: `quantum AND 'error correction' NOT classical` listed
`quantum` and `classical` and said nothing about the phrase, which is the
narrowest branch and the one the planner starts from.

Both now walk into phrases. A phrase is listed as one leaf, bounded by its
rarest word, because that is exactly what `estimate` gives the planner:

```
  narrowest first — the order the planner intersects in:
    "error correction"                       21,533
    classical                                28,472
    quantum                                 110,257
```

The header only appears when there is more than one leaf; a single-term or
single-phrase query has no plan to show.

### And one conclusion above is wrong

The repetition table says content words occur once per document, and concludes
that anchoring on the rarest word can only matter for phrases mixing a content
word with an article. Real abstracts say otherwise. From one screen of hits:

```
  0705.2342   Continuous quantum error correction for non-Markovian decoherence
              quantum×2 error×7 correction×8
  0705.4128   System Design for a Long-Line Quantum Repeater
              quantum×12 error×1 correction×1
  0706.3400   Channel-Adapted Quantum Error Correction
              quantum×7 error×5 correction×2
```

`quantum×12`, `correction×8`, `error×7` — content words, in the documents that
actually match, repeating as often as `the` does in the synthetic corpus. A
paper about quantum error correction says "quantum error correction" a dozen
times. So the anchor choice applies to ordinary content phrases too, and the 1×
figure is an artefact of the generator: assembling documents from independently
drawn chunks spreads a word's occurrences *across* documents instead of
concentrating them *within* one, which is the opposite of how a real document
about a subject behaves.

Third finding in three days about the same generator, and the one that matters
most for day 12: a benchmark built on it would understate every positional cost,
because positional work is proportional to occurrences per document and the
generator produces a third to a twelfth of the real number. Before day 12's
percentiles mean anything, documents need topic coherence — draw each document's
chunks from a small subset of the pool rather than the whole of it.
