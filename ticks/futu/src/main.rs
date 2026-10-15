//! `tick-futu`: the Options Tick on option pricing from a running Futu OpenD.

use std::{process::ExitCode, sync::Arc};

use clap::{Args, Parser, Subcommand};
use tick_command::{
    cli::{CONFIGURATION_HELP, GlobalArgs, TickCommand},
    entry::{self, Running, Services},
};
use tick_futu::FutuOptions;
use tick_market::{args::OptionsArgs, metadata::TickKind};
use tick_provider::{error::Result, options::OptionsApi};

const ABOUT: &str =
    "Futu Ticks: option pricing from a running Futu OpenD as a standalone terminal command";

const EXAMPLES: &str = "Examples:
  tick-futu options AAPL.US
  tick-futu options AAPL.US --port 11111
  tick-futu options 00700.HK --expiry 2026-10-29 --strikes 21
  tick-futu options AAPL.US --once --format json";

const EXIT_CODES: &str = "Exit codes:
  0  success
  2  usage or configuration error
  4  network or API error";

#[derive(Parser)]
#[command(
    name = "tick-futu",
    version,
    about = ABOUT,
    after_help = format!("{EXAMPLES}\n\n{CONFIGURATION_HELP}\n\n{EXIT_CODES}")
)]
struct Cli {
    #[command(flatten)]
    global: GlobalArgs,
    #[command(flatten)]
    connection: Connection,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct Connection {
    /// OpenD host
    #[arg(long, global = true, default_value = "127.0.0.1")]
    host: String,
    /// OpenD port
    #[arg(long, global = true, default_value_t = 11111)]
    port: u16,
}

#[derive(Subcommand)]
enum Command {
    /// Option pricing, IV and saved snapshots for one underlying
    Options(OptionsArgs),
    /// Print Tick descriptors and minimum Panel sizes as JSON
    Describe,
}

fn main() -> ExitCode {
    entry::run(execute)
}

async fn execute(cli: Cli) -> Result<()> {
    let args = match cli.command {
        Command::Options(args) => args,
        Command::Describe => {
            entry::refuse_in_instance()?;
            return entry::describe(&[TickKind::Options]);
        }
    };
    if args.output.once {
        entry::refuse_in_instance()?;
    }
    entry::check_locale()?;
    let settings = cli.global.settings()?;
    let api: Arc<dyn OptionsApi> = Arc::new(FutuOptions::lazy(
        &cli.connection.host,
        cli.connection.port,
    )?);
    if args.check {
        let request = tick_market::options::OptionsRequest::new(
            &args.symbol,
            args.expiry.as_deref(),
            args.strikes,
        )?;
        tick_market::options::load(api.as_ref(), &request).await?;
        return Ok(());
    }
    let services = Services {
        quote: None,
        trade: None,
        news: None,
        options: Some(api),
        community: None,
    };
    entry::run_tick(
        Running::Provider(TickCommand::Options(args), services),
        &settings,
        cli.global.size,
    )
    .await
}
