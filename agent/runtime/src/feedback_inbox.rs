use std::{
    fs,
    io::{self, Read},
    path::Path,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tickloop_agent_base::private_fs::{
    OpenTarget, PRIVATE_FILE_MODE, PrivateDirectory, open_without_following_links,
    replace_file_atomically,
};
use uuid::Uuid;

const MAX_RECORD_BYTES: u64 = 256 * 1024;
const MAX_PENDING_INPUTS: usize = 512;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FeedbackIdentity {
    pub(super) delivery_id: Uuid,
    pub(super) feedback_id: String,
    pub(super) chat_id: Uuid,
    pub(super) loop_id: Uuid,
    pub(super) origin: String,
    pub(super) account_id: String,
    pub(super) artifact_id: String,
}

impl FeedbackIdentity {
    /// The ledger input of the request: the one its delivery names on every host.
    pub(super) fn input_id(&self) -> Uuid {
        tickloop_agent_transcript::input_ids::feedback(
            &self.origin,
            &self.account_id,
            self.delivery_id,
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FeedbackInput {
    pub(super) identity: FeedbackIdentity,
    pub(super) prompt: String,
    sequence: u64,
}

impl FeedbackInput {
    pub(super) fn new(identity: FeedbackIdentity, prompt: String) -> Self {
        Self {
            identity,
            prompt,
            sequence: 0,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct FeedbackUnlink {
    pub(super) loop_id: Uuid,
    pub(super) source: tickloop_agent_protocol::EventSourceBinding,
}

pub(super) struct FeedbackInbox {
    /// `None` for a Chat without a Folder, which keeps no inbox: nothing is saved in it, and
    /// its host delivers each request with `TurnOp::Deliver`.
    root: Option<PrivateDirectory>,
    chat_id: Uuid,
}

impl FeedbackInbox {
    pub(super) fn identities_for(chat: &super::Chat) -> io::Result<Vec<FeedbackIdentity>> {
        let mut identities =
            Self::new(chat.tools.chat_directory(), chat.snapshot.id).identities()?;
        identities.extend(chat.feedback_inputs.values().cloned());
        let ledger_inputs = chat.bookkeeping.lifecycle.queued_inputs();
        for input in chat
            .snapshot
            .pending_inputs
            .iter()
            .chain(&ledger_inputs)
            .filter(|input| super::event_sources::is_feedback_input(input))
        {
            let identity = super::event_sources::queued_feedback_identity(chat.snapshot.id, input)
                .ok_or_else(|| io::Error::other("Feedback account identity is unavailable"))?;
            if !identities.contains(&identity) {
                identities.push(identity);
            }
        }
        Ok(identities)
    }

    pub(super) fn new(chat_directory: Option<&PrivateDirectory>, chat_id: Uuid) -> Self {
        Self {
            root: chat_directory.map(|directory| directory.join("feedback")),
            chat_id,
        }
    }

    fn directory(&self, name: &str) -> io::Result<PrivateDirectory> {
        self.root
            .as_ref()
            .map(|root| root.join(name))
            .ok_or_else(|| io::Error::other("this Chat has no Folder to keep Artifact feedback"))
    }

    fn key(identity: &FeedbackIdentity) -> String {
        let mut digest = Sha256::new();
        for value in [
            identity.origin.as_bytes(),
            identity.account_id.as_bytes(),
            identity.delivery_id.as_bytes(),
        ] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value);
        }
        format!("{:x}.json", digest.finalize())
    }

    fn read<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
        let file = match open_without_following_links(path, OpenTarget::Entry) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if file.metadata()?.len() > MAX_RECORD_BYTES {
            return Err(io::Error::other("Feedback inbox record exceeds its limit"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(io::Error::other)
    }

    fn write<T: Serialize>(directory: &PrivateDirectory, name: &str, value: &T) -> io::Result<()> {
        directory.create()?;
        let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(io::Error::other("Feedback inbox record exceeds its limit"));
        }
        replace_file_atomically(&directory.path().join(name), &bytes, PRIVATE_FILE_MODE)?;
        open_without_following_links(
            directory.path(),
            OpenTarget::Directory { create_mode: None },
        )?
        .sync_all()
    }

    /// Persists the input before the cloud may acknowledge its delivery. Reclaims keep the first received context.
    pub(super) fn stage(&self, mut input: FeedbackInput) -> io::Result<bool> {
        if input.identity.chat_id != self.chat_id {
            return Err(io::Error::other("Feedback inbox belongs to another Chat"));
        }
        let key = Self::key(&input.identity);
        if let Some(received) =
            Self::read::<FeedbackIdentity>(&self.directory("received")?.path().join(&key))?
        {
            if received != input.identity {
                return Err(io::Error::other("Feedback delivery identity changed"));
            }
            return Ok(false);
        }
        if let Some(pending) =
            Self::read::<FeedbackInput>(&self.directory("pending")?.path().join(&key))?
        {
            if pending.identity != input.identity {
                return Err(io::Error::other("Feedback delivery identity changed"));
            }
            return Ok(true);
        }
        let pending = self.pending()?;
        if pending.len() >= MAX_PENDING_INPUTS {
            return Err(io::Error::other(
                "Feedback inbox is full; delivery remains in the cloud",
            ));
        }
        input.sequence = pending
            .iter()
            .map(|entry| entry.sequence)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Feedback inbox sequence exhausted"))?;
        Self::write(&self.directory("pending")?, &key, &input)?;
        Ok(true)
    }

    pub(super) fn pending(&self) -> io::Result<Vec<FeedbackInput>> {
        if self.root.is_none() {
            return Ok(Vec::new());
        }
        let directory = self.directory("pending")?;
        match open_without_following_links(
            directory.path(),
            OpenTarget::Directory { create_mode: None },
        ) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        }
        let entries = match fs::read_dir(directory.path()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut inputs = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Some(input) = Self::read::<FeedbackInput>(&path)? else {
                continue;
            };
            if input.identity.chat_id != self.chat_id
                || path.file_name().and_then(|value| value.to_str())
                    != Some(Self::key(&input.identity).as_str())
            {
                return Err(io::Error::other("Invalid feedback inbox identity"));
            }
            if Self::read::<FeedbackIdentity>(
                &self
                    .directory("received")?
                    .path()
                    .join(Self::key(&input.identity)),
            )?
            .is_none()
            {
                inputs.push(input);
            }
        }
        inputs.sort_by_key(|entry| entry.sequence);
        Ok(inputs)
    }

    fn unlink_key(input: &FeedbackUnlink) -> String {
        let source = &input.source;
        let identity = FeedbackIdentity {
            delivery_id: source.binding_id,
            feedback_id: String::new(),
            chat_id: Uuid::nil(),
            loop_id: input.loop_id,
            origin: source.origin.clone(),
            account_id: source.account_id.clone(),
            artifact_id: source.artifact_id.clone(),
        };
        Self::key(&identity)
    }

    pub(super) fn queue_unlink(&self, input: &FeedbackUnlink) -> io::Result<()> {
        Self::write(&self.directory("unlinks")?, &Self::unlink_key(input), input)
    }

    pub(super) fn unlinks(&self) -> io::Result<Vec<FeedbackUnlink>> {
        if self.root.is_none() {
            return Ok(Vec::new());
        }
        let directory = self.directory("unlinks")?;
        match open_without_following_links(
            directory.path(),
            OpenTarget::Directory { create_mode: None },
        ) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        }
        let mut inputs = Vec::new();
        for entry in fs::read_dir(directory.path())? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            if let Some(input) = Self::read::<FeedbackUnlink>(&path)? {
                if path.file_name().and_then(|value| value.to_str())
                    != Some(Self::unlink_key(&input).as_str())
                {
                    return Err(io::Error::other("Invalid feedback unlink identity"));
                }
                inputs.push(input);
            }
        }
        Ok(inputs)
    }

    pub(super) fn unlinked(&self, input: &FeedbackUnlink) -> io::Result<()> {
        let directory = self.directory("unlinks")?;
        match fs::remove_file(directory.path().join(Self::unlink_key(input))) {
            Ok(()) => open_without_following_links(
                directory.path(),
                OpenTarget::Directory { create_mode: None },
            )?
            .sync_all(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub(super) fn identities(&self) -> io::Result<Vec<FeedbackIdentity>> {
        if self.root.is_none() {
            return Ok(Vec::new());
        }
        let mut identities = self
            .pending()?
            .into_iter()
            .map(|input| input.identity)
            .collect::<Vec<_>>();
        let directory = self.directory("received")?;
        match open_without_following_links(
            directory.path(),
            OpenTarget::Directory { create_mode: None },
        ) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(identities),
            Err(error) => return Err(error),
        }
        for entry in fs::read_dir(directory.path())? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            if let Some(identity) = Self::read::<FeedbackIdentity>(&path)? {
                if identity.chat_id != self.chat_id
                    || path.file_name().and_then(|value| value.to_str())
                        != Some(Self::key(&identity).as_str())
                {
                    return Err(io::Error::other("Invalid feedback receipt identity"));
                }
                identities.push(identity);
            }
        }
        Ok(identities)
    }

    /// The caller checkpoints model-visible history before replacing an input with its durable receipt.
    pub(super) fn received(&self, identity: &FeedbackIdentity) -> io::Result<()> {
        let key = Self::key(identity);
        Self::write(&self.directory("received")?, &key, identity)?;
        let pending = self.directory("pending")?;
        match fs::remove_file(pending.path().join(key)) {
            Ok(()) => open_without_following_links(
                pending.path(),
                OpenTarget::Directory { create_mode: None },
            )?
            .sync_all(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(chat_id: Uuid) -> FeedbackInput {
        FeedbackInput::new(
            FeedbackIdentity {
                chat_id,
                loop_id: Uuid::new_v4(),
                delivery_id: Uuid::new_v4(),
                feedback_id: Uuid::new_v4().to_string(),
                origin: "https://docs.example".into(),
                account_id: "owner".into(),
                artifact_id: "document".into(),
            },
            "@tickloop feedback: shorten the title".into(),
        )
    }

    #[test]
    fn unconsumed_feedback_survives_reopening_and_keeps_its_first_context() {
        let directory = tempfile::tempdir().unwrap();
        let private = PrivateDirectory::trusted(directory.path().canonicalize().unwrap());
        let chat = Uuid::new_v4();
        let inbox = FeedbackInbox::new(Some(&private), chat);
        let first = input(chat);
        inbox.stage(first.clone()).unwrap();
        let mut reclaimed = first.clone();
        reclaimed.prompt = "Later thread context".into();
        inbox.stage(reclaimed).unwrap();
        let restored = FeedbackInbox::new(Some(&private), chat).pending().unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].prompt, first.prompt);
        inbox.received(&first.identity).unwrap();
        assert!(
            FeedbackInbox::new(Some(&private), chat)
                .pending()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            FeedbackInbox::new(Some(&private), chat)
                .identities()
                .unwrap(),
            vec![first.identity.clone()]
        );
        assert!(!inbox.stage(first).unwrap());
    }

    #[test]
    fn receipts_are_scoped_to_the_origin_and_account_and_reject_changed_identity() {
        let directory = tempfile::tempdir().unwrap();
        let private = PrivateDirectory::trusted(directory.path().canonicalize().unwrap());
        let chat = Uuid::new_v4();
        let inbox = FeedbackInbox::new(Some(&private), chat);
        let first = input(chat);
        inbox.stage(first.clone()).unwrap();
        let mut other_account = first.clone();
        other_account.identity.account_id = "other".into();
        inbox.stage(other_account).unwrap();
        let mut other_origin = first.clone();
        other_origin.identity.origin = "https://other.example".into();
        inbox.stage(other_origin).unwrap();
        assert_eq!(inbox.pending().unwrap().len(), 3);
        let mut changed = first;
        changed.identity.artifact_id = "another document".into();
        assert!(inbox.stage(changed).is_err());
    }
}
