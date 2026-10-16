# dotenv stand-in

The Longbridge SDK (`longbridge` and `longbridge-httpcli` at revision `5929410`)
calls `dotenv::dotenv()` from `dotenv` 0.15 whenever it builds a configuration.
That loads a `.env` file from the working directory or any directory above it
into the process environment, where the SDK then reads its `LONGBRIDGE_*`
settings, among them a log directory for logs that record whole API responses,
paper trading, the cache directory, and proxy variables for its HTTP client.
`tick-longbridge` runs in the Agent's working directory, which is a project's,
so none of these may come from there.

The root Cargo manifest patches crates.io `dotenv` with this crate. It contains
no upstream code: `dotenv()` reads nothing and returns an error, which both SDK
callers ignore (`let _ = dotenv::dotenv()`). Nothing else in the workspace
depends on `dotenv`. Settings in the Tick's own environment still apply, and the
Tick sets its endpoints explicitly.

Regression coverage: `ticks/longbridge/tests/all/hosted_startup.rs`,
`a_dotenv_file_in_the_working_directory_sets_nothing_for_the_sdk`.
