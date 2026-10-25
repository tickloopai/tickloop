//! Bounded image work off the terminal input thread. Only prepared results cross back to the UI.
use std::{collections::HashMap, sync::mpsc, thread};

use tickloop_harness_protocol::Image;
use uuid::Uuid;

use super::{image_cache::ImageCache, paste_blocks::PasteBlockId};

const MAX_PENDING: usize = 8;
type ReadImage = Box<dyn FnOnce() -> Result<Option<Image>, String> + Send>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Key {
    pub(super) owner: Uuid,
    pub(super) id: PasteBlockId,
}

pub(super) struct Pending {
    pub(super) fallback: String,
    pub(super) missing_message: Option<String>,
}

struct Job {
    key: Key,
    read: ReadImage,
    cache: Option<(ImageCache, Uuid, u32)>,
}

pub(super) struct Prepared {
    pub(super) key: Key,
    pub(super) result: Result<Option<Image>, String>,
}

enum Event {
    Prepared(Prepared),
    Finished,
}

#[derive(Default)]
pub(super) struct ImagePreparation {
    sender: Option<mpsc::SyncSender<Job>>,
    receiver: Option<mpsc::Receiver<Event>>,
    outstanding: usize,
    pub(super) pending: HashMap<Key, Pending>,
}

impl ImagePreparation {
    pub(super) fn submit(
        &mut self,
        key: Key,
        pending: Pending,
        cache: Option<(ImageCache, Uuid, u32)>,
        read: impl FnOnce() -> Result<Option<Image>, String> + Send + 'static,
    ) -> Result<(), String> {
        if self.outstanding >= MAX_PENDING {
            return Err("Too many images preparing; wait before pasting again".into());
        }
        if self.sender.is_none() {
            let (sender, jobs) = mpsc::sync_channel::<Job>(MAX_PENDING);
            let (events, receiver) = mpsc::channel();
            thread::Builder::new()
                .name("agent-image-preparation".into())
                .spawn(move || {
                    while let Ok(job) = jobs.recv() {
                        let result =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(job.read))
                                .unwrap_or_else(|_| Err("Image preparation failed".into()));
                        // The link cache is optional: publish the attachment before doing disk I/O.
                        let cached_image = job
                            .cache
                            .as_ref()
                            .and_then(|_| result.as_ref().ok()?.clone());
                        if events
                            .send(Event::Prepared(Prepared {
                                key: job.key,
                                result,
                            }))
                            .is_err()
                        {
                            break;
                        }
                        if let (Some((cache, chat, id)), Some(image)) = (job.cache, cached_image) {
                            let _ = cache.store(chat, id, &image);
                        }
                        if events.send(Event::Finished).is_err() {
                            break;
                        }
                    }
                })
                .map_err(|error| format!("Cannot start image preparation: {error}"))?;
            self.sender = Some(sender);
            self.receiver = Some(receiver);
        }
        self.sender
            .as_ref()
            .expect("worker started")
            .try_send(Job {
                key,
                read: Box::new(read),
                cache,
            })
            .map_err(|_| "Image preparation worker unavailable; try again".to_owned())?;
        self.outstanding += 1;
        self.pending.insert(key, pending);
        Ok(())
    }

    pub(super) fn poll(&mut self) -> Vec<Prepared> {
        let mut prepared = Vec::new();
        if let Some(receiver) = &self.receiver {
            while let Ok(event) = receiver.try_recv() {
                match event {
                    Event::Prepared(result) => prepared.push(result),
                    Event::Finished => self.outstanding = self.outstanding.saturating_sub(1),
                }
            }
        }
        prepared
    }

    #[cfg(test)]
    pub(super) fn is_idle(&self) -> bool {
        self.outstanding == 0
    }
}
