//! Command-line interface for the search engine.
//!
//! Three subcommands, matching the three things the project has to prove it can
//! do: build an index, query it, and be faster than not having one.

use std::error::Error as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use boolsearch::{DocStore, Document, IndexBuilder, JsonlCorpus, Result};
use clap::{Parser, Subcommand};

/// Boolean + proximity search over a text corpus.
#[derive(Debug, Parser)]
#[command(name = "boolsearch", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Build a positional inverted index from a corpus.
    Index {
        /// Corpus file, one JSON document per line.
        #[arg(short, long, value_name = "FILE")]
        input: PathBuf,

        /// Where to write the index.
        #[arg(short, long, value_name = "FILE", default_value = "index.bin")]
        output: PathBuf,

        /// Stop after N documents, for quick runs during development.
        #[arg(short, long, value_name = "N")]
        limit: Option<usize>,
    },

    /// Run a query against an index.
    Search {
        /// The query, e.g. `quantum AND "error correction" NEAR/5 surface`.
        #[arg(value_name = "QUERY")]
        query: String,

        /// Index file to search.
        #[arg(short, long, value_name = "FILE", default_value = "index.bin")]
        index: PathBuf,

        /// Maximum number of hits to print.
        #[arg(short = 'n', long, value_name = "N", default_value_t = 10)]
        limit: usize,
    },

    /// Compare index lookups against a naive linear scan.
    Bench {
        /// Index file to benchmark.
        #[arg(short, long, value_name = "FILE", default_value = "index.bin")]
        index: PathBuf,

        /// File of queries to run, one per line. Defaults to a built-in set.
        #[arg(short, long, value_name = "FILE")]
        queries: Option<PathBuf>,
    },
}

/// Returns an [`ExitCode`] rather than a `Result` so that failures print as
/// prose instead of as a `Debug` dump, and so the whole cause chain is shown —
/// "no such file" on its own is never enough to act on.
fn main() -> ExitCode {
    let cli = Cli::parse();

    match run(&cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            let mut cause = error.source();
            while let Some(current) = cause {
                eprintln!("  caused by: {current}");
                cause = current.source();
            }
            ExitCode::FAILURE
        }
    }
}

fn run(command: &Command) -> Result<()> {
    match command {
        Command::Index {
            input,
            output,
            limit,
        } => index_corpus(input, output, *limit)?,

        Command::Search {
            query,
            index,
            limit,
        } => {
            println!("search");
            println!("  query:  {query}");
            println!("  index:  {}", index.display());
            println!("  hits:   up to {limit}");
            println!("  -> days 6-11 of docs/PLAN.md fill this in");
        }

        Command::Bench { index, queries } => {
            println!("bench");
            println!("  index:   {}", index.display());
            match queries {
                Some(path) => println!("  queries: {}", path.display()),
                None => println!("  queries: built-in workload"),
            }
            println!("  -> day 12 of docs/PLAN.md fills this in");
        }
    }

    Ok(())
}

/// How many malformed records to name before falling silent. A corrupt file
/// should not produce four million lines of warnings.
const MAX_REPORTED_MALFORMED: usize = 5;

/// What one pass over the corpus saw.
struct PassSummary {
    documents: usize,
    bytes: u64,
    malformed: usize,
}

/// Streams the corpus once, handing each usable document to `visit`.
///
/// Both index passes go through here, so they see the same documents in the
/// same order — which the two-pass build depends on absolutely.
fn each_document<F>(
    input: &Path,
    limit: Option<usize>,
    report_problems: bool,
    mut visit: F,
) -> Result<PassSummary>
where
    F: FnMut(&Document),
{
    let mut corpus = JsonlCorpus::open(input)?;
    let mut malformed = 0usize;
    let mut documents = 0usize;

    // `by_ref` so the byte counter inside `corpus` survives the loop.
    // `filter_map` decides the policy for bad records, and `take` applies the
    // limit to *documents* rather than lines.
    let usable = corpus
        .by_ref()
        .filter_map(|item| match item {
            Ok(document) => Some(document),
            Err(error) => {
                malformed += 1;
                if report_problems && malformed <= MAX_REPORTED_MALFORMED {
                    eprintln!("warning: {error}");
                } else if report_problems && malformed == MAX_REPORTED_MALFORMED + 1 {
                    eprintln!("warning: further malformed records will not be reported");
                }
                None
            }
        })
        .take(limit.unwrap_or(usize::MAX));

    for document in usable {
        visit(&document);
        documents += 1;
    }

    Ok(PassSummary {
        documents,
        bytes: corpus.bytes_read(),
        malformed,
    })
}

/// Builds a positional inverted index over the corpus and reports what it cost.
///
/// Day 5 writes the result to `output`; for now it is built, measured and
/// dropped.
fn index_corpus(input: &Path, output: &Path, limit: Option<usize>) -> Result<()> {
    let started = Instant::now();

    // Pass one: learn the vocabulary and how much room each term needs.
    let mut builder = IndexBuilder::new();
    let counting = each_document(input, limit, true, |document| {
        builder.add_document(document);
    })?;
    let after_counting = started.elapsed();
    let terms = builder.terms();

    // The prefix sum between the passes: counts become starting offsets.
    let mut assembler = builder.finish_counting()?;

    // Pass two: write postings and positions into arrays that already fit.
    let mut store = DocStore::with_capacity(counting.documents);
    let filling = each_document(input, limit, false, |document| {
        assembler.add_document(document);
        store.push(document);
    })?;

    let index = assembler.finish()?;
    let elapsed = started.elapsed();
    let stats = index.stats();

    println!(
        "indexed {} documents from {}",
        stats.documents,
        input.display()
    );
    println!("  terms:        {}", format_count(stats.terms as f64));
    println!("  postings:     {}", format_count(stats.postings as f64));
    println!("  positions:    {}", format_count(stats.positions as f64));
    println!("  index size:   {}", format_bytes(stats.bytes as u64));
    println!(
        "  vs corpus:    {:.1}% of {} scanned",
        100.0 * stats.bytes as f64 / counting.bytes as f64,
        format_bytes(counting.bytes),
    );
    println!(
        "  build time:   {} ({} counting, {} filling)",
        format_duration(elapsed),
        format_duration(after_counting),
        format_duration(elapsed - after_counting),
    );
    println!(
        "  rate:         {} docs/s, {} positions/s",
        format_count(rate(stats.documents as f64, elapsed)),
        format_count(rate(stats.positions as f64, elapsed)),
    );
    if counting.malformed > 0 {
        println!("  malformed:    {} record(s) skipped", counting.malformed);
    }

    debug_assert_eq!(terms, stats.terms);
    debug_assert_eq!(counting.documents, filling.documents);

    // A concrete example, so the numbers above are not the only evidence the
    // index actually holds anything.
    if let Some(postings) = index.postings_for("quantum") {
        let documents = postings.len();
        if let Some(first) = index.postings_for("quantum").and_then(|mut p| p.next()) {
            let title = store
                .get(first.doc_id)
                .map_or("", |meta| meta.title.as_str());
            println!(
                "  \"quantum\":    {} documents, first is {} at position(s) {:?}",
                format_count(documents as f64),
                first.doc_id,
                &first.positions[..first.positions.len().min(4)],
            );
            if !title.is_empty() {
                println!("                {}", truncate(title, 66));
            }
        }
    }

    println!("  -> day 5 writes this to {}", output.display());

    Ok(())
}

/// Shortens `text` to at most `limit` characters, adding an ellipsis.
///
/// Counts characters rather than bytes: slicing a `String` by byte index panics
/// if the cut lands inside a multi-byte character, and arXiv titles include
/// accented Latin, Greek and CJK.
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }

    let mut shortened: String = text.chars().take(limit.saturating_sub(1)).collect();
    shortened.push('\u{2026}');
    shortened
}

/// A per-second rate, guarding against a zero-length measurement.
fn rate(amount: f64, elapsed: Duration) -> f64 {
    let seconds = elapsed.as_secs_f64();
    if seconds > 0.0 { amount / seconds } else { 0.0 }
}

/// Formats a byte count in binary units.
fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Formats a large count with a thousands separator.
fn format_count(count: f64) -> String {
    let whole = count.round() as u64;
    let digits = whole.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);

    for (position, digit) in digits.chars().enumerate() {
        if position > 0 && (digits.len() - position) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }

    out
}

/// Formats a duration at a sensible precision for a human reading a report.
fn format_duration(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs_f64();
    if seconds >= 1.0 {
        format!("{seconds:.2} s")
    } else {
        format!("{:.0} ms", seconds * 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::{CommandFactory, Parser};

    use super::{Cli, Command};

    /// Catches the mistakes clap can only find at runtime: two flags claiming
    /// the same short letter, an argument named twice, a default that does not
    /// parse as its own type.
    #[test]
    fn the_cli_definition_is_self_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn search_takes_a_bare_query_and_defaults_the_rest() {
        let cli = Cli::parse_from(["boolsearch", "search", "a AND b"]);

        let Command::Search {
            query,
            index,
            limit,
        } = cli.command
        else {
            panic!("expected a search command");
        };

        assert_eq!(query, "a AND b");
        assert_eq!(index, PathBuf::from("index.bin"));
        assert_eq!(limit, 10);
    }

    #[test]
    fn index_requires_an_input_and_accepts_a_limit() {
        let cli = Cli::parse_from(["boolsearch", "index", "-i", "arxiv.jsonl", "-l", "1000"]);

        let Command::Index {
            input,
            output,
            limit,
        } = cli.command
        else {
            panic!("expected an index command");
        };

        assert_eq!(input, PathBuf::from("arxiv.jsonl"));
        assert_eq!(output, PathBuf::from("index.bin"));
        assert_eq!(limit, Some(1000));
    }

    #[test]
    fn index_without_an_input_is_rejected() {
        let error = Cli::try_parse_from(["boolsearch", "index"]).unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn the_stubbed_subcommands_still_succeed() {
        let workloads = [
            vec!["boolsearch", "search", "a AND b"],
            vec!["boolsearch", "bench"],
        ];

        for argv in workloads {
            let cli = Cli::parse_from(argv.iter().copied());
            assert!(super::run(&cli.command).is_ok(), "{argv:?} failed");
        }
    }

    #[test]
    fn indexing_the_fixture_corpus_succeeds() {
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/tiny.jsonl");
        let cli = Cli::parse_from(["boolsearch", "index", "-i", fixture, "-l", "5"]);

        assert!(super::run(&cli.command).is_ok());
    }

    #[test]
    fn indexing_a_missing_corpus_fails_with_the_path() {
        let cli = Cli::parse_from(["boolsearch", "index", "-i", "no-such-corpus.jsonl"]);
        let error = super::run(&cli.command).expect_err("the corpus does not exist");

        assert!(error.to_string().contains("no-such-corpus.jsonl"));
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        assert_eq!(super::truncate("short", 10), "short");
        assert_eq!(super::truncate("exactly-10", 10), "exactly-10");
        assert_eq!(super::truncate("truncate me", 8), "truncat\u{2026}");

        // Multi-byte characters: slicing by byte index here would panic.
        let japanese = "量子誤り訂正符号の構成について";
        assert_eq!(super::truncate(japanese, 5), "量子誤り\u{2026}");
        assert_eq!(super::truncate(japanese, 100), japanese);
    }

    #[test]
    fn byte_formatting_switches_units_at_the_right_boundaries() {
        assert_eq!(super::format_bytes(0), "0 B");
        assert_eq!(super::format_bytes(1023), "1023 B");
        assert_eq!(super::format_bytes(1024), "1.0 KiB");
        assert_eq!(super::format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(super::format_bytes(4_724_464_025), "4.4 GiB");
    }

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(super::format_count(0.0), "0");
        assert_eq!(super::format_count(999.0), "999");
        assert_eq!(super::format_count(1000.0), "1,000");
        assert_eq!(super::format_count(2_713_961.0), "2,713,961");
    }

    #[test]
    fn a_zero_length_measurement_does_not_divide_by_zero() {
        let rate = super::rate(100.0, std::time::Duration::ZERO);
        assert!(rate.is_finite(), "got {rate}");
        assert_eq!(rate, 0.0);
    }

    #[test]
    fn durations_switch_from_milliseconds_to_seconds() {
        use std::time::Duration;
        assert_eq!(super::format_duration(Duration::from_millis(250)), "250 ms");
        assert_eq!(
            super::format_duration(Duration::from_millis(1500)),
            "1.50 s"
        );
    }
}
