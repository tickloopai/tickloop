use super::*;

fn install_design_plugin(fixture: &PrivateFixture, name: &str) {
    let source = fixture.path(&format!("{name}-source"));
    std::fs::create_dir_all(source.join(".claude-plugin")).unwrap();
    std::fs::create_dir_all(source.join("skills/design")).unwrap();
    std::fs::write(
        source.join(".claude-plugin/plugin.json"),
        format!(r#"{{"name":"{name}"}}"#),
    )
    .unwrap();
    std::fs::write(source.join("skills/design/SKILL.md"), "---\nname: frontend-design\ndescription: Design a polished page\n---\nSKILL_DESIGN_GUIDANCE $ARGUMENTS").unwrap();
    tick_plugins::PluginStore::in_config_root(&fixture.path("config"))
        .install_path(&source)
        .unwrap();
}

async fn run_skill_completion(fixture: &PrivateFixture) {
    install_design_plugin(fixture, "canvas");
    for (path, name, invocable) in [
        ("config/skills/personal", "f-personal", true),
        ("work/.tickloop/skills/project", "f-project", true),
        ("work/.tickloop/skills/hidden", "f-hidden", false),
    ] {
        let directory = fixture.path(path);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("SKILL.md"), format!("---\nname: {name}\ndescription: Local guidance\nuser-invocable: {invocable}\n---\nBody")).unwrap();
    }
    let mut view = PtyView::start(fixture);
    view.wait_for("new input ready", new_input_is_visible).await;
    view.send(b"/f");
    let frame = view
        .wait_for("all user-invocable sources appear in Skills", |frame| {
            frame.contains("/canvas:frontend-design")
                && frame.contains("Design a polished page")
                && frame.contains("Plugin")
                && frame.contains("/f-personal")
                && frame.contains("User")
                && frame.contains("/f-project")
                && frame.contains("Project")
                && frame.contains("Skills")
        })
        .await;
    assert!(!frame.contains("/f-hidden"));
    view.send(b"ront");
    view.wait_for("bare-name prefix filters the namespaced Skill", |frame| {
        frame.contains("/canvas:frontend-design") && !frame.contains("/f-personal")
    })
    .await;
    view.send(b"\x0e\x10\r");
    view.wait_for_editor(
        "ctrl+n and ctrl+p select the full Skill name",
        Some("/canvas:frontend-design"),
    )
    .await;
    view.send(b"\x03/front");
    view.wait_for("completion is open before disable", |frame| {
        frame.contains("/canvas:frontend-design")
    })
    .await;
    let store = tick_plugins::PluginStore::in_config_root(&fixture.path("config"));
    store.set_enabled("canvas", false).unwrap();
    view.wait_for("disable refreshes an already open completion", |frame| {
        !frame.contains("/canvas:frontend-design") && !frame.contains("Skills")
    })
    .await;
    store.set_enabled("canvas", true).unwrap();
    view.wait_for("enable restores completion without restarting", |frame| {
        frame.contains("/canvas:frontend-design") && frame.contains("Skills")
    })
    .await;
    std::fs::remove_file(fixture.path("config/skills/personal/SKILL.md")).unwrap();
    std::fs::remove_file(fixture.path("work/.tickloop/skills/project/SKILL.md")).unwrap();
    view.send(b"\x03\x03/f");
    view.wait_for_editor("edited-files query is in the input", Some("/f"))
        .await;
    view.wait_for("removed disk Skills leave the cached candidates", |frame| {
        frame.contains("/canvas:frontend-design")
            && !frame.contains("/f-personal")
            && !frame.contains("/f-project")
    })
    .await;
    view.resize(120, 60).await;
    view.send(b"\x03\x03/");
    view.send(&[0x0e; 40]);
    let grouped = view
        .wait_for("all Skills share one contiguous group", |frame| {
            frame.contains("/canvas:frontend-design")
                && frame.contains("/loop")
                && frame.contains("/slides")
                && frame.contains("/tickloop")
        })
        .await;
    assert_eq!(
        grouped
            .lines()
            .filter(|line| line.trim() == "Skills")
            .count(),
        1,
        "{grouped}"
    );
    view.close().await;
}

#[tokio::test]
async fn actual_cli_completes_discovered_plugin_skills() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_skill_completion(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn run_skill_rows(fixture: &PrivateFixture) {
    install_design_plugin(fixture, "frontend-design");
    let (base_url, mut requests) = model_http_support::serve_responses(vec![
        (200, chat_reply(None, "First design ready")),
        (200, chat_reply(None, "Second design ready")),
    ])
    .await;
    fixture.set_chat_model(&base_url);
    let mut view = PtyView::launch(fixture, &[], &[], true);
    view.wait_for("new input ready", new_input_is_visible).await;
    view.send(b"/frontend-design first\r");
    let (_, request) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        request["messages"]
            .to_string()
            .contains("SKILL_DESIGN_GUIDANCE first")
    );
    view.wait_for("first Skill load is visible", |frame| {
        frame.contains("Skill · frontend-design:frontend-design")
            && frame.contains("Plugin · frontend-design")
            && frame.contains("First design ready")
    })
    .await;
    view.send(b"/frontend-design second\r");
    let (_, request) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        request["messages"]
            .to_string()
            .contains("SKILL_DESIGN_GUIDANCE second")
    );
    view.wait_for("second Skill load is visible", |frame| {
        frame
            .matches("Skill · frontend-design:frontend-design")
            .count()
            == 2
            && frame.contains("Second design ready")
    })
    .await;
    let client = fixture.client().await;
    let chat = catalog(&client).await[0].chat_id;
    view.close().await;
    drop(client);
    fixture.shutdown().await;
    tick_plugins::PluginStore::in_config_root(&fixture.path("config"))
        .set_enabled("frontend-design", false)
        .unwrap();
    let mut reopened = PtyView::launch(fixture, &["--resume", &chat.to_string()], &[], true);
    reopened
        .wait_for("Skill rows survive reopening", |frame| {
            frame
                .matches("Skill · frontend-design:frontend-design")
                .count()
                == 2
                && frame.contains("Plugin · frontend-design")
        })
        .await;
    reopened.keep_history_paused().await;
    reopened.close().await;
}

#[tokio::test]
async fn actual_cli_short_skill_invocations_show_source_and_restore_history_rows() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_skill_rows(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn run_ambiguous_skills(fixture: &PrivateFixture) {
    install_design_plugin(fixture, "alpha");
    install_design_plugin(fixture, "beta");
    let directory = fixture.path("config/skills/help");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("SKILL.md"),
        "---\nname: help\ndescription: Shadow help\n---\nSHOULD_NOT_RUN",
    )
    .unwrap();
    let (base_url, mut requests) = model_http_support::serve_responses(vec![(
        200,
        chat_reply(None, "Qualified design ready"),
    )])
    .await;
    fixture.set_chat_model(&base_url);
    let mut view = PtyView::start(fixture);
    view.wait_for("new input ready", new_input_is_visible).await;
    view.send(b"/help report\r");
    view.wait_for("built-in help reserves its name before creation", |frame| {
        frame.contains("Usage: /help")
    })
    .await;
    view.send(b"\x03");
    view.send(b"/frontend-design first\r");
    view.wait_for("new Agent rejects ambiguous short name", |frame| {
        frame.contains("ambiguous skill")
            && frame.contains("/alpha:frontend-design")
            && frame.contains("/beta:frontend-design")
    })
    .await;
    let client = fixture.client().await;
    assert!(catalog(&client).await.is_empty());
    view.send(b"\x03/alpha:frontend-design first\r");
    let (_, request) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        request["messages"]
            .to_string()
            .contains("SKILL_DESIGN_GUIDANCE first")
    );
    view.wait_for("full name runs despite the short-name collision", |frame| {
        frame.contains("Qualified design ready")
    })
    .await;
    view.send(b"/frontend-design again\r");
    view.wait_for("existing Chat rejects ambiguous short name", |frame| {
        frame.contains("ambiguous skill")
            && frame.contains("/alpha:frontend-design")
            && frame.contains("/beta:frontend-design")
    })
    .await;
    view.send(b"/help report\r");
    view.wait_for("built-in help reserves its name in a Chat", |frame| {
        frame.contains("Usage: /help")
    })
    .await;
    assert_eq!(catalog(&client).await.len(), 1);
    view.close().await;
}

#[tokio::test]
async fn actual_cli_short_skill_collisions_are_refused_before_creation_and_in_chat() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_ambiguous_skills(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn run_path_messages(fixture: &PrivateFixture) {
    install_design_plugin(fixture, "canvas");
    let (base_url, mut requests) = model_http_support::serve_responses(vec![
        (200, chat_reply(None, "Read the folder path")),
        (200, chat_reply(None, "Read the root path")),
    ])
    .await;
    fixture.set_chat_model(&base_url);
    let folder_message = format!("{} what is in it", fixture.path("work").display());
    let mut view = PtyView::launch(fixture, &[], &[], true);
    view.wait_for("new input ready", new_input_is_visible).await;
    view.send(folder_message.as_bytes());
    view.wait_for_editor("the path message is in the input", Some(&folder_message))
        .await;
    assert!(!view.frame().contains("/canvas:frontend-design"));
    view.send(b"\r");
    let (_, request) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(request["messages"].to_string().contains(&folder_message));
    view.wait_for(
        "a leading folder path starts the Agent as a message",
        |frame| frame.contains("Read the folder path"),
    )
    .await;
    let root_path_message = "/tmp is where the logs go";
    view.send(format!("{root_path_message}\r").as_bytes());
    let (_, request) = tokio::time::timeout(UI_TIMEOUT, requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(request["messages"].to_string().contains(root_path_message));
    view.wait_for("an existing root path is a message in the Chat", |frame| {
        frame.contains("Read the root path")
    })
    .await;
    view.send(b"/nosuchskill\r");
    view.wait_for("a well-formed unknown name is still reported", |frame| {
        frame.contains("Unknown or unavailable Skill")
    })
    .await;
    view.close().await;
}

#[tokio::test]
async fn actual_cli_sends_input_starting_with_a_path_as_a_message() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(run_path_messages(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
