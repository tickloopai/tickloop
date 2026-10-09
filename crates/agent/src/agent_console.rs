//! Local navigation and global Agent projections. Execution remains behind AgentAccess.
use crate::{agent_access::*, runtime::LoopState};
use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};
use uuid::Uuid;
const FINISHED_CHILD_GRACE_MS: u64 = 30_000;

#[derive(Clone, PartialEq, Eq)]
struct TerminalTransition {
    run_id: Option<crate::runtime::RunId>,
    state: LoopState,
    finished_at: Option<u64>,
}

#[derive(Default)]
struct ChildRetirement {
    completion: Option<TerminalTransition>,
    deadline: Option<u64>,
    departed_at: Option<u64>,
}

#[derive(Clone, Debug)]
pub enum Intent {
    Overview,
    Back,
    NewDraft,
    Open(ExecutionRef),
    ChooseLocation { generation: u64, view: ViewRef },
    DispatchDirectory(PathBuf),
    Filter(AgentFilter),
    AcknowledgeUnconfirmedDispatch(Uuid),
    Close,
}

#[derive(Clone, Debug)]
pub struct NavigationEffect {
    pub id: Uuid,
    pub generation: u64,
    pub target: ExecutionRef,
    pub destination: Destination,
}

#[derive(Clone, Debug)]
pub enum Destination {
    Unavailable(Option<TranscriptRef>),
    Present,
    Focus(Box<DisplayedView>),
    Choose(Vec<DisplayedView>),
}

#[derive(Clone, Debug)]
pub struct Projection {
    pub overview: bool,
    pub visible: Option<ExecutionRef>,
    pub dispatch_directory: PathBuf,
    pub rows: Vec<AgentSummary>,
    pub waiting_permissions: usize,
    pub waiting_questions: usize,
    pub generation: u64,
    pub closed: bool,
    pub unconfirmed_dispatch: Option<Uuid>,
    /// The execution named by the latest resolved creation receipt of this overview.
    pub created: Option<ExecutionRef>,
}

pub struct AgentConsole {
    access: Arc<dyn AgentAccess>,
    epoch: Uuid,
    host: Option<HostLocation>,
    visible: Option<ExecutionRef>,
    overview: bool,
    directory: PathBuf,
    filter: AgentFilter,
    rows: Vec<AgentSummary>,
    generation: u64,
    pending: Option<NavigationEffect>,
    recent: HashMap<(String, String, ExecutionRef), ViewRef>,
    unavailable_locations: HashSet<(ViewRef, u64)>,
    closed: bool,
    unconfirmed_dispatch: Option<(Uuid, Uuid)>,
    created: Option<ExecutionRef>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    last_viewed: HashMap<ExecutionRef, ExecutionRef>,
    retirements: HashMap<ExecutionRef, ChildRetirement>,
}

impl AgentConsole {
    pub fn new(
        access: Arc<dyn AgentAccess>,
        epoch: Uuid,
        directory: PathBuf,
        host: Option<HostLocation>,
    ) -> Self {
        Self {
            access,
            epoch,
            host,
            visible: None,
            overview: false,
            directory,
            filter: AgentFilter::default(),
            rows: Vec::new(),
            generation: 0,
            pending: None,
            recent: HashMap::new(),
            unavailable_locations: HashSet::new(),
            closed: false,
            unconfirmed_dispatch: None,
            created: None,
            clock: Arc::new(crate::private_fs::unix_time_milliseconds),
            last_viewed: HashMap::new(),
            retirements: HashMap::new(),
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }

    pub async fn refresh(&mut self) -> Result<Projection, AccessError> {
        let mut rows = Vec::new();
        loop {
            let observation = self
                .access
                .observe(ObservationRequest::Overview {
                    filter: AgentFilter::default(),
                    page: Page {
                        offset: rows.len(),
                        limit: MAX_PAGE_LIMIT,
                    },
                    after: None,
                })
                .await?;
            let ObservationValue::Overview(page) = observation.initial else {
                return Err(AccessError::unavailable("invalid overview response"));
            };
            let complete = page.len() < MAX_PAGE_LIMIT;
            self.epoch = observation.cursor.daemon_epoch;
            rows.extend(page);
            if complete {
                break;
            }
        }
        rows.sort_by_key(|row| (AttentionGroup::of(row), row.execution));
        self.rows = rows;
        self.refresh_retirements();
        self.unavailable_locations.retain(|(view, version)| {
            self.rows.iter().any(|row| {
                row.views
                    .iter()
                    .any(|location| location.view == *view && location.version == *version)
            })
        });
        Ok(self.projection())
    }

    pub fn projection(&self) -> Projection {
        let mut rows = self
            .rows
            .iter()
            .filter(|row| {
                self.filter
                    .workdir
                    .as_ref()
                    .is_none_or(|cwd| &row.workdir == cwd)
                    && self
                        .filter
                        .workspace
                        .as_ref()
                        .is_none_or(|workspace| row.workspace.as_ref() == Some(workspace))
            })
            .cloned()
            .collect::<Vec<_>>();
        sort_overview_families(&mut rows);
        Projection {
            overview: self.overview,
            visible: self.visible,
            dispatch_directory: self.directory.clone(),
            waiting_permissions: self.rows.iter().map(|row| row.waiting_permissions).sum(),
            waiting_questions: self.rows.iter().map(|row| row.waiting_questions).sum(),
            rows,
            generation: self.generation,
            closed: self.closed,
            unconfirmed_dispatch: self.unconfirmed_dispatch.map(|(_, id)| id),
            created: self.created,
        }
    }

    pub fn interpret(&mut self, intent: Intent) -> Result<Option<NavigationEffect>, AccessError> {
        if self.closed {
            return Err(AccessError::unavailable("this View has closed"));
        }
        match intent {
            Intent::Overview => {
                self.invalidate();
                self.overview = true;
            }
            Intent::Back => {
                self.invalidate();
                self.overview = false;
                self.created = None;
            }
            Intent::NewDraft => {
                self.invalidate();
                self.overview = false;
                self.visible_ready(None);
                self.created = None;
            }
            Intent::Close => {
                self.invalidate();
                self.closed = true;
                self.created = None;
            }
            Intent::DispatchDirectory(directory) => {
                self.directory = directory;
            }
            Intent::Filter(filter) => {
                self.filter = filter;
            }
            Intent::AcknowledgeUnconfirmedDispatch(command_id) => {
                if self
                    .unconfirmed_dispatch
                    .is_none_or(|(_, pending)| pending != command_id)
                {
                    return Err(AccessError::unavailable(
                        "there is no matching unconfirmed creation",
                    ));
                }
                self.unconfirmed_dispatch = None;
            }
            Intent::Open(target) => {
                self.invalidate();
                let root = self.family_root(target);
                let target = if root == target
                    && (self.overview
                        || self
                            .visible
                            .is_none_or(|visible| self.family_root(visible) != root))
                {
                    self.last_viewed
                        .get(&root)
                        .copied()
                        .filter(|member| {
                            self.rows.iter().any(|row| {
                                row.execution == *member
                                    && row.availability == ExecutionAvailability::Live
                            }) && self.family_root(*member) == root
                                && !self.is_retired(*member)
                        })
                        .unwrap_or(target)
                } else {
                    target
                };
                let row = self
                    .rows
                    .iter()
                    .find(|row| row.execution == target)
                    .ok_or_else(|| {
                        AccessError::unavailable("the selected Agent is no longer live")
                    })?;
                let destination = if row.availability == ExecutionAvailability::Lost {
                    Destination::Unavailable(row.record.clone())
                } else if self.is_descendant(target, self.visible) {
                    Destination::Present
                } else {
                    let locations = row
                        .views
                        .iter()
                        .filter(|view| {
                            !self
                                .unavailable_locations
                                .contains(&(view.view, view.version))
                                && self.host.as_ref().zip(view.host.as_ref()).is_some_and(
                                    |(source, destination)| {
                                        source.host_instance == destination.host_instance
                                            && source.socket == destination.socket
                                            && destination.pane.is_some()
                                    },
                                )
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    let preferred = self
                        .host
                        .as_ref()
                        .and_then(|host| {
                            host.client.as_ref().and_then(|client| {
                                self.recent.get(&(
                                    host.host_instance.clone(),
                                    client.clone(),
                                    target,
                                ))
                            })
                        })
                        .and_then(|view| locations.iter().find(|location| &location.view == view));
                    if let Some(preferred) = preferred {
                        Destination::Focus(Box::new(preferred.clone()))
                    } else {
                        match locations.len() {
                            0 => Destination::Present,
                            1 => Destination::Focus(Box::new(locations[0].clone())),
                            _ => Destination::Choose(locations),
                        }
                    }
                };
                let effect = NavigationEffect {
                    id: Uuid::new_v4(),
                    generation: self.generation,
                    target,
                    destination,
                };
                self.pending = Some(effect.clone());
                return Ok(Some(effect));
            }
            Intent::ChooseLocation { generation, view } => {
                let pending = self
                    .pending
                    .as_ref()
                    .filter(|pending| pending.generation == generation)
                    .ok_or_else(|| AccessError::unavailable("the navigation choice has expired"))?;
                let Destination::Choose(locations) = &pending.destination else {
                    return Err(AccessError::unavailable("there is no location choice"));
                };
                let destination = locations
                    .iter()
                    .find(|location| location.view == view)
                    .ok_or_else(|| {
                        AccessError::unavailable("the selected location no longer exists")
                    })?
                    .clone();
                let effect = NavigationEffect {
                    destination: Destination::Focus(Box::new(destination)),
                    ..pending.clone()
                };
                self.pending = Some(effect.clone());
                return Ok(Some(effect));
            }
        }
        Ok(None)
    }

    pub fn is_current(&self, effect: &NavigationEffect) -> bool {
        !self.closed
            && self.pending.as_ref().is_some_and(|pending| {
                pending.id == effect.id
                    && pending.generation == effect.generation
                    && pending.target == effect.target
            })
    }

    pub fn location_unavailable(&mut self, effect: &NavigationEffect) -> bool {
        if !self.is_current(effect) {
            return false;
        }
        let Destination::Focus(location) = &effect.destination else {
            return false;
        };
        self.unavailable_locations
            .insert((location.view, location.version));
        true
    }

    pub async fn revalidate(&mut self, effect: &NavigationEffect) -> Result<bool, AccessError> {
        if !self.is_current(effect) {
            return Ok(false);
        }
        self.refresh().await?;
        if !self.is_current(effect) {
            return Ok(false);
        }
        Ok(self
            .rows
            .iter()
            .find(|row| {
                row.execution == effect.target && row.availability == ExecutionAvailability::Live
            })
            .is_some_and(|row| match &effect.destination {
                Destination::Focus(destination) => row.views.iter().any(|view| {
                    view.view == destination.view
                        && view.version == destination.version
                        && view.execution == destination.execution
                        && view.host == destination.host
                }),
                Destination::Present => true,
                Destination::Choose(_) => false,
                Destination::Unavailable(_) => false,
            }))
    }

    pub fn complete(&mut self, effect: &NavigationEffect) -> bool {
        if !self.is_current(effect) {
            return false;
        }
        match &effect.destination {
            Destination::Present => {
                self.visible_ready(Some(effect.target));
                self.overview = false;
            }
            Destination::Focus(destination) => {
                if let Some(host) = &self.host
                    && let Some(client) = &host.client
                {
                    self.recent.insert(
                        (host.host_instance.clone(), client.clone(), effect.target),
                        destination.view,
                    );
                }
            }
            Destination::Choose(_) | Destination::Unavailable(_) => return false,
        }
        if self.created == Some(effect.target) {
            self.created = None;
        }
        self.pending = None;
        true
    }

    pub fn visible_ready(&mut self, visible: Option<ExecutionRef>) {
        if !self.closed {
            let now = (self.clock)();
            if self.visible != visible
                && let Some(previous) = self.visible
                && self.retirements.contains_key(&previous)
            {
                let retirement = self.retirements.get_mut(&previous).unwrap();
                retirement.departed_at = Some(now);
                if retirement.completion.is_some() {
                    retirement.deadline = Some(now.saturating_add(FINISHED_CHILD_GRACE_MS));
                }
            }
            self.visible = visible;
            if let Some(visible) = visible {
                if let Some(retirement) = self.retirements.get_mut(&visible) {
                    retirement.deadline = None;
                    retirement.departed_at = None;
                }
                self.last_viewed.insert(self.family_root(visible), visible);
            }
        }
    }

    pub fn family_members(&self, visible: ExecutionRef) -> Vec<AgentSummary> {
        let root = self.family_root(visible);
        self.rows
            .iter()
            .filter(|row| row.execution == root || self.is_descendant(row.execution, Some(root)))
            .cloned()
            .collect()
    }

    pub fn family(&self, visible: ExecutionRef) -> Vec<AgentSummary> {
        let root = self.family_root(visible);
        self.family_members(visible)
            .into_iter()
            .filter(|row| {
                row.execution == root || row.execution == visible || !self.is_retired(row.execution)
            })
            .collect()
    }

    fn family_root(&self, visible: ExecutionRef) -> ExecutionRef {
        let mut root = visible;
        let mut visited = HashSet::new();
        while visited.insert(root) {
            let Some(parent) = self
                .rows
                .iter()
                .find(|row| row.execution == root)
                .and_then(|row| row.parent)
                .filter(|parent| self.rows.iter().any(|row| row.execution == *parent))
            else {
                break;
            };
            root = parent;
        }
        root
    }

    fn is_retired(&self, execution: ExecutionRef) -> bool {
        self.visible != Some(execution)
            && self
                .retirements
                .get(&execution)
                .and_then(|retirement| retirement.deadline)
                .is_some_and(|deadline| deadline <= (self.clock)())
    }

    fn refresh_retirements(&mut self) {
        let now = (self.clock)();
        self.retirements.retain(|execution, _| {
            self.rows
                .iter()
                .any(|row| row.execution == *execution && row.parent.is_some())
        });
        for row in &self.rows {
            if row.parent.is_none() {
                continue;
            }
            let retirement = self.retirements.entry(row.execution).or_default();
            let terminal = matches!(
                row.state,
                LoopState::Completed
                    | LoopState::Failed
                    | LoopState::Stopped
                    | LoopState::Interrupted
                    | LoopState::Expired
            );
            if row.availability == ExecutionAvailability::Live
                && terminal
                && row.waiting_permissions + row.waiting_questions == 0
            {
                let completion = TerminalTransition {
                    run_id: row.run_id,
                    state: row.state.clone(),
                    finished_at: row.updated_at,
                };
                if retirement.completion.as_ref() != Some(&completion) {
                    let finished_at = row.updated_at.unwrap_or(now);
                    let grace_start = retirement
                        .departed_at
                        .filter(|departure| *departure >= finished_at)
                        .unwrap_or(finished_at);
                    retirement.completion = Some(completion);
                    retirement.deadline = Some(grace_start.saturating_add(FINISHED_CHILD_GRACE_MS));
                }
                if self.visible == Some(row.execution) {
                    retirement.deadline = None;
                }
            } else {
                retirement.completion = None;
                retirement.deadline = None;
            }
        }
        let invalid = self
            .last_viewed
            .iter()
            .filter(|(_, member)| {
                self.is_retired(**member)
                    || !self.rows.iter().any(|row| {
                        row.execution == **member && row.availability == ExecutionAvailability::Live
                    })
            })
            .map(|(root, _)| *root)
            .collect::<Vec<_>>();
        for root in invalid {
            self.last_viewed.remove(&root);
        }
    }

    pub async fn dispatch(
        &mut self,
        mut launch: LaunchSpec,
        input: UserInput,
        origin: Option<InteractionSource>,
    ) -> Result<CommandReceipt, AccessError> {
        if let Some((_, command_id)) = self.unconfirmed_dispatch {
            return Err(AccessError::new(
                AccessErrorCode::OutcomeUnknown,
                format!(
                    "Previous creation {command_id} is still unconfirmed; inspect its receipt before submitting again"
                ),
            ));
        }
        self.created = None;
        launch.workdir = self.directory.clone();
        launch.exact_chat = None;
        let command_id = Uuid::new_v4();
        match self
            .access
            .execute(CommandEnvelope {
                daemon_epoch: self.epoch,
                command_id,
                command: AgentCommand::Create {
                    launch,
                    initial: Some(input),
                    origin,
                },
            })
            .await
        {
            Ok(receipt)
                if !matches!(
                    receipt.state,
                    ReceiptState::Completed | ReceiptState::Failed
                ) =>
            {
                self.unconfirmed_dispatch = Some((self.epoch, command_id));
                Err(AccessError::new(
                    AccessErrorCode::OutcomeUnknown,
                    format!(
                        "Creation {command_id} is still being processed; wait for its receipt before submitting again"
                    ),
                ))
            }
            Err(error) if error.code == AccessErrorCode::OutcomeUnknown => {
                self.unconfirmed_dispatch = Some((self.epoch, command_id));
                if let Some(receipt) = self.reconcile_dispatch().await? {
                    return Ok(receipt);
                }
                Err(AccessError::new(
                    AccessErrorCode::OutcomeUnknown,
                    format!(
                        "Creation {command_id} acceptance is unknown; query this receipt before sending it again"
                    ),
                ))
            }
            Ok(receipt) => {
                self.created = created_execution(&receipt);
                Ok(receipt)
            }
            Err(error) => Err(error),
        }
    }

    pub async fn reconcile_dispatch(&mut self) -> Result<Option<CommandReceipt>, AccessError> {
        let Some((epoch, command_id)) = self.unconfirmed_dispatch else {
            return Ok(None);
        };
        let result = self
            .access
            .observe(ObservationRequest::Receipt { epoch, command_id })
            .await?;
        let ObservationValue::Receipt(receipt) = result.initial else {
            return Err(AccessError::unavailable("invalid command receipt response"));
        };
        if let Some(receipt) = receipt
            && matches!(
                receipt.state,
                ReceiptState::Completed | ReceiptState::Failed
            )
        {
            self.unconfirmed_dispatch = None;
            self.created = created_execution(&receipt);
            return Ok(Some(*receipt));
        }
        Ok(None)
    }

    fn invalidate(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.pending = None;
    }
    fn is_descendant(&self, target: ExecutionRef, parent: Option<ExecutionRef>) -> bool {
        let Some(parent) = parent else {
            return false;
        };
        let mut current = target;
        let mut visited = HashSet::new();
        while visited.insert(current) {
            let Some(next) = self
                .rows
                .iter()
                .find(|row| row.execution == current)
                .and_then(|row| row.parent)
            else {
                break;
            };
            if next == parent {
                return true;
            }
            current = next;
        }
        false
    }
}

fn created_execution(receipt: &CommandReceipt) -> Option<ExecutionRef> {
    match &receipt.result {
        Some(CommandResult::Created { execution, .. }) if receipt.error.is_none() => {
            Some(*execution)
        }
        _ => None,
    }
}

/// How much attention an Agent row asks for. Overview families group and sort by their most
/// urgent member, so work or errors below a finished root stay prominent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AttentionGroup {
    NeedsInput,
    Errors,
    Working,
    Waiting,
    Idle,
    Done,
    /// Idle Agents whose producer did not report whether they ever worked.
    Unknown,
    Empty,
}

impl AttentionGroup {
    pub const ALL: [Self; 8] = [
        Self::NeedsInput,
        Self::Errors,
        Self::Working,
        Self::Waiting,
        Self::Idle,
        Self::Done,
        Self::Unknown,
        Self::Empty,
    ];

    /// Requests, loss and the reported state decide first; a missing work fact only matters for
    /// an Agent that would otherwise read as Idle.
    pub fn of(row: &AgentSummary) -> Self {
        if row.waiting_permissions + row.waiting_questions > 0 {
            return Self::NeedsInput;
        }
        if row.availability == ExecutionAvailability::Lost {
            return Self::Errors;
        }
        if row.has_work == Some(false) {
            return Self::Empty;
        }
        match row.state {
            LoopState::WaitingPermission | LoopState::WaitingQuestion | LoopState::WaitingPlan => {
                Self::NeedsInput
            }
            LoopState::Failed => Self::Errors,
            LoopState::Running | LoopState::Queued => Self::Working,
            LoopState::WaitingTrigger => Self::Waiting,
            LoopState::Idle if row.has_work.is_none() => Self::Unknown,
            LoopState::Idle => Self::Idle,
            LoopState::Completed
            | LoopState::Stopped
            | LoopState::Interrupted
            | LoopState::Expired => Self::Done,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::NeedsInput => "Needs input",
            Self::Errors => "Errors",
            Self::Working => "Working",
            Self::Waiting => "Waiting",
            Self::Idle => "Idle",
            Self::Done => "Done",
            Self::Unknown => "Unknown",
            Self::Empty => "Empty",
        }
    }
}

/// Families list by their most urgent member, then by their latest reported state change (unknown
/// last), then by root reference as a stable tie-break that claims no order in time. Members stay
/// under their root, siblings newest first.
fn sort_overview_families(rows: &mut [AgentSummary]) {
    let summaries = rows
        .iter()
        .map(|row| (row.execution, row))
        .collect::<HashMap<_, _>>();
    let mut roots = HashMap::new();
    let mut groups = HashMap::new();
    let mut latest = HashMap::<ExecutionRef, Option<u64>>::new();
    for row in rows.iter() {
        let mut root = row.execution;
        let mut visited = HashSet::from([root]);
        while let Some(parent) = summaries[&root]
            .parent
            .filter(|parent| summaries.contains_key(parent))
        {
            if !visited.insert(parent) {
                root = *visited.iter().min().unwrap();
                break;
            }
            root = parent;
        }
        roots.insert(row.execution, root);
        let group = AttentionGroup::of(row);
        groups
            .entry(root)
            .and_modify(|family: &mut AttentionGroup| *family = (*family).min(group))
            .or_insert(group);
        latest
            .entry(root)
            .and_modify(|family| *family = (*family).max(row.updated_at))
            .or_insert(row.updated_at);
    }
    rows.sort_by_key(|row| {
        let root = roots[&row.execution];
        (
            groups[&root],
            Reverse(latest[&root]),
            root,
            row.execution != root,
            Reverse(row.updated_at),
            row.execution,
        )
    });
}
