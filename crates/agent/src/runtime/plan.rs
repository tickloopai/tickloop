//! 计划与进度。
//!
//! 计划属于 Loop：照 Claude Code 为子代理生成 `<slug>-agent-<id>.md` 的方式，计划文件为
//! `<计划目录>/<slug>-loop-<loopId>.md`，slug 按 Chat 生成并记录在链上条目中。Plan 模式下模型用
//! `Write`／`Edit` 编写当前 Loop 的计划文件，`ExitPlanMode` 由宿主读取并提交审批。进度是该 Loop 的
//! 待办列表，由 `TodoWrite` 整体替换。计划版本与进度不单独保存，恢复时沿对话链重建：版本取该 Loop
//! 成功提交计划的调用，进度取最后一次成功的 `TodoWrite` 调用的输入（照 Claude Code 从最后一次
//! TodoWrite 调用重建）。
//!
//! 提交时宿主把版本号与内容摘要写成计划文件首行的 HTML 注释（Markdown 渲染时不可见），与内容
//! 一起原子替换。按版本批准只对与该版本号一同写出的内容生效：恢复时文件中的版本与链上记录不一致，
//! 或摘要与内容不符（提交后又被编辑、写出结果前崩溃等），计划以从未使用过的新版本恢复。

use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::*;
use crate::model::ToolDefinition;
use crate::private_fs::{
    PRIVATE_FILE_MODE, create_private_directory, replace_file_atomically, verify_private_directory,
};
use crate::tools::{FileReadRecord, ToolResult};
use crate::transcript::{PrivateDirectory, private_project_temp_directory};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

mod prompts;
pub(super) use prompts::{enter_plan_mode_declined, plan_mode_reminder};

pub const ENTER_PLAN_MODE_TOOL_NAME: &str = "EnterPlanMode";
pub const EXIT_PLAN_MODE_TOOL_NAME: &str = "ExitPlanMode";
pub const TODO_WRITE_TOOL_NAME: &str = "TodoWrite";
/// 计划文件的目录与文件名 `<slug>-loop-<loopId>.md` 的组成部分；过期清理按同一格式识别。
pub(crate) const PLANS_DIRECTORY: &str = "plans";
pub(crate) const PLAN_LOOP_SEPARATOR: &str = "-loop-";
pub(crate) const PLAN_FILE_EXTENSION: &str = ".md";
const PLANS_DIRECTORY_DESCRIPTION: &str = "plans directory";
const MAX_PLAN_BYTES: usize = 64 * 1024;
const PLAN_VERSION_HEADER_PREFIX: &str = "<!-- tickloop plan version: ";
const PLAN_DIGEST_SEPARATOR: &str = " sha256: ";
const PLAN_VERSION_HEADER_SUFFIX: &str = " -->";
const MAX_PLAN_HEADER_BYTES: usize = 128;
const MAX_PLAN_FILE_BYTES: usize = MAX_PLAN_BYTES + MAX_PLAN_HEADER_BYTES;
pub(crate) const MAX_PLAN_SLUG_BYTES: usize = 64;
const MAX_TODOS: usize = 100;
const MAX_TODO_TEXT_BYTES: usize = 4096;
/// 照 Claude Code `MAX_SLUG_RETRIES`：生成的 slug 已被计划文件占用时重新生成。
const MAX_SLUG_RETRIES: usize = 10;

/// 照 Claude Code `generateWordSlug` 的"形容词-动词-名词"格式，词表取自其词表的前一部分。
const SLUG_ADJECTIVES: [&str; 32] = [
    "abundant",
    "ancient",
    "bright",
    "calm",
    "cheerful",
    "clever",
    "cozy",
    "curious",
    "dapper",
    "dazzling",
    "deep",
    "delightful",
    "eager",
    "elegant",
    "enchanted",
    "fancy",
    "fluffy",
    "gentle",
    "gleaming",
    "golden",
    "graceful",
    "happy",
    "hidden",
    "humble",
    "jolly",
    "joyful",
    "keen",
    "kind",
    "lively",
    "lovely",
    "lucky",
    "luminous",
];
const SLUG_VERBS: [&str; 32] = [
    "baking",
    "beaming",
    "booping",
    "bouncing",
    "brewing",
    "bubbling",
    "chasing",
    "churning",
    "coalescing",
    "conjuring",
    "cooking",
    "crafting",
    "crunching",
    "cuddling",
    "dancing",
    "dazzling",
    "discovering",
    "doodling",
    "dreaming",
    "drifting",
    "enchanting",
    "exploring",
    "finding",
    "floating",
    "fluttering",
    "foraging",
    "forging",
    "frolicking",
    "gathering",
    "giggling",
    "gliding",
    "greeting",
];
const SLUG_NOUNS: [&str; 32] = [
    "aurora",
    "avalanche",
    "blossom",
    "breeze",
    "brook",
    "bubble",
    "canyon",
    "cascade",
    "cloud",
    "clover",
    "comet",
    "coral",
    "cosmos",
    "creek",
    "crescent",
    "crystal",
    "dawn",
    "dewdrop",
    "dusk",
    "eclipse",
    "ember",
    "feather",
    "fern",
    "firefly",
    "flame",
    "flurry",
    "fog",
    "forest",
    "frost",
    "galaxy",
    "garden",
    "glacier",
];

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Plan {
    pub loop_id: LoopId,
    pub version: u64,
    pub markdown: String,
    /// The execution mode selected for this process-local plan approval.
    pub approved_execution_mode: Option<ExecutionMode>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// Claude Code's `TodoItemSchema`: `content` is the imperative form and `activeForm` the present
/// continuous form, which names the current activity while the todo is in progress.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
    pub active_form: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Progress {
    pub loop_id: LoopId,
    pub todos: Vec<TodoItem>,
}

impl Progress {
    /// The `activeForm` of the first in-progress todo, which names the current activity like
    /// Claude Code's spinner.
    pub fn active_form(&self) -> Option<&str> {
        self.todos
            .iter()
            .find(|todo| todo.status == TodoStatus::InProgress)
            .map(|todo| todo.active_form.as_str())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TodoWriteInput {
    todos: Vec<TodoItem>,
}

impl TodoWriteInput {
    /// Claude Code requires non-empty `content` and `activeForm`; blank text is rejected as well,
    /// and the list is bounded because it is saved and shown to the model on every request.
    fn parse(input: &Value) -> Result<Vec<TodoItem>, String> {
        let invalid = |reason: String| format!("invalid {TODO_WRITE_TOOL_NAME} input: {reason}");
        let parsed = serde_json::from_value::<Self>(input.clone())
            .map_err(|error| invalid(error.to_string()))?;
        if parsed.todos.len() > MAX_TODOS {
            return Err(invalid(format!(
                "input.todos has more than {MAX_TODOS} items"
            )));
        }
        for (index, todo) in parsed.todos.iter().enumerate() {
            for (field, text) in [
                ("content", &todo.content),
                ("activeForm", &todo.active_form),
            ] {
                if text.trim().is_empty() {
                    return Err(invalid(format!(
                        "input.todos[{index}].{field} cannot be empty"
                    )));
                }
                if text.len() > MAX_TODO_TEXT_BYTES {
                    return Err(invalid(format!(
                        "input.todos[{index}].{field} exceeds {MAX_TODO_TEXT_BYTES} bytes"
                    )));
                }
            }
        }
        Ok(parsed.todos)
    }
}

/// 计划文件中的计划内容，以及宿主提交计划时写出的版本行。
struct PlanFile {
    /// 版本行记录的版本；从未提交过的草稿没有版本行。
    version: Option<u64>,
    /// 版本行的摘要与内容相符，即内容正是以 `version` 提交的计划，之后没有再被编辑。
    is_submitted: bool,
    markdown: String,
}

/// 链上 Loop 条目携带的计划版本与进度。`position` 是条目之前的消息数。
pub(super) struct PlanCheckpoint {
    pub position: usize,
    pub loop_id: LoopId,
    pub plan_version: Option<u64>,
    pub progress: Option<Vec<TodoItem>>,
}

pub(super) fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: ENTER_PLAN_MODE_TOOL_NAME.into(),
            description: prompts::enter_plan_mode_description(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        },
        // 照 Claude Code 的 `.passthrough()`：不声明 additionalProperties，其他字段被接受并忽略。
        ToolDefinition {
            name: EXIT_PLAN_MODE_TOOL_NAME.into(),
            description: prompts::exit_plan_mode_description(),
            input_schema: json!({"type":"object","properties":{"allowedPrompts":{
                "type":"array",
                "description":"Prompt-based permissions needed to implement the plan. These describe categories of actions rather than specific commands.",
                "items":{"type":"object","properties":{
                    "tool":{"type":"string","enum":["Bash"],"description":"The tool this prompt applies to"},
                    "prompt":{"type":"string","description":"Semantic description of the action, e.g. \"run tests\", \"install dependencies\""}
                },"required":["tool","prompt"]}
            }}}),
        },
        ToolDefinition {
            name: TODO_WRITE_TOOL_NAME.into(),
            description: prompts::todo_write_description(),
            input_schema: json!({"type":"object","properties":{"todos":{
                "type":"array",
                "description":"The updated todo list",
                "items":{"type":"object","properties":{
                    "content":{"type":"string","minLength":1},
                    "status":{"type":"string","enum":["pending","in_progress","completed"]},
                    "activeForm":{"type":"string","minLength":1}
                },"required":["content","status","activeForm"],"additionalProperties":false}
            }},"required":["todos"],"additionalProperties":false}),
        },
    ]
}

pub(super) fn plans_directory(root: &Path) -> PathBuf {
    root.join(PLANS_DIRECTORY)
}

/// slug 由小写字母单词以连字符连接。链上的 slug 可能被篡改，拼接路径前必须通过校验。
pub(crate) fn is_valid_plan_slug(slug: &str) -> bool {
    slug.len() <= MAX_PLAN_SLUG_BYTES
        && slug
            .split('-')
            .all(|word| !word.is_empty() && word.bytes().all(|byte| byte.is_ascii_lowercase()))
}

fn invalid_input(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn invalid_data(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// `<slug>-loop-<loopId>.md` 文件名中的 slug；文件名不是这种格式时返回 `None`（不校验 slug）。
pub(crate) fn plan_file_slug(file_name: &str) -> Option<&str> {
    let stem = file_name.strip_suffix(PLAN_FILE_EXTENSION)?;
    let (slug, loop_id) = stem.rsplit_once(PLAN_LOOP_SEPARATOR)?;
    Uuid::try_parse(loop_id).ok()?;
    Some(slug)
}

fn plan_file_path(directory: &Path, slug: &str, loop_id: LoopId) -> io::Result<PathBuf> {
    if !is_valid_plan_slug(slug) {
        return Err(invalid_input(format!("invalid plan file prefix {slug:?}")));
    }
    let file_name = format!("{slug}{PLAN_LOOP_SEPARATOR}{loop_id}{PLAN_FILE_EXTENSION}");
    let path = directory.join(&file_name);
    if path.file_name() != Some(file_name.as_ref()) || path.parent() != Some(directory) {
        return Err(invalid_input(format!(
            "plan file {file_name:?} is outside {}",
            directory.display()
        )));
    }
    Ok(path)
}

/// Chooses the Chat's plan file prefix on first use, like Claude Code's `getPlanSlug`; returns
/// whether it was chosen now.
fn ensure_plan_slug(snapshot: &mut ChatSnapshot, directory: &Path) -> bool {
    if snapshot.plan_slug.is_some() {
        return false;
    }
    snapshot.plan_slug = Some(generate_plan_slug(Some(directory)));
    true
}

/// The Loop's plan file under `directory`; `None` before the Chat has a plan file prefix.
pub(super) fn current_plan_file(
    snapshot: &ChatSnapshot,
    directory: &Path,
    loop_id: LoopId,
) -> Option<PathBuf> {
    plan_file_path(directory, snapshot.plan_slug.as_deref()?, loop_id).ok()
}

/// 计划目录必须是当前用户拥有、没有 group/other 权限位的真实目录，与系统临时目录中的私有目录
/// 校验一致；否则拒绝读写其中的计划文件。
fn verify_plans_directory(directory: &Path) -> io::Result<()> {
    verify_private_directory(directory, PLANS_DIRECTORY_DESCRIPTION)
}

/// 照 Claude Code `getPlanSlug`：随机生成 slug，已有计划文件使用该 slug 时重试，最多 10 次。
pub(super) fn generate_plan_slug(directory: Option<&Path>) -> String {
    let random_slug = || {
        let random = Uuid::new_v4().as_u128();
        let pick = |words: &[&'static str], shift: u32| {
            words[((random >> shift) % words.len() as u128) as usize]
        };
        format!(
            "{}-{}-{}",
            pick(&SLUG_ADJECTIVES, 0),
            pick(&SLUG_VERBS, 16),
            pick(&SLUG_NOUNS, 32)
        )
    };
    let mut slug = random_slug();
    let Some(directory) = directory else {
        return slug;
    };
    for _ in 1..MAX_SLUG_RETRIES {
        let prefix = format!("{slug}{PLAN_LOOP_SEPARATOR}");
        let is_taken = std::fs::read_dir(directory).is_ok_and(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(&prefix))
            })
        });
        if !is_taken {
            break;
        }
        slug = random_slug();
    }
    slug
}

fn plan_digest(markdown: &str) -> String {
    Sha256::digest(markdown.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 把版本行与内容写入同目录的临时文件并同步到磁盘，再改名替换计划文件：任何时刻计划文件
/// 要么是完整的旧版本，要么是完整的新版本。失败时删除临时文件。返回写出的完整文本。
pub(super) fn write_plan_file(
    directory: &Path,
    slug: &str,
    loop_id: LoopId,
    version: u64,
    markdown: &str,
) -> io::Result<String> {
    let path = plan_file_path(directory, slug, loop_id)?;
    create_private_directory(directory)?;
    verify_plans_directory(directory)?;
    let contents = format!(
        "{PLAN_VERSION_HEADER_PREFIX}{version}{PLAN_DIGEST_SEPARATOR}{}{PLAN_VERSION_HEADER_SUFFIX}\n{markdown}",
        plan_digest(markdown)
    );
    replace_file_atomically(&path, contents.as_bytes(), PRIVATE_FILE_MODE)?;
    Ok(contents)
}

/// 读取计划文件：不跟随符号链接，只接受普通文件，超过大小上限视为读取失败。
fn read_plan_file(directory: &Path, slug: &str, loop_id: LoopId) -> io::Result<PlanFile> {
    let path = plan_file_path(directory, slug, loop_id)?;
    verify_plans_directory(directory)?;
    // O_NONBLOCK 避免打开被放在该路径上的 FIFO 时阻塞；对普通文件没有影响。
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid_data(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.take(MAX_PLAN_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_PLAN_FILE_BYTES {
        return Err(invalid_data(format!(
            "{} exceeds {MAX_PLAN_FILE_BYTES} bytes",
            path.display()
        )));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| invalid_data(format!("{} is not UTF-8", path.display())))?;
    let plan_file = text
        .split_once('\n')
        .and_then(|(header, body)| {
            let (version, digest) = header
                .strip_prefix(PLAN_VERSION_HEADER_PREFIX)?
                .strip_suffix(PLAN_VERSION_HEADER_SUFFIX)?
                .split_once(PLAN_DIGEST_SEPARATOR)?;
            Some(PlanFile {
                version: Some(version.parse().ok()?),
                is_submitted: digest == plan_digest(body),
                markdown: body.to_owned(),
            })
        })
        .unwrap_or(PlanFile {
            version: None,
            is_submitted: false,
            markdown: text,
        });
    if plan_file.markdown.len() > MAX_PLAN_BYTES {
        return Err(invalid_data(format!(
            "{} exceeds {MAX_PLAN_BYTES} bytes",
            path.display()
        )));
    }
    Ok(plan_file)
}

/// 照 Claude Code `copyPlanForFork`：`--fork-session` 与 Branch 使用新的 slug，把计划内容写入新文件，
/// 之后双方各自修改互不影响。复制失败的计划不进入新 Chat，只保留其版本号，之后的计划从更大的
/// 版本继续。
pub(super) fn copy_plans_to_new_slug(snapshot: &mut ChatSnapshot, directory: Option<&Path>) {
    if snapshot.plan_slug.is_none() {
        return;
    }
    let slug = generate_plan_slug(directory);
    if let Some(directory) = directory {
        for plan in std::mem::take(&mut snapshot.plans) {
            match write_plan_file(directory, &slug, plan.loop_id, plan.version, &plan.markdown) {
                Ok(_) => snapshot.plans.push(plan),
                Err(_) => {
                    snapshot
                        .unavailable_plan_versions
                        .insert(plan.loop_id, plan.version);
                }
            }
        }
    }
    snapshot.plan_slug = Some(slug);
}

/// 该 Loop 已使用过的最高计划版本，包括内容不可用的计划。
pub(super) fn highest_plan_version(snapshot: &ChatSnapshot, loop_id: LoopId) -> Option<u64> {
    let current = snapshot
        .plans
        .iter()
        .find(|plan| plan.loop_id == loop_id)
        .map(|plan| plan.version);
    current.max(snapshot.unavailable_plan_versions.get(&loop_id).copied())
}

/// 沿对话链重建计划版本与进度：Loop 条目携带的检查点与之后成功的计划、进度工具调用按链上
/// 顺序生效。版本取检查点与调用结果中较大的值；进度取最后一次生效的内容。返回恢复时发现的
/// 问题，供加载警告显示。
///
/// 计划内容从计划文件读取，并与链上记录核对：文件版本行记录的版本等于重建的版本、且摘要与内容
/// 相符，才沿用该版本；否则文件在链上记录之外发生过变化，计划以大于文件与链上版本的新版本恢复，
/// 按旧版本号的批准都不会命中。链上没有记录但文件带有宿主写出的版本行时同样按新版本恢复；
/// 从未提交过的草稿不是计划。文件缺失或不可读时保留已知的最高版本，之后的计划从更大的版本继续。
pub(super) fn rebuild_plans_and_progress(
    snapshot: &mut ChatSnapshot,
    checkpoints: &[PlanCheckpoint],
    directory: Option<&Path>,
) -> Result<Vec<String>, AgentError> {
    enum Change {
        Version(u64),
        NextVersion,
        Progress(Vec<TodoItem>),
    }
    // 排序键：位置为 p 的检查点在第 p 条消息之前（2p），结果位于第 r 条消息的调用紧随其后（2r+1）。
    let mut changes = Vec::new();
    for checkpoint in checkpoints {
        let order = checkpoint.position * 2;
        if let Some(version) = checkpoint.plan_version {
            changes.push((order, checkpoint.loop_id, Change::Version(version)));
        }
        if let Some(todos) = &checkpoint.progress {
            changes.push((order, checkpoint.loop_id, Change::Progress(todos.clone())));
        }
    }
    for call in super::context::history_calls(snapshot)? {
        let Some((result_message, result_content)) = call.result else {
            continue;
        };
        let Some(loop_id) = call
            .run_id
            .and_then(|run| snapshot.run_loops.get(&run).copied())
        else {
            continue;
        };
        let (
            Content::ToolUse { name, input, .. },
            Content::ToolResult {
                content, is_error, ..
            },
        ) = (
            &snapshot.messages[call.message_index].content[call.content_index],
            &snapshot.messages[result_message].content[result_content],
        )
        else {
            continue;
        };
        if *is_error {
            continue;
        }
        let position = result_message * 2 + 1;
        match name.as_str() {
            EXIT_PLAN_MODE_TOOL_NAME => {
                let change = match prompts::submitted_plan_version(&content.text()) {
                    Some(version) => Change::Version(version),
                    None => Change::NextVersion,
                };
                changes.push((position, loop_id, change));
            }
            TODO_WRITE_TOOL_NAME => {
                if let Ok(todos) = TodoWriteInput::parse(input) {
                    changes.push((position, loop_id, Change::Progress(todos)));
                }
            }
            _ => {}
        }
    }
    changes.sort_by_key(|(position, _, _)| *position);
    let mut versions = HashMap::<LoopId, u64>::new();
    let mut progress = HashMap::<LoopId, Vec<TodoItem>>::new();
    for (_, loop_id, change) in changes {
        match change {
            Change::Version(version) => {
                let current = versions.entry(loop_id).or_default();
                *current = (*current).max(version);
            }
            Change::NextVersion => {
                *versions.entry(loop_id).or_default() += 1;
            }
            Change::Progress(todos) => {
                progress.insert(loop_id, todos);
            }
        }
    }
    let mut warnings = Vec::new();
    // 与 `--fork-session` 相同，换用新的 slug；旧 slug 无法安全定位计划文件，已记录版本的计划按缺失处理。
    let mut location = directory.zip(snapshot.plan_slug.clone());
    if let Some(slug) = snapshot.plan_slug.take_if(|slug| !is_valid_plan_slug(slug)) {
        warnings.push(format!(
            "ignored the invalid plan file prefix {slug:?} recorded in the conversation; plans are saved under a new prefix"
        ));
        snapshot.plan_slug = Some(generate_plan_slug(directory));
        location = None;
    }
    let loop_ids = snapshot
        .loops
        .iter()
        .map(|goal| goal.id)
        .collect::<Vec<_>>();
    let mut plans = Vec::new();
    let mut unavailable = BTreeMap::new();
    for loop_id in &loop_ids {
        let recorded = versions.remove(loop_id).filter(|version| *version > 0);
        let file = location
            .as_ref()
            .map(|(directory, slug)| read_plan_file(directory, slug, *loop_id));
        match (recorded, file) {
            (Some(version), Some(Ok(file))) => {
                let restored_version = if file.is_submitted && file.version == Some(version) {
                    version
                } else {
                    let fresh = file.version.unwrap_or(0).max(version).saturating_add(1);
                    warnings.push(format!(
                        "the plan file of goal {loop_id} does not match plan version {version} recorded in the conversation; it is restored as unapproved version {fresh}"
                    ));
                    fresh
                };
                plans.push(Plan {
                    loop_id: *loop_id,
                    version: restored_version,
                    markdown: file.markdown,
                    approved_execution_mode: None,
                });
            }
            (
                None,
                Some(Ok(PlanFile {
                    version: Some(file_version),
                    markdown,
                    ..
                })),
            ) => {
                let fresh = file_version.saturating_add(1);
                warnings.push(format!(
                    "the plan file of goal {loop_id} was not recorded in the conversation; it is restored as unapproved version {fresh}"
                ));
                plans.push(Plan {
                    loop_id: *loop_id,
                    version: fresh,
                    markdown,
                    approved_execution_mode: None,
                });
            }
            (Some(version), file) => {
                let reason = match file {
                    Some(Err(error)) => error.to_string(),
                    _ => "the plan file cannot be located".into(),
                };
                warnings.push(format!(
                    "cannot restore plan version {version} of goal {loop_id}: {reason}; the next plan will be version {}",
                    version.saturating_add(1)
                ));
                unavailable.insert(*loop_id, version);
            }
            (None, _) => {}
        }
    }
    snapshot.plans = plans;
    snapshot.unavailable_plan_versions = unavailable;
    snapshot.progress = loop_ids
        .iter()
        .filter_map(|loop_id| {
            Some(Progress {
                loop_id: *loop_id,
                todos: progress.remove(loop_id)?,
            })
        })
        .collect();
    Ok(warnings)
}

impl Agent {
    /// Approves the reviewed plan version and switches the Chat to `mode`. Returns the message
    /// that starts implementing it, in the words of Claude Code's approved `ExitPlanMode` result.
    pub async fn approve_plan(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        version: u64,
        mode: ExecutionMode,
    ) -> Result<String, AgentError> {
        if mode == ExecutionMode::Plan {
            return Err(AgentError::Invalid(
                "select an execution mode when approving a plan".into(),
            ));
        }
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        if chat
            .snapshot
            .delegation
            .as_ref()
            .is_some_and(|delegation| delegation.ceiling.policy.mode != mode)
        {
            return Err(AgentError::Invalid(
                "Unsupported: plan approval cannot expand or replace the delegated execution mode"
                    .into(),
            ));
        }
        if chat.active.is_some() {
            return Err(AgentError::Busy);
        }
        let mut snapshot = chat.snapshot.clone();
        let goal = snapshot
            .loops
            .iter()
            .find(|goal| goal.id == loop_id)
            .ok_or(AgentError::NotFound)?;
        if goal.state == LoopState::Stopped {
            return Err(AgentError::Invalid("goal is stopped".into()));
        }
        let plan = snapshot
            .plans
            .iter_mut()
            .find(|plan| plan.loop_id == loop_id)
            .ok_or_else(|| AgentError::Invalid("goal has no plan".into()))?;
        if plan.version != version {
            return Err(AgentError::Invalid(
                "plan was revised; review the current version".into(),
            ));
        }
        plan.approved_execution_mode = Some(mode);
        let markdown = plan.markdown.clone();
        let plan_file = current_plan_file(
            &snapshot,
            self.plans_directory(&snapshot.workdir).path(),
            loop_id,
        )
        .ok_or_else(|| AgentError::Invalid("the plan file cannot be located".into()))?;
        snapshot.policy.mode = mode;
        snapshot.policy.revision += 1;
        self.save(&snapshot);
        chat.snapshot = snapshot;
        Ok(prompts::approved_plan(&plan_file, &markdown))
    }

    /// The directory of plan files: `<root>/plans` when Chats are saved, otherwise `plans` under
    /// the project's private temporary directory, like File History.
    pub(super) fn plans_directory(&self, workdir: &Path) -> PrivateDirectory {
        match &self.inner.recorder {
            Some(recorder) => PrivateDirectory::trusted(recorder.plans_directory()),
            None => private_project_temp_directory(workdir).join(PLANS_DIRECTORY),
        }
    }

    /// The plan mode section of a conversation request while the Chat is in Plan mode. It names
    /// the Loop's plan file, choosing the Chat's plan file prefix on first use.
    pub(super) fn plan_mode_section(&self, chat: &mut Chat, loop_id: LoopId) -> Option<String> {
        if chat.snapshot.policy.mode != ExecutionMode::Plan {
            return None;
        }
        let directory = self.plans_directory(&chat.snapshot.workdir);
        // The model's Write would create a missing plans directory with ordinary permissions, so
        // the private one is created first; if that fails, ExitPlanMode reports the reason.
        let _ = directory.create();
        if ensure_plan_slug(&mut chat.snapshot, directory.path()) {
            self.save(&chat.snapshot);
        }
        let plan_file = current_plan_file(&chat.snapshot, directory.path(), loop_id)?;
        Some(plan_mode_reminder(&plan_file, plan_file.exists()))
    }

    pub(super) async fn control_tool(
        &self,
        receipt: &Receipt,
        name: &str,
        input: &Value,
    ) -> Option<Result<ToolResult, String>> {
        if !matches!(
            name,
            ENTER_PLAN_MODE_TOOL_NAME | EXIT_PLAN_MODE_TOOL_NAME | TODO_WRITE_TOOL_NAME
        ) {
            return None;
        }
        Some(
            async {
                let mut chats = self.inner.chats.lock().await;
                let chat = chats
                    .get_mut(&receipt.chat_id)
                    .ok_or_else(|| "chat is unavailable".to_string())?;
                if chat
                    .active
                    .as_ref()
                    .is_none_or(CancellationToken::is_cancelled)
                {
                    return Err("run was cancelled".into());
                }
                if chat.snapshot.policy.rules.iter().any(|rule| {
                    rule.decision == Decision::Deny
                        && (rule.tool == name || rule.tool == "*")
                        && rule.loop_id.is_none_or(|id| id == receipt.loop_id)
                        && rule.input.as_ref().is_none_or(|value| value == input)
                }) {
                    return Err("permission denied".into());
                }
                let mut snapshot = chat.snapshot.clone();
                let (kind, content) = match name {
                    ENTER_PLAN_MODE_TOOL_NAME => {
                        if snapshot.policy.mode == ExecutionMode::Plan {
                            return Ok(ToolResult {
                                content: prompts::already_in_plan_mode(),
                                is_error: false,
                                output_ref: None,
                            });
                        }
                        snapshot.policy.mode = ExecutionMode::Plan;
                        snapshot.policy.revision += 1;
                        (None, prompts::entered_plan_mode())
                    }
                    EXIT_PLAN_MODE_TOOL_NAME => {
                        let (plan, content) =
                            self.submit_plan_file(chat, &mut snapshot, receipt.loop_id)?;
                        (Some(EventKind::PlanUpdated { plan }), content)
                    }
                    _ => {
                        let progress = Progress {
                            loop_id: receipt.loop_id,
                            todos: TodoWriteInput::parse(input)?,
                        };
                        snapshot
                            .progress
                            .retain(|progress| progress.loop_id != receipt.loop_id);
                        snapshot.progress.push(progress.clone());
                        self.track_todo_progress(chat, &progress);
                        (
                            Some(EventKind::ProgressUpdated { progress }),
                            prompts::TODO_WRITE_RESULT.into(),
                        )
                    }
                };
                self.save(&snapshot);
                chat.snapshot = snapshot;
                if let Some(kind) = kind {
                    self.emit(chat, receipt, kind);
                }
                Ok(ToolResult {
                    content,
                    is_error: false,
                    output_ref: None,
                })
            }
            .await,
        )
    }

    /// `ExitPlanMode`: reads the Loop's plan file and records its content as the next plan
    /// version for approval. The file is rewritten with that version's line, and a Chat that has
    /// read the file keeps editing it without reading it again.
    fn submit_plan_file(
        &self,
        chat: &Chat,
        snapshot: &mut ChatSnapshot,
        loop_id: LoopId,
    ) -> Result<(Plan, String), String> {
        if snapshot.policy.mode != ExecutionMode::Plan {
            return Err(prompts::NOT_IN_PLAN_MODE.into());
        }
        let directory = self.plans_directory(&snapshot.workdir);
        directory
            .create()
            .map_err(|error| format!("cannot prepare the plans directory: {error}"))?;
        let directory = directory.path();
        ensure_plan_slug(snapshot, directory);
        let slug = snapshot.plan_slug.clone().unwrap_or_default();
        let path = plan_file_path(directory, &slug, loop_id).map_err(|error| error.to_string())?;
        let markdown = match read_plan_file(directory, &slug, loop_id) {
            Ok(file) => file.markdown,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(prompts::missing_plan_file(&path));
            }
            Err(error) => {
                return Err(format!(
                    "cannot read the plan file {}: {error}",
                    path.display()
                ));
            }
        };
        if markdown.trim().is_empty() {
            return Err(prompts::empty_plan_file(&path));
        }
        let version = highest_plan_version(snapshot, loop_id).unwrap_or(0) + 1;
        let contents = write_plan_file(directory, &slug, loop_id, version, &markdown)
            .map_err(|error| format!("cannot save the plan: {error}"))?;
        let file_reads = chat.tools.file_reads();
        if file_reads.get(&path).is_some()
            && let Ok(modified) = std::fs::metadata(&path).and_then(|metadata| metadata.modified())
        {
            file_reads.record(
                path.clone(),
                FileReadRecord::whole_file(&contents, modified),
            );
        }
        snapshot.unavailable_plan_versions.remove(&loop_id);
        let plan = Plan {
            loop_id,
            version,
            markdown,
            approved_execution_mode: None,
        };
        snapshot.plans.retain(|plan| plan.loop_id != loop_id);
        snapshot.plans.push(plan.clone());
        let content = prompts::submitted_plan(version, &path, &plan.markdown);
        Ok((plan, content))
    }
}
