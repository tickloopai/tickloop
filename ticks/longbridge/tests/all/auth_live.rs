#![cfg(feature = "live-api")]

#[tokio::test]
#[ignore = "interactive device login; run explicitly in your own terminal"]
async fn device_login_against_longbridge() {
    let path = tick_market::config::default_path().unwrap();
    let settings = tick_market::config::Settings::load(&path, false).unwrap();
    let storage = tick_longbridge::auth::default_storage().unwrap();
    tick_longbridge::auth::run_login(
        settings.region,
        &path,
        &storage,
        None,
        &mut std::io::stdin().lock(),
        &mut std::io::stdout(),
    )
    .await
    .unwrap();
}
