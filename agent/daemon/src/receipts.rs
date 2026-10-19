use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use uuid::Uuid;

use tickloop_agent_protocol::{
    AccessError, AccessErrorCode, CommandEnvelope, CommandReceipt, CommandResult, ExecutionRef,
    PersistenceState, RECEIPT_CAPACITY, RECEIPT_RETENTION_SECONDS, ReceiptState,
};

pub(crate) struct ReceiptStore {
    epoch: Uuid,
    entries: Mutex<HashMap<Uuid, Entry>>,
}
struct Entry {
    digest: [u8; 32],
    receipt: CommandReceipt,
    updated: Instant,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ReceiptCheckpoint {
    entries: Vec<RetainedReceipt>,
}

#[derive(Serialize, Deserialize)]
struct RetainedReceipt {
    digest: [u8; 32],
    receipt: CommandReceipt,
    age: Duration,
}

impl ReceiptStore {
    pub async fn checkpoint(&self) -> Result<ReceiptCheckpoint, AccessError> {
        let entries = self.entries.lock().await;
        if entries.values().any(|entry| {
            matches!(
                entry.receipt.state,
                ReceiptState::Accepted | ReceiptState::Running
            )
        }) {
            return Err(AccessError::unavailable(
                "commands are still being dispatched",
            ));
        }
        Ok(ReceiptCheckpoint {
            entries: entries
                .values()
                .map(|entry| RetainedReceipt {
                    digest: entry.digest,
                    receipt: entry.receipt.clone(),
                    age: entry.updated.elapsed(),
                })
                .collect(),
        })
    }

    pub async fn restore(&self, checkpoint: ReceiptCheckpoint) -> Result<(), AccessError> {
        let mut entries = self.entries.lock().await;
        if !entries.is_empty() || checkpoint.entries.len() > RECEIPT_CAPACITY {
            return Err(AccessError::invalid("invalid receipt handoff"));
        }
        let now = Instant::now();
        for retained in checkpoint.entries {
            if retained.receipt.daemon_epoch != self.epoch
                || matches!(
                    retained.receipt.state,
                    ReceiptState::Accepted | ReceiptState::Running
                )
            {
                return Err(AccessError::invalid("receipt handoff is not settled"));
            }
            if retained.age < Duration::from_secs(RECEIPT_RETENTION_SECONDS) {
                entries.insert(
                    retained.receipt.command_id,
                    Entry {
                        digest: retained.digest,
                        receipt: retained.receipt,
                        updated: now.checked_sub(retained.age).unwrap_or(now),
                    },
                );
            }
        }
        Ok(())
    }

    pub fn new(epoch: Uuid) -> Self {
        Self {
            epoch,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub async fn begin(
        &self,
        command: &CommandEnvelope,
    ) -> Result<Option<CommandReceipt>, AccessError> {
        if command.daemon_epoch != self.epoch {
            return Err(AccessError::new(
                AccessErrorCode::EpochChanged,
                "command belongs to an earlier daemon",
            ));
        }
        let payload = serde_json::to_vec(&command.command)
            .map_err(|error| AccessError::invalid(error.to_string()))?;
        let digest: [u8; 32] = Sha256::digest(&payload).into();
        let mut entries = self.entries.lock().await;
        let now = Instant::now();
        entries.retain(|_, entry| {
            matches!(
                entry.receipt.state,
                ReceiptState::Accepted | ReceiptState::Running
            ) || now.duration_since(entry.updated) < Duration::from_secs(RECEIPT_RETENTION_SECONDS)
        });
        if let Some(entry) = entries.get(&command.command_id) {
            return if entry.digest == digest {
                Ok(Some(entry.receipt.clone()))
            } else {
                Err(AccessError::new(
                    AccessErrorCode::CommandIdConflict,
                    "CommandId has already been used for another target or payload",
                ))
            };
        }
        if entries.len() >= RECEIPT_CAPACITY {
            return Err(AccessError::new(
                AccessErrorCode::CapacityExceeded,
                "command receipt capacity is full; retry after retained commands expire",
            ));
        }
        entries.insert(
            command.command_id,
            Entry {
                digest,
                updated: now,
                receipt: CommandReceipt {
                    daemon_epoch: self.epoch,
                    command_id: command.command_id,
                    target: command.command.execution(),
                    state: ReceiptState::Accepted,
                    result: None,
                    error: None,
                    persistence: PersistenceState::BestEffortPending,
                },
            },
        );
        Ok(None)
    }

    pub async fn finish(
        &self,
        command_id: Uuid,
        result: Result<CommandResult, AccessError>,
        persistence: PersistenceState,
    ) -> CommandReceipt {
        let mut entries = self.entries.lock().await;
        let entry = entries
            .get_mut(&command_id)
            .expect("accepted receipt is retained until it finishes");
        entry.updated = Instant::now();
        entry.receipt.state = match &result {
            Ok(_) => ReceiptState::Completed,
            Err(error) if error.code == AccessErrorCode::OutcomeUnknown => {
                ReceiptState::OutcomeUnknown
            }
            Err(_) => ReceiptState::Failed,
        };
        if let Ok(
            CommandResult::Created { execution, .. }
            | CommandResult::Loaded { execution, .. }
            | CommandResult::Branch { execution, .. },
        ) = &result
        {
            entry.receipt.target = Some(*execution);
        }
        match result {
            Ok(result) => entry.receipt.result = Some(result),
            Err(error) => entry.receipt.error = Some(error),
        }
        entry.receipt.persistence = persistence;
        entry.receipt.clone()
    }

    pub async fn set_persistence(&self, command_id: Uuid, persistence: PersistenceState) {
        if let Some(entry) = self.entries.lock().await.get_mut(&command_id) {
            entry.receipt.persistence = persistence;
        }
    }

    pub async fn set_target(&self, command_id: Uuid, target: ExecutionRef) {
        if let Some(entry) = self.entries.lock().await.get_mut(&command_id) {
            entry.receipt.target = Some(target);
        }
    }

    pub async fn lookup(&self, epoch: Uuid, command_id: Uuid) -> Option<CommandReceipt> {
        if epoch != self.epoch {
            return None;
        }
        self.entries
            .lock()
            .await
            .get(&command_id)
            .filter(|entry| {
                matches!(
                    entry.receipt.state,
                    ReceiptState::Accepted | ReceiptState::Running
                ) || entry.updated.elapsed() < Duration::from_secs(RECEIPT_RETENTION_SECONDS)
            })
            .map(|entry| entry.receipt.clone())
    }
}
