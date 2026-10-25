//! Transcript stores for tests: one kept in memory, and a wrapper that injects storage faults
//! into any store.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tickloop_harness_protocol::ChatId;
use tickloop_harness_runtime::transcript::{
    AppendAck, AppendBatch, BatchId, ChatPlacement, LoadRange, StorePosition, StoredTranscript,
    TranscriptError, TranscriptStore,
};
use tickloop_harness_transcript::Line;

/// A store in memory that enforces expected positions, can leave gaps between positions, as a
/// store numbering events as well as Lines does, and can be fenced by a newer holder.
#[derive(Default)]
pub struct InMemoryTranscriptStore {
    state: Mutex<MemoryState>,
}

#[derive(Default)]
struct MemoryState {
    chats: HashMap<ChatId, MemoryChat>,
    position_gap: u64,
    fenced: bool,
}

#[derive(Default)]
struct MemoryChat {
    lines: Vec<Line>,
    position: StorePosition,
    committed: HashMap<BatchId, AppendAck>,
    received: Vec<AppendBatch>,
}

impl InMemoryTranscriptStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Each commit also skips `gap` positions.
    pub fn with_position_gaps(gap: u64) -> Self {
        let store = Self::default();
        store.state.lock().unwrap().position_gap = gap;
        store
    }

    /// History a previous holder committed, possibly sealed long ago, at a position that does
    /// not count its Lines.
    pub fn seed(&self, chat: ChatId, lines: Vec<Line>, position: StorePosition) {
        let mut state = self.state.lock().unwrap();
        let stored = state.chats.entry(chat).or_default();
        stored.lines = lines;
        stored.position = position;
    }

    /// A newer holder took the Chats over: every later registration, load and append is fenced.
    pub fn fence(&self) {
        self.state.lock().unwrap().fenced = true;
    }

    pub fn lines(&self, chat: ChatId) -> Vec<Line> {
        self.state
            .lock()
            .unwrap()
            .chats
            .get(&chat)
            .map(|stored| stored.lines.clone())
            .unwrap_or_default()
    }

    pub fn position(&self, chat: ChatId) -> Option<StorePosition> {
        self.state
            .lock()
            .unwrap()
            .chats
            .get(&chat)
            .map(|stored| stored.position)
    }

    /// Every append the store received for the Chat, retries included, in order.
    pub fn received(&self, chat: ChatId) -> Vec<AppendBatch> {
        self.state
            .lock()
            .unwrap()
            .chats
            .get(&chat)
            .map(|stored| stored.received.clone())
            .unwrap_or_default()
    }

    fn commit(&self, chat: ChatId, batch: &AppendBatch) -> Result<AppendAck, TranscriptError> {
        let mut state = self.state.lock().unwrap();
        if state.fenced {
            return Err(TranscriptError::Fenced);
        }
        let gap = state.position_gap;
        let stored = state.chats.get_mut(&chat).ok_or_else(|| {
            TranscriptError::Storage(format!("Chat {} is not registered", chat.public()))
        })?;
        stored.received.push(batch.clone());
        if let Some(ack) = stored.committed.get(&batch.id) {
            return Ok(*ack);
        }
        if let Some(expected) = batch.expected
            && expected != stored.position
        {
            return Err(TranscriptError::Conflict(format!(
                "expected position {} but the Transcript is at {}",
                expected.0, stored.position.0
            )));
        }
        if !batch.lines.is_empty() {
            stored.lines.extend(batch.lines.iter().cloned());
            stored.position = StorePosition(stored.position.0 + batch.lines.len() as u64 + gap);
        }
        let ack = AppendAck {
            position: stored.position,
        };
        stored.committed.insert(batch.id, ack);
        Ok(ack)
    }
}

#[async_trait]
impl TranscriptStore for InMemoryTranscriptStore {
    async fn register(
        &self,
        chat: ChatId,
        _placement: &ChatPlacement,
    ) -> Result<StorePosition, TranscriptError> {
        let mut state = self.state.lock().unwrap();
        if state.fenced {
            return Err(TranscriptError::Fenced);
        }
        Ok(state.chats.entry(chat).or_default().position)
    }

    async fn load(
        &self,
        chat: ChatId,
        range: LoadRange,
    ) -> Result<Option<StoredTranscript>, TranscriptError> {
        let state = self.state.lock().unwrap();
        if state.fenced {
            return Err(TranscriptError::Fenced);
        }
        let Some(stored) = state
            .chats
            .get(&chat)
            .filter(|stored| !stored.lines.is_empty())
        else {
            return Ok(None);
        };
        let start = match range {
            LoadRange::Full => 0,
            LoadRange::FromLatestBoundary => stored
                .lines
                .iter()
                .rposition(|line| matches!(line, Line::System(entry) if entry.restarts_chain()))
                .unwrap_or(0),
        };
        // The chain from the latest compact boundary, the retained Lines from the latest snapshot.
        Ok(Some(StoredTranscript {
            retained: tickloop_harness_runtime::transcript::retained_before(&stored.lines, start),
            lines: stored.lines[start..].to_vec(),
            position: stored.position,
            warnings: Vec::new(),
        }))
    }

    async fn append(
        &self,
        chat: ChatId,
        batch: &AppendBatch,
    ) -> Result<AppendAck, TranscriptError> {
        self.commit(chat, batch)
    }

    fn append_blocking(
        &self,
        chat: ChatId,
        batch: &AppendBatch,
    ) -> Result<AppendAck, TranscriptError> {
        self.commit(chat, batch)
    }
}

/// Wraps a store and makes its appends fail, or its loads wait, on request.
pub struct FaultyTranscriptStore {
    inner: Arc<dyn TranscriptStore>,
    faults: Mutex<Faults>,
    loads_released: tokio::sync::Notify,
    appends_released: tokio::sync::Notify,
}

#[derive(Default)]
struct Faults {
    lose_next_acknowledgement: bool,
    failing: bool,
    fenced: bool,
    appends: usize,
    registrations: usize,
    batches: Vec<BatchId>,
    holds_loads: bool,
    waiting_loads: usize,
    holds_appends: bool,
    waiting_appends: usize,
}

impl FaultyTranscriptStore {
    pub fn new(inner: Arc<dyn TranscriptStore>) -> Self {
        Self {
            inner,
            faults: Mutex::default(),
            loads_released: tokio::sync::Notify::new(),
            appends_released: tokio::sync::Notify::new(),
        }
    }

    /// Every later append reaches the store and waits there until `release_appends`.
    pub fn hold_appends(&self) {
        self.faults.lock().unwrap().holds_appends = true;
    }

    pub fn release_appends(&self) {
        self.faults.lock().unwrap().holds_appends = false;
        self.appends_released.notify_waiters();
    }

    /// How many appends wait in the store for `release_appends`.
    pub fn waiting_appends(&self) -> usize {
        self.faults.lock().unwrap().waiting_appends
    }

    /// Every later load reads what the store holds now, then waits until `release_loads` to
    /// return it.
    pub fn hold_loads(&self) {
        self.faults.lock().unwrap().holds_loads = true;
    }

    pub fn release_loads(&self) {
        self.faults.lock().unwrap().holds_loads = false;
        self.loads_released.notify_waiters();
    }

    /// How many loads wait for `release_loads`.
    pub fn waiting_loads(&self) -> usize {
        self.faults.lock().unwrap().waiting_loads
    }

    /// The next append commits, then reports a storage error.
    pub fn lose_next_acknowledgement(&self) {
        self.faults.lock().unwrap().lose_next_acknowledgement = true;
    }

    /// Every append fails without committing until `heal`.
    pub fn fail_appends(&self) {
        self.faults.lock().unwrap().failing = true;
    }

    pub fn heal(&self) {
        self.faults.lock().unwrap().failing = false;
    }

    /// Every later append is fenced without committing.
    pub fn fence(&self) {
        self.faults.lock().unwrap().fenced = true;
    }

    /// How many appends reached this wrapper, retries and refused ones included.
    pub fn appends(&self) -> usize {
        self.faults.lock().unwrap().appends
    }

    /// How many registrations reached this wrapper.
    pub fn registrations(&self) -> usize {
        self.faults.lock().unwrap().registrations
    }

    /// The batch of every append that reached this wrapper, in order.
    pub fn batches(&self) -> Vec<BatchId> {
        self.faults.lock().unwrap().batches.clone()
    }

    fn before_append(&self, batch: &AppendBatch) -> Result<(), TranscriptError> {
        let mut faults = self.faults.lock().unwrap();
        faults.appends += 1;
        faults.batches.push(batch.id);
        if faults.fenced {
            return Err(TranscriptError::Fenced);
        }
        if faults.failing {
            return Err(TranscriptError::Storage("injected storage failure".into()));
        }
        Ok(())
    }

    fn after_append(
        &self,
        result: Result<AppendAck, TranscriptError>,
    ) -> Result<AppendAck, TranscriptError> {
        let ack = result?;
        let mut faults = self.faults.lock().unwrap();
        if std::mem::take(&mut faults.lose_next_acknowledgement) {
            return Err(TranscriptError::Storage(
                "injected: the acknowledgement was lost".into(),
            ));
        }
        Ok(ack)
    }
}

#[async_trait]
impl TranscriptStore for FaultyTranscriptStore {
    async fn register(
        &self,
        chat: ChatId,
        placement: &ChatPlacement,
    ) -> Result<StorePosition, TranscriptError> {
        self.faults.lock().unwrap().registrations += 1;
        self.inner.register(chat, placement).await
    }

    async fn load(
        &self,
        chat: ChatId,
        range: LoadRange,
    ) -> Result<Option<StoredTranscript>, TranscriptError> {
        let loaded = self.inner.load(chat, range).await;
        loop {
            let released = self.loads_released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            {
                let mut faults = self.faults.lock().unwrap();
                if !faults.holds_loads {
                    break;
                }
                faults.waiting_loads += 1;
            }
            released.await;
            self.faults.lock().unwrap().waiting_loads -= 1;
        }
        loaded
    }

    async fn append(
        &self,
        chat: ChatId,
        batch: &AppendBatch,
    ) -> Result<AppendAck, TranscriptError> {
        self.before_append(batch)?;
        loop {
            let released = self.appends_released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            {
                let mut faults = self.faults.lock().unwrap();
                if !faults.holds_appends {
                    break;
                }
                faults.waiting_appends += 1;
            }
            released.await;
            self.faults.lock().unwrap().waiting_appends -= 1;
        }
        let result = self.inner.append(chat, batch).await;
        self.after_append(result)
    }

    fn append_blocking(
        &self,
        chat: ChatId,
        batch: &AppendBatch,
    ) -> Result<AppendAck, TranscriptError> {
        self.before_append(batch)?;
        let result = self.inner.append_blocking(chat, batch);
        self.after_append(result)
    }
}
