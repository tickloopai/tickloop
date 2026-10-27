use std::time::Instant;

pub(crate) struct Phase {
    name: &'static str,
    started: Instant,
    span: tracing::Span,
}
impl Phase {
    pub(crate) fn new(name: &'static str) -> Self {
        Self {
            name,
            started: Instant::now(),
            span: tracing::Span::current(),
        }
    }
}
impl Drop for Phase {
    fn drop(&mut self) {
        if matches!(self.name, "mcp_catalog_read" | "mcp_catalog_build") {
            let _ = CATALOG.try_with(|catalog| {
                let mut catalog = catalog.borrow_mut();
                catalog.reads += u64::from(self.name == "mcp_catalog_read");
                catalog.builds += u64::from(self.name == "mcp_catalog_build");
                catalog.elapsed_us += self.started.elapsed().as_micros() as u64;
            });
        }
        tracing::debug!(target: "tickloop::request_timing", parent: &self.span, phase = self.name, elapsed_us = self.started.elapsed().as_micros() as u64, "request timing");
    }
}

struct CatalogTiming {
    started: Instant,
    reads: u64,
    builds: u64,
    elapsed_us: u64,
}
impl Default for CatalogTiming {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            reads: 0,
            builds: 0,
            elapsed_us: 0,
        }
    }
}
pub(crate) fn dispatched() {
    let _ = CATALOG.try_with(|catalog| {
        tracing::debug!(target: "tickloop::request_timing", phase = "model_dispatch", elapsed_us = catalog.borrow().started.elapsed().as_micros() as u64, "request timing");
    });
}
tokio::task_local! {
    static CATALOG: std::cell::RefCell<CatalogTiming>;
}
pub(crate) async fn request<T>(
    receipt: &tickloop_harness_protocol::Receipt,
    future: impl std::future::Future<Output = T>,
) -> T {
    use tracing::Instrument;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let request = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let span = tracing::debug_span!(target: "tickloop::request_timing", "request_preparation", chat = ?receipt.chat_id, run = ?receipt.run_id, request);
    CATALOG
        .scope(
            Default::default(),
            async {
                let result = future.await;
                CATALOG.with(|catalog| {
                    let catalog = catalog.borrow();
                    tracing::debug!(
                        target: "tickloop::request_timing", catalog_reads = catalog.reads,
                        catalog_builds = catalog.builds,
                        catalog_elapsed_us = catalog.elapsed_us,
                        "request catalog summary"
                    );
                });
                result
            }
            .instrument(span),
        )
        .await
}

pub(crate) struct ChatsMutex<T>(tokio::sync::Mutex<T>);
impl<T> ChatsMutex<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(tokio::sync::Mutex::new(value))
    }
    pub(crate) fn lock(&self) -> impl std::future::Future<Output = tokio::sync::MutexGuard<'_, T>> {
        // Keep instrumentation out of the large futures that hosts await on small stacks.
        Box::pin(async {
            let _timing = Phase::new("chats_lock_wait");
            self.0.lock().await
        })
    }
}
impl<T> std::ops::Deref for ChatsMutex<T> {
    type Target = tokio::sync::Mutex<T>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> std::ops::DerefMut for ChatsMutex<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
