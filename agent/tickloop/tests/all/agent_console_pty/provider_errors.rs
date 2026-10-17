use super::*;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kimi_http_refusal_shows_its_message_next_steps_and_clickable_link() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = PrivateFixture::new();
    let auth = fixture.path("config/auth");
    std::fs::create_dir(&auth).unwrap();
    std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o700)).unwrap();
    let credentials = auth.join("kimi-code-oauth.json");
    std::fs::write(&credentials, json!({"provider":"kimi", "generation":Uuid::new_v4(), "credentials":{"token":"fxA91q7Z83mP46r2T", "refresh_token":"fixture-refresh", "identity":Uuid::new_v4(), "expires_at_ms":u64::MAX}}).to_string()).unwrap();
    std::fs::set_permissions(credentials, std::fs::Permissions::from_mode(0o600)).unwrap();
    let body = r#"{"error":{"message":"You've reached your monthly usage limit for this billing cycle. Your quota will be refreshed in the next cycle. To continue now, purchase extra usage or upgrade your plan: https://www.kimi.com/membership/subscription?tab=quota","type":"access_terminated_error"}}"#;
    let (url, captured) = model_http_support::serve_response(403, body.into()).await;
    let mut model = tickloop_agent_model::cards::ModelCatalog::defaults()
        .get("kimi-code-oauth", "k3")
        .unwrap();
    model.base_url = url;
    fixture.set_model(&model);
    let result = AssertUnwindSafe(async {
        let mut view = PtyView::start_with_environment(
            &fixture,
            &[],
            &[("FORCE_HYPERLINK", std::path::Path::new("1"))],
        );
        view.wait_for("Kimi input ready", new_input_is_visible)
            .await;
        view.send(b"Explain this fixture\r");
        tokio::time::timeout(UI_TIMEOUT, captured)
            .await
            .unwrap()
            .unwrap();
        let frame = view
            .wait_for("Kimi account refusal explained", |frame| {
                frame.contains("HTTP 403")
                    && frame.contains("monthly usage limit")
                    && frame.contains("K3 plan access")
                    && frame.contains("Usage limit OR")
            })
            .await;
        assert!(
            !frame.contains("not logged in") && !frame.contains("fxA91q7Z83mP46r2T"),
            "{frame}"
        );
        eprintln!("[provider-error-screen]\n{frame}\n[/provider-error-screen]");
        view.wait_for("persistent provider failure status", |frame| {
            frame.contains("HTTP 403 · usage / plan limit")
        })
        .await;
        let raw = String::from_utf8_lossy(&view.output.lock().unwrap()).into_owned();
        assert!(
            raw.contains("\x1b]8;;https://www.kimi.com/membership/subscription?tab=quota\x07"),
            "provider link must be clickable across wrapped rows"
        );
        view.send(b"\x0f");
        view.wait_for("Transcript preserves refusal", |frame| {
            frame.contains("Showing detailed transcript") && frame.contains("monthly usage limit")
        })
        .await;
        view.send(b"\x1b");
        view.wait_for("input returns after Transcript", |frame| {
            !frame.contains("Showing detailed transcript")
        })
        .await;
        view.send(b"/provider\r");
        view.wait_for("Provider directory", |frame| {
            frame.contains("Choose Provider")
        })
        .await;
        view.send(b"p");
        let details = view
            .wait_for("last Kimi refusal in Connection Details", |frame| {
                frame.contains("Connection ·") && frame.contains("Last request failed")
            })
            .await;
        assert!(details.contains("HTTP 403"), "{details}");
        eprintln!("[provider-error-details]\n{details}\n[/provider-error-details]");
        let dialog = details.split_once("Connection ·").unwrap().1;
        assert!(
            dialog.contains("higher Kimi Code plan")
                && dialog.contains("plan access and usage limits"),
            "{dialog}"
        );
        view.send(b"\x1b");
        view.wait_for("back from Details", |frame| {
            frame.contains("Choose Provider")
        })
        .await;
        view.send(b"\x1b");
        view.wait_for("input after Provider directory", |frame| {
            !frame.contains("Choose Provider") && !frame.contains("Connection ·")
        })
        .await;
        view.close().await;
    })
    .catch_unwind()
    .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
