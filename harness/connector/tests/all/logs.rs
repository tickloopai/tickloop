use crate::support::{Fixture, query_field};
use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};
use tracing::instrument::WithSubscriber;

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);
impl Write for Log {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn debug_logging_does_not_reveal_the_authorization_code_or_tokens() {
    let log = Log::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    let (code, access, refresh) = async {
        let fixture = Fixture::new();
        let login = fixture.start_login().await;
        let callback = fixture.approve(&login).await;
        let code = query_field(&callback, "code").unwrap();
        login.paste().submit(&callback).unwrap();
        login.finish(&fixture.cancellation).await.unwrap();
        let token = fixture.credentials.current().unwrap().token.unwrap();
        (code, token.access_token, token.refresh_token.unwrap())
    }
    .with_subscriber(subscriber)
    .await;
    let recorded = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    for secret in [code, access, refresh] {
        assert!(
            !recorded.contains(&secret),
            "OAuth secrets appeared in tracing output"
        );
    }
}
