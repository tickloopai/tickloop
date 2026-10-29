//! Runs the work `/hooks` asks for, and offers the review a New Agent shows when its Folder has
//! plugin or repository hooks that are new or changed: once per Folder for the Console's
//! lifetime. Hooks are read and trusted through the daemon, whose Settings Store keeps their
//! trust records in the user's settings.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use tickloop_harness::protocol::hooks::HookList;
use tokio::task::JoinSet;
use uuid::Uuid;

use super::{
    console::Console,
    execution::ConsoleExecution,
    hooks_panel::{HooksPanel, HooksRequest, HooksResult, HooksWork, needs_review},
    safe_line,
    user_panel::UserPanel,
};

const PANEL_COMMAND: &str = "/hooks";

pub(super) enum Done {
    Result {
        panel: Uuid,
        result: HooksResult,
    },
    /// The hooks of `folder`, read to tell whether a New Agent there offers their review.
    ReviewRead {
        folder: PathBuf,
        listing: Result<HookList, String>,
    },
}

/// What a panel works on: its Chat, when it has one, and the Folder whose hooks it shows.
struct PanelContext {
    chat: Option<Uuid>,
    folder: PathBuf,
}

/// The `/hooks` work under way, and the reviews this Console has offered.
#[derive(Default)]
pub(super) struct HookTasks {
    running: JoinSet<Done>,
    reads: u64,
    contexts: HashMap<Uuid, PanelContext>,
    /// The Folders whose review this Console has shown.
    reviewed: HashSet<PathBuf>,
    /// The Folder of the New Agent shown, once its hooks have been asked for.
    checked: Option<PathBuf>,
    /// A review found for the New Agent's Folder, waiting for the screen to be free.
    waiting: Option<(PathBuf, HookList)>,
}

impl HookTasks {
    pub(super) fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    /// The next finished work; `None` for work that panicked.
    pub(super) async fn join_next(&mut self) -> Option<Done> {
        self.running.join_next().await?.ok()
    }

    fn read(&mut self, agent: &ConsoleExecution, panel: Uuid) {
        let Some(context) = self.contexts.get(&panel) else {
            return;
        };
        let (chat, folder) = (context.chat, context.folder.clone());
        self.reads += 1;
        let read = self.reads;
        let agent = agent.clone();
        self.running.spawn(async move {
            let listing = read_hooks(&agent, chat, folder).await;
            Done::Result {
                panel,
                result: HooksResult::Listed { read, listing },
            }
        });
    }
}

/// The Folder `chat` works in, or `folder` before an Agent starts there.
async fn chat_folder(agent: &ConsoleExecution, chat: Option<Uuid>, folder: PathBuf) -> PathBuf {
    match chat {
        Some(chat) => agent
            .status(chat)
            .await
            .ok()
            .and_then(|status| status.folder)
            .unwrap_or(folder),
        None => folder,
    }
}

async fn read_hooks(
    agent: &ConsoleExecution,
    chat: Option<Uuid>,
    folder: PathBuf,
) -> Result<HookList, String> {
    let folder = chat_folder(agent, chat, folder).await;
    agent.hooks(folder).await.map_err(|error| error.to_string())
}

/// Opens `/hooks` for `chat`, or for the New Agent in `folder`, and reads its hooks.
pub(super) fn open(
    console: &Console,
    agent: &ConsoleExecution,
    chat: Option<Uuid>,
    folder: PathBuf,
    tasks: &mut HookTasks,
) -> Result<(), String> {
    let panel = HooksPanel::new(chat);
    let id = panel.id();
    console.show_user_panel(UserPanel::Hooks(panel))?;
    tasks.contexts.insert(id, PanelContext { chat, folder });
    tasks.read(agent, id);
    Ok(())
}

pub(super) fn start(agent: &ConsoleExecution, work: HooksWork, tasks: &mut HookTasks) {
    let panel = work.panel;
    match work.request {
        HooksRequest::Load => tasks.read(agent, panel),
        HooksRequest::Write {
            changes,
            expected_revision,
        } => {
            let Some(context) = tasks.contexts.get(&panel) else {
                return;
            };
            let (chat, folder) = (context.chat, context.folder.clone());
            let agent = agent.clone();
            tasks.running.spawn(async move {
                let folder = chat_folder(&agent, chat, folder).await;
                let result = agent
                    .write_hook_trust(folder, changes, expected_revision)
                    .await
                    .map_err(|error| error.to_string());
                Done::Result {
                    panel,
                    result: HooksResult::Written(result),
                }
            });
        }
    }
}

pub(super) fn finish(
    console: &Console,
    agent: &ConsoleExecution,
    done: Done,
    tasks: &mut HookTasks,
) -> Result<(), String> {
    let (panel, result) = match done {
        Done::ReviewRead { folder, listing } => {
            if tasks.checked.as_ref() == Some(&folder)
                && let Ok(listing) = listing
                && listing.hooks.iter().any(needs_review)
            {
                tasks.waiting = Some((folder, listing));
            }
            return Ok(());
        }
        Done::Result { panel, result } => (panel, result),
    };
    let failed_write = matches!(&result, HooksResult::Written(Err(_)));
    let write_error = match &result {
        HooksResult::Written(Err(error)) => Some(error.clone()),
        _ => None,
    };
    match console.apply_hooks_result(panel, result)? {
        Some(true) => {
            // A refused write may have met records another View changed; show them as they are.
            if failed_write {
                tasks.read(agent, panel);
            }
        }
        Some(false) => {
            tasks.contexts.remove(&panel);
            console.close_user_panel(panel)?;
        }
        None => {
            tasks.contexts.remove(&panel);
            if let Some(error) = write_error {
                console.write_line(&format!("{PANEL_COMMAND}: {}", safe_line(&error)))?;
            }
        }
    }
    Ok(())
}

/// Offers the review of the New Agent's hooks in `folder` while `on_new_agent`: reads them the
/// first time the New Agent shows that Folder, and shows the review once the screen is free.
/// Each Folder's review shows at most once for the Console's lifetime.
pub(super) fn offer_review(
    console: &Console,
    agent: &ConsoleExecution,
    on_new_agent: bool,
    folder: &Path,
    tasks: &mut HookTasks,
) -> Result<(), String> {
    if !on_new_agent || tasks.checked.as_deref() != Some(folder) {
        tasks.checked = None;
        tasks.waiting = None;
    }
    if !on_new_agent || tasks.reviewed.contains(folder) {
        return Ok(());
    }
    if tasks.checked.is_none() {
        tasks.checked = Some(folder.to_owned());
        let (agent, folder) = (agent.clone(), folder.to_owned());
        tasks.running.spawn(async move {
            let listing = agent
                .hooks(folder.clone())
                .await
                .map_err(|error| error.to_string());
            Done::ReviewRead { folder, listing }
        });
        return Ok(());
    }
    if tasks.waiting.is_none() || !console.can_offer_hook_review()? {
        return Ok(());
    }
    let (folder, listing) = tasks.waiting.take().expect("a review waits");
    let panel = HooksPanel::review(listing);
    tasks.contexts.insert(
        panel.id(),
        PanelContext {
            chat: None,
            folder: folder.clone(),
        },
    );
    tasks.reviewed.insert(folder);
    console.show_user_panel(UserPanel::Hooks(panel))
}
