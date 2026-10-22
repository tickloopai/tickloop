//! Test handshakes around family stop admission and completion recording.

use std::collections::HashMap;
use std::sync::Mutex;

use tickloop_agent_protocol::ChatId;
use tokio::sync::oneshot;

use crate::Agent;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Point {
    CompletionRecorded,
    StopAdmitted,
}

type Gate = (oneshot::Sender<()>, oneshot::Receiver<()>);
type At = (usize, ChatId, Point);
static GATES: Mutex<Option<HashMap<At, Gate>>> = Mutex::new(None);

pub struct Pause {
    pub entered: oneshot::Receiver<()>,
    pub release: oneshot::Sender<()>,
}

impl Agent {
    #[doc(hidden)]
    pub fn pause_family_for_tests(&self, chat: ChatId, point: Point) -> Pause {
        let (entered_sender, entered) = oneshot::channel();
        let (release, receiver) = oneshot::channel();
        GATES
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(
                (std::sync::Arc::as_ptr(&self.inner) as usize, chat, point),
                (entered_sender, receiver),
            );
        Pause { entered, release }
    }

    pub(crate) async fn wait_family_pause(&self, chat: ChatId, point: Point) {
        let gate = GATES.lock().unwrap().as_mut().and_then(|gates| {
            gates.remove(&(std::sync::Arc::as_ptr(&self.inner) as usize, chat, point))
        });
        if let Some((entered, release)) = gate {
            let _ = entered.send(());
            let _ = release.await;
        }
    }
}
