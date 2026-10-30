//! Test seam: holds a server's work at a named point until the test releases it, so a test can
//! interleave it with other work.

use std::collections::HashMap;
use std::sync::{Mutex, mpsc};

/// Where work waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PausePoint {
    /// A redactor build, once it knows its new secrets and before it is installed.
    RedactorBuild,
    /// A renewal on an open connection, once it has its token and before it takes it up.
    Renewal,
    /// A reconcile keeping a server's entry, once it found the saved login the entry's own and
    /// before it takes it up.
    Reconcile,
    /// Remembering a saved login whose token could not be used, once it found the login the
    /// entry's own and before it takes it up.
    Invalidation,
    /// A prepared call's check just before it is sent, after it was admitted.
    Revalidate,
}

type Gate = (mpsc::Sender<()>, mpsc::Receiver<()>);

static GATES: Mutex<Option<HashMap<(PausePoint, String), Gate>>> = Mutex::new(None);

/// Holds the next time the server `server` reaches `point`: `entered` hears when it waits, and
/// sending on `release`, or dropping it, lets it go on.
pub struct Pause {
    pub entered: mpsc::Receiver<()>,
    pub release: mpsc::Sender<()>,
}

pub fn pause_next(point: PausePoint, server: &str) -> Pause {
    let (entered_sender, entered) = mpsc::channel();
    let (release, release_receiver) = mpsc::channel();
    GATES
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(
            (point, server.to_owned()),
            (entered_sender, release_receiver),
        );
    Pause { entered, release }
}

pub(in crate::mcp) fn wait(point: PausePoint, server: &str) {
    let gate = GATES
        .lock()
        .unwrap()
        .as_mut()
        .and_then(|gates| gates.remove(&(point, server.to_owned())));
    if let Some((entered, release)) = gate {
        let _ = entered.send(());
        let _ = release.recv();
    }
}
