use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use uuid::Uuid;

use super::entry::{
    AiTitleEntry, BranchOriginEntry, CustomTitleEntry, DelegationEntry, LastPromptEntry, Line,
    SubagentsEntry,
};
use super::listing::{LITE_READ_BUF_SIZE, last_json_string_field};
use tickloop_agent_base::private_fs::{PRIVATE_FILE_MODE, create_private_directory};

/// 照 Claude Code `FLUSH_INTERVAL_MS`：条目先排队，最多每 100ms 批量追加一次。
pub(crate) const FLUSH_INTERVAL: Duration = Duration::from_millis(100);

/// 同类元数据的最后一条；退出时重新追加到文件末尾，让列表只读尾部就能取到。
#[derive(Clone, Debug, Default)]
pub(crate) struct CachedMetadata {
    pub artifact_ticks: Option<super::entry::ArtifactTicksEntry>,
    pub monitor_members: Option<super::entry::MonitorMembersEntry>,
    pub custom_title: Option<CustomTitleEntry>,
    pub ai_title: Option<AiTitleEntry>,
    pub subagents: Option<SubagentsEntry>,
    pub family_completions: Option<super::entry::FamilyCompletionsEntry>,
    pub family_inputs: Option<super::entry::FamilyInputsEntry>,
    pub sent_agent_messages: Option<super::entry::SentAgentMessagesEntry>,
    pub last_prompt: Option<LastPromptEntry>,
    pub branch_origin: Option<BranchOriginEntry>,
    pub delegation: Option<DelegationEntry>,
}

impl CachedMetadata {
    fn remember(&mut self, line: &Line) {
        match line {
            Line::ArtifactTicks(entry) => self.artifact_ticks = Some(entry.clone()),
            Line::MonitorMembers(entry) => self.monitor_members = Some(entry.clone()),
            Line::CustomTitle(entry) => self.custom_title = Some(entry.clone()),
            Line::AiTitle(entry) => self.ai_title = Some(entry.clone()),
            Line::Subagents(entry) => self.subagents = Some(entry.clone()),
            Line::FamilyCompletions(entry) => self.family_completions = Some(entry.clone()),
            Line::FamilyInputs(entry) => self.family_inputs = Some(entry.clone()),
            Line::SentAgentMessages(entry) => self.sent_agent_messages = Some(entry.clone()),
            Line::LastPrompt(entry) => self.last_prompt = Some(entry.clone()),
            Line::BranchOrigin(entry) => self.branch_origin = Some(entry.clone()),
            Line::Delegation(entry) => self.delegation = Some(entry.clone()),
            _ => {}
        }
    }

    fn lines(&self) -> Vec<Line> {
        self.custom_title
            .clone()
            .map(Line::CustomTitle)
            .into_iter()
            .chain(self.ai_title.clone().map(Line::AiTitle))
            .chain(self.subagents.clone().map(Line::Subagents))
            .chain(self.family_completions.clone().map(Line::FamilyCompletions))
            .chain(self.family_inputs.clone().map(Line::FamilyInputs))
            .chain(
                self.sent_agent_messages
                    .clone()
                    .map(Line::SentAgentMessages),
            )
            .chain(self.last_prompt.clone().map(Line::LastPrompt))
            .chain(self.branch_origin.clone().map(Line::BranchOrigin))
            .chain(self.delegation.clone().map(Line::Delegation))
            .chain(self.artifact_ticks.clone().map(Line::ArtifactTicks))
            .chain(self.monitor_members.clone().map(Line::MonitorMembers))
            .collect()
    }
}

#[derive(Default)]
struct FileState {
    /// 文件已经开始记录：第一条 user／assistant 条目已入队，或恢复时从磁盘读取。
    materialized: bool,
    /// 文件已在磁盘上：本进程追加成功过，或恢复时从磁盘读取。之后的追加不再创建文件。
    exists_on_disk: bool,
    /// 已在磁盘上的文件被外部删除。之后丢弃该文件的全部条目，不重建文件：
    /// 只含后续条目的文件父指针断链，恢复时丢失之前的对话。
    deleted: bool,
    /// 第一条 user／assistant 条目到来前缓存的条目，文件此时尚未创建。
    buffered: Vec<Line>,
    queued: Vec<u8>,
    written_uuids: HashSet<Uuid>,
    metadata: CachedMetadata,
    /// 最近一次追加失败的原因；之后一次追加成功时清除。
    write_failure: Option<String>,
}

/// 按文件排队的追加写入器。不加文件锁；写入失败不影响运行，失败的批次放回队首，
/// 下次冲刷时先重写，已登记的 uuid 不变，因此既不重复也不断链。
#[derive(Default)]
pub(crate) struct TranscriptWriter {
    files: Mutex<HashMap<PathBuf, FileState>>,
    /// 串行化实际写盘，保证先入队的批次先落盘。
    io: Mutex<()>,
}

impl TranscriptWriter {
    pub(crate) fn enqueue(&self, path: &Path, lines: Vec<Line>) {
        if lines.is_empty() {
            return;
        }
        let mut files = self.files.lock().unwrap();
        let file = files.entry(path.to_path_buf()).or_default();
        if file.deleted {
            return;
        }
        let mut accepted = Vec::with_capacity(lines.len());
        for line in lines {
            if let Some(entry) = line.chain_entry()
                && !file.written_uuids.insert(entry.uuid)
            {
                continue;
            }
            file.metadata.remember(&line);
            accepted.push(line);
        }
        if !file.materialized {
            if !accepted.iter().any(|line| {
                line.is_conversation_message()
                    || matches!(line, Line::ArtifactTicks(entry) if entry.cwd.is_some() && (!entry.artifact_ticks.is_empty() || !entry.removed_artifact_ticks.is_empty()))
                    || matches!(line, Line::MonitorMembers(entry) if entry.materializes_chat())
            }) {
                file.buffered.extend(accepted);
                return;
            }
            file.materialized = true;
            accepted.splice(0..0, std::mem::take(&mut file.buffered));
        }
        for line in accepted {
            append_line(&mut file.queued, &line);
        }
    }

    /// 恢复已有文件后调用：之后的条目直接追加，不再重复写入已在文件中的 uuid。
    pub(crate) fn adopt_existing(
        &self,
        path: &Path,
        uuids: HashSet<Uuid>,
        metadata: CachedMetadata,
    ) {
        let mut files = self.files.lock().unwrap();
        let file = files.entry(path.to_path_buf()).or_default();
        file.materialized = true;
        file.exists_on_disk = true;
        file.written_uuids.extend(uuids);
        file.metadata = metadata;
    }

    pub(crate) fn is_materialized(&self, path: &Path) -> bool {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .is_some_and(|file| file.materialized)
    }

    pub(crate) fn flush_blocking(&self) {
        let _io = self.io.lock().unwrap();
        let batches = self
            .files
            .lock()
            .unwrap()
            .iter_mut()
            .filter(|(_, file)| !file.queued.is_empty())
            .map(|(path, file)| {
                let bytes = std::mem::take(&mut file.queued);
                (path.clone(), bytes, file.exists_on_disk)
            })
            .collect::<Vec<_>>();
        for (path, bytes, exists_on_disk) in batches {
            let outcome = append_to_transcript(&path, &bytes, exists_on_disk);
            let mut files = self.files.lock().unwrap();
            let file = files.entry(path.clone()).or_default();
            match outcome {
                Ok(()) => {
                    file.exists_on_disk = true;
                    file.write_failure = None;
                }
                Err(error) if is_deleted(&error, exists_on_disk) => file.mark_deleted(&path),
                Err(error) => {
                    let mut retry = bytes;
                    retry.append(&mut file.queued);
                    file.queued = retry;
                    file.write_failure = Some(describe_failure(&path, &error));
                }
            }
        }
    }

    pub(crate) fn checkpoint_blocking(&self, path: &Path) -> io::Result<()> {
        let _io = self.io.lock().unwrap();
        let (bytes, exists_on_disk) = {
            let mut files = self.files.lock().unwrap();
            let file = files
                .get_mut(path)
                .filter(|file| file.materialized && !file.deleted)
                .ok_or_else(|| {
                    io::Error::other("Transcript is unavailable for a connection checkpoint")
                })?;
            (std::mem::take(&mut file.queued), file.exists_on_disk)
        };
        if !bytes.is_empty() {
            let outcome = append_to_transcript(path, &bytes, exists_on_disk);
            let mut files = self.files.lock().unwrap();
            let file = files.get_mut(path).expect("checkpoint file");
            match outcome {
                Ok(()) => {
                    file.exists_on_disk = true;
                    file.write_failure = None;
                }
                Err(error) => {
                    if is_deleted(&error, exists_on_disk) {
                        file.mark_deleted(path);
                    } else {
                        let mut retry = bytes;
                        retry.append(&mut file.queued);
                        file.queued = retry;
                        file.write_failure = Some(describe_failure(path, &error));
                    }
                    return Err(error);
                }
            }
        }
        std::fs::File::open(path)?.sync_all()?;
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    }

    /// 先冲刷，再把缓存的元数据追加到文件末尾。追加前读取尾部 64KB，
    /// 采纳其他进程写入的标题。
    pub(crate) fn reappend_metadata_blocking(&self, path: &Path) {
        self.flush_blocking();
        let _io = self.io.lock().unwrap();
        let lines = {
            let mut files = self.files.lock().unwrap();
            let Some(file) = files
                .get_mut(path)
                .filter(|file| file.exists_on_disk && !file.deleted)
            else {
                return;
            };
            if let Ok(tail) = read_tail(path)
                && let Some(title) = last_json_string_field(&tail, "customTitle")
                && let Some(current) = &mut file.metadata.custom_title
            {
                current.custom_title = title;
            }
            file.metadata.lines()
        };
        if lines.is_empty() {
            return;
        }
        let mut bytes = Vec::new();
        for line in &lines {
            append_line(&mut bytes, line);
        }
        let outcome = append_to_transcript(path, &bytes, true);
        if let Some(file) = self.files.lock().unwrap().get_mut(path) {
            match outcome {
                Err(error) if is_deleted(&error, true) => file.mark_deleted(path),
                Err(error) => file.write_failure = Some(describe_failure(path, &error)),
                Ok(()) if file.queued.is_empty() => file.write_failure = None,
                Ok(()) => {}
            }
        }
    }

    /// 仍未解决的写入失败：某个文件最近一次追加失败且之后没有成功追加过，或文件已被外部删除
    /// （不再恢复）。
    pub(crate) fn unresolved_error(&self) -> Option<String> {
        self.files
            .lock()
            .unwrap()
            .values()
            .find_map(|file| file.write_failure.clone())
    }
}

impl FileState {
    fn mark_deleted(&mut self, path: &Path) {
        self.deleted = true;
        self.buffered.clear();
        self.queued.clear();
        self.write_failure = Some(format!(
            "Transcript {} was deleted, so the rest of this conversation is not saved",
            path.display()
        ));
    }
}

fn is_deleted(error: &io::Error, exists_on_disk: bool) -> bool {
    exists_on_disk && error.kind() == io::ErrorKind::NotFound
}

fn describe_failure(path: &Path, error: &io::Error) -> String {
    format!("cannot append Transcript {}: {error}", path.display())
}

fn append_line(buffer: &mut Vec<u8>, line: &Line) {
    if serde_json::to_writer(&mut *buffer, line).is_ok() {
        buffer.push(b'\n');
    }
}

/// 文件已在磁盘上时只追加、不创建：文件不存在说明已被外部删除，由调用方停止记录。
/// 第一次写入时创建文件，必要时先创建目录。
fn append_to_transcript(path: &Path, bytes: &[u8], exists_on_disk: bool) -> io::Result<()> {
    if exists_on_disk {
        return append(path, bytes, false);
    }
    match append(path, bytes, true) {
        Ok(()) => Ok(()),
        Err(_) => {
            if let Some(parent) = path.parent() {
                create_private_directory(parent)?;
            }
            append(path, bytes, true)
        }
    }
}

/// 写入中途失败时把已写出的半截字节截回原长度，避免留下坏行，也避免重试时重复写入。
fn append(path: &Path, bytes: &[u8], create: bool) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.create(create).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, PRIVATE_FILE_MODE);
    let mut file = options.open(path)?;
    let original_length = file.metadata()?.len();
    if let Err(error) = file.write_all(bytes) {
        let _ = file.set_len(original_length);
        return Err(error);
    }
    Ok(())
}

fn read_tail(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(LITE_READ_BUF_SIZE as u64);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::super::entry::ChainEntry;
    use super::*;
    use tickloop_agent_protocol::{Content, Message, Role};

    fn chain(uuid: Uuid, role: Role) -> Line {
        Line::chain(
            role,
            ChainEntry {
                uuid,
                parent_uuid: None,
                logical_parent_uuid: None,
                is_sidechain: false,
                chat_id: Uuid::nil(),
                timestamp: tickloop_agent_base::timestamp::now_timestamp(),
                cwd: PathBuf::from("/tmp"),
                version: "test".into(),
                git_branch: None,
                message: Message {
                    role,
                    content: vec![Content::Text { text: "x".into() }],
                },
                invoked_skills: Vec::new(),
                is_meta: false,
                host_tools: None,
                execution_mode: None,
                is_inherited_prompt: false,
                original_message_id: None,
                agent_message_origin: None,
                human_family_origin: None,
                run_id: None,
                tool_call_recoveries: Vec::new(),
                contexts: None,
                continues_message: false,
                subtype: None,
                compact_metadata: None,
                is_compact_summary: false,
                summarize_metadata: None,
                is_visible_in_transcript_only: false,
                usage: None,
                model: None,
                private_source: None,
                loop_id: None,
                slug: None,
                loop_record: None,
            },
        )
    }

    fn title() -> Line {
        Line::CustomTitle(CustomTitleEntry {
            chat_id: Uuid::nil(),
            custom_title: "title".into(),
        })
    }

    #[test]
    fn file_is_created_by_the_first_message_and_duplicates_are_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/chat.jsonl");
        let writer = TranscriptWriter::default();
        writer.enqueue(&path, vec![title()]);
        writer.flush_blocking();
        assert!(!path.exists());

        let uuid = Uuid::new_v4();
        writer.enqueue(&path, vec![chain(uuid, Role::User)]);
        writer.enqueue(&path, vec![chain(uuid, Role::User)]);
        writer.flush_blocking();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(text.lines().next().unwrap().contains("\"custom-title\""));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, PRIVATE_FILE_MODE);
        }
    }

    /// 已写出的文件被外部删除后不再重建：只含后续条目的文件父指针断链，
    /// 退出时追加的元数据也会写出空壳。改为报告写入错误并停止记录该文件。
    #[test]
    fn a_deleted_transcript_is_not_recreated_and_the_deletion_is_reported() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        let path = project.join("chat.jsonl");
        let writer = TranscriptWriter::default();
        writer.enqueue(&path, vec![title(), chain(Uuid::new_v4(), Role::User)]);
        writer.flush_blocking();
        assert!(path.is_file());

        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&project).unwrap();
        writer.enqueue(&path, vec![chain(Uuid::new_v4(), Role::Assistant)]);
        writer.flush_blocking();
        writer.enqueue(&path, vec![chain(Uuid::new_v4(), Role::User)]);
        writer.reappend_metadata_blocking(&path);

        assert!(!project.exists());
        let error = writer.unresolved_error().expect("the deletion is reported");
        assert!(error.contains("was deleted"), "{error}");
    }

    #[test]
    fn an_adopted_transcript_that_was_deleted_is_not_recreated() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chat.jsonl");
        std::fs::write(&path, "").unwrap();
        let writer = TranscriptWriter::default();
        writer.adopt_existing(&path, HashSet::new(), CachedMetadata::default());

        std::fs::remove_file(&path).unwrap();
        writer.enqueue(&path, vec![chain(Uuid::new_v4(), Role::User)]);
        writer.flush_blocking();

        assert!(!path.exists());
        assert!(writer.unresolved_error().is_some());
    }
}
