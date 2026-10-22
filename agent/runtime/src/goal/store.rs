//! The Chat's goal as the store keeps it for every process that holds the Chat:
//! `<Chat directory>/goal/state.json`, replaced atomically under the flock of `goal/.lock`. Each
//! change reads the stored goal, applies the event to it and writes the result with the next
//! generation, so a process never writes back a goal another process cleared, paused or edited;
//! a process adopts a stored goal whose generation is newer than the one it knows.

use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tickloop_agent_base::private_fs::{
    PRIVATE_FILE_MODE, PrivateDirectory, replace_file_atomically,
};
use tickloop_agent_protocol::GoalSnapshot;

const GOAL_DIRECTORY: &str = "goal";
const STATE_FILE: &str = "state.json";
const WRITE_LOCK_FILE: &str = ".lock";

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct StoredGoal {
    pub generation: u64,
    pub goal: Option<GoalSnapshot>,
}

#[derive(Clone, Debug)]
pub(crate) struct GoalStore {
    directory: PrivateDirectory,
}

impl GoalStore {
    pub(crate) fn in_chat_directory(chat_directory: &PrivateDirectory) -> Self {
        Self {
            directory: chat_directory.join(GOAL_DIRECTORY),
        }
    }

    fn state_path(&self) -> PathBuf {
        self.directory.path().join(STATE_FILE)
    }

    /// The stored goal; `None` while no process stored one for the Chat.
    pub(crate) fn read(&self) -> io::Result<Option<StoredGoal>> {
        let path = self.state_path();
        match std::fs::read(&path) {
            Ok(content) => serde_json::from_slice(&content).map(Some).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is unreadable: {error}", path.display()),
                )
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Applies `change` to the stored goal, or to `unstored` while the Chat has none stored yet,
    /// under the store's lock, and stores the result with the next generation when it differs.
    /// Returns the goal as stored afterwards with its generation, and what `change` returned; an
    /// error of `change` stores nothing.
    pub(crate) fn update<T, E>(
        &self,
        unstored: Option<GoalSnapshot>,
        change: impl FnOnce(Option<&GoalSnapshot>) -> Result<(Option<GoalSnapshot>, T), E>,
    ) -> io::Result<Result<(StoredGoal, T), E>> {
        self.directory.create()?;
        let guard = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.directory.path().join(WRITE_LOCK_FILE))?;
        guard.lock()?;
        let current = self.read()?.unwrap_or(StoredGoal {
            generation: 0,
            goal: unstored,
        });
        let (goal, outcome) = match change(current.goal.as_ref()) {
            Ok(changed) => changed,
            Err(error) => return Ok(Err(error)),
        };
        if goal == current.goal && current.generation > 0 {
            return Ok(Ok((current, outcome)));
        }
        let stored = StoredGoal {
            generation: current.generation + 1,
            goal,
        };
        let contents = serde_json::to_vec(&stored).map_err(io::Error::other)?;
        replace_file_atomically(&self.state_path(), &contents, PRIVATE_FILE_MODE)?;
        Ok(Ok((stored, outcome)))
    }
}
