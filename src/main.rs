//! Command-line interface for the search engine.
//!
//! Three subcommands, matching the three things the project has to prove it can
//! do: build an index, query it, and be faster than not having one.

use std::error::Error as _;
use std::path::PathBuf;
use std::process::ExitCode;

use boolsearch::Result;
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
        } => {
            println!("index");
            println!("  corpus: {}", input.display());
            println!("  output: {}", output.display());
            match limit {
                Some(n) => println!("  limit:  {n} documents"),
                None => println!("  limit:  none, index the whole corpus"),
            }
            println!("  -> day 2 of docs/PLAN.md fills this in");
        }

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
    fn every_subcommand_succeeds_as_a_stub() {
        let workloads = [
            vec!["boolsearch", "index", "-i", "corpus.jsonl"],
            vec!["boolsearch", "search", "a AND b"],
            vec!["boolsearch", "bench"],
        ];

        for argv in workloads {
            let cli = Cli::parse_from(argv.iter().copied());
            assert!(super::run(&cli.command).is_ok(), "{argv:?} failed");
        }
    }
}
