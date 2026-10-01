const TERMINAL_WORKER_THREADS: usize = 2;

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(TERMINAL_WORKER_THREADS)
        .enable_all()
        .build()
        .expect("cannot start the Agent terminal runtime");
    if let Err(error) = runtime.block_on(tickloop_agent::terminal::run()) {
        eprintln!("Error: {}", tickloop_agent::terminal::safe_text(&error));
        std::process::exit(1);
    }
}
