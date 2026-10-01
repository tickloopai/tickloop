mod support;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use serde_json::json;
use support::{ScriptedModel, call, config, next_event, text, transcript_file};
use tickloop_agent::{
    model::{Content, Message, ModelRequest, Role},
    permissions::{Decision, ExecutionMode, PermissionRule},
    runtime::{
        Agent, BranchRequest, ChatId, DirectedSummaryRequest, EventKind, LoopId, Receipt,
        SummaryDirection,
    },
};
use uuid::Uuid;

const PERSISTED_OUTPUT_TAG: &str = "<persisted-output>";
const PREVIEW_SIZE: usize = 2000;

async fn complete_turn(agent: &Agent, chat: ChatId, goal: Option<LoopId>, prompt: &str) -> Receipt {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, goal, prompt.into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => {
                panic!("unexpected permission request for {}", request.call_id)
            }
            EventKind::Completed { .. } => return receipt,
            EventKind::Failed(error) => panic!("turn failed: {error}"),
            _ => {}
        }
    }
}

/// 发给模型的对话消息（去掉每次请求都会重建的宿主 system 消息），逐条序列化用于逐字节比较。
fn conversation(request: &ModelRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|message| message.role != Role::System)
        .map(|message| serde_json::to_string(message).unwrap())
        .collect()
}

fn tool_results(request: &ModelRequest) -> Vec<(String, String)> {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                ..
            } => Some((tool_use_id.clone(), content.text().into_owned())),
            _ => None,
        })
        .collect()
}

fn requests(model: &ScriptedModel) -> Vec<ModelRequest> {
    model.requests.lock().unwrap().clone()
}

/// Claude Code `formatFileSize`。
fn format_file_size(size: usize) -> String {
    let kilobytes = size as f64 / 1024.0;
    if kilobytes < 1.0 {
        return format!("{size} bytes");
    }
    let trim = |value: f64, unit: &str| {
        let text = format!("{value:.1}");
        format!("{}{unit}", text.strip_suffix(".0").unwrap_or(&text))
    };
    if kilobytes < 1024.0 {
        return trim(kilobytes, "KB");
    }
    trim(kilobytes / 1024.0, "MB")
}

/// Claude Code `buildLargeToolResultMessage` 与 `generatePreview`（测试内容为 ASCII）。
fn expected_replacement(path: &Path, content: &str) -> String {
    let (preview, has_more) = if content.len() <= PREVIEW_SIZE {
        (content, false)
    } else {
        let truncated = &content[..PREVIEW_SIZE];
        let cut = match truncated.rfind('\n') {
            Some(newline) if newline > PREVIEW_SIZE / 2 => newline,
            _ => PREVIEW_SIZE,
        };
        (&content[..cut], true)
    };
    format!(
        "{PERSISTED_OUTPUT_TAG}\nOutput too large ({}). Full output saved to: {}\n\nPreview (first {}):\n{preview}{}</persisted-output>",
        format_file_size(content.len()),
        path.display(),
        format_file_size(PREVIEW_SIZE),
        if has_more { "\n...\n" } else { "\n" },
    )
}

/// Transcript 同级的 Chat 目录（Agent 使用 canonical 根目录，替换文本中的路径同样是 canonical）。
fn chat_directory(root: &Path, chat: ChatId) -> PathBuf {
    transcript_file(root, chat)
        .canonicalize()
        .unwrap()
        .with_extension("")
}

fn snapshot_tool_result(messages: &[Message], call_id: &str) -> String {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == call_id => Some(content.text().into_owned()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("tool result {call_id} is missing"))
}

#[tokio::test]
async fn a_large_search_result_is_persisted_and_the_next_request_is_identical_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let lines = (0..2000)
        .map(|index| format!("delivery item {index:04} is waiting at the dock"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(directory.path().join("notes.txt"), lines).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "search-1",
            "Grep",
            json!({"path":"notes.txt","pattern":"delivery","output_mode":"content","head_limit":0}),
        ),
        text("Found every delivery."),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, None, "Find the deliveries").await;
    agent.flush_transcripts().await;

    let persisted_path = chat_directory(&root, chat).join("tool-results/search-1.txt");
    let persisted = std::fs::read_to_string(&persisted_path).unwrap();
    assert_eq!(persisted.lines().count(), 2000);
    assert!(persisted.starts_with("1:delivery item 0000 is waiting at the dock\n"));
    let before_restart = requests(&model)[1].clone();
    let (_, replacement) = tool_results(&before_restart).pop().unwrap();
    assert_eq!(
        replacement,
        expected_replacement(&persisted_path, &persisted)
    );
    assert_eq!(
        snapshot_tool_result(&agent.snapshot(chat).await.unwrap().messages, "search-1"),
        replacement,
        "the conversation keeps the replacement the model saw"
    );
    agent.shutdown().await;
    drop(agent);

    let resumed_model = Arc::new(ScriptedModel::new(vec![text("Still there.")]));
    let resumed = Agent::open_embedded(root, config(), resumed_model.clone()).unwrap();
    resumed.resume_chat(chat).await.unwrap();
    complete_turn(&resumed, chat, None, "Anything else?").await;
    let after_restart = conversation(&requests(&resumed_model)[0]);
    assert_eq!(after_restart[..3], conversation(&before_restart)[..]);
    resumed.shutdown().await;
}

/// 在工作目录中生成五个子目录，每个子目录的 Glob 结果约 44–48K 字符：单个结果都低于
/// 50K 的单工具阈值，合计超过 200K 的单条消息预算。Glob 最多列出 100 个文件，路径相对
/// 工作目录显示，所以每个子目录恰好 100 个文件，用长目录名与文件名凑足每行长度。
fn budget_fixture(workdir: &Path) -> Vec<Content> {
    const LISTED_FILES: usize = 100;
    let mut calls = Vec::new();
    for (index, name) in ["a", "b", "c", "d", "e"].into_iter().enumerate() {
        let relative_directory = Path::new(name).join("n".repeat(240));
        let nested = workdir.join(&relative_directory);
        std::fs::create_dir_all(&nested).unwrap();
        let prefix_length = relative_directory.as_os_str().len() + "/".len();
        let line_length = (44_000 + index * 1000) / LISTED_FILES;
        let file_name_length = line_length - "\n".len() - prefix_length;
        let number_width = file_name_length - ".txt".len();
        for file in 0..LISTED_FILES {
            std::fs::write(nested.join(format!("{file:0>number_width$}.txt")), "").unwrap();
        }
        calls.push(Content::ToolUse {
            id: format!("glob-{name}"),
            name: "Glob".into(),
            input: json!({"path": name, "pattern": "*"}),
        });
    }
    calls
}

async fn assert_resumed_request_matches(
    root: &Path,
    chat: ChatId,
    expected_prefix: &[String],
    resume: bool,
) {
    let model = Arc::new(ScriptedModel::new(vec![text("Still listed.")]));
    let agent = Agent::open_embedded(root.to_path_buf(), config(), model.clone()).unwrap();
    if resume {
        agent.resume_chat(chat).await.unwrap();
    }
    complete_turn(&agent, chat, None, "Continue after restart").await;
    let sent = conversation(&requests(&model)[0]);
    assert!(sent.len() >= expected_prefix.len(), "{sent:?}");
    // 对话可能含数 MB 的提示，只报告第一处不同消息的开头。
    const REPORTED_PREFIX: usize = 400;
    if let Some(index) =
        (0..expected_prefix.len()).find(|index| sent[*index] != expected_prefix[*index])
    {
        let excerpt = |message: &str| message.chars().take(REPORTED_PREFIX).collect::<String>();
        panic!(
            "message {index} differs:\n  sent: {}\n  expected: {}",
            excerpt(&sent[index]),
            excerpt(&expected_prefix[index])
        );
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn message_budget_replacements_survive_resume_fork_and_branch_byte_for_byte() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let calls = budget_fixture(directory.path());
    let model = Arc::new(ScriptedModel::new(vec![
        calls,
        text("Listed every directory."),
        text("Nothing changed."),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete_turn(&agent, chat, None, "List the directories").await;

    let first = requests(&model)[1].clone();
    let results = tool_results(&first);
    let replaced = results
        .iter()
        .filter(|(_, content)| content.starts_with(PERSISTED_OUTPUT_TAG))
        .map(|(id, _)| id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(replaced, ["glob-e"], "only the largest result is replaced");
    let messages = agent.snapshot(chat).await.unwrap().messages;
    let original = snapshot_tool_result(&messages, "glob-e");
    assert!(original.len() < 50_000 && !original.starts_with(PERSISTED_OUTPUT_TAG));
    agent.flush_transcripts().await;
    let persisted_path = chat_directory(&root, chat).join("tool-results/glob-e.txt");
    assert_eq!(std::fs::read_to_string(&persisted_path).unwrap(), original);
    let replacement = &results.iter().find(|(id, _)| id == "glob-e").unwrap().1;
    assert_eq!(
        replacement,
        &expected_replacement(&persisted_path, &original)
    );

    let expected_prefix = conversation(&first);
    let mut expected_with_answer = expected_prefix.clone();
    expected_with_answer.push(
        serde_json::to_string(&Message {
            role: Role::Assistant,
            content: text("Listed every directory."),
        })
        .unwrap(),
    );
    complete_turn(&agent, chat, None, "Anything new?").await;
    assert_eq!(
        conversation(&requests(&model)[2])[..expected_with_answer.len()],
        expected_with_answer[..]
    );
    agent.shutdown().await;
    drop(agent);

    assert_resumed_request_matches(&root, chat, &expected_with_answer, true).await;

    let copying = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            call(
                "read-persisted",
                "Read",
                json!({"file_path": persisted_path, "limit": 64}),
            ),
            text("Read the saved output."),
        ])),
    )
    .unwrap();
    let copy = copying.resume_as_new_chat(chat).await.unwrap().id;
    complete_turn(&copying, copy, None, "Read the saved listing").await;
    copying.shutdown().await;
    drop(copying);
    assert_resumed_request_matches(&root, copy, &expected_with_answer, true).await;

    let branching = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("Branch listed.")])),
    )
    .unwrap();
    branching.resume_chat(chat).await.unwrap();
    let branch = branching
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: chat,
            source_loop_id: None,
            name: None,
        })
        .await
        .unwrap()
        .child_chat_id;
    branching.shutdown().await;
    drop(branching);
    assert_resumed_request_matches(&root, branch, &expected_with_answer, true).await;
}

/// 超过 5MB 的 Transcript 做定向摘要后，加载时跳过新边界之前的内容；保留消息中被预算替换的
/// 结果的替换记录随边界重写到边界之后，重启、`--fork-session` 式恢复为新 Chat（新文件同样超过 5MB）与 Branch 后下一轮
/// 请求仍与摘要进程逐字节一致。
#[tokio::test]
async fn budget_replacements_preserved_by_a_directed_summary_of_a_large_transcript_survive_restart_new_chat_resume_and_branch()
 {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let calls = budget_fixture(directory.path());
    let large_prompt = format!("Large prompt {}", "x".repeat(6 * 1024 * 1024));
    let model = Arc::new(ScriptedModel::new(vec![
        text("Early answer."),
        calls,
        text("Listed every directory."),
        text("Large answer."),
        text("Summary of the early turn."),
        text("Nothing changed."),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent.set_context_window(chat, 50_000_000).await.unwrap();
    complete_turn(&agent, chat, None, "Early prompt").await;
    complete_turn(&agent, chat, None, "List the directories").await;
    complete_turn(&agent, chat, None, &large_prompt).await;
    let listing_point = agent.snapshot(chat).await.unwrap().rewind_points()[1].clone();
    agent
        .summarize_checkpoint(
            chat,
            DirectedSummaryRequest {
                message_id: listing_point.message_id,
                direction: SummaryDirection::UpToHere,
                guidance: None,
            },
        )
        .await
        .unwrap();
    complete_turn(&agent, chat, None, "Anything new?").await;
    let summarized_request = requests(&model).last().unwrap().clone();
    let replaced = tool_results(&summarized_request)
        .into_iter()
        .filter(|(_, content)| content.starts_with(PERSISTED_OUTPUT_TAG))
        .map(|(id, _)| id)
        .collect::<Vec<_>>();
    assert_eq!(replaced, ["glob-e"]);
    let mut expected_prefix = conversation(&summarized_request);
    expected_prefix.push(
        serde_json::to_string(&Message {
            role: Role::Assistant,
            content: text("Nothing changed."),
        })
        .unwrap(),
    );
    agent.shutdown().await;
    drop(agent);
    let large_transcript = 5 * 1024 * 1024;
    assert!(
        std::fs::metadata(transcript_file(&root, chat))
            .unwrap()
            .len()
            > large_transcript
    );

    assert_resumed_request_matches(&root, chat, &expected_prefix, true).await;

    let copying = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let copy = copying.resume_as_new_chat(chat).await.unwrap().id;
    copying.shutdown().await;
    drop(copying);
    assert!(
        std::fs::metadata(transcript_file(&root, copy))
            .unwrap()
            .len()
            > large_transcript
    );
    assert_resumed_request_matches(&root, copy, &expected_prefix, true).await;

    let branching = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    branching.resume_chat(chat).await.unwrap();
    let branch = branching
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: chat,
            source_loop_id: None,
            name: None,
        })
        .await
        .unwrap()
        .child_chat_id;
    branching.shutdown().await;
    drop(branching);
    assert_resumed_request_matches(&root, branch, &expected_prefix, true).await;
}

#[tokio::test]
async fn background_shell_output_of_concurrent_agents_is_written_under_each_chat() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let first = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let second = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::new())));
    let mut outputs = Vec::new();
    for (agent, marker) in [(&first, "first-output"), (&second, "second-output")] {
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let receipt = agent
            .submit_user_shell_for_goal(chat, None, format!("printf {marker}"), true)
            .await
            .unwrap();
        outputs.push((agent, chat, receipt.command_id, marker));
    }
    let project_temp_root = user_temp_directory();
    let mut paths = Vec::new();
    for (agent, chat, command_id, marker) in outputs {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let path = loop {
            let snapshot = agent.snapshot(chat).await.unwrap();
            let record = snapshot
                .user_shells
                .iter()
                .find(|record| record.id == command_id)
                .unwrap()
                .clone();
            if record.output_bytes as usize == marker.len() {
                break record.output_ref.expect("output file");
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "shell did not finish"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(
            path.file_name().unwrap().to_string_lossy(),
            format!("{command_id}.output")
        );
        let chat_directory = path.parent().unwrap().parent().unwrap();
        assert_eq!(path.parent().unwrap().file_name().unwrap(), "tasks");
        assert_eq!(
            chat_directory.file_name().unwrap().to_string_lossy(),
            chat.to_string()
        );
        assert_eq!(
            chat_directory.parent().unwrap().parent().unwrap(),
            project_temp_root
        );
        assert!(!path.starts_with(directory.path().canonicalize().unwrap()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), marker);
        paths.push((chat, path));
    }
    assert_ne!(paths[0].1, paths[1].1);
    first.flush_transcripts().await;
    first.shutdown().await;
    second.shutdown().await;
    let transcript_project = transcript_file(&root, paths[0].0);
    let project_name = transcript_project.parent().unwrap().file_name().unwrap();
    for (_, path) in &paths {
        assert_eq!(
            path.parent()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .file_name()
                .unwrap(),
            project_name
        );
    }
    assert!(!directory.path().join(".tickloop").exists());
}

#[tokio::test]
async fn file_change_reviews_are_kept_in_the_chat_directory() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "write-1",
            "Write",
            json!({"file_path":"report.txt","content":"quarterly report\n"}),
        ),
        text("Wrote the report."),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Allow,
                tool: "Write".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    complete_turn(&agent, chat, None, "Write the report").await;
    agent.flush_transcripts().await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    let review = snapshot
        .operations
        .iter()
        .find_map(|operation| operation.file_change.clone())
        .expect("a reviewed file change");
    assert!(
        review
            .artifact
            .path
            .starts_with(chat_directory(&root, chat).join("file-changes")),
        "{}",
        review.artifact.path.display()
    );
    let loaded = tickloop_agent::file_change::load_file_change_review(&review.artifact).unwrap();
    assert_eq!(loaded.summary(), review.summary);
    assert!(!directory.path().join(".tickloop").exists());
    agent.shutdown().await;
}

fn persisted_path(replacement: &str) -> PathBuf {
    let (_, rest) = replacement
        .split_once("Full output saved to: ")
        .unwrap_or_else(|| panic!("not a persisted replacement: {replacement:.200}"));
    PathBuf::from(rest.split_once("\n\nPreview").unwrap().0)
}

#[tokio::test]
async fn large_results_with_repeated_or_unsafe_call_ids_are_still_persisted_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    for (file, word) in [("north.txt", "north"), ("south.txt", "south")] {
        let lines = (0..2000)
            .map(|index| format!("{word} delivery item {index:04} is waiting at the dock"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(directory.path().join(file), lines).unwrap();
    }
    let grep = |file: &str| json!({"path": file, "pattern": "delivery", "output_mode": "content", "head_limit": 0});
    let model = Arc::new(ScriptedModel::new(vec![
        call("lookup", "Grep", grep("north.txt")),
        text("North listed."),
        call("lookup", "Grep", grep("south.txt")),
        text("South listed."),
        call("../lookup:1", "Grep", grep("north.txt")),
        text("North listed again."),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    for prompt in ["List north", "List south", "List north again"] {
        complete_turn(&agent, chat, None, prompt).await;
    }
    agent.flush_transcripts().await;

    let snapshot = agent.snapshot(chat).await.unwrap();
    for (_, content) in tool_results(&ModelRequest {
        history_sources: Vec::new(),
        messages: snapshot.messages.clone(),
        ..requests(&model)[0].clone()
    }) {
        assert!(
            content.starts_with(PERSISTED_OUTPUT_TAG),
            "the conversation keeps a raw result of {} bytes",
            content.len()
        );
    }
    let results = tool_results(&requests(&model)[5]);
    assert_eq!(results.len(), 3);
    let mut paths = Vec::new();
    for (index, (_, replacement)) in results.iter().enumerate() {
        assert!(
            replacement.starts_with(PERSISTED_OUTPUT_TAG),
            "result {index} reached the model without being persisted ({} bytes)",
            replacement.len()
        );
        let path = persisted_path(replacement);
        assert!(path.starts_with(chat_directory(&root, chat).join("tool-results")));
        let persisted = std::fs::read_to_string(&path).unwrap();
        assert_eq!(replacement, &expected_replacement(&path, &persisted));
        paths.push((path, persisted));
    }
    assert_eq!(paths[0].0.file_name().unwrap(), "lookup.txt");
    assert_ne!(
        paths[0].0, paths[1].0,
        "a repeated id must not reuse a different result"
    );
    assert!(paths[1].1.contains("south delivery"));
    assert!(paths[2].1.contains("north delivery"));
    let unsafe_name = paths[2]
        .0
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        unsafe_name
            .trim_end_matches(".txt")
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "{unsafe_name}"
    );
    let before_restart = conversation(&requests(&model)[5]);
    agent.shutdown().await;
    drop(agent);

    let resumed_model = Arc::new(ScriptedModel::new(vec![text("Still there.")]));
    let resumed = Agent::open_embedded(root, config(), resumed_model.clone()).unwrap();
    resumed.resume_chat(chat).await.unwrap();
    complete_turn(&resumed, chat, None, "Anything else?").await;
    let after_restart = conversation(&requests(&resumed_model)[0]);
    assert_eq!(
        after_restart[..before_restart.len() + 1],
        [
            before_restart.clone(),
            vec![
                serde_json::to_string(&Message {
                    role: Role::Assistant,
                    content: text("North listed again."),
                })
                .unwrap()
            ]
        ]
        .concat()[..]
    );
    resumed.shutdown().await;
}

/// 与 Agent 相同的用户临时目录：`TICKLOOP_TMPDIR`（未设置时 `/tmp`）解析符号链接后的
/// `tickloop-<uid>`。
fn user_temp_directory() -> PathBuf {
    let base = std::env::var_os("TICKLOOP_TMPDIR")
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    base.canonicalize()
        .unwrap()
        .join(format!("tickloop-{}", unsafe { libc::getuid() }))
}

/// 与 Agent 相同的项目临时目录：`<用户临时目录>/<项目目录名>`。
fn project_temp_directory(workdir: &Path) -> PathBuf {
    use std::os::unix::fs::DirBuilderExt;
    let user_directory = user_temp_directory();
    match std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&user_directory)
    {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => panic!("cannot create {}: {error}", user_directory.display()),
    }
    let name = workdir
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    user_directory.join(name)
}

/// 测试结束时移除预先布置在系统临时目录中的项目临时目录（符号链接只删除链接本身）。
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        match std::fs::symlink_metadata(&self.0) {
            Ok(metadata) if metadata.is_dir() => {
                let _ = std::fs::remove_dir_all(&self.0);
            }
            Ok(_) => {
                let _ = std::fs::remove_file(&self.0);
            }
            Err(_) => {}
        }
    }
}

/// 模拟共享 `/tmp` 上的另一用户预先布置项目临时目录：`tamper` 布置目录并返回要读取的文件。
/// 该文件的只读访问必须请求授权，后台任务输出必须拒绝写入该目录。
async fn assert_untrusted_project_temp_directory(
    tamper: impl FnOnce(&Path, &Path) -> PathBuf,
    expected_reason: &str,
) {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let project_temp = project_temp_directory(directory.path());
    let _cleanup = RemoveOnDrop(project_temp.clone());
    let secret = tamper(&project_temp, outside.path());

    let model = Arc::new(ScriptedModel::new(vec![
        call("read-secret", "Read", json!({"file_path": secret})),
        text("Done."),
    ]));
    let agent = Agent::open_embedded(directory.path().join("root"), config(), model).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "Read the secret".into())
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => {
                assert_eq!(request.call_id, "read-secret");
                agent
                    .answer_permission(chat, request.id, false)
                    .await
                    .unwrap();
                break;
            }
            EventKind::ToolCompleted { .. } | EventKind::Completed { .. } => {
                panic!("a file behind an untrusted temporary directory was read without permission")
            }
            EventKind::Failed(error) => panic!("turn failed: {error}"),
            _ => {}
        }
    }

    let shell_chat = agent.create_chat(directory.path().into()).await.unwrap();
    let shell = agent
        .submit_user_shell_for_goal(shell_chat, None, "printf leaked".into(), true)
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let record = loop {
        let snapshot = agent.snapshot(shell_chat).await.unwrap();
        let record = snapshot
            .user_shells
            .iter()
            .find(|record| record.id == shell.command_id)
            .unwrap()
            .clone();
        if record.state == tickloop_agent::runtime::UserShellState::Failed {
            break record;
        }
        assert!(
            !matches!(
                record.state,
                tickloop_agent::runtime::UserShellState::Completed
                    | tickloop_agent::runtime::UserShellState::Background
            ),
            "task output was written into an untrusted directory: {record:?}"
        );
        assert!(tokio::time::Instant::now() < deadline, "shell did not fail");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let reason = record.reason.unwrap_or_default();
    assert!(
        reason.contains(&project_temp.display().to_string()) && reason.contains(expected_reason),
        "{reason}"
    );
    assert!(!outside.path().join(shell_chat.to_string()).exists());
    agent.shutdown().await;
}

#[tokio::test]
async fn a_symlinked_project_temp_directory_is_not_trusted() {
    assert_untrusted_project_temp_directory(
        |project_temp, outside| {
            let secret = outside.join("secret.txt");
            std::fs::write(&secret, "classified").unwrap();
            std::os::unix::fs::symlink(outside, project_temp).unwrap();
            secret
        },
        "symbolic link",
    )
    .await;
}

#[tokio::test]
async fn a_project_temp_directory_accessible_to_other_users_is_not_trusted() {
    use std::os::unix::fs::PermissionsExt;
    assert_untrusted_project_temp_directory(
        |project_temp, _| {
            std::fs::create_dir(project_temp).unwrap();
            std::fs::set_permissions(project_temp, std::fs::Permissions::from_mode(0o755)).unwrap();
            let secret = project_temp.join("secret.txt");
            std::fs::write(&secret, "classified").unwrap();
            secret
        },
        "group or other",
    )
    .await;
}

#[tokio::test]
async fn task_output_in_the_project_temp_directory_is_readable_without_permission() {
    let directory = tempfile::tempdir().unwrap();
    let _cleanup = RemoveOnDrop(project_temp_directory(directory.path()));
    let model = Arc::new(ScriptedModel::new(Vec::new()));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let shell = agent
        .submit_user_shell_for_goal(chat, None, "printf visible".into(), true)
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let output = loop {
        let snapshot = agent.snapshot(chat).await.unwrap();
        let record = snapshot
            .user_shells
            .iter()
            .find(|record| record.id == shell.command_id)
            .unwrap()
            .clone();
        if record.output_bytes == 7 {
            break record.output_ref.unwrap();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "shell did not finish"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(output.starts_with(project_temp_directory(directory.path())));
    let reader = Arc::new(ScriptedModel::new(vec![
        call("read-output", "Read", json!({"file_path": output})),
        text("Read it."),
    ]));
    let agent_with_reader = Agent::new_embedded(config(), reader.clone());
    let reader_chat = agent_with_reader
        .create_chat(directory.path().into())
        .await
        .unwrap();
    complete_turn(&agent_with_reader, reader_chat, None, "Read the output").await;
    assert!(tool_results(&requests(&reader)[1])[0].1.contains("visible"));
    agent.shutdown().await;
    agent_with_reader.shutdown().await;
}

#[tokio::test]
async fn a_failed_tool_returns_its_error_to_the_model_and_the_turn_continues() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("present.txt"), "present value").unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        vec![
            Content::ToolUse {
                id: "missing".into(),
                name: "Read".into(),
                input: json!({"file_path":"missing.txt"}),
            },
            Content::ToolUse {
                id: "present".into(),
                name: "Read".into(),
                input: json!({"file_path":"present.txt"}),
            },
        ],
        text("recovered"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();

    complete_turn(&agent, chat, None, "read both files").await;

    let requests = requests(&model);
    assert_eq!(requests.len(), 2);
    let results = requests[1]
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some((tool_use_id.as_str(), content.text(), *is_error)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        matches!(&results[..], [("missing", _, true), ("present", present, false)] if present.contains("present value"))
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        snapshot.loops[0].state,
        tickloop_agent::runtime::LoopState::Completed
    );
    agent.shutdown().await;
}
