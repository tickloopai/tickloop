use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread;
use uuid::Uuid;

use super::super::file_mentions::{FileMentionIndex, IndexGeneration, MentionDirectory};
use super::State;

pub(super) struct IndexRequest {
    pub owner: Uuid,
    pub generation: IndexGeneration,
    pub project: PathBuf,
    pub additional: Vec<PathBuf>,
    pub directory: MentionDirectory,
}

struct Job {
    request: IndexRequest,
    canceled: Arc<AtomicBool>,
}

#[derive(Default)]
struct Queue {
    wanted: Option<(Uuid, IndexGeneration)>,
    pending: Option<Job>,
    active: Option<Arc<AtomicBool>>,
    closed: bool,
}

pub(super) struct FileIndexWorker {
    queue: Arc<(Mutex<Queue>, Condvar)>,
}

impl FileIndexWorker {
    pub(super) fn start(state: Weak<Mutex<State>>, stop: Arc<AtomicBool>) -> io::Result<Arc<Self>> {
        let worker = Arc::new(Self {
            queue: Arc::new((Mutex::new(Queue::default()), Condvar::new())),
        });
        let queue = worker.queue.clone();
        thread::Builder::new()
            .name("agent-file-index".into())
            .spawn(move || {
                loop {
                    let job = {
                        let (lock, ready) = &*queue;
                        let Ok(mut pending) = lock.lock() else {
                            return;
                        };
                        while !pending.closed && pending.pending.is_none() {
                            let Ok(next) = ready.wait(pending) else {
                                return;
                            };
                            pending = next;
                        }
                        if pending.closed || stop.load(Ordering::Acquire) {
                            return;
                        }
                        let Some(job) = pending.pending.take() else {
                            continue;
                        };
                        pending.active = Some(job.canceled.clone());
                        job
                    };
                    let canceled =
                        || job.canceled.load(Ordering::Acquire) || stop.load(Ordering::Acquire);
                    let request = &job.request;
                    let index = FileMentionIndex::build_directory(
                        &request.project,
                        &request.additional,
                        request.directory.clone(),
                        &canceled,
                    );
                    if !canceled()
                        && let Ok(index) = index
                        && let Some(state) = state.upgrade()
                        && let Ok(mut state) = state.lock()
                        && !canceled()
                        && state.replace_file_index(request.owner, request.generation, index)
                    {
                        state.refresh_completion();
                        let _ = state.draw(&mut io::stdout().lock());
                    }
                }
            })?;
        Ok(worker)
    }

    pub(super) fn request(&self, request: Option<IndexRequest>) {
        let (lock, ready) = &*self.queue;
        let Ok(mut queue) = lock.lock() else {
            return;
        };
        let wanted = request
            .as_ref()
            .map(|request| (request.owner, request.generation));
        if queue.closed || queue.wanted == wanted {
            return;
        }
        if let Some(active) = queue.active.take() {
            active.store(true, Ordering::Release);
        }
        if let Some(pending) = queue.pending.take() {
            pending.canceled.store(true, Ordering::Release);
        }
        queue.wanted = wanted;
        queue.pending = request.map(|request| Job {
            request,
            canceled: Arc::new(AtomicBool::new(false)),
        });
        ready.notify_one();
    }

    pub(super) fn close(&self) {
        let (lock, ready) = &*self.queue;
        if let Ok(mut queue) = lock.lock() {
            queue.closed = true;
            if let Some(active) = queue.active.take() {
                active.store(true, Ordering::Release);
            }
            queue.pending = None;
            ready.notify_one();
        }
    }
}

impl Drop for FileIndexWorker {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::console_with_state;
    use super::*;

    #[test]
    fn plain_prompts_never_scan_and_hidden_or_attached_inputs_cancel_their_scan() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("one.txt"), "one").unwrap();
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        let console = console_with_state(state);
        console
            .set_input_context(None, directory.path(), &[])
            .unwrap();
        let mut state = console.state.lock().unwrap();
        let worker = state.file_index_worker.clone().unwrap();
        state.insert("看看特斯拉K线");
        assert!(worker.queue.0.lock().unwrap().wanted.is_none());
        state.replace_draft("@".into(), 1);
        let canceled = {
            let queue = worker.queue.0.lock().unwrap();
            assert!(queue.wanted.is_some());
            queue
                .active
                .clone()
                .or_else(|| queue.pending.as_ref().map(|job| job.canceled.clone()))
                .unwrap()
        };
        state.terminal_lent = true;
        state.schedule_file_index();
        assert!(canceled.load(Ordering::Acquire));
        assert!(worker.queue.0.lock().unwrap().wanted.is_none());
        state.terminal_lent = false;
        state.schedule_file_index();
        assert!(worker.queue.0.lock().unwrap().wanted.is_some());
        state.file_index_focused = false;
        state.schedule_file_index();
        assert!(worker.queue.0.lock().unwrap().wanted.is_none());
        drop(state);
    }

    #[test]
    fn pending_requests_are_coalesced_and_superseded_scans_are_canceled() {
        let worker = FileIndexWorker {
            queue: Arc::new((Mutex::new(Queue::default()), Condvar::new())),
        };
        let mut mentions = super::super::FileMentionState::default();
        let owner = Uuid::new_v4();
        let mut request = || IndexRequest {
            owner,
            generation: mentions.request_index_generation(),
            project: PathBuf::from("/project"),
            additional: Vec::new(),
            directory: MentionDirectory::Roots,
        };
        worker.request(Some(request()));
        let first = worker
            .queue
            .0
            .lock()
            .unwrap()
            .pending
            .as_ref()
            .unwrap()
            .canceled
            .clone();
        worker.request(Some(request()));
        assert!(first.load(Ordering::Acquire));
        let second = worker
            .queue
            .0
            .lock()
            .unwrap()
            .pending
            .as_ref()
            .unwrap()
            .canceled
            .clone();
        worker.close();
        assert!(worker.queue.0.lock().unwrap().pending.is_none());
        assert!(worker.queue.0.lock().unwrap().closed);
        assert!(!Arc::ptr_eq(&first, &second));
    }
}
