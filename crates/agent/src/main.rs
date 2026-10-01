const TERMINAL_WORKER_THREADS: usize = 2;

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(TERMINAL_WORKER_THREADS)
        .enable_all()
        .build()
        .expect("cannot start the Agent terminal runtime");
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    let result = runtime.block_on(async {
        match tickloop_agent::agent_daemon::run_internal(&arguments).await {
            Some(result) => result,
            None => tickloop_agent::terminal::run().await,
        }
    });
    if let Err(error) = result {
        eprintln!("Error: {}", tickloop_agent::terminal::safe_text(&error));
        std::process::exit(1);
    }
}
