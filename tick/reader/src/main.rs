//! `tick-reader`: the Reader Tick, bundled with Tickloop. Its arguments are the documents to read
//! and `describe` is its one subcommand, so a local file named describe is given as `./describe`
//! wherever it appears.

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tick_command::{
    cli::{CONFIGURATION_HELP, GlobalArgs},
    entry::{self, Running},
};
use tick_market::{args::ReaderArgs, metadata::TickKind};
use tick_provider::error::{Error, Result};

const ABOUT: &str = "Read original financial documents from URLs or local files";

const DESCRIBE: &str = "describe";

const EXAMPLES: &str = "Examples:
  tick-reader ./earnings-release.pdf
  tick-reader ./earnings-release.pdf --symbol AAPL.US --fiscal-period FY2026Q3
  tick-reader https://example.com/investors/results.html --once --format json
  tick-reader ./describe    a local file named describe";

const EXIT_CODES: &str = "Exit codes:
  0  success
  2  usage or configuration error
  4  network or API error";

#[derive(Parser)]
#[command(
    name = "tick-reader",
    version,
    about = ABOUT,
    after_help = format!("{EXAMPLES}\n\n{CONFIGURATION_HELP}\n\n{EXIT_CODES}"),
    disable_help_subcommand = true
)]
struct Cli {
    #[command(flatten)]
    global: GlobalArgs,
    #[command(flatten)]
    reader: ReaderArgs,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print the Reader's Tick descriptor and minimum Panel size as JSON
    Describe,
}

fn main() -> ExitCode {
    if std::env::args_os().skip(1).collect::<Vec<_>>() == ["--build-identity"] {
        println!("{}", env!("TICKLOOP_BUILD_IDENTITY"));
        return ExitCode::SUCCESS;
    }
    let _lease = match tickhost_connect::executable_lease::retain_current_executable() {
        Ok(lease) => lease,
        Err(error) => {
            eprintln!("tick-reader: cannot retain running executable: {error}");
            return ExitCode::FAILURE;
        }
    };
    entry::run(execute)
}

async fn execute(cli: Cli) -> Result<()> {
    if let Some(Command::Describe) = cli.command {
        entry::refuse_in_instance()?;
        return entry::describe(&[TickKind::Reader]);
    }
    if cli
        .reader
        .documents
        .iter()
        .any(|document| document == DESCRIBE)
    {
        return Err(Error::usage(
            "describe is a command, not a document; give a local file named describe as ./describe",
        ));
    }
    if cli.reader.output.once {
        entry::refuse_in_instance()?;
    }
    entry::check_locale()?;
    let settings = cli.global.settings()?;
    let args = cli.reader;
    let context = tick_market::research::ResearchContext::from_environment()?;
    let library = if args.check || args.output.once {
        tick_market::reader::load(&args).await?
    } else {
        tick_market::reader::load_for_context(&args, context.as_ref()).await?
    };
    if args.check {
        return Ok(());
    }
    entry::run_tick(Running::Reader(args, library), &settings, cli.global.size).await
}
