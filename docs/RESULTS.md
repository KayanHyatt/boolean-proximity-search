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
