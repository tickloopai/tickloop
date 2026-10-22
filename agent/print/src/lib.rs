//! A bounded, non-interactive client of AgentAccess. This crate never links the execution runtime.

mod input;
mod options;
mod output;
mod session;

pub use input::decode_stdin;
pub use options::{HELP, MAX_STDIN_BYTES, Options, OutputFormat, compose_prompt, parse};
pub use output::{BoundedOutput, MAX_OUTPUT_BYTES, PrintResult};
pub use session::run_with_client;

use std::{
    io::IsTerminal,
    os::{fd::AsFd, unix::ffi::OsStrExt},
    time::Duration,
};

use tickloop_agent_client::managed::{ManagedClient, connect};
use tickloop_agent_config::{UserSettings, config_root_directory};
use tickloop_agent_protocol::{
    AgentHostOptions, DirectoryTrustPolicy, EnvironmentValue, ExecutionProfile, LaunchSpec,
    PromptPolicy,
};
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::mpsc,
    time::Instant,
};

/// With a PROMPT, stdin is optional source material: a pipe that sends nothing this long is
/// skipped, so an inherited stdin that never closes (ssh, docker exec -i, a parent process's
/// default stdio) cannot hold the invocation until its timeout.
const STDIN_FIRST_INPUT_WAIT: Duration = Duration::from_secs(3);

/// argv excludes the executable and includes -p/--print. No terminal UI is initialized.
pub async fn run(arguments: Vec<String>) -> i32 {
    let mut options = match parse(&arguments) {
        Ok(options) => options,
        Err(error) => {
            output::report(&format!("Error: {error}"));
            return 2;
        }
    };
    let mut stdout = match std::io::stdout().as_fd().try_clone_to_owned() {
        Ok(stdout) => BoundedOutput::new(stdout),
        Err(error) => {
            output::report(&format!("Error: cannot prepare stdout: {error}"));
            return 1;
        }
    };
    if options.help {
        use std::io::Write;
        return if stdout.write_all(HELP.as_bytes()).is_ok() {
            0
        } else {
            1
        };
    }
    let started = Instant::now();
    let mut signals = match signals() {
        Ok(signals) => signals,
        Err(error) => {
            return early_failure(
                &mut stdout,
                options.format,
                1,
                format!("cannot watch signals: {error}"),
            );
        }
    };
    let prepared = async {
        let stdin = if std::io::stdin().is_terminal() {
            None
        } else {
            let wait = options
                .prompt
                .as_deref()
                .filter(|prompt| *prompt != "-")
                .map(|_| STDIN_FIRST_INPUT_WAIT);
            let stdin = input::read_stdin(wait).await.map_err(|error| (2, error))?;
            if stdin.is_none() {
                output::report(
                    "Note: no stdin data arrived within 3s; continuing without it. Redirect stdin (< /dev/null) to skip this wait.",
                );
            }
            stdin
        };
        let prompt = compose_prompt(&options, stdin).map_err(|error| (2, error))?;
        let launch = launch(&options)?;
        let executable = std::env::current_exe().map_err(|error| (1, error.to_string()))?;
        let client = connect(executable)
            .await
            .map_err(|error| (1, error.to_string()))?;
        Ok::<_, (i32, String)>((prompt, launch, client))
    };
    let prepared = tokio::select! {
        biased;
        code = session::next_signal(&mut signals) => Err((code, "print invocation cancelled".into())),
        result = tokio::time::timeout(options.timeout, prepared) => match result {
            Ok(result) => result,
            Err(_) => Err((124, "print invocation timed out before submission".into())),
        },
    };
    let (prompt, launch, client): (_, _, ManagedClient) = match prepared {
        Ok(prepared) => prepared,
        Err((code, message)) => return early_failure(&mut stdout, options.format, code, message),
    };
    options.timeout = options
        .timeout
        .saturating_sub(started.elapsed())
        .max(Duration::from_millis(1));
    let result = run_with_client(&options, launch, prompt, &client, &mut stdout, signals).await;
    if let Some(error) = result.error {
        output::report(&format!("Error: {error}"));
    }
    result.exit_code
}

fn early_failure(
    stdout: &mut BoundedOutput,
    format: OutputFormat,
    code: i32,
    message: String,
) -> i32 {
    let result = PrintResult::failure(code, message);
    if let Some(error) = &result.error {
        output::report(&format!("Error: {error}"));
    }
    if output::Output::new(stdout, format).finish(&result).is_err() {
        1
    } else {
        code
    }
}

/// Exit 2 is only for an unusable --cwd; configuration failures are ordinary failures.
fn launch(options: &Options) -> Result<LaunchSpec, (i32, String)> {
    let argument = if options.cwd.is_some() { 2 } else { 1 };
    let cwd = options
        .cwd
        .clone()
        .map_or_else(std::env::current_dir, Ok)
        .and_then(|path| path.canonicalize())
        .map_err(|error| {
            (
                argument,
                format!("cannot resolve working directory: {error}"),
            )
        })?;
    if !cwd.is_dir() {
        return Err((argument, "--cwd must name an existing directory".into()));
    }
    let config_root = config_root_directory()
        .map(std::path::absolute)
        .transpose()
        .map_err(|error| {
            (
                1,
                format!("cannot resolve configuration directory: {error}"),
            )
        })?;
    let mut settings = config_root
        .as_ref()
        .map(|root| UserSettings::in_root(root).load());
    let model = settings
        .as_mut()
        .map(|settings| settings.configured_model())
        .transpose()
        .map_err(|error| (1, error))?
        .flatten();
    if let Some(settings) = settings {
        for warning in settings.into_warnings() {
            output::report(&format!("Warning: {warning}"));
        }
    }
    let mut launch = LaunchSpec::embedded(cwd);
    launch.config_root = config_root;
    launch.model = model;
    launch.profile = ExecutionProfile::OneShot;
    launch.session_persistence = options.persist;
    launch.host_options = AgentHostOptions {
        execution_mode_override: Some(options.mode),
        directory_trust: if options.trusted {
            DirectoryTrustPolicy::CallerManaged
        } else {
            DirectoryTrustPolicy::RequireUserTrust
        },
        prompt_policy: PromptPolicy::DenyOnPrompt,
    };
    launch.environment = std::env::vars_os()
        .map(|(name, value)| EnvironmentValue {
            name: name.as_os_str().as_bytes().to_vec(),
            value: value.as_os_str().as_bytes().to_vec(),
        })
        .collect();
    Ok(launch)
}

/// Yields 130 (SIGINT) or 143 (SIGTERM) for every signal. Once registered, these handlers replace
/// the default disposition for the rest of the process, so each later signal still needs a reader.
fn signals() -> std::io::Result<mpsc::Receiver<i32>> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let (sender, receiver) = mpsc::channel(4);
    tokio::spawn(async move {
        loop {
            let code = tokio::select! {
                Some(()) = interrupt.recv() => 130,
                Some(()) = terminate.recv() => 143,
                else => return,
            };
            if sender.send(code).await.is_err() {
                return;
            }
        }
    });
    Ok(receiver)
}
