use std::{collections::hash_map::Entry, sync::Arc, time::Duration};

use tokio::time::Instant;

use super::{Agent, Chat, ChatId, EventKind, LoopId, Progress, Receipt, TodoItem, TodoStatus};

const TODO_COMPLETION_GRACE: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(super) struct CompletionGrace {
    deadline: Instant,
    todos: Vec<TodoItem>,
}

impl Agent {
    pub(super) fn track_todo_progress(&self, chat: &mut Chat, progress: &Progress) {
        if progress.todos.is_empty()
            || progress
                .todos
                .iter()
                .any(|todo| todo.status != TodoStatus::Completed)
        {
            chat.todo_completion_graces.remove(&progress.loop_id);
            return;
        }
        match chat.todo_completion_graces.entry(progress.loop_id) {
            Entry::Occupied(mut entry) => entry.get_mut().todos = progress.todos.clone(),
            Entry::Vacant(entry) => {
                let deadline = Instant::now() + TODO_COMPLETION_GRACE;
                entry.insert(CompletionGrace {
                    deadline,
                    todos: progress.todos.clone(),
                });
                let owner = Arc::downgrade(&self.inner);
                let shutdown = self.inner.shutdown.clone();
                let chat_id = chat.snapshot.id;
                let loop_id = progress.loop_id;
                tokio::spawn(async move {
                    tokio::select! {
                        _ = shutdown.cancelled() => {},
                        _ = tokio::time::sleep_until(deadline) => {
                            if let Some(inner) = owner.upgrade() {
                                Agent { inner }.retire_completed_todos(chat_id, loop_id, deadline).await;
                            }
                        }
                    }
                });
            }
        }
    }

    pub(super) async fn start_restored_todo_graces(&self, chat_id: ChatId) {
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&chat_id) else {
            return;
        };
        for progress in chat.snapshot.progress.clone() {
            self.track_todo_progress(chat, &progress);
        }
    }

    async fn retire_completed_todos(&self, chat_id: ChatId, loop_id: LoopId, deadline: Instant) {
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&chat_id) else {
            return;
        };
        let Some(grace) = chat.todo_completion_graces.get(&loop_id) else {
            return;
        };
        if grace.deadline != deadline || Instant::now() < deadline {
            return;
        }
        let Some(progress) = chat
            .snapshot
            .progress
            .iter_mut()
            .find(|p| p.loop_id == loop_id)
        else {
            return;
        };
        if progress.todos != grace.todos {
            return;
        }
        progress.todos.clear();
        let progress = progress.clone();
        chat.todo_completion_graces.remove(&loop_id);
        if let Some(recorder) = &self.inner.recorder {
            recorder.record_todo_retirement(&chat.snapshot, loop_id);
        }
        let run_id = chat
            .snapshot
            .loops
            .iter()
            .find(|goal| goal.id == loop_id)
            .and_then(|goal| goal.run_id)
            .or_else(|| {
                chat.snapshot
                    .message_runs
                    .iter()
                    .rev()
                    .find_map(|(_, run)| {
                        (chat.snapshot.run_loops.get(run) == Some(&loop_id)).then_some(*run)
                    })
            });
        if let Some(run_id) = run_id {
            self.emit(
                chat,
                &Receipt {
                    chat_id,
                    loop_id,
                    run_id,
                },
                EventKind::ProgressUpdated { progress },
            );
        }
        self.save(&chat.snapshot);
    }
}
