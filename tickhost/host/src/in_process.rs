//! A privately owned Tickhost with a runtime that survives its caller's Tokio runtime.
use crate::{
    host::{self, HostConfig},
    paths::HostPaths,
};
use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub struct InProcessHost {
    stop: CancellationToken,
    thread: Option<std::thread::JoinHandle<()>>,
    result: oneshot::Receiver<io::Result<()>>,
}

impl InProcessHost {
    pub fn start(paths: HostPaths, config: HostConfig) -> io::Result<Self> {
        Self::start_until(paths, config, std::future::pending())
    }

    pub fn start_until(
        paths: HostPaths,
        config: HostConfig,
        stop: impl Future<Output = ()> + Send + 'static,
    ) -> io::Result<Self> {
        let cancellation = CancellationToken::new();
        let stopped = cancellation.clone();
        let (sender, result) = oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("owned-tickhost".into())
            .spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .and_then(|runtime| {
                        runtime.block_on(host::serve_until(paths, config, async move {
                            tokio::select! { () = stopped.cancelled() => {}, () = stop => {} }
                        }))
                    });
                let _ = sender.send(result);
            })?;
        Ok(Self {
            stop: cancellation,
            thread: Some(thread),
            result,
        })
    }

    pub fn is_finished(&self) -> bool {
        self.thread
            .as_ref()
            .is_none_or(|thread| thread.is_finished())
    }
}

impl Future for InProcessHost {
    type Output = Result<io::Result<()>, oneshot::error::RecvError>;
    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.result).poll(context)
    }
}

impl Drop for InProcessHost {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
