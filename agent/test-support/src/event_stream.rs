//! An Event Stream in memory with the epoch fence a stateless host publishes under: `activate`
//! raises the fence to the new holder's lease epoch before it publishes anything, and `publish`
//! is accepted only at exactly the fenced epoch.

use std::collections::HashMap;
use std::sync::Mutex;

use tickloop_agent_protocol::Event;
use tickloop_agent_runtime::transcript::Line;
use uuid::Uuid;

/// What one stream entry carries to clients.
#[derive(Clone, Debug)]
pub enum StreamItem {
    /// The Lines one append committed; the entry's seq became the Chat's committed `last_seq`.
    Committed { lines: Vec<Line> },
    /// A Harness event, released in order behind the commits that reflect it.
    Event(Box<Event>),
    /// Clients reload the history: something before this seq may never reach them.
    Resync,
}

#[derive(Clone, Debug)]
pub struct StreamEntry {
    pub seq: u64,
    pub item: StreamItem,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamError {
    /// A newer epoch activated the stream.
    StaleEpoch { fence: u64 },
    /// The publishing epoch never activated the stream.
    NotActivated { fence: u64 },
    /// The first seq is not above the stream's top.
    SeqConflict { top: u64 },
    /// Nothing was written; publishing again may succeed.
    Unavailable,
}

/// A publish the stream refused.
#[derive(Clone, Debug)]
pub struct Rejection {
    pub chat: Uuid,
    pub epoch: u64,
    pub error: StreamError,
}

type PublishObserver = Box<dyn Fn(Uuid, &[StreamEntry]) + Send + Sync>;

#[derive(Default)]
pub struct MemoryEventStream {
    state: Mutex<StreamState>,
}

#[derive(Default)]
struct StreamState {
    chats: HashMap<Uuid, StreamChat>,
    unavailable: bool,
    failing_publishes: usize,
    rejections: Vec<Rejection>,
    observers: Vec<PublishObserver>,
}

#[derive(Default)]
struct StreamChat {
    fence: u64,
    top: u64,
    entries: Vec<StreamEntry>,
}

impl MemoryEventStream {
    pub fn new() -> Self {
        Self::default()
    }

    /// Raises the Chat's fence to `epoch`, refusing a lower one, and returns the stream's top
    /// seq (0 when nothing was published). No lower epoch publishes from now on, even before
    /// this epoch's first publish.
    pub fn activate(&self, chat: Uuid, epoch: u64) -> Result<u64, StreamError> {
        let mut state = self.state.lock().unwrap();
        if state.unavailable {
            state.rejections.push(Rejection {
                chat,
                epoch,
                error: StreamError::Unavailable,
            });
            return Err(StreamError::Unavailable);
        }
        let stream = state.chats.entry(chat).or_default();
        if stream.fence > epoch {
            let error = StreamError::StaleEpoch {
                fence: stream.fence,
            };
            state.rejections.push(Rejection {
                chat,
                epoch,
                error: error.clone(),
            });
            return Err(error);
        }
        stream.fence = epoch;
        Ok(stream.top)
    }

    /// Appends `entries` in order when `epoch` is exactly the activated fence and the first
    /// seq is above the top. Either all of them are written or none.
    pub fn publish(
        &self,
        chat: Uuid,
        epoch: u64,
        entries: &[StreamEntry],
    ) -> Result<(), StreamError> {
        let mut state = self.state.lock().unwrap();
        let checked = state.check_publish(chat, epoch, entries);
        if let Err(error) = checked {
            state.rejections.push(Rejection {
                chat,
                epoch,
                error: error.clone(),
            });
            return Err(error);
        }
        for observer in &state.observers {
            observer(chat, entries);
        }
        let stream = state.chats.get_mut(&chat).unwrap();
        if let Some(last) = entries.last() {
            stream.top = last.seq;
        }
        stream.entries.extend(entries.iter().cloned());
        Ok(())
    }

    /// The next `count` publishes fail without writing anything.
    pub fn fail_next_publishes(&self, count: usize) {
        self.state.lock().unwrap().failing_publishes = count;
    }

    /// Every activation and publish fails without writing anything until this is called with
    /// `false`.
    pub fn set_unavailable(&self, unavailable: bool) {
        self.state.lock().unwrap().unavailable = unavailable;
    }

    /// Calls `observer` with every accepted publish just before it is written, while nothing
    /// else can publish.
    pub fn observe(&self, observer: impl Fn(Uuid, &[StreamEntry]) + Send + Sync + 'static) {
        self.state
            .lock()
            .unwrap()
            .observers
            .push(Box::new(observer));
    }

    pub fn entries(&self, chat: Uuid) -> Vec<StreamEntry> {
        self.read_after(chat, 0)
    }

    pub fn read_after(&self, chat: Uuid, seq: u64) -> Vec<StreamEntry> {
        self.state
            .lock()
            .unwrap()
            .chats
            .get(&chat)
            .map(|stream| {
                stream
                    .entries
                    .iter()
                    .filter(|entry| entry.seq > seq)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn top(&self, chat: Uuid) -> u64 {
        self.state
            .lock()
            .unwrap()
            .chats
            .get(&chat)
            .map_or(0, |stream| stream.top)
    }

    pub fn rejections(&self) -> Vec<Rejection> {
        self.state.lock().unwrap().rejections.clone()
    }
}

impl StreamState {
    fn check_publish(
        &mut self,
        chat: Uuid,
        epoch: u64,
        entries: &[StreamEntry],
    ) -> Result<(), StreamError> {
        let stream = self.chats.entry(chat).or_default();
        if stream.fence > epoch {
            return Err(StreamError::StaleEpoch {
                fence: stream.fence,
            });
        }
        if stream.fence < epoch {
            return Err(StreamError::NotActivated {
                fence: stream.fence,
            });
        }
        let mut previous = stream.top;
        for entry in entries {
            if entry.seq <= previous {
                return Err(StreamError::SeqConflict { top: stream.top });
            }
            previous = entry.seq;
        }
        if self.unavailable {
            return Err(StreamError::Unavailable);
        }
        if self.failing_publishes > 0 {
            self.failing_publishes -= 1;
            return Err(StreamError::Unavailable);
        }
        Ok(())
    }
}
