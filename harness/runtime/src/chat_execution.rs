//! The Execution Environment Port of a Chat with a Folder: bound once for the Chat's Folder and
//! Additional Directories, together with the capabilities the Port reported then, and shared by
//! the Chat's tool listing and its shell host. A change of directories binds a new Port in place
//! of the old one; Shells already started keep the Port they started on.
//!
//! The Port also says where the Folder is. A Folder root reported without a path on this machine
//! is only in the environment: the Harness neither resolves nor checks it here, runs no local
//! command for it and keeps no local scratchpad, plans, Skills, Chat directory or file-history
//! backups for it. Any other Folder, also
//! one a successfully bound environment reports no root for, is a directory on this machine.
//! A failed binding never authorizes local access.

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
    authority: ExecutionAuthority,
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

    pub(crate) fn same_capabilities(&self, candidate: &Self) -> bool {
        let current = self.binding.read().unwrap();
        let candidate = candidate.binding.read().unwrap();
        match (&current.port, &candidate.port) {
            (Ok(current), Ok(candidate)) => {
                let current = &current.capabilities;
                let candidate = &candidate.capabilities;
                current.ticks == candidate.ticks
                    && current.shells == candidate.shells
                    && current.shell_output == candidate.shell_output
                    && current.files.outside_roots() == candidate.files.outside_roots()
                    && current
                        .files
                        .roots()
                        .iter()
                        .map(|root| (&root.name, root))
                        .collect::<std::collections::BTreeMap<_, _>>()
                        == candidate
                            .files
                            .roots()
                            .iter()
                            .map(|root| (&root.name, root))
                            .collect::<std::collections::BTreeMap<_, _>>()
            }
            (Err(_), Err(_)) => {
                current
                    .scope
                    .as_ref()
                    .is_none_or(|scope| scope.folder.is_none())
                    && candidate
                        .scope
                        .as_ref()
                        .is_none_or(|scope| scope.folder.is_none())
            }
            _ => false,
        }
    }

    pub(crate) fn replace(&self, candidate: &Self) {
        let generation = self.bindings.fetch_add(1, Ordering::Relaxed) + 1;
        let candidate = candidate.binding.read().unwrap();
        let port = candidate.port.clone().map(|mut bound| {
            bound.generation = generation;
            bound
        });
        *self.binding.write().unwrap() = Binding {
            scope: candidate.scope.clone(),
            port,
            generation,
        };
    }

    pub(crate) fn is_local_directory(&self, path: &std::path::Path) -> bool {
        let binding = self.binding.read().unwrap();
        let Ok(bound) = &binding.port else {
            return false;
        };
        bound
            .capabilities
            .files
            .roots()
            .iter()
            .find(|root| std::path::Path::new(&root.display_path) == path)
            .map_or(!bound.folder_in_environment(), |root| {
                root.local_path.is_some()
            })
    }

    pub(crate) fn authority(&self) -> ExecutionAuthority {
        self.binding
            .read()
            .unwrap()
            .port
            .as_ref()
            .map_or_else(|_| self.authority.clone(), |bound| bound.authority.clone())
    }

    pub(crate) fn matches_directories(&self, snapshot: &ChatSnapshot) -> bool {
        self.binding.read().unwrap().scope.as_ref() == Some(&scope(snapshot))
    }

    pub(crate) fn admitted_scope(&self, snapshot: &ChatSnapshot) {
        self.binding.write().unwrap().scope = Some(scope(snapshot));
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

    /// Whether the bound Port keeps the Chat's Folder only in the environment.
    pub(crate) fn folder_in_environment(&self) -> bool {
        let binding = self.binding.read().unwrap();
        binding.port.as_ref().map_or_else(
            |_| {
                binding
                    .scope
                    .as_ref()
                    .is_some_and(|scope| scope.folder.is_some())
            },
            |bound| bound.folder_in_environment(),
        )
    }

    /// Whether the bound Port runs the Chat's Shells.
    pub(crate) fn runs_shells(&self) -> bool {
        self.binding
            .read()
            .unwrap()
            .port
            .as_ref()
            .is_ok_and(|bound| bound.capabilities.shells)
    }
}

impl BoundPort {
    /// Whether this Port keeps the Folder only in the environment: it reports the Folder root
    /// without a path on this machine.
    pub(crate) fn folder_in_environment(&self) -> bool {
        self.capabilities
            .files
            .folder()
            .is_some_and(|folder| folder.local_path.is_none())
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
            authority: authority.clone(),
            capabilities,
            generation,
        }),
        _ => Err(ExecutionError::new(
            "ProtocolViolation",
            "execution environment returned no capabilities",
        )),
    }
}
