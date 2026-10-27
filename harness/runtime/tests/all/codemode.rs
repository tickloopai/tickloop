use serde_json::json;
use tickloop_harness_runtime::codemode::{Options, run};

#[tokio::test]
async fn engine_awaits_tools_and_returns_only_explicit_output() {
    let result = run(
        "const rows = await tools.list_rows({limit: 2}); text(rows[1]); return rows.length;",
        Options::default(),
        vec!["list-rows".into()],
        |name, input, _| async move {
            assert_eq!(name, "list-rows");
            assert_eq!(input, json!({"limit":2}));
            Ok(json!(["private", "selected"]))
        },
    )
    .await;
    assert_eq!(result.error, None);
    assert_eq!(result.output, "selected\n2");
}

#[tokio::test]
async fn deadline_stops_a_pending_tool_and_rejects_invalid_options() {
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        run(
            "// @options: {\"timeout_ms\": 20}\nawait tools.wait({});",
            Options::default(),
            vec!["wait".into()],
            |_, _, cancel| async move {
                cancel.cancelled().await;
                Err("cancelled".into())
            },
        ),
    )
    .await
    .expect("script deadline must end the wait");
    assert!(result.error.unwrap().contains("timed out"));
    let result = run(
        "// @options: {\"timeout_ms\": -1}\nreturn 1;",
        Options::default(),
        vec![],
        |_, _, _| async { unreachable!() },
    )
    .await;
    assert!(result.error.unwrap().contains("timeout_ms"));
}

#[tokio::test]
async fn sandbox_globals_and_exit_cannot_reach_host_capabilities() {
    let result = run(
        r#"
        console.log([typeof fetch, typeof require, typeof setTimeout, typeof process,
            typeof std, typeof os, typeof searchTools, typeof describeTool, typeof models]);
        try { await import('std'); } catch (error) { text('modules blocked'); }
        try { exit(); } catch (error) {}
        text('after exit');
        await tools.forbidden({});
        "#,
        Options::default(),
        vec!["forbidden".into()],
        |_, _, _| async { panic!("exit must stop dispatch") },
    )
    .await;
    assert_eq!(result.error, None);
    assert_eq!(
        result.output,
        "[\"undefined\",\"undefined\",\"undefined\",\"undefined\",\"undefined\",\"undefined\",\"undefined\",\"undefined\",\"undefined\"]\nmodules blocked"
    );
}

#[tokio::test]
async fn output_limit_keeps_start_and_end_and_saves_the_complete_text() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let result = run(
        "// @options: {\"max_output_tokens\": 10}\ntext('start-' + '中'.repeat(100) + '-end');",
        Options {
            output_directory: Some(directory.path().to_owned()),
            ..Default::default()
        },
        vec![],
        |_, _, _| async { unreachable!() },
    )
    .await;
    assert_eq!(result.error, None);
    assert!(result.output.starts_with("start-"));
    assert!(result.output.ends_with("-end"));
    assert!(result.output.chars().count() < 100);
    let path = result
        .full_output_path
        .expect("full output must remain available");
    assert!(path.starts_with(directory.path()));
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        format!("start-{}-end", "中".repeat(100))
    );
}

#[tokio::test]
async fn engine_forwards_only_explicit_valid_images() {
    use base64::Engine;
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(1, 1)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    let png = base64::engine::general_purpose::STANDARD.encode(bytes.into_inner());
    let result = run(
        &format!("image({{type:'image', mimeType:'image/png', data:'{png}'}});"),
        Options::default(),
        vec![],
        |_, _, _| async { unreachable!() },
    )
    .await;
    assert_eq!(result.error, None);
    assert_eq!(result.images.len(), 1);
    let result = run(
        "image('https://example.com/private.png');",
        Options::default(),
        vec![],
        |_, _, _| async { unreachable!() },
    )
    .await;
    assert!(result.error.is_some());
}

#[tokio::test]
async fn engine_bounds_memory_cpu_microtasks_and_unresolvable_promises() {
    for source in [
        "await new Promise(() => {});",
        "// @options: {\"timeout_ms\": 30}\nwhile (true) {}",
        "// @options: {\"timeout_ms\": 30}\nwhile (true) { await Promise.resolve(); }",
        "new ArrayBuffer(300 * 1024 * 1024);",
    ] {
        let result = run(source, Options::default(), vec![], |_, _, _| async {
            unreachable!()
        })
        .await;
        assert!(result.error.is_some(), "{source}");
    }
}

#[tokio::test]
async fn paused_clock_excludes_host_wait_time() {
    let control = tickloop_harness_runtime::codemode::Control::default();
    let options = Options {
        control: control.clone(),
        ..Default::default()
    };
    let result = run(
        "// @options: {\"timeout_ms\": 500}\nreturn await tools.wait({});",
        options,
        vec!["wait".into()],
        move |_, _, _| {
            let control = control.clone();
            async move {
                let _paused = control.pause();
                tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                Ok(json!("resumed"))
            }
        },
    )
    .await;
    assert_eq!(result.error, None);
    assert_eq!(result.output, "resumed");
}

#[tokio::test]
async fn ending_script_cancels_and_settles_inflight_calls() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let settled = Arc::new(AtomicBool::new(false));
    let observed = settled.clone();
    let result = run(
        "tools.wait({}); await tools.ready({}); return 'done';",
        Options::default(),
        vec!["wait".into(), "ready".into()],
        move |name, _, cancel| {
            let settled = settled.clone();
            async move {
                if name == "wait" {
                    cancel.cancelled().await;
                    settled.store(true, Ordering::SeqCst);
                    Err("OutcomeUnknown".into())
                } else {
                    Ok(json!(true))
                }
            }
        },
    )
    .await;
    assert_eq!(result.error, None);
    assert_eq!(result.output, "done");
    assert!(
        observed.load(Ordering::SeqCst),
        "inflight callback must settle before the script returns"
    );
}

#[tokio::test]
async fn agent_runs_script_calls_through_normal_tools_and_records_children() {
    use std::sync::Arc;
    use tickloop_harness_protocol::{EventKind, ExecutionMode};
    use tickloop_harness_runtime::Agent;
    use tickloop_harness_test_support::{ScriptedModel, call, config, next_event, text};
    let directory = tempfile::tempdir().unwrap();
    let script = "await tools.Write({file_path:'note.txt',content:'HIDDEN_BODY'}); const value = await tools.Read({file_path:'note.txt'}); text(value.includes('HIDDEN_BODY') ? 'SELECTED_OK' : 'BAD');";
    let model = Arc::new(ScriptedModel::new(vec![
        call("script", "Codemode", json!({"code":script})),
        text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Write and read a note with a script".into())
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(
        std::fs::read_to_string(directory.path().join("note.txt")).unwrap(),
        "HIDDEN_BODY"
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    let children: Vec<_> = snapshot
        .operations
        .iter()
        .filter(|operation| operation.parent_call_id.as_deref() == Some("script"))
        .collect();
    assert_eq!(children.len(), 2);
    assert!(
        snapshot
            .operations
            .iter()
            .find(|operation| operation.call_id == "script")
            .unwrap()
            .result
            .as_ref()
            .unwrap()
            .content
            .contains("SELECTED_OK")
    );
    {
        let requests = model.requests.lock().unwrap();
        assert!(requests[0].tools.iter().any(|tool| tool.name == "Codemode"));
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn script_mcp_calls_pause_for_confirmation_and_receive_full_redacted_results() {
    use std::{collections::BTreeMap, sync::Arc, time::Duration};
    use tickloop_harness_protocol::{
        EventKind, ExecutionMode,
        mcp::{McpServerConfig, McpTransport, ToolExposure},
    };
    use tickloop_harness_runtime::{
        Agent,
        mcp::{HostCapabilities, McpConnections, McpServerState, McpToolProvider},
    };
    use tickloop_harness_test_support::{
        ScriptedModel, call, config, mcp_service::McpService, next_event, text,
    };
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"rows","inputSchema":{"type":"object","properties":{}}}]));
    server.set_result(json!({"content":[{"type":"text","text":"z".repeat(30000)}],"structuredContent":{"secret":"SENSITIVE_FIXTURE","count":7}}));
    let configs = vec![McpServerConfig {
        name: "data-source".into(),
        source: "fixture".into(),
        transport: McpTransport::Http {
            url: server.url(),
            headers: BTreeMap::new(),
        },
        timeout_ms: 5000,
        oauth: Default::default(),
        sensitive_values: vec!["SENSITIVE_FIXTURE".into()],
        disabled: false,
        exposure: ToolExposure::Codemode,
        tool_exposure: vec![],
        description: String::new(),
    }];
    let connections = McpConnections::new(
        HostCapabilities {
            scripts: true,
            local_processes: false,
        },
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections.reconcile(configs.clone()).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while connections.snapshot()[0].state != McpServerState::Connected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let script = "// @options: {\"timeout_ms\": 500}\nconst first = await tools.mcp__data_source__rows({}); const second = await tools.mcp__data_source__rows({}); text([first.content[0].text.length,second.structuredContent]);";
    let model = Arc::new(ScriptedModel::new(vec![
        call("script", "Codemode", json!({"code":script})),
        text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(McpToolProvider::new(
            connections,
            Arc::new(move || configs.clone()),
        )))
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Read two sets of rows".into())
        .await
        .unwrap();
    let mut approvals = 0;
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => {
                assert_eq!(request.parent_call.as_ref().unwrap().name, "Codemode");
                assert_eq!(request.parent_call.as_ref().unwrap().input["code"], script);
                if approvals == 0 {
                    tokio::time::sleep(Duration::from_millis(600)).await;
                }
                agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap();
                approvals += 1;
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    let result = snapshot
        .operations
        .iter()
        .find(|operation| operation.call_id == "script")
        .unwrap()
        .result
        .as_ref()
        .unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("30000"));
    assert!(result.content.contains("[redacted]"));
    assert_eq!(approvals, 2);
    assert_eq!(server.calls().len(), 2);
    assert!(
        !model.requests.lock().unwrap()[0]
            .tools
            .iter()
            .any(|tool| tool.name.starts_with("mcp__"))
    );
    agent.shutdown().await;
}

struct ReviewTools(std::sync::atomic::AtomicUsize);
#[async_trait::async_trait]
impl tickloop_harness_runtime::host_tools::HostToolProvider for ReviewTools {
    fn call_authority(
        &self,
        _: &str,
        _: &serde_json::Value,
    ) -> Result<Option<tickloop_harness_protocol::HostToolAuthority>, String> {
        Ok(Some(tickloop_harness_protocol::HostToolAuthority::Mcp {
            server_id: "fixture".into(),
            account_id: "fixture".into(),
        }))
    }
    fn tool_authorities(
        &self,
        _: &str,
    ) -> Result<std::collections::BTreeSet<tickloop_harness_protocol::HostToolAuthority>, String>
    {
        Ok(std::collections::BTreeSet::from([
            tickloop_harness_protocol::HostToolAuthority::Mcp {
                server_id: "fixture".into(),
                account_id: "fixture".into(),
            },
        ]))
    }
    fn definitions(&self) -> Vec<tickloop_harness_protocol::ToolDefinition> {
        ["Observe", "Review"]
            .into_iter()
            .map(|name| tickloop_harness_protocol::ToolDefinition {
                name: name.into(),
                description: "Perform a fixture operation".into(),
                input_schema: json!({"type":"object","properties":{}}),
            })
            .collect()
    }
    fn permission(
        &self,
        _: tickloop_harness_protocol::ChatId,
        name: &str,
        _: &serde_json::Value,
    ) -> tickloop_harness_runtime::host_tools::HostToolPermission {
        use tickloop_harness_runtime::host_tools::HostToolPermission;
        if name == "Observe" {
            HostToolPermission::ReadOnly
        } else {
            HostToolPermission::Reviewable
        }
    }
    async fn execute(
        &self,
        _: tickloop_harness_protocol::ChatId,
        _: &str,
        _: serde_json::Value,
        _: tokio_util::sync::CancellationToken,
    ) -> tickloop_harness_protocol::ToolResult {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tickloop_harness_protocol::ToolResult {
            content: "OBSERVED_EVIDENCE".into(),
            is_error: false,
            output_ref: None,
        }
    }
}

#[tokio::test]
async fn auto_denial_ends_the_script_and_returns_the_reason_with_full_review_context() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tickloop_harness_protocol::{EventKind, ExecutionMode};
    use tickloop_harness_runtime::Agent;
    use tickloop_harness_test_support::{
        ScriptedModel, auto_classification, call, config, next_event, text,
    };
    let script = "const prior = await tools.Observe({}); try {await tools.Review({});} catch (error) {await tools.Observe({});} text('ESCAPED');";
    let model = Arc::new(ScriptedModel::new(vec![
        call("script", "Codemode", json!({"code":script})),
        text("done"),
    ]));
    let mut denied = auto_classification(true);
    denied["reason"] = json!("REVIEW_DENIED_FIXTURE");
    let reviewer = Arc::new(ScriptedModel::new(vec![text(&denied.to_string()); 2]));
    let provider = Arc::new(ReviewTools(AtomicUsize::new(0)));
    let agent = Agent::new_embedded(config(), model);
    agent.install_host_tools(provider.clone()).unwrap();
    agent.set_auto_reviewer(reviewer.clone()).await;
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Inspect the request".into())
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(provider.0.load(Ordering::SeqCst), 1);
    let snapshot = agent.snapshot(chat).await.unwrap();
    let result = snapshot
        .operations
        .iter()
        .find(|operation| operation.call_id == "script")
        .unwrap()
        .result
        .as_ref()
        .unwrap();
    assert!(result.is_error);
    assert!(
        result.content.contains("REVIEW_DENIED_FIXTURE"),
        "{}",
        result.content
    );
    assert!(!result.content.contains("ESCAPED"));
    let requests = serde_json::to_string(
        &reviewer
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| &request.messages)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(requests.contains("OBSERVED_EVIDENCE"));
    assert!(requests.contains("Model-authored nested calls"));
    assert!(requests.contains("ESCAPED"));
    let history = agent.approval_history_view(chat).await.unwrap();
    assert!(
        history.records.iter().any(|record| {
            let value = serde_json::to_value(record).unwrap();
            value["tool"] == "Review" && value["outcome"] == "deny" && value["path"] == "auto_llm"
        }),
        "{}",
        serde_json::to_string(&history).unwrap()
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn artifact_mcp_results_keep_the_call_tool_result_shape_in_scripts() {
    use std::sync::Arc;
    use tickloop_harness_protocol::EventKind;
    use tickloop_harness_runtime::{Agent, mcp_tools::McpHostToolProvider};
    use tickloop_harness_test_support::{
        ScriptedModel, artifact_service::ArtifactService, call, config, next_event, text,
    };
    let directory = tempfile::tempdir().unwrap();
    let service = ArtifactService::start().await;
    let provider = Arc::new(McpHostToolProvider::new(
        service.login(directory.path()).await,
        None,
    ));
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "script",
            "Codemode",
            json!({"code":"const result = await tools.ArtifactRead({action:'list'}); text({items:result.structuredContent.items.length, type:result.content[0].type});"}),
        ),
        text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent.install_host_tools(provider).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "List my artifacts".into())
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    let result = snapshot
        .operations
        .iter()
        .find(|operation| operation.call_id == "script")
        .unwrap()
        .result
        .as_ref()
        .unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("\"type\":\"text\""));
    agent.shutdown().await;
}

#[tokio::test]
async fn plan_and_delegation_rules_still_bound_script_calls() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tickloop_harness_protocol::{Decision, EventKind, ExecutionMode, PermissionRule};
    use tickloop_harness_runtime::Agent;
    use tickloop_harness_test_support::{ScriptedModel, call, config, next_event, text};
    for (mode, child, denied) in [
        (ExecutionMode::Plan, false, false),
        (ExecutionMode::FullAccess, true, true),
        (ExecutionMode::FullAccess, true, false),
    ] {
        let mut steps = Vec::new();
        if child {
            steps.push(call(
                "delegate",
                "Subagent",
                json!({"task":"Review through a script"}),
            ));
        }
        steps.push(call(
            "script",
            "Codemode",
            json!({"code":"return await tools.Review({});"}),
        ));
        steps.push(text("done"));
        if child {
            steps.push(text("parent done"));
        }
        let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(steps)));
        let provider = Arc::new(ReviewTools(AtomicUsize::new(0)));
        agent.install_host_tools(provider.clone()).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent.set_execution_mode(chat, mode).await.unwrap();
        if denied {
            agent
                .set_permission_rules(
                    chat,
                    vec![PermissionRule {
                        id: tickloop_ids::generate(),
                        decision: Decision::Deny,
                        tool: "Review".into(),
                        specifier: None,
                        input: None,
                        loop_id: None,
                        persistent: false,
                    }],
                )
                .await
                .unwrap();
        }
        let mut events = agent.subscribe();
        agent
            .submit(chat, "Review through a script".into())
            .await
            .unwrap();
        loop {
            let event = next_event(&mut events).await;
            match event.kind {
                EventKind::Completed { .. } if event.chat_id == chat => break,
                EventKind::PermissionRequested { .. } => panic!("unexpected confirmation"),
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert_eq!(
            provider.0.load(Ordering::SeqCst),
            usize::from(mode == ExecutionMode::FullAccess && !denied),
            "mode={mode:?}, child={child}, denied={denied}, snapshot={}",
            serde_json::to_string(&agent.snapshot(chat).await.unwrap()).unwrap()
        );
        if child {
            assert_eq!(agent.subagents(chat).await.unwrap().len(), 1);
        }
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn script_cannot_call_loop_control_or_nested_execution_tools() {
    use std::sync::Arc;
    use tickloop_harness_protocol::{EventKind, ExecutionMode};
    use tickloop_harness_runtime::Agent;
    use tickloop_harness_test_support::{ScriptedModel, call, config, next_event, text};
    let script = "const names = ['Codemode','ExecuteTools','AskUserQuestion','EnterPlanMode','ExitPlanMode','Subagent','Skill','CreateGoal','GetGoal','UpdateGoal','CronCreate','CronList','CronDelete','CronUpdate','ScheduleWakeup']; return await Promise.all(names.map(async name => {try { await tools[name]({}); return 'CALLED'; } catch(error) { return error instanceof Error; }}));";
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![
            call("script", "Codemode", json!({"code":script})),
            text("done"),
        ])),
    );
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Inspect script restrictions".into())
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::PermissionRequested { .. } => panic!("a forbidden call requested approval"),
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    let result = snapshot
        .operations
        .iter()
        .find(|operation| operation.call_id == "script")
        .unwrap()
        .result
        .as_ref()
        .unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert!(!result.content.contains("CALLED"));
    assert!(
        result
            .content
            .contains(&serde_json::to_string(&vec![true; 15]).unwrap())
    );
    assert!(agent.subagents(chat).await.unwrap().is_empty());
    agent.shutdown().await;
}

pub(crate) async fn slow_mcp_provider() -> (
    tickloop_harness_test_support::mcp_service::McpService,
    std::sync::Arc<tickloop_harness_runtime::mcp::McpToolProvider>,
) {
    use std::{collections::BTreeMap, sync::Arc, time::Duration};
    use tickloop_harness_protocol::mcp::{McpServerConfig, McpTransport, ToolExposure};
    use tickloop_harness_runtime::mcp::{
        HostCapabilities, McpConnections, McpServerState, McpToolProvider,
    };
    let server = tickloop_harness_test_support::mcp_service::McpService::start().await;
    server.set_tools(json!([{"name":"write","inputSchema":{"type":"object","properties":{}}}]));
    server.set_result(json!({"content":[{"type":"text","text":"WROTE"}]}));
    let configs = vec![McpServerConfig {
        name: "slow".into(),
        source: "fixture".into(),
        transport: McpTransport::Http {
            url: server.url(),
            headers: BTreeMap::new(),
        },
        timeout_ms: 60000,
        oauth: Default::default(),
        sensitive_values: vec![],
        disabled: false,
        exposure: ToolExposure::Codemode,
        tool_exposure: vec![],
        description: String::new(),
    }];
    let connections = McpConnections::new(
        HostCapabilities {
            scripts: true,
            local_processes: false,
        },
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections.reconcile(configs.clone()).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while connections.snapshot()[0].state != McpServerState::Connected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    (
        server,
        Arc::new(McpToolProvider::new(
            connections,
            Arc::new(move || configs.clone()),
        )),
    )
}

#[tokio::test]
async fn esc_and_stop_settle_dispatched_script_calls_as_unknown_without_retry() {
    use std::{sync::Arc, time::Duration};
    use tickloop_harness_protocol::{ExecutionMode, OperationState};
    use tickloop_harness_runtime::Agent;
    use tickloop_harness_test_support::{ScriptedModel, call, config};
    for stop in [false, true] {
        let (server, provider) = slow_mcp_provider().await;
        server.set_delay(Duration::from_secs(30));
        let agent = Agent::new_embedded(
            config(),
            Arc::new(ScriptedModel::new(vec![call(
                "script",
                "Codemode",
                json!({"code":"await tools.mcp__slow__write({});"}),
            )])),
        );
        agent.install_host_tools(provider).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::FullAccess)
            .await
            .unwrap();
        agent.submit(chat, "Write once".into()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while server.calls().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if stop {
            agent.stop(chat).await.unwrap();
        } else {
            agent.interrupt(chat).await.unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = agent.snapshot(chat).await.unwrap();
                if snapshot.operations.iter().any(|operation| {
                    operation.parent_call_id.as_deref() == Some("script")
                        && operation.state == OperationState::Unknown
                        && operation.result.is_some()
                }) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dispatched call must settle as unknown");
        assert_eq!(server.calls().len(), 1);
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn human_denial_throws_an_error_the_script_can_handle() {
    use std::sync::Arc;
    use tickloop_harness_protocol::{EventKind, ExecutionMode};
    use tickloop_harness_runtime::Agent;
    use tickloop_harness_test_support::{ScriptedModel, call, config, next_event, text};
    let (server, provider) = slow_mcp_provider().await;
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![
            call(
                "script",
                "Codemode",
                json!({"code":"try {await tools.mcp__slow__write({});} catch(error) {text(error instanceof Error);} return 'handled';"}),
            ),
            text("done"),
        ])),
    );
    agent.install_host_tools(provider).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Try the request".into()).await.unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => {
                agent
                    .answer_permission(chat, request.id, false)
                    .await
                    .unwrap();
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    let result = snapshot
        .operations
        .iter()
        .find(|operation| operation.call_id == "script")
        .unwrap()
        .result
        .as_ref()
        .unwrap();
    assert!(!result.is_error, "{}", result.content);
    assert!(result.content.contains("true\nhandled"));
    assert!(server.calls().is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn normalized_name_collisions_preserve_unrelated_tools_and_exact_names() {
    let result = run(
        "text(await tools.Read({})); text(await tools['a-b']({})); text(await tools.a_b({}));",
        Options::default(),
        vec!["a-b".into(), "a_b".into(), "Read".into()],
        |name, _, _| async move { Ok(json!(name)) },
    )
    .await;
    assert_eq!(result.error, None);
    assert_eq!(result.output, "Read\na-b\na_b");
}

#[tokio::test]
async fn cancellation_settlement_has_a_bound_even_for_unresponsive_callbacks() {
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        run(
            "// @options: {\"timeout_ms\": 20}\nawait tools.wait({});",
            Options::default(),
            vec!["wait".into()],
            |_, _, _| std::future::pending::<Result<serde_json::Value, String>>(),
        ),
    )
    .await
    .expect("an unresponsive callback must not defeat the script deadline");
    assert!(result.error.is_some());
}

#[tokio::test]
async fn retained_images_obey_the_host_memory_budget() {
    use base64::Engine;
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(1, 1)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    let data = base64::engine::general_purpose::STANDARD.encode(bytes.into_inner());
    let result = run(
        &format!("for (let i=0;i<5000;i++) image('data:image/png;base64,{data}');"),
        Options {
            memory_limit_bytes: 512 * 1024,
            ..Default::default()
        },
        vec![],
        |_, _, _| async { unreachable!() },
    )
    .await;
    assert!(result.error.is_some());
    assert!(result.images.len() < 5000);
}

#[tokio::test]
async fn tool_bridge_bounds_concurrency_without_losing_queued_calls() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let seen = peak.clone();
    let result = run(
        "return (await Promise.all(Array.from({length:40},()=>tools.read({})))).length;",
        Options::default(),
        vec!["read".into()],
        move |_, _, _| {
            let active = active.clone();
            let peak = peak.clone();
            async move {
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(count, Ordering::SeqCst);
                tokio::task::yield_now().await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(json!(true))
            }
        },
    )
    .await;
    assert_eq!(result.error, None);
    assert_eq!(result.output, "40");
    assert!(seen.load(Ordering::SeqCst) <= 10);
}

struct ConfirmationGate(tokio::sync::Notify);
#[async_trait::async_trait]
impl tickloop_harness_runtime::host_tools::HostToolProvider for ConfirmationGate {
    fn definitions(&self) -> Vec<tickloop_harness_protocol::ToolDefinition> {
        ["Pending", "Gate"]
            .into_iter()
            .map(|name| tickloop_harness_protocol::ToolDefinition {
                name: name.into(),
                description: "Controlled fixture call".into(),
                input_schema: json!({"type":"object","properties":{}}),
            })
            .collect()
    }
    fn permission(
        &self,
        _: tickloop_harness_protocol::ChatId,
        name: &str,
        _: &serde_json::Value,
    ) -> tickloop_harness_runtime::host_tools::HostToolPermission {
        use tickloop_harness_runtime::host_tools::HostToolPermission;
        if name == "Pending" {
            HostToolPermission::ThirdParty
        } else {
            HostToolPermission::ReadOnly
        }
    }
    async fn execute(
        &self,
        _: tickloop_harness_protocol::ChatId,
        name: &str,
        _: serde_json::Value,
        _: tokio_util::sync::CancellationToken,
    ) -> tickloop_harness_protocol::ToolResult {
        assert_eq!(name, "Gate", "unconfirmed call must never dispatch");
        self.0.notified().await;
        tickloop_harness_protocol::ToolResult {
            content: "ready".into(),
            is_error: false,
            output_ref: None,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn successful_script_end_with_pending_confirmation_clears_it_and_records_not_executed() {
    use std::sync::Arc;
    use tickloop_harness_protocol::{EventKind, ExecutionMode, OperationState};
    use tickloop_harness_runtime::Agent;
    use tickloop_harness_test_support::{ScriptedModel, call, config, next_event, text};
    let provider = Arc::new(ConfirmationGate(tokio::sync::Notify::new()));
    let agent = Agent::new_embedded(
        config(),
        Arc::new(ScriptedModel::new(vec![
            call(
                "script",
                "Codemode",
                json!({"code":"tools.Pending({}); await tools.Gate({}); return 'done';"}),
            ),
            text("finished"),
        ])),
    );
    agent.install_host_tools(provider.clone()).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Run the script".into()).await.unwrap();
    let mut pending = None;
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => {
                pending = Some(request.id);
                provider.0.notify_one();
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(snapshot.permissions.is_empty());
    let child = snapshot
        .operations
        .iter()
        .find(|operation| operation.name == "Pending")
        .unwrap();
    assert_eq!(child.state, OperationState::Failed);
    assert!(child.result.as_ref().unwrap().is_error);
    assert!(
        agent
            .answer_permission(chat, pending.unwrap(), true)
            .await
            .is_err()
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn first_script_can_spill_output_before_any_filesystem_tool_runs() {
    use std::sync::Arc;
    use tickloop_harness_protocol::EventKind;
    use tickloop_harness_runtime::Agent;
    use tickloop_harness_test_support::{ScriptedModel, call, config, next_event, text};
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "script",
            "Codemode",
            json!({"code":"// @options: {\"max_output_tokens\":10}\ntext('spill'.repeat(1000));"}),
        ),
        text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Produce the sample output".into())
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    let result = snapshot
        .operations
        .iter()
        .find(|operation| operation.call_id == "script")
        .unwrap()
        .result
        .as_ref()
        .unwrap();
    assert!(!result.is_error, "{}", result.content);
    let path = result
        .content
        .split("Full output saved to: ")
        .nth(1)
        .unwrap()
        .lines()
        .next()
        .unwrap();
    assert!(path.contains(&chat.uuid().to_string()));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "spill".repeat(1000));
    agent.shutdown().await;
}

#[tokio::test]
async fn auto_reviews_script_mcp_calls_against_the_script_and_earlier_redacted_results() {
    use std::{collections::BTreeMap, sync::Arc, time::Duration};
    use tickloop_harness_protocol::{
        EventKind, ExecutionMode,
        mcp::{McpServerConfig, McpTransport, ToolExposure},
    };
    use tickloop_harness_runtime::{
        Agent,
        mcp::{HostCapabilities, McpConnections, McpServerState, McpToolProvider},
    };
    use tickloop_harness_test_support::{
        ScriptedModel, auto_classification, call, config, mcp_service::McpService, next_event, text,
    };
    let server = McpService::start().await;
    server.set_tools(json!([{"name":"rows","inputSchema":{"type":"object","properties":{}},"annotations":{"readOnlyHint":true}}]));
    server
        .set_result(json!({"content":[{"type":"text","text":"ROWS_EVIDENCE SENSITIVE_FIXTURE"}]}));
    let configs = vec![McpServerConfig {
        name: "data-source".into(),
        source: "fixture".into(),
        transport: McpTransport::Http {
            url: server.url(),
            headers: BTreeMap::new(),
        },
        timeout_ms: 5000,
        oauth: Default::default(),
        sensitive_values: vec!["SENSITIVE_FIXTURE".into()],
        disabled: false,
        exposure: ToolExposure::Codemode,
        tool_exposure: vec![],
        description: String::new(),
    }];
    let connections = McpConnections::new(
        HostCapabilities {
            scripts: true,
            local_processes: false,
        },
        BTreeMap::new(),
        BTreeMap::new(),
    );
    connections.reconcile(configs.clone()).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while connections.snapshot()[0].state != McpServerState::Connected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let script = "await tools.mcp__data_source__rows({}); await tools.mcp__data_source__rows({}); text('SCRIPT_FINISHED');";
    let model = Arc::new(ScriptedModel::new(vec![
        call("script", "Codemode", json!({"code":script})),
        text("done"),
    ]));
    let mut rejection = auto_classification(true);
    rejection["reason"] = json!("SCRIPT_REVIEW_DENIED");
    let reviewer = Arc::new(ScriptedModel::new(vec![
        text(&auto_classification(false).to_string()),
        text(&rejection.to_string()),
        text(&rejection.to_string()),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.set_auto_reviewer(reviewer.clone()).await;
    agent
        .install_host_tools(Arc::new(McpToolProvider::new(
            connections,
            Arc::new(move || configs.clone()),
        )))
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Read the rows".into()).await.unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { .. } => panic!("Auto review decides these calls"),
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(server.calls().len(), 1);
    let snapshot = agent.snapshot(chat).await.unwrap();
    let result = snapshot
        .operations
        .iter()
        .find(|operation| operation.call_id == "script")
        .unwrap()
        .result
        .as_ref()
        .unwrap();
    assert!(result.is_error);
    assert!(
        result.content.contains("SCRIPT_REVIEW_DENIED"),
        "{}",
        result.content
    );
    assert!(!result.content.contains("SCRIPT_FINISHED"));
    let reviews = reviewer.requests.lock().unwrap();
    assert_eq!(reviews.len(), 3);
    let first = serde_json::to_string(&reviews[0].messages).unwrap();
    assert!(first.contains("Model-authored nested calls"));
    assert!(
        first.contains("SCRIPT_FINISHED"),
        "the whole script is evidence"
    );
    let second = serde_json::to_string(&reviews[1].messages).unwrap();
    assert!(
        second.contains("ROWS_EVIDENCE"),
        "earlier results are evidence"
    );
    let every_review = reviews
        .iter()
        .map(|review| serde_json::to_string(&review.messages).unwrap())
        .collect::<String>();
    assert!(!every_review.contains("SENSITIVE_FIXTURE"));
    drop(reviews);
    agent.shutdown().await;
}
