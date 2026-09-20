# Test fixtures

`tiny.jsonl` is 20 records shaped exactly like the real arXiv metadata dump
(`arxiv-metadata-oai-snapshot.json`), including the fields the engine ignores.
**The content is synthetic** — the arXiv ids are real-looking but the titles and
abstracts are invented, chosen to give later days phrases, proximity pairs,
shared prefixes (`comp*`), Unicode and awkward punctuation to test against.

`malformed.jsonl` is the same shape with a blank line, a truncated JSON object
and a record missing its `abstract`, so error handling has something to catch.

Tests must never depend on the real 4.6 GB dump.
