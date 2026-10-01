#![cfg(feature = "live-api")]

#[tokio::test]
#[ignore = "interactive device login; run explicitly in your own terminal"]
async fn device_login_against_longbridge() {
    let path = tickloop_ticks::config::default_path().unwrap();
    let settings = tickloop_ticks::config::Settings::load(&path, false).unwrap();
    let storage = tickloop_ticks::auth::default_storage().unwrap();
    tickloop_ticks::auth::run_login(
        &settings,
        &path,
        &storage,
        None,
        &mut std::io::stdin().lock(),
        &mut std::io::stdout(),
    )
    .await
    .unwrap();
}
