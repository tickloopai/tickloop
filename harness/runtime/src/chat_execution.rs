//! The Execution Environment Port of a Chat with a Folder: bound once for the Chat's Folder and
//! Additional Directories, together with the capabilities the Port reported then, and shared by
//! the Chat's tool listing and its shell host. A change of directories binds a new Port in place
//! of the old one; Shells already started keep the Port they started on.

use std::{
    path::PathBuf,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use tickloop_harness_protocol::{ChatId, ChatSnapshot};

use crate::environment::{
    ExecutionAuthority, ExecutionCapabilities, ExecutionEnvironment, ExecutionError,
    ExecutionInspection, ExecutionPort, ExecutionQuery, ExecutionScope, FileCapabilities,
    error_code,
};

#[derive(Clone)]
pub(crate) struct BoundPort {
    pub(crate) port: Arc<dyn ExecutionPort>,
    pub(crate) capabilities: ExecutionCapabilities,
    /// Which binding of the Chat this Port is; a later binding supersedes it.
    pub(crate) generation: u64,
}

#[derive(Clone)]
pub(crate) struct ChatExecution {
    authority: ExecutionAuthority,
    binding: Arc<RwLock<Binding>>,
    bindings: Arc<AtomicU64>,
}

struct Binding {
    scope: Option<ExecutionScope>,
    port: Result<BoundPort, ExecutionError>,
    generation: u64,
}

impl ChatExecution {
    /// A Chat without a Folder, which binds no Port.
    pub(crate) fn unbound(chat: ChatId) -> Self {
        Self {
            authority: ExecutionAuthority::new(chat),
            binding: Arc::new(RwLock::new(Binding {
                scope: None,
                port: Err(no_folder()),
                generation: 0,
            })),
            bindings: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) async fn bind(
        environment: Option<&Arc<dyn ExecutionEnvironment>>,
        snapshot: &ChatSnapshot,
    ) -> Self {
        let execution = Self::unbound(snapshot.id);
        execution.follow(environment, snapshot).await;
        execution
    }

    /// Binds a new Port when the Chat's directories differ from those the current one was bound
    /// for.
    pub(crate) async fn follow(
        &self,
        environment: Option<&Arc<dyn ExecutionEnvironment>>,
        snapshot: &ChatSnapshot,
    ) {
        let scope = scope(snapshot);
        if self.binding.read().unwrap().scope == Some(scope.clone()) {
            return;
        }
        let generation = self.bindings.fetch_add(1, Ordering::Relaxed) + 1;
        let port = match environment {
            _ if scope.folder.is_none() => Err(no_folder()),
            Some(environment) => {
                bind_port(environment.as_ref(), &scope, &self.authority, generation).await
            }
            None => Err(ExecutionError::new(
                error_code::UNSUPPORTED,
                "no execution environment",
            )),
        };
        *self.binding.write().unwrap() = Binding {
            scope: Some(scope),
            port,
            generation,
        };
    }

    pub(crate) fn authority(&self) -> &ExecutionAuthority {
        &self.authority
    }

    pub(crate) fn port(&self) -> Result<BoundPort, ExecutionError> {
        self.binding.read().unwrap().port.clone()
    }

    /// Whether `generation` is still the Chat's binding, so that its roots are the Chat's
    /// directories.
    pub(crate) fn is_current(&self, generation: u64) -> bool {
        self.binding.read().unwrap().generation == generation
    }

    /// The file roots of the bound Port; none when binding failed.
    pub(crate) fn files(&self) -> FileCapabilities {
        self.binding
            .read()
            .unwrap()
            .port
            .as_ref()
            .map(|bound| bound.capabilities.files.clone())
            .unwrap_or_default()
    }

    /// Where the Folder is on this machine, when the environment says it is here.
    pub(crate) fn local_folder(&self) -> Option<PathBuf> {
        self.files()
            .folder()
            .and_then(|folder| folder.local_path.clone())
    }
}

fn no_folder() -> ExecutionError {
    ExecutionError::new(error_code::UNSUPPORTED, "this Chat has no Folder")
}

fn scope(snapshot: &ChatSnapshot) -> ExecutionScope {
    ExecutionScope {
        chat: snapshot.id,
        folder: snapshot
            .folder
            .as_ref()
            .map(|folder| folder.to_string_lossy().into_owned()),
        additional_directories: snapshot
            .additional_directories
            .iter()
            .map(|directory| directory.to_string_lossy().into_owned())
            .collect(),
    }
}

async fn bind_port(
    environment: &dyn ExecutionEnvironment,
    scope: &ExecutionScope,
    authority: &ExecutionAuthority,
    generation: u64,
) -> Result<BoundPort, ExecutionError> {
    let port = environment.bind(scope.clone(), authority.clone())?;
    match port.inspect(ExecutionQuery::Capabilities).await? {
        ExecutionInspection::Capabilities(capabilities) => Ok(BoundPort {
            port,
            capabilities,
            generation,
        }),
        _ => Err(ExecutionError::new(
            "ProtocolViolation",
            "execution environment returned no capabilities",
        )),
    }
}
