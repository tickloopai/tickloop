use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
};

use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use tickloop_agent_protocol::{SessionPermissionDenial, WorkspaceScope};
use tickloop_agent_tools::{
    PreparedTool, Workspace,
    permissions::{SessionPermissionDecision, permission_rule_content},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct PermissionOwner {
    chat_id: Uuid,
    loop_id: Uuid,
}

impl PermissionOwner {
    pub(crate) fn new(chat_id: Uuid, loop_id: Uuid) -> Result<Self, SessionPermissionError> {
        if chat_id.is_nil() || loop_id.is_nil() {
            return Err(SessionPermissionError::InvalidOwner);
        }
        Ok(Self { chat_id, loop_id })
    }
}

pub(crate) fn capture_workspace_scope(
    workspace: &Workspace,
) -> Result<WorkspaceScope, SessionPermissionError> {
    let folder = workspace
        .folder
        .as_deref()
        .map(canonical_directory)
        .transpose()?;
    let mut additional_directories = workspace
        .additional_directories
        .iter()
        .map(|directory| canonical_directory(directory))
        .collect::<Result<BTreeSet<_>, _>>()?;
    if let Some(folder) = &folder {
        additional_directories.remove(folder);
    }
    Ok(WorkspaceScope::new(
        folder,
        additional_directories.into_iter().collect(),
    ))
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SessionPermissionOperation {
    tool: String,
    /// What a grant covers: the call's normalized input, or the rule content of tools whose
    /// rules name one aspect of it, such as the host a `WebFetch` call fetches.
    identity: Value,
    workspace: WorkspaceScope,
}

pub(crate) fn denial_matches(
    denial: &SessionPermissionDenial,
    operation: &SessionPermissionOperation,
) -> bool {
    denial.tool() == operation.tool
        && *denial.workspace() == operation.workspace
        && denial.identity_sha256() == identity_digest(&operation.identity)
}

fn identity_digest(identity: &Value) -> String {
    let digest = Sha256::digest(serde_json::to_vec(identity).expect("normalized input serializes"));
    format!("{digest:x}")
}

impl SessionPermissionOperation {
    pub(crate) fn from_prepared(
        operation: &PreparedTool,
        configured_workspace: &Workspace,
    ) -> Result<Self, SessionPermissionError> {
        Self::from_normalized(
            operation.name.clone(),
            operation.input.clone(),
            capture_workspace_scope(configured_workspace)?,
        )
    }

    pub(crate) fn from_normalized(
        tool: String,
        normalized_input: Value,
        workspace: WorkspaceScope,
    ) -> Result<Self, SessionPermissionError> {
        if tool.trim().is_empty() || tool.trim() != tool {
            return Err(SessionPermissionError::InvalidTool);
        }
        let identity = permission_rule_content(&tool, &normalized_input)
            .map_or(normalized_input, Value::String);
        Ok(Self {
            tool,
            identity,
            workspace,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct SessionPermissionGrantId(Uuid);

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SessionPermissionRevision(u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionPermissionToken {
    revision: SessionPermissionRevision,
    owner: PermissionOwner,
    operation: SessionPermissionOperation,
    decision: Option<SessionPermissionDecision>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionPermissionEvaluation {
    pub(crate) decision: Option<SessionPermissionDecision>,
    pub(crate) token: SessionPermissionToken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoopInheritance {
    Inherited,
    Unregistered,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum SessionPermissionError {
    #[error("session permission owner must use non-nil Chat and Loop IDs")]
    InvalidOwner,
    #[error("session permission tool must be a non-empty normalized name")]
    InvalidTool,
    #[error("session permission workspace directory is unavailable: {0}")]
    InvalidWorkspace(PathBuf),
    #[error("a Chat cannot inherit session permissions from itself")]
    SelfBranch,
    #[error("session permission branch lineage would contain a cycle")]
    BranchCycle,
    #[error("the child Chat already has a different session permission source")]
    ChildAlreadyRegistered,
    #[error("the child Loop already has a different session permission source")]
    ChildLoopAlreadyInherited,
    #[error("session permission revision is exhausted")]
    RevisionExhausted,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct SessionPermissionGrant {
    id: SessionPermissionGrantId,
    owner: PermissionOwner,
    operation: SessionPermissionOperation,
    decision: SessionPermissionDecision,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct BranchRegistration {
    source_chat_id: Uuid,
    source_loop_id: Uuid,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct SessionPermissionContext {
    revision: u64,
    grants: Vec<SessionPermissionGrant>,
    branches: HashMap<Uuid, BranchRegistration>,
    #[serde(with = "crate::upgrade::map_entries")]
    owner_parents: HashMap<PermissionOwner, PermissionOwner>,
}

impl SessionPermissionContext {
    pub(crate) fn freeze_denials(&self, owner: PermissionOwner) -> Vec<SessionPermissionDenial> {
        let owners = self.owner_ancestors(owner);
        self.grants
            .iter()
            .filter(|grant| {
                owners.contains(&grant.owner) && grant.decision == SessionPermissionDecision::Deny
            })
            .map(|grant| {
                SessionPermissionDenial::new(
                    grant.operation.tool.clone(),
                    identity_digest(&grant.operation.identity),
                    grant.operation.workspace.clone(),
                )
            })
            .collect()
    }

    pub(crate) fn revision(&self) -> SessionPermissionRevision {
        SessionPermissionRevision(self.revision)
    }

    pub(crate) fn record(
        &mut self,
        owner: PermissionOwner,
        operation: SessionPermissionOperation,
        decision: SessionPermissionDecision,
    ) -> Result<SessionPermissionGrantId, SessionPermissionError> {
        let id = self.next_grant_id();
        self.bump_revision()?;
        self.grants.push(SessionPermissionGrant {
            id,
            owner,
            operation,
            decision,
        });
        Ok(id)
    }

    pub(crate) fn register_branch(
        &mut self,
        source_chat_id: Uuid,
        source_loop_id: Uuid,
        child_chat_id: Uuid,
    ) -> Result<(), SessionPermissionError> {
        if source_chat_id.is_nil() || child_chat_id.is_nil() || source_loop_id.is_nil() {
            return Err(SessionPermissionError::InvalidOwner);
        }
        if source_chat_id == child_chat_id {
            return Err(SessionPermissionError::SelfBranch);
        }
        let registration = BranchRegistration {
            source_chat_id,
            source_loop_id,
        };
        if let Some(existing) = self.branches.get(&child_chat_id) {
            return if existing.source_chat_id == source_chat_id
                && existing.source_loop_id == source_loop_id
            {
                Ok(())
            } else {
                Err(SessionPermissionError::ChildAlreadyRegistered)
            };
        }
        if self.chat_ancestors(source_chat_id).contains(&child_chat_id) {
            return Err(SessionPermissionError::BranchCycle);
        }
        self.bump_revision()?;
        self.branches.insert(child_chat_id, registration);
        Ok(())
    }

    pub(crate) fn inherit_loop(
        &mut self,
        child_chat_id: Uuid,
        child_loop_id: Uuid,
    ) -> Result<LoopInheritance, SessionPermissionError> {
        if child_chat_id.is_nil() || child_loop_id.is_nil() {
            return Err(SessionPermissionError::InvalidOwner);
        }
        let Some(registration) = self.branches.get(&child_chat_id).copied() else {
            return Ok(LoopInheritance::Unregistered);
        };
        let child = PermissionOwner::new(child_chat_id, child_loop_id)?;
        let source =
            PermissionOwner::new(registration.source_chat_id, registration.source_loop_id)?;
        if let Some(existing) = self.owner_parents.get(&child) {
            return if *existing == source {
                Ok(LoopInheritance::Inherited)
            } else {
                Err(SessionPermissionError::ChildLoopAlreadyInherited)
            };
        }
        self.bump_revision()?;
        self.owner_parents.insert(child, source);
        Ok(LoopInheritance::Inherited)
    }

    pub(crate) fn evaluate(
        &self,
        owner: PermissionOwner,
        operation: &SessionPermissionOperation,
    ) -> SessionPermissionEvaluation {
        let decision = self.evaluate_decision(owner, operation);
        SessionPermissionEvaluation {
            decision,
            token: SessionPermissionToken {
                revision: self.revision(),
                owner,
                operation: operation.clone(),
                decision,
            },
        }
    }

    pub(crate) fn revalidate(
        &self,
        token: &SessionPermissionToken,
        owner: PermissionOwner,
        operation: &SessionPermissionOperation,
    ) -> bool {
        token.revision == self.revision()
            && token.owner == owner
            && token.operation == *operation
            && token.decision == self.evaluate_decision(owner, operation)
    }

    fn evaluate_decision(
        &self,
        owner: PermissionOwner,
        operation: &SessionPermissionOperation,
    ) -> Option<SessionPermissionDecision> {
        let owners = self.owner_ancestors(owner);
        let mut matched_allow = false;
        for grant in &self.grants {
            if owners.contains(&grant.owner) && grant.operation == *operation {
                match grant.decision {
                    SessionPermissionDecision::Deny => {
                        return Some(SessionPermissionDecision::Deny);
                    }
                    SessionPermissionDecision::Allow => matched_allow = true,
                }
            }
        }
        matched_allow.then_some(SessionPermissionDecision::Allow)
    }

    fn owner_ancestors(&self, owner: PermissionOwner) -> HashSet<PermissionOwner> {
        let mut ancestors = HashSet::new();
        let mut current = Some(owner);
        while let Some(owner) = current {
            if !ancestors.insert(owner) {
                break;
            }
            current = self.owner_parents.get(&owner).copied();
        }
        ancestors
    }

    fn chat_ancestors(&self, chat_id: Uuid) -> HashSet<Uuid> {
        let mut ancestors = HashSet::new();
        let mut current = Some(chat_id);
        while let Some(chat_id) = current {
            if !ancestors.insert(chat_id) {
                break;
            }
            current = self
                .branches
                .get(&chat_id)
                .map(|registration| registration.source_chat_id);
        }
        ancestors
    }

    fn next_grant_id(&self) -> SessionPermissionGrantId {
        loop {
            let candidate = SessionPermissionGrantId(tickloop_ids::generate());
            if self.grants.iter().all(|grant| grant.id != candidate) {
                return candidate;
            }
        }
    }

    fn bump_revision(&mut self) -> Result<(), SessionPermissionError> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or(SessionPermissionError::RevisionExhausted)?;
        Ok(())
    }
}

fn canonical_directory(path: &Path) -> Result<PathBuf, SessionPermissionError> {
    let canonical = path
        .canonicalize()
        .map_err(|_| SessionPermissionError::InvalidWorkspace(path.to_path_buf()))?;
    if !canonical.is_dir() {
        return Err(SessionPermissionError::InvalidWorkspace(path.to_path_buf()));
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests;
