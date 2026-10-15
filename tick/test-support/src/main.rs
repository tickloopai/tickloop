//! `tick-mock`: runs a financial Tick on the mock data the way a Tick Command runs it on its
//! provider, for tests that need a whole program. `make build`, `make install` and
//! `make package` leave it out.

use std::{process::ExitCode, sync::Arc};

use clap::Parser;
use tick_command::{
    cli::{GlobalArgs, TickCommand},
    entry::{self, Running, Services},
};
use tick_provider::error::Result;
use tick_test_support::{MockApi, MockCommunity, MockOptions};

#[derive(Parser)]
#[command(name = "tick-mock", about = "Run a Tick on mock data, for tests")]
struct Cli {
    #[command(flatten)]
    global: GlobalArgs,
    #[command(subcommand)]
    tick: TickCommand,
}

fn main() -> ExitCode {
    entry::run(execute)
}

async fn execute(cli: Cli) -> Result<()> {
    let settings = cli.global.settings()?;
    let api = MockApi::new()?;
    let services = Services {
        quote: Some(&api),
        trade: Some(&api),
        news: Some(&api),
        options: Some(Arc::new(MockOptions)),
        community: Some(Arc::new(MockCommunity)),
    };
    entry::run_tick(
        Running::Provider(cli.tick, services),
        &settings,
        cli.global.size,
    )
    .await
}
