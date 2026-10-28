use super::*;

async fn exercise_nested_family(fixture: &PrivateFixture) {
    let spawn = |name, description, task| {
        chat_reply(
            Some((
                "Subagent",
                name,
                json!({"name":name,"description":description,"task":task}),
            )),
            "",
        )
    };
    // Every member runs in the background, so replies follow who asks, not the order of asking.
    let (base_url, mut requests) = model_http_support::serve_router(move |request| {
        let history = request["messages"].to_string();
        match delegated_by(request).as_deref() {
            None if !has_result(request, "alpha") => {
                spawn("alpha", "Nested Alpha", "Delegate to beta")
            }
            None if history.contains("ALPHA-DONE") => chat_reply(None, "NESTED-FAMILY-DONE"),
            None => chat_reply(None, "Root delegated"),
            Some("alpha") if !has_result(request, "beta") => {
                spawn("beta", "Nested Beta", "Delegate to gamma")
            }
            Some("alpha") => chat_reply(None, "ALPHA-DONE"),
            Some("beta") if !has_result(request, "gamma") => {
                spawn("gamma", "Nested Gamma", "Return the leaf report")
            }
            Some("beta") => chat_reply(None, "BETA-DONE"),
            _ => chat_reply(None, "GAMMA-DONE"),
        }
    })
    .await;
    fixture.set_chat_model(&base_url);
    let mut pty = PtyView::launch(fixture, &["--dangerously-skip-permissions"], &[], true);
    pty.wait_for("nested family input ready", new_input_is_visible)
        .await;
    let client = fixture.client().await;
    pty.send(b"Delegate three levels and return.\r");
    pty.wait_for(
        "nested results return to the root and all members remain visible",
        |frame| {
            frame.contains("NESTED-FAMILY-DONE")
                && frame.contains("Nested Alpha")
                && frame.contains("Nested Beta")
                && frame.contains("Nested Gamma")
                && frame.contains("Subagents 3")
        },
    )
    .await;
    let members = catalog(&client).await;
    assert_eq!(members.len(), 4);
    let root = members
        .iter()
        .find(|row| row.parent.is_none())
        .unwrap()
        .execution;
    let alpha = members
        .iter()
        .find(|row| {
            row.subagent
                .as_ref()
                .is_some_and(|member| member.handle == "alpha")
        })
        .unwrap();
    let beta = members
        .iter()
        .find(|row| {
            row.subagent
                .as_ref()
                .is_some_and(|member| member.handle == "beta")
        })
        .unwrap();
    let gamma = members
        .iter()
        .find(|row| {
            row.subagent
                .as_ref()
                .is_some_and(|member| member.handle == "gamma")
        })
        .unwrap();
    assert_eq!(alpha.parent, Some(root));
    assert_eq!(beta.parent, Some(alpha.execution));
    assert_eq!(gamma.parent, Some(beta.execution));
    let snapshot = wait_snapshot(
        &client,
        gamma.execution,
        "deepest member is addressable",
        |snapshot| {
            snapshot
                .conversation
                .run_response_text(snapshot.conversation.chat_loop.run_id.unwrap())
                .as_deref()
                == Some("GAMMA-DONE")
        },
    )
    .await;
    assert_eq!(
        snapshot.conversation.delegation.as_ref().unwrap().depth(),
        3
    );
    let mut offered = Vec::new();
    while let Ok((_, request)) = requests.try_recv() {
        let subagent = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["function"]["name"] == "Subagent")
            .cloned();
        offered.push((delegated_by(&request), subagent));
    }
    for (member, subagent) in &offered {
        match (member.as_deref(), subagent) {
            (Some("gamma"), subagent) => {
                assert!(subagent.is_none(), "the third level cannot delegate")
            }
            (member, Some(subagent)) => assert!(
                subagent["function"]["parameters"]["properties"]
                    .get("run_in_background")
                    .is_none(),
                "{member:?} follows the terminal root and is not offered run_in_background"
            ),
            (member, None) => panic!("{member:?} is offered Subagent"),
        }
    }
    assert!(
        offered
            .iter()
            .any(|(member, _)| member.as_deref() == Some("gamma"))
    );
    pty.close().await;
}

#[tokio::test]
async fn actual_cli_three_level_subagents_share_one_flat_family_and_keep_real_parents() {
    let fixture = PrivateFixture::new();
    let result = AssertUnwindSafe(exercise_nested_family(&fixture))
        .catch_unwind()
        .await;
    fixture.shutdown().await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
