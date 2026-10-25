use super::*;

const MAX_DELAYED_SUBMISSIONS: usize = 8;
const MAX_DELAYED_BYTES: usize = 32 * 1024 * 1024;
const WAITING_STATUS: &str = "Message waiting locally; you can type the next message";

#[derive(Clone, serde::Serialize)]
pub(super) struct DelayedSubmission {
    pub(super) id: Uuid,
    pub(super) chat: Option<Uuid>,
    pub(super) owner: Uuid,
    pub(super) history_session: String,
    pub(super) history_project: PathBuf,
    pub(super) prompt: SubmittedPrompt,
    pub(super) file_references: Vec<PathBuf>,
    pub(super) family_target: Option<ExecutionRef>,
    pub(super) restored_contexts: Vec<InputContext>,
    pub(super) retry_of: Option<Uuid>,
    #[serde(serialize_with = "serialize_pasted_texts")]
    pub(super) pasted_texts: Vec<PastedText>,
    pub(super) send_now: bool,
    pub(super) failed: Option<String>,
    pub(super) in_flight: bool,
}

fn serialize_pasted_texts<S: serde::Serializer>(
    texts: &[PastedText],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    #[derive(serde::Serialize)]
    struct Reference<'a> {
        id: u64,
        range: &'a std::ops::Range<usize>,
    }
    serde::Serialize::serialize(
        &texts
            .iter()
            .map(|text| Reference {
                id: text.id,
                range: &text.range,
            })
            .collect::<Vec<_>>(),
        serializer,
    )
}

/// Count the actual wire representation without allocating a second copy of image data.
struct BoundedSize(usize);

impl io::Write for BoundedSize {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        if self.0 > MAX_DELAYED_BYTES {
            return Err(io::Error::other("local waiting queue exceeds 32 MiB"));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl State {
    pub(super) fn complete_delayed_image(
        &mut self,
        owner: Uuid,
        id: super::super::paste_blocks::PasteBlockId,
        result: Result<Option<Image>, String>,
        pending: super::super::image_preparation::Pending,
    ) -> bool {
        let Some(index) = self
            .delayed_submissions
            .iter()
            .position(|entry| entry.owner == owner)
        else {
            return false;
        };
        let mut candidate = self.delayed_submissions[index].clone();
        match result {
            Ok(Some(image)) => {
                if !candidate.prompt.paste_blocks.complete_pending_image(
                    &candidate.prompt.draft,
                    id,
                    image,
                ) {
                    candidate.failed =
                        Some("Image reservation changed; take back and paste again".into());
                }
            }
            result => {
                let error = match result {
                    Err(error) => error,
                    _ => pending.missing_message.unwrap_or_else(|| {
                        "Image preparation failed; take back and paste again".into()
                    }),
                };
                candidate.failed = Some(error.chars().take(256).collect());
                if !pending.fallback.is_empty()
                    && let Ok(Some(replacement)) = candidate.prompt.paste_blocks.fail_pending_image(
                        &candidate.prompt.draft,
                        id,
                        &pending.fallback,
                    )
                {
                    candidate.prompt.draft = replacement.draft;
                }
            }
        }
        let entries = self
            .delayed_submissions
            .iter()
            .enumerate()
            .map(|(i, entry)| if i == index { &candidate } else { entry })
            .collect::<Vec<_>>();
        if serde_json::to_writer(&mut BoundedSize(0), &entries).is_ok() {
            self.delayed_submissions[index] = candidate;
        } else {
            self.delayed_submissions[index].failed =
                Some("Image exceeds local 32 MiB queue limit; take back and paste again".into());
        }
        true
    }

    pub(super) fn freeze_delayed_submission(
        &mut self,
        file_references: Vec<PathBuf>,
        family_target: Option<ExecutionRef>,
    ) -> bool {
        if self.editor.draft().trim().is_empty() {
            return false;
        }
        if self.delayed_submissions.len() >= MAX_DELAYED_SUBMISSIONS {
            self.hold_status("Local waiting queue is full (8 messages); draft kept");
            return false;
        }
        if self
            .paste_blocks
            .source_text(self.editor.draft())
            .trim_start()
            .starts_with('/')
        {
            self.hold_status(
                "Wait for images and local messages before running a command; draft kept",
            );
            return false;
        }
        let mut submission = DelayedSubmission {
            id: tickloop_ids::generate(),
            chat: self.chat,
            owner: self.input_owner,
            history_session: self.history_session.clone(),
            history_project: self.history_project.clone(),
            prompt: SubmittedPrompt {
                draft: self.editor.draft().to_owned(),
                paste_blocks: self.paste_blocks.clone(),
                mentions: self.file_mentions.accepted_mentions().cloned().collect(),
                family_mentions: self.file_mentions.accepted_family_mentions().to_vec(),
            },
            file_references,
            family_target,
            restored_contexts: self.restored_contexts.clone(),
            retry_of: self.retry_of,
            pasted_texts: self.pasted_texts(),
            send_now: self.delayed_send_now,
            failed: None,
            in_flight: false,
        };
        // Keep room for later failure metadata on all eight entries.
        let mut size = BoundedSize(4096);
        // Serialize as one array so delimiters and every item's metadata count too.
        let entries = self
            .delayed_submissions
            .iter()
            .chain(std::iter::once(&submission));
        if serde_json::to_writer(&mut size, &entries.collect::<Vec<_>>()).is_err() {
            self.hold_status("Local waiting queue exceeds 32 MiB; draft kept");
            return false;
        }
        // No destructive operation happens until admission succeeds.
        submission.prompt.paste_blocks = self.paste_blocks.take();
        submission.prompt.mentions = self.file_mentions.take_accepted();
        submission.prompt.family_mentions = self.file_mentions.take_family();
        submission.restored_contexts = std::mem::take(&mut self.restored_contexts);
        submission.retry_of = self.retry_of.take();
        self.editor.replace_draft(String::new(), 0);
        self.input_owner = tickloop_ids::generate();
        self.completion.dismiss();
        self.file_mentions.dismiss();
        self.file_mentions.update_at("", 0, 15);
        self.escape_confirmation = None;
        self.delayed_submissions.push_back(submission);
        self.hold_status(WAITING_STATUS);
        true
    }

    pub(super) fn poll_delayed_submission(&mut self) -> Option<InputEvent> {
        if !self.ordinary_input_is_visible()
            || self.terminal_lent
            || self.workspace_trust.is_some()
            || self.active_delayed_submission.is_some()
        {
            return None;
        }
        let front = self
            .delayed_submissions
            .iter_mut()
            .find(|item| item.chat == self.chat)?;
        if front.failed.is_some()
            || front.in_flight
            || front.prompt.paste_blocks.has_pending_images()
        {
            return None;
        }
        front.in_flight = true;
        Some(InputEvent::DelayedPrompt { id: front.id })
    }

    pub(super) fn take_back_delayed(&mut self) -> bool {
        if !self.editor.draft().is_empty() || !self.restored_contexts.is_empty() {
            return false;
        }
        let Some(index) = self
            .delayed_submissions
            .iter()
            .rposition(|item| item.chat == self.chat)
        else {
            return false;
        };
        if self.delayed_submissions[index].in_flight {
            return false;
        }
        let mut item = self
            .delayed_submissions
            .remove(index)
            .expect("position exists");
        for id in item.prompt.paste_blocks.pending_image_ids() {
            let key = super::super::image_preparation::Key {
                owner: item.owner,
                id,
            };
            if !self.image_preparation.pending.contains_key(&key)
                && let Ok(Some(deletion)) =
                    item.prompt
                        .paste_blocks
                        .fail_pending_image(&item.prompt.draft, id, "")
            {
                item.prompt.draft = deletion.draft;
            }
        }
        let cursor = item.prompt.draft.len();
        self.editor.replace_draft(item.prompt.draft, cursor);
        self.paste_blocks.restore(item.prompt.paste_blocks);
        self.file_mentions
            .restore_accepted(self.editor.draft(), item.prompt.mentions);
        self.file_mentions
            .restore_family(item.prompt.family_mentions);
        self.file_mentions.dismiss();
        self.completion.dismiss();
        self.restored_contexts = item.restored_contexts;
        self.retry_of = item.retry_of;
        // Pending worker completions follow the frozen owner back into the editor.
        self.input_owner = item.owner;
        self.escape_confirmation = None;
        true
    }

    pub(super) fn local_delayed_lines(&self) -> Vec<String> {
        self.delayed_submissions
            .iter()
            .filter(|item| item.chat == self.chat)
            .map(|item| {
                let (ready, total) = item.prompt.paste_blocks.image_counts(&item.prompt.draft);
                let snippet: String = item.prompt.draft.chars().take(48).collect();
                let snippet = display_text(&snippet).replace('\n', " ");
                let status = if let Some(error) = &item.failed {
                    format!("failed: {}", display_text(error).replace('\n', " "))
                } else if item.in_flight {
                    "sending…".into()
                } else {
                    format!("waiting · images {ready}/{total}")
                };
                format!("Local {status} · {snippet} · up to edit")
            })
            .collect()
    }
}

pub(in super::super) struct DelayedSubmissionGuard<'a> {
    console: &'a Console,
    id: Uuid,
    accepted: std::cell::Cell<bool>,
}

impl DelayedSubmissionGuard<'_> {
    pub(in super::super) fn mark_accepted(&self) {
        self.accepted.set(true);
    }
}

impl Drop for DelayedSubmissionGuard<'_> {
    fn drop(&mut self) {
        let Ok(mut state) = self.console.state.lock() else {
            return;
        };
        if state.active_delayed_submission != Some(self.id) {
            return;
        }
        if let Some(index) = state
            .delayed_submissions
            .iter()
            .position(|entry| entry.id == self.id)
        {
            if self.accepted.get() {
                state.delayed_submissions.remove(index);
                // Release only our own waiting notice, never a newer draft warning.
                if state.status == WAITING_STATUS {
                    state.status.clear();
                    state.status_held = false;
                }
            } else {
                state.delayed_submissions[index]
                    .failed
                    .get_or_insert_with(|| "Not sent; take back to review and retry".into());
                state.delayed_submissions[index].in_flight = false;
            }
        }
        state.active_delayed_submission = None;
        state.delayed_restored_contexts = None;
        state.submitted_prompt = None;
        state.submitted_pasted_texts = None;
        state.submitted_retry_of = None;
        let _ = state.draw(&mut io::stdout().lock());
    }
}

impl Console {
    pub(in super::super) fn repark_delayed_submission(&self, id: Uuid) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if state.active_delayed_submission != Some(id)
            && let Some(entry) = state
                .delayed_submissions
                .iter_mut()
                .find(|entry| entry.id == id)
        {
            entry.in_flight = false;
        }
        Ok(())
    }

    pub(in super::super) fn begin_delayed_submission(
        &self,
        id: Uuid,
        selected_chat: Option<Uuid>,
    ) -> Result<Option<(InputEvent, DelayedSubmissionGuard<'_>)>, String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let Some(index) = state
            .delayed_submissions
            .iter()
            .position(|entry| entry.id == id)
        else {
            return Ok(None);
        };
        let valid = state.chat == selected_chat
            && state.delayed_submissions[index].chat == selected_chat
            && state.overview.projection.is_none()
            && state.active_delayed_submission.is_none()
            && state
                .delayed_submissions
                .iter()
                .position(|entry| entry.chat == selected_chat)
                == Some(index)
            && state.delayed_submissions[index].failed.is_none()
            && state.delayed_submissions[index].in_flight
            && !state.delayed_submissions[index]
                .prompt
                .paste_blocks
                .has_pending_images();
        if !valid {
            state.delayed_submissions[index].in_flight = false;
            return Ok(None);
        }
        let entry = state.delayed_submissions[index].clone();
        let text = entry
            .prompt
            .paste_blocks
            .source_text(&entry.prompt.draft)
            .to_owned();
        let images = entry.prompt.paste_blocks.images(&entry.prompt.draft);
        state.submitted_pasted_texts = Some((text.clone(), entry.pasted_texts));
        state.submitted_retry_of = entry.retry_of;
        state.submitted_prompt = Some(entry.prompt);
        state.delayed_restored_contexts = Some(entry.restored_contexts);
        state.active_delayed_submission = Some(id);
        Ok(Some((
            InputEvent::Prompt {
                family_target: entry.family_target,
                text,
                file_references: entry.file_references,
                images,
                send_now: entry.send_now,
            },
            DelayedSubmissionGuard {
                console: self,
                id,
                accepted: std::cell::Cell::new(false),
            },
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        State::new(true, 120, 30)
    }

    #[test]
    fn controlled_exit_keeps_waiting_snapshots_and_ctrl_c_still_clears_next_draft() {
        let mut state = state();
        state.prompt_enabled = true;
        state.editor.replace_draft("waiting".into(), 7);
        assert!(state.freeze_delayed_submission(Vec::new(), None));
        assert!(!state.request_exit('D'));
        assert!(!state.request_exit('D'));
        assert!(state.exit_confirmation.is_none());
        state.editor.replace_draft("next".into(), 4);
        state.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(state.editor.draft().is_empty());
        assert_eq!(state.delayed_submissions.len(), 1);
        state.editor.replace_draft("/exit".into(), 5);
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
                .is_none()
        );
        assert!(state.editor.draft().is_empty());
        assert!(state.status.contains("Unsent local messages"));
        assert!(state.take_back_delayed());
        assert_eq!(state.editor.draft(), "waiting");
    }

    #[test]
    fn compact_queue_rows_report_both_local_and_native_overflow() {
        let mut state = state();
        for _ in 0..3 {
            state.editor.replace_draft("waiting".into(), 7);
            assert!(state.freeze_delayed_submission(Vec::new(), None));
        }
        let chat = Uuid::new_v4();
        state.queued_inputs.push(QueuedInput {
            id: Uuid::new_v4(),
            chat_id: chat.into(),
            loop_id: None,
            kind: QueuedInputKind::Message,
            text: "native queued".into(),
            contexts: Vec::new(),
            queued_sequence: 0,
            armed: true,
        });
        assert!(state.visible_pending_input_lines(0).is_empty());
        let one = state.visible_pending_input_lines(1);
        assert_eq!(one.len(), 1);
        assert!(one[0].text().contains("2 local"));
        assert!(one[0].text().contains("1 Agent queued"));
        let two = state.visible_pending_input_lines(2);
        assert_eq!(two.len(), 2);
        assert!(two[1].text().contains("2 local"));
        assert!(two[1].text().contains("1 Agent queued"));
    }

    #[test]
    fn no_chat_fifo_migrates_only_while_its_first_submission_creates_chat() {
        let mut state = state();
        for text in ["first", "second"] {
            state.editor.replace_draft(text.into(), text.len());
            assert!(state.freeze_delayed_submission(Vec::new(), None));
        }
        let first = state.delayed_submissions[0].id;
        state.active_delayed_submission = Some(first);
        let chat = Uuid::new_v4();
        let console = super::super::tests::console_with_state(state);
        console.select_created_chat(chat, &[]).unwrap();
        let state = console.state.lock().unwrap();
        assert!(
            state
                .delayed_submissions
                .iter()
                .all(|entry| entry.chat == Some(chat))
        );
        assert!(state.editor.draft().is_empty());
    }

    #[test]
    fn failed_path_takeback_preserves_original_source_text() {
        let mut state = state();
        let id = state.paste_blocks.next_id();
        let text = PasteBlocks::image_placeholder(id).unwrap();
        state.editor.replace_draft(text.clone(), text.len());
        state
            .paste_blocks
            .register_pending_image(&text, 0..text.len(), id)
            .unwrap();
        let owner = state.input_owner;
        assert!(state.freeze_delayed_submission(Vec::new(), None));
        assert!(state.complete_delayed_image(
            owner,
            id,
            Err("File missing".into()),
            super::super::super::image_preparation::Pending {
                fallback: "missing.png".into(),
                missing_message: None
            }
        ));
        assert!(state.poll_delayed_submission().is_none());
        assert!(state.take_back_delayed());
        assert_eq!(state.editor.draft(), "missing.png");
    }

    #[test]
    fn clear_cancels_local_intent_without_discarding_content() {
        let chat = Uuid::new_v4();
        let console = Console::test_for_chat(chat);
        {
            let mut state = console.state.lock().unwrap();
            state.editor.replace_draft("kept".into(), 4);
            assert!(state.freeze_delayed_submission(Vec::new(), None));
        }
        console.clear_visible_agent().unwrap();
        let mut state = console.state.lock().unwrap();
        state.switch_view_chat(Some(chat));
        assert!(state.poll_delayed_submission().is_none());
        assert!(state.delayed_submissions[0].failed.is_some());
        assert!(state.take_back_delayed());
        assert_eq!(state.editor.draft(), "kept");
    }

    #[test]
    fn freeze_keeps_next_draft_and_takeback_does_not_overwrite_it() {
        let mut state = state();
        let owner = state.input_owner;
        state.editor.replace_draft("first".into(), 5);
        assert!(state.freeze_delayed_submission(Vec::new(), None));
        assert!(state.editor.draft().is_empty());
        assert_ne!(state.input_owner, owner);
        assert_eq!(state.delayed_submissions[0].owner, owner);
        state.editor.replace_draft("second".into(), 6);
        assert!(!state.take_back_delayed());
        assert_eq!(state.editor.draft(), "second");
        state.editor.replace_draft(String::new(), 0);
        assert!(state.take_back_delayed());
        assert_eq!(state.editor.draft(), "first");
        assert_eq!(state.input_owner, owner);
    }

    #[test]
    fn queue_admission_is_bounded_and_preserves_rejected_draft() {
        let mut state = state();
        assert!(!state.freeze_delayed_submission(Vec::new(), None));
        state.editor.replace_draft(" \n".into(), 2);
        assert!(!state.freeze_delayed_submission(Vec::new(), None));
        assert!(state.delayed_submissions.is_empty());
        for _ in 0..MAX_DELAYED_SUBMISSIONS {
            state.editor.replace_draft("waiting".into(), 7);
            assert!(state.freeze_delayed_submission(Vec::new(), None));
        }
        state.editor.replace_draft("kept".into(), 4);
        assert!(!state.freeze_delayed_submission(Vec::new(), None));
        assert_eq!(state.editor.draft(), "kept");
        assert_eq!(state.delayed_submissions.len(), MAX_DELAYED_SUBMISSIONS);
    }

    #[test]
    fn serialized_metadata_counts_towards_byte_limit() {
        let mut state = state();
        state.editor.replace_draft("kept".into(), 4);
        let large_path = PathBuf::from("a".repeat(MAX_DELAYED_BYTES));
        assert!(!state.freeze_delayed_submission(vec![large_path], None));
        assert_eq!(state.editor.draft(), "kept");
        assert!(state.delayed_submissions.is_empty());
    }

    #[test]
    fn guard_retains_unaccepted_and_removes_only_explicitly_accepted() {
        let chat = Uuid::new_v4();
        let console = Console::test_for_chat(chat);
        let id = {
            let mut state = console.state.lock().unwrap();
            state.editor.replace_draft("first".into(), 5);
            assert!(state.freeze_delayed_submission(Vec::new(), None));
            state.delayed_submissions[0].in_flight = true;
            state.delayed_submissions[0].id
        };
        let (_, guard) = console
            .begin_delayed_submission(id, Some(chat))
            .unwrap()
            .unwrap();
        {
            let mut state = console.state.lock().unwrap();
            state.editor.replace_draft("next draft".into(), 10);
        }
        drop(guard);
        {
            let mut state = console.state.lock().unwrap();
            assert!(state.delayed_submissions[0].failed.is_some());
            assert!(!state.delayed_submissions[0].in_flight);
            assert_eq!(state.editor.draft(), "next draft");
            state.delayed_submissions[0].failed = None;
            state.delayed_submissions[0].in_flight = true;
        }
        let (_, guard) = console
            .begin_delayed_submission(id, Some(chat))
            .unwrap()
            .unwrap();
        guard.mark_accepted();
        drop(guard);
        let state = console.state.lock().unwrap();
        assert!(state.delayed_submissions.is_empty());
        assert_eq!(state.editor.draft(), "next draft");
    }

    #[test]
    fn changed_chat_reparks_instead_of_sending_to_wrong_chat() {
        let chat = Uuid::new_v4();
        let console = Console::test_for_chat(chat);
        let id = {
            let mut state = console.state.lock().unwrap();
            state.editor.replace_draft("first".into(), 5);
            assert!(state.freeze_delayed_submission(Vec::new(), None));
            state.delayed_submissions[0].in_flight = true;
            state.delayed_submissions[0].id
        };
        assert!(
            console
                .begin_delayed_submission(id, Some(Uuid::new_v4()))
                .unwrap()
                .is_none()
        );
        let state = console.state.lock().unwrap();
        assert!(!state.delayed_submissions[0].in_flight);
        assert!(state.active_delayed_submission.is_none());
    }

    #[test]
    fn oversized_completion_retains_reservation_and_takeback_removes_only_failed_marker() {
        let mut state = state();
        let id = state.paste_blocks.next_id();
        let draft = PasteBlocks::image_placeholder(id).unwrap();
        state
            .paste_blocks
            .register_pending_image(&draft, 0..draft.len(), id)
            .unwrap();
        state.editor.replace_draft(draft.clone(), draft.len());
        let owner = state.input_owner;
        assert!(state.freeze_delayed_submission(Vec::new(), None));
        let image = Image {
            media_type: tickloop_harness_protocol::ImageMediaType::Png,
            data: "a".repeat(MAX_DELAYED_BYTES),
            width: 1,
            height: 1,
        };
        assert!(state.complete_delayed_image(
            owner,
            id,
            Ok(Some(image)),
            super::super::super::image_preparation::Pending {
                fallback: String::new(),
                missing_message: None
            }
        ));
        assert!(state.delayed_submissions[0].failed.is_some());
        assert!(
            state.delayed_submissions[0]
                .prompt
                .paste_blocks
                .has_pending_images()
        );
        assert!(
            state.delayed_submissions[0]
                .prompt
                .paste_blocks
                .images(&draft)
                .is_empty()
        );
        assert!(state.take_back_delayed());
        assert!(state.editor.draft().is_empty());
        assert!(!state.paste_blocks.has_pending_images());
    }

    #[test]
    fn takeback_uses_unsent_tail() {
        let mut state = state();
        for text in ["oldest", "newest"] {
            state.editor.replace_draft(text.into(), text.len());
            assert!(state.freeze_delayed_submission(Vec::new(), None));
        }
        assert!(state.take_back_delayed());
        assert_eq!(state.editor.draft(), "newest");
        assert_eq!(state.delayed_submissions[0].prompt.draft, "oldest");
    }

    #[test]
    fn failed_oldest_blocks_later_and_inflight_cannot_be_taken_back() {
        let mut state = state();
        for _ in 0..2 {
            state.editor.replace_draft("waiting".into(), 7);
            assert!(state.freeze_delayed_submission(Vec::new(), None));
        }
        state.delayed_submissions[0].failed = Some("unavailable".into());
        assert!(state.poll_delayed_submission().is_none());
        state.delayed_submissions[0].failed = None;
        state.delayed_submissions[0].in_flight = true;
        state.delayed_submissions[1].in_flight = true;
        assert!(state.poll_delayed_submission().is_none());
        assert!(!state.take_back_delayed());
    }
}
