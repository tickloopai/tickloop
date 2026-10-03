use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tickloop_agent::runtime::TodoItem;

use super::workspace_trust::{TRUST_ACCEPT_CHOICE_INDEX, TrustDecision, WorkspaceTrust};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{self, Clear, ClearType, ScrollUp};
use crossterm::{execute, queue};
use tickloop_agent::{
    file_change::FileChangeReview,
    image::Image,
    runtime::{
        ImageContext, ImageOrigin, InputContext, QueuedInput, QueuedInputKind,
        TickSubscriptionStatus,
    },
    settings::DisplayPreferences,
};
use tokio::sync::mpsc;
use unicode_width::UnicodeWidthChar;
use uuid::Uuid;

mod agent_layout;
#[cfg(test)]
#[path = "console/connection_tests.rs"]
mod connection_tests;
#[cfg(test)]
#[path = "console/conversation_picker_tests.rs"]
mod conversation_picker_tests;
mod input_dialog;
#[cfg(test)]
mod input_dialog_pty;
#[cfg(test)]
mod input_dialog_tests;
#[cfg(test)]
#[path = "console/layout_tests.rs"]
mod layout_tests;
#[cfg(test)]
#[path = "console/model_dialog_pty.rs"]
mod model_dialog_pty;
mod prompt_suggestion;
#[cfg(test)]
mod prompt_suggestion_pty;
#[cfg(test)]
mod prompt_suggestion_tests;
#[cfg(test)]
#[path = "console/request_layout_tests.rs"]
mod request_layout_tests;
#[cfg(test)]
#[path = "console/tick_list_tests.rs"]
mod tick_list_tests;
#[cfg(test)]
#[path = "console/unified_footer_tests.rs"]
mod unified_footer_tests;
#[cfg(test)]
#[path = "console/workspace_trust_tests.rs"]
mod workspace_trust_tests;
use agent_layout::OverviewState;
use agent_layout::OverviewTarget;
pub(super) use agent_layout::{
    AgentOverview, AgentTreeRow, MIN_REQUEST_AGENT_TITLE_COLUMNS, RowExtras, status_glyph,
};

use super::{
    clipboard::Clipboard,
    clipboard_hint::{CLIPBOARD_IMAGE_HINT, ClipboardImageHint},
    completion::{
        CHILD_AUTHORIZATION_FIXED, CommandScope, Completion, CompletionAction, CompletionKey,
        unavailable_command,
    },
    conversation_picker::{ConversationPicker, ConversationPickerEffect},
    diff_catalog::{
        CatalogChangeDetail, ChangeCatalog, ChangeSourceId, ChangeSourceState, CurrentFileStatus,
        GitFileState,
    },
    diff_dialog::{DiffDialog, DiffDialogAction, DiffDialogMode, DiffDialogOutcome},
    diff_view::{
        DiffLineKind, DiffNavigation, DiffNavigationAction, DiffViewport, navigate_diff,
        navigate_error, render_diff, render_error,
    },
    display::{DisplayProfile, Glyph},
    editor::{Editor, EditorCommand, EditorOutcome},
    external_editor::{ExternalEditorLauncher, ExternalEditorOutcome, edit_prompt},
    file_mentions::{
        AcceptedMention, FileMentionAction, FileMentionIndex, FileMentionKey, FileMentionState,
        IndexGeneration, IndexReplacement, draft_mentions_path,
    },
    footer::{FooterAction, FooterEffect, FooterFrame, FooterItemId, FooterState},
    hyperlink,
    image_cache::{ImageCache, ImageLinks},
    input_history::{
        HistoryScope, HistorySearch, HistorySearchInput, HistorySearchOutcome, InputHistory,
        PastedText,
    },
    input_view::InputView,
    paste_blocks::{PasteBlocks, PasteRegistration},
    pasted_image_paths::{find_image_paths, read_pasted_image},
    permissions_surface::{
        PermissionsEffect, PermissionsPage, PermissionsReturn, PermissionsSubject,
        PermissionsSurface,
    },
    renderer::{self, Renderer},
    scrollback::{InputEcho, ScrollAction, Scrollback, ToolCallState},
    selection::{Cell, Selection, line_width, split_columns},
    shell_input::{
        InputKind, ShellCompletionKey, ShellCompletionKind, ShellEmptyControl, ShellExecution,
        ShellInputAction, ShellInputState, UserShellSubmission,
    },
    status_area::{BackgroundStatusNotice, StatusArea},
    style::{ColourDepth, Role, RoleLine, RoleSpan, Span, StyledLine, ThemeName},
    subscription_surface::{
        SubscriptionAction, SubscriptionEffect, SubscriptionFocus, SubscriptionSurface,
        SubscriptionViewport,
    },
    task_surface::{
        TaskOutputUpdate, TaskSurface, TaskSurfaceAction, TaskSurfaceEffect, TaskSurfaceFocus,
        TaskSurfaceSnapshot, TaskViewport,
    },
    tick_list::{TICK_LIST_AUTO_MIN_COLUMNS, TICK_LIST_COLUMNS, TickGroup, TickList},
    transcript::{Transcript, TranscriptDocument, TranscriptEvent},
};

/// DECSTBM with no parameters: drop any scroll region the frame set up.
const RESET_SCROLL_REGION: &str = "\x1b[r";
/// What a queued input names as its target when it starts a Loop of its own; one bound to an
/// existing Loop of the shown Agent names nothing further.
const NEW_LOOP_TARGET: &str = "new Loop";
const MAX_PENDING_INPUT_ROWS: usize = 4;
const DIVIDER_EDGE_RULE_COLUMNS: usize = 1;
const DIVIDER_ENTRY_GAP_COLUMNS: usize = 1;
const CREATION_ATTACH_UNAVAILABLE_PREFIX: &str = "New Agent created; local connection unavailable";
const EXECUTION_CONNECTION_DETAILS_HINT: &str = "Ctrl+O details";

/// Pairs the escape sequences that move between screen buffers with the record
/// of which buffer the terminal is on, so the two cannot drift apart. The record
/// lives outside the state lock: a poisoned lock must never be able to strand
/// the terminal on the alternate screen. Both directions err towards "possibly
/// on the alternate screen", because a spurious leave is harmless while a
/// missed one leaves the user with an unusable terminal.
#[derive(Clone)]
struct TerminalSurface {
    alternate_screen: Arc<AtomicBool>,
    mouse: bool,
}

impl TerminalSurface {
    fn new(mouse: bool) -> Self {
        Self {
            alternate_screen: Arc::new(AtomicBool::new(false)),
            mouse,
        }
    }

    fn renderer(&self) -> Renderer {
        if self.alternate_screen.load(Ordering::Acquire) {
            Renderer::Fullscreen
        } else {
            Renderer::Inline
        }
    }

    fn take(&self, renderer: Renderer, output: &mut impl Write) -> io::Result<()> {
        self.alternate_screen
            .store(renderer.uses_alternate_screen(), Ordering::Release);
        renderer.enter(self.mouse, output)
    }

    fn release(&self, renderer: Renderer, output: &mut impl Write) -> io::Result<()> {
        let left = renderer.leave(self.mouse, output);
        if left.is_ok() {
            self.alternate_screen.store(false, Ordering::Release);
        }
        left
    }
}

#[cfg(test)]
use super::tick_list::TickListEntry;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RequestPresentation {
    Presented,
    Deferred,
}

const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const INPUT_CHANNEL_CAPACITY: usize = 128;
const FOOTER_ROWS: usize = 3;
const FOOTER_ITEMS_ROWS: usize = 1;
const MIN_FOOTER_ITEMS_TERMINAL_ROWS: usize = FOOTER_ROWS + FOOTER_ITEMS_ROWS;
const MIN_SCROLL_ROWS: usize = 2;
const MIN_MENU_BODY_ROWS: usize = 5;
const MIN_MENU_TERMINAL_ROWS: usize = FOOTER_ROWS + MIN_SCROLL_ROWS + MIN_MENU_BODY_ROWS;
const TASK_SURFACE_HEADER_FOOTER_ROWS: usize = 2;
/// The subscription view's own title row and footer row.
const SUBSCRIPTION_SURFACE_HEADER_ROWS: usize = 1;
const SUBSCRIPTION_SURFACE_FOOTER_ROWS: usize = 1;
const EXIT_CONFIRMATION_WINDOW: Duration = Duration::from_millis(800);
const ESCAPE_CONFIRMATION_WINDOW: Duration = Duration::from_millis(800);
const SHELL_BACKGROUND_CHORD_WINDOW: Duration = Duration::from_millis(800);
const SECRET_PASTE_ERROR: &str = "API key paste must contain one line of text";
pub(super) const REFRESH_ACCOUNT_MODELS_LABEL: &str = "Refresh account models";
const COMPACT_PICKER_COLUMNS: usize = 100;
// 照 Claude Code `shellHistoryCompletion.ts`：输入期间历史不会变化，缓存 60 秒。
const SHELL_HISTORY_CACHE_TTL: Duration = Duration::from_secs(60);
const NO_CLIPBOARD_IMAGE: &str = "No image found in clipboard";
const NO_CLIPBOARD_IMAGE_OVER_SSH: &str = "No image found in clipboard · over SSH this reads the remote host's clipboard; attach the image with @path instead";
const IMAGE_INPUT_UNSUPPORTED: &str = "Current model does not support image input · switch to a model that accepts images with /model";

pub(super) enum InputEvent {
    Line(String),
    Prompt {
        text: String,
        file_references: Vec<PathBuf>,
        /// Pasted images in the order of their `[Image #N]` placeholders.
        images: Vec<ImageContext>,
    },
    UserShellSubmitted(UserShellSubmission),
    BackgroundForegroundShell,
    CycleExecutionMode,
    OpenOverview,
    CloseOverview,
    OpenAgent(Uuid),
    ToggleTickList {
        show: bool,
    },
    ArtifactChoice(Box<super::artifacts_picker::ArtifactChoice>),
    TickAction {
        chat: Uuid,
        action: super::tick_actions::PreparedTickAction,
    },
    TaskSurface(TaskSurfaceEffect),
    Subscription(SubscriptionEffect),
    Permissions(PermissionsEffect),
    Choice {
        menu_id: Uuid,
        index: usize,
        comment: Option<String>,
    },
    PickerAction {
        menu_id: Uuid,
        index: usize,
        action: PickerAction,
    },
    ExpandChoice {
        menu_id: Uuid,
        index: usize,
    },
    /// Moves a question menu to the previous or next question.
    SwitchQuestion {
        menu_id: Uuid,
        next: bool,
    },
    Cancel {
        menu_id: Uuid,
    },
    OpenTranscript {
        id: Uuid,
    },
    OpenRewind,
    Suspend,
    Interrupt,
    InterruptActivity,
    TakeBackQueued {
        draft_empty: bool,
    },
    Escape,
    Eof,
    Error(String),
}

impl fmt::Debug for InputEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ToggleTickList { show } => formatter
                .debug_struct("ToggleTickList")
                .field("show", show)
                .finish(),
            Self::ArtifactChoice(choice) => formatter
                .debug_tuple("ArtifactChoice")
                .field(choice)
                .finish(),
            Self::TickAction { chat, action } => formatter
                .debug_struct("TickAction")
                .field("chat", chat)
                .field("action", action)
                .finish(),
            Self::InterruptActivity => formatter.write_str("InterruptActivity"),
            Self::Line(value) => formatter.debug_tuple("Line").field(value).finish(),
            Self::Prompt {
                text,
                file_references,
                images,
            } => formatter
                .debug_struct("Prompt")
                .field("text", text)
                .field("file_references", file_references)
                .field("images", &images.len())
                .finish(),
            Self::UserShellSubmitted(value) => formatter
                .debug_tuple("UserShellSubmitted")
                .field(value)
                .finish(),
            Self::BackgroundForegroundShell => formatter.write_str("BackgroundForegroundShell"),
            Self::CycleExecutionMode => formatter.write_str("CycleExecutionMode"),
            Self::OpenOverview => formatter.write_str("OpenOverview"),
            Self::CloseOverview => formatter.write_str("CloseOverview"),
            Self::OpenAgent(value) => formatter.debug_tuple("OpenAgent").field(value).finish(),
            Self::TaskSurface(value) => formatter.debug_tuple("TaskSurface").field(value).finish(),
            Self::Subscription(value) => {
                formatter.debug_tuple("Subscription").field(value).finish()
            }
            Self::Permissions(_) => formatter.write_str("Permissions"),
            Self::Choice {
                menu_id,
                index,
                comment,
            } => formatter
                .debug_struct("Choice")
                .field("menu_id", menu_id)
                .field("index", index)
                .field("comment", &comment.as_ref().map(|_| "[redacted]"))
                .finish(),
            Self::PickerAction {
                menu_id,
                index,
                action,
            } => formatter
                .debug_struct("PickerAction")
                .field("menu_id", menu_id)
                .field("index", index)
                .field("action", action)
                .finish(),
            Self::ExpandChoice { menu_id, index } => formatter
                .debug_struct("ExpandChoice")
                .field("menu_id", menu_id)
                .field("index", index)
                .finish(),
            Self::SwitchQuestion { menu_id, next } => formatter
                .debug_struct("SwitchQuestion")
                .field("menu_id", menu_id)
                .field("next", next)
                .finish(),
            Self::Cancel { menu_id } => formatter
                .debug_struct("Cancel")
                .field("menu_id", menu_id)
                .finish(),
            Self::OpenTranscript { id } => formatter
                .debug_struct("OpenTranscript")
                .field("id", id)
                .finish(),
            Self::OpenRewind => formatter.write_str("OpenRewind"),
            Self::Suspend => formatter.write_str("Suspend"),
            Self::Interrupt => formatter.write_str("Interrupt"),
            Self::TakeBackQueued { draft_empty } => formatter
                .debug_struct("TakeBackQueued")
                .field("draft_empty", draft_empty)
                .finish(),
            Self::Escape => formatter.write_str("Escape"),
            Self::Eof => formatter.write_str("Eof"),
            Self::Error(value) => formatter.debug_tuple("Error").field(value).finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PickerAction {
    ThinkingPrevious,
    ThinkingNext,
    ToggleConnections,
    ConfigureProvider,
    RefreshAccountModels,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PickerKind {
    Provider,
    Actions,
    Model,
}

#[derive(Clone)]
pub(super) struct CompactPickerRow {
    pub(super) label: String,
    pub(super) detail: String,
}

#[derive(Clone, Default)]
pub(super) struct PickerPresentation {
    pub(super) headers: Vec<(usize, String)>,
    pub(super) hint: String,
    pub(super) refresh: bool,
    pub(super) connection_options: Option<usize>,
    pub(super) row_hints: Vec<String>,
    pub(super) compact_rows: Vec<CompactPickerRow>,
}

#[derive(Clone)]
enum MenuFocus {
    WhenInputEmpty,
    Menu,
    Plan,
    MenuWithLink(String),
    Secret(String),
    Input(String),
    Picker(PickerKind, PickerPresentation),
    Question(QuestionMenu),
}

/// Keys an AskUserQuestion menu adds to a selection menu.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct QuestionMenu {
    /// The "Other" row: Enter, Space or its number opens a text field prefilled with
    /// `entry_text` instead of choosing the row.
    pub entry_option: Option<usize>,
    pub entry_text: String,
    /// Space chooses the focused row like Enter, toggling multi-select options.
    pub multi_select: bool,
    /// ←/→ and h/l move between questions.
    pub navigable: bool,
}

const QUESTION_ENTRY_LABEL: &str = "Answer";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum BranchSubmission {
    #[default]
    Idle,
    Pending,
    FinishedCommitted,
    FinishedCancelled,
    FinishedCancelledForSummary,
    PreservedCancelled,
}

#[derive(Clone)]
pub(super) struct Menu {
    pub id: Uuid,
    pub title: String,
    pub context: Vec<String>,
    pub options: Vec<String>,
    pub selected: usize,
    pub confirmation: bool,
}

#[derive(Clone)]
pub(super) struct Console {
    state: Arc<Mutex<State>>,
    _lifetime: Arc<Lifetime>,
    message_id: Option<String>,
    #[cfg(test)]
    reset_chat_failure: Arc<Mutex<Option<String>>>,
    #[cfg(test)]
    branch_commit_failure: Arc<Mutex<Option<String>>>,
}

pub(super) struct ArtifactMetadataSink {
    state: std::sync::Weak<Mutex<State>>,
}

impl ArtifactMetadataSink {
    pub(super) fn update(
        &self,
        chat: Uuid,
        id: String,
        metadata: Option<crate::artifacts::ArtifactMetadata>,
    ) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let Ok(mut state) = state.lock() else {
            return;
        };
        let list = if state.chat == Some(chat) {
            &mut state.tick_list
        } else {
            &mut state.view_cache.entry(Some(chat)).or_default().tick_list
        };
        list.metadata_failures.remove(&id);
        list.metadata.insert(id, metadata);
        list.rebuild();
    }

    pub(super) fn failed(&self, chat: Uuid, id: String, error: String) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let Ok(mut state) = state.lock() else {
            return;
        };
        let is_current = state.chat == Some(chat);
        let list = if is_current {
            &mut state.tick_list
        } else {
            &mut state.view_cache.entry(Some(chat)).or_default().tick_list
        };
        list.metadata_failures.insert(id, error.clone());
        list.rebuild();
        if is_current {
            state.hold_status(&format!(
                "Artifact refresh failed: {} · r retries",
                super::safe_text(&error)
            ));
        }
    }
}

struct Lifetime {
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    reader: Mutex<Option<JoinHandle<()>>>,
    interactive: bool,
    surface: TerminalSurface,
}

pub(super) struct State {
    workspace_trust: Option<WorkspaceTrust>,
    interactive: bool,
    /// Plain output ends inside a line, such as a reply streamed without its line feed yet.
    plain_line_open: bool,
    renderer: Renderer,
    surface: TerminalSurface,
    synchronized_output: bool,
    clipboard: Clipboard,
    external_editor: ExternalEditorLauncher,
    /// Plain text of the rows most recently drawn. Selection is what the user
    /// can see, so it is taken from here rather than from the conversation.
    rendered_screen: Vec<String>,
    /// Every row as it was last written, so a frame repaints only the rows
    /// that changed since.
    rendered_rows: Vec<String>,
    /// Frames stay off the terminal while a [`FrameHold`] is alive; the frame
    /// asked for meanwhile is drawn when the last hold ends.
    held_frames: usize,
    frame_deferred: bool,
    selection: Option<Selection>,
    display: DisplayProfile,
    colour_depth: ColourDepth,
    prompt_enabled: bool,
    prompt_suggestion_enabled: bool,
    prompt_suggestion: Option<String>,
    prompt_suggestion_request: Option<super::prompt_suggestion::SuggestionRequest>,
    foreground_shell: bool,
    shell_background_chord_started_at: Option<Instant>,
    width: usize,
    height: usize,
    scrollback: Scrollback,
    /// Startup and local feedback from before this View's draft created its Chat.
    creation_prefixes: HashMap<Option<Uuid>, Vec<CreationPrefixEntry>>,
    echoed_inputs: HashSet<Uuid>,
    awaiting_input_echoes: HashSet<Uuid>,
    pub(super) chat: Option<Uuid>,
    activity_interruptible: bool,
    back_to_bottom_target: Option<BackToBottomTarget>,
    back_to_bottom_pressed: bool,
    tick_toggle_target: Option<BackToBottomTarget>,
    tick_row_targets: Vec<TickRowTarget>,
    footer_bar_target: Option<FooterBarTarget>,
    pointer_input: Option<InputEvent>,
    completion: Completion,
    file_mentions: FileMentionState,
    input_owner: Uuid,
    editor: Editor,
    editor_cache: HashMap<Option<Uuid>, ChatInput>,
    view_cache: HashMap<Option<Uuid>, ViewPresentation>,
    paste_blocks: PasteBlocks,
    restored_contexts: Vec<InputContext>,
    input_stash: Option<InputStash>,
    /// Whether the current Chat's model takes images, so `Ctrl+V` can refuse before
    /// inserting a placeholder. The Agent still checks again when the prompt is sent.
    accepts_images: bool,
    /// macOS terminals turn `Cmd+V` on a clipboard image into an empty bracketed paste
    /// (Claude Code `usePasteHandler`), so there an empty paste reads the clipboard image.
    empty_paste_reads_clipboard_image: bool,
    /// Where pasted images are kept for their `[Image #N]` links; `None` when Chats are not saved.
    image_cache: Option<ImageCache>,
    terminal_shows_links: bool,
    clipboard_hint: ClipboardImageHint,
    shell_input: ShellInputState,
    queued_inputs: Vec<QueuedInput>,
    input_history: Option<InputHistory>,
    history_search: Option<HistorySearch>,
    history_project: PathBuf,
    history_session: String,
    shell_history_cache: Option<ShellHistoryCache>,
    /// 最近一次 Enter 提交的原文及其折叠粘贴，供记录输入历史时还原粘贴位置。
    submitted_pasted_texts: Option<(String, Vec<PastedText>)>,
    /// Editor state of the last sent prompt, put back if the Agent refuses it.
    submitted_prompt: Option<SubmittedPrompt>,
    /// The input with an unknown acceptance result that this unchanged, restored draft retries.
    /// Any edit or switch of the input target clears it.
    retry_of: Option<Uuid>,
    /// `retry_of` of the input just submitted, read once by the run loop.
    submitted_retry_of: Option<Uuid>,
    mention_roots: Option<(PathBuf, Vec<PathBuf>)>,
    suspended_dialog: Option<input_dialog::DialogState>,
    dialog_cache:
        HashMap<Option<Uuid>, (input_dialog::DialogState, Option<input_dialog::DialogState>)>,
    resumed_task_effect: Option<(Uuid, TaskSurfaceEffect)>,
    dialog_cursor_visible: bool,
    dialog_option_rows: Vec<(std::ops::Range<usize>, usize)>,
    request_queue_count: usize,
    menu: Option<Menu>,
    menu_rule_role: Option<Role>,
    picker_kind: Option<PickerKind>,
    picker_presentation: PickerPresentation,
    /// The `/resume` picker owns input while open; the
    /// draft and conversation underneath stay untouched.
    artifacts_picker: Option<super::artifacts_picker::ArtifactsPicker>,
    conversation_picker: Option<ConversationPicker>,
    menu_link: Option<(Uuid, String)>,
    menu_review: Option<MenuReviewState>,
    full_review: bool,
    menu_plan: bool,
    menu_comment: Option<MenuCommentState>,
    menu_expand_options: Vec<usize>,
    question_menu: Option<QuestionMenu>,
    request_layout: Option<Uuid>,
    request_menu: Option<Uuid>,
    menu_submission_pending: Option<Uuid>,
    menu_submission_finished: Option<Uuid>,
    branch_submission: BranchSubmission,
    menu_focused: bool,
    review_focused: bool,
    context_offset: usize,
    pub(super) rendered_region: Option<RenderedRegion>,
    screen_initialized: bool,
    transcript: Option<(Uuid, Transcript)>,
    diff_screen: Option<DiffScreen>,
    diff_screen_cache: HashMap<Uuid, DiffDialog>,
    pub(super) tick_list: TickList,
    tick_list_preference: tickloop_agent::settings::TickListPreference,
    task_surface: Option<TaskSurface<Uuid>>,
    task_surface_cache: HashMap<Uuid, TaskSurface<Uuid>>,
    subscription_surface: Option<SubscriptionSurface>,
    permissions_surface: Option<PermissionsSurface>,
    footer: FooterState,
    status_area: StatusArea,
    overview: OverviewState,
    overview_input: ChatInput,
    overview_source_input: Option<ChatInput>,
    overview_source_view: Option<ViewPresentation>,
    help_open: bool,
    help_offset: usize,
    last_input_edit: Option<Instant>,
    footer_list_focused: bool,
    conversation_focused: bool,
    status: String,
    configured_status_lines: Option<Vec<String>>,
    background_status_notice: Option<BackgroundStatusNotice>,
    connection_notice: Option<String>,
    execution_connection_notice: Option<ExecutionConnectionNotice>,
    execution_losses: HashMap<Uuid, Vec<ExecutionLossNotice>>,
    last_connection_diagnostics: HashMap<Uuid, String>,
    rendered_execution_losses: HashSet<Uuid>,
    creation_notice: Option<String>,
    /// The visible Agent's owner-reported delegation; reset whenever the visible Agent changes.
    visible_delegated: bool,
    /// The status explains why the last action did nothing; periodic refreshes keep it until the
    /// next key.
    status_held: bool,
    exit_confirmation: Option<ExitConfirmation>,
    escape_confirmation: Option<EscapeConfirmation>,
}

/// The size and view `rendered_rows` were written under. A frame of the same
/// view at the same size rewrites only the rows whose output changed; any other
/// frame starts from a cleared screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RenderedRegion {
    width: usize,
    height: usize,
    view: RenderedView,
}

impl RenderedRegion {
    fn same_view_and_size(self, other: Self) -> bool {
        self.width == other.width
            && self.height == other.height
            && std::mem::discriminant(&self.view) == std::mem::discriminant(&other.view)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderedView {
    /// Conversation history in rows `0..body_rows`, the input area below them.
    Conversation {
        body_rows: usize,
    },
    Overview,
    Transcript,
    DiffScreen,
}

#[derive(Clone, Copy)]
struct ExitConfirmation {
    control: char,
    requested_at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EscapeAction {
    ClearDraft,
    OpenRewind,
}

#[derive(Clone, Copy)]
struct EscapeConfirmation {
    action: EscapeAction,
    requested_at: Instant,
}

struct SubmittedPrompt {
    draft: String,
    paste_blocks: PasteBlocks,
    mentions: Vec<AcceptedMention>,
}

/// The editor of a Chat that is not shown, with the folded pastes and images of its draft.
struct ChatInput {
    editor: Editor,
    paste_blocks: PasteBlocks,
    restored_contexts: Vec<InputContext>,
    input_stash: Option<InputStash>,
    shell_input: ShellInputState,
    completion: Completion,
    file_mentions: FileMentionState,
    owner: Uuid,
}

struct InputStash {
    paste_blocks: PasteBlocks,
    restored_contexts: Vec<InputContext>,
    accepted_mentions: Vec<AcceptedMention>,
    shell_input: ShellInputState,
}

impl Default for ChatInput {
    fn default() -> Self {
        Self {
            editor: Editor::default(),
            paste_blocks: PasteBlocks::default(),
            restored_contexts: Vec::new(),
            input_stash: None,
            shell_input: ShellInputState::default(),
            completion: Completion::default(),
            file_mentions: FileMentionState::default(),
            owner: Uuid::new_v4(),
        }
    }
}

#[derive(Default)]
struct ViewPresentation {
    pub(super) tick_list: TickList,
    scrollback: Scrollback,
    echoed_inputs: HashSet<Uuid>,
    awaiting_input_echoes: HashSet<Uuid>,
    footer: FooterState,
    status_area: StatusArea,
    queued_inputs: Vec<QueuedInput>,
    history_search: Option<HistorySearch>,
    history_project: PathBuf,
    history_session: String,
    mention_roots: Option<(PathBuf, Vec<PathBuf>)>,
    help_open: bool,
    help_offset: usize,
    last_input_edit: Option<Instant>,
    footer_list_focused: bool,
    conversation_focused: bool,
    status: String,
    execution_connection_notice: Option<ExecutionConnectionNotice>,
    rendered_execution_losses: HashSet<Uuid>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ExecutionConnectionNotice {
    Lost,
    LostHistory,
    CachedExecution,
    CachedView,
}

impl ExecutionConnectionNotice {
    fn text(self) -> &'static str {
        match self {
            Self::Lost => "Lost · outcome unknown",
            Self::LostHistory => "Lost · cached history",
            Self::CachedExecution => "Disconnected · cached execution",
            Self::CachedView => "Disconnected · cached view",
        }
    }

    fn compact(self) -> &'static str {
        match self {
            Self::Lost => "Lost · outcome unknown",
            Self::LostHistory => "Lost · cached history",
            Self::CachedExecution => "Offline · cached",
            Self::CachedView => "Offline · cached view",
        }
    }
}

#[derive(Clone)]
pub(super) struct ExecutionLossNotice {
    pub(super) run_id: Uuid,
    pub(super) summary: String,
    pub(super) technical: String,
}

/// Shell 补全用的历史命令，由新到旧；按键过滤只读这里，不读历史文件。
struct ShellHistoryCache {
    commands: Vec<String>,
    loaded_at: Instant,
}

struct MenuCommentState {
    options: Vec<usize>,
    values: HashMap<usize, String>,
    label: &'static str,
    editing: bool,
    cursor: usize,
    secret: bool,
    standalone_input: bool,
}

struct MenuReviewState {
    review: Result<FileChangeReview, String>,
    navigation: DiffNavigation,
    viewport: DiffViewport,
}

struct DiffScreen {
    chat_id: Uuid,
    catalog: ChangeCatalog,
    dialog: DiffDialog,
}

struct BackToBottomTarget {
    row: usize,
    columns: std::ops::Range<usize>,
}

struct FooterBarTarget {
    row: usize,
    inset: usize,
    categories: Vec<(std::ops::Range<usize>, FooterItemId)>,
}

struct TickRowTarget {
    chat: Uuid,
    row: usize,
    columns: std::ops::Range<usize>,
    id: String,
}

fn status_notice_line(
    notice: &str,
    background: Option<&BackgroundStatusNotice>,
    pending: Option<&str>,
    width: usize,
) -> RoleLine {
    let mut line = RoleLine::role(notice, Role::StatusBar);
    if pending.is_none() {
        if let Some(background) = background {
            let available = width.saturating_sub(display_width(notice) + 3);
            if !notice.is_empty() {
                line.push(" · ", Some(Role::MutedText));
            }
            line.spans.extend(background.line(available).spans);
        }
        return line.filled(Role::StatusBar);
    }
    let urgent_background = background.filter(|background| background.necessary);
    if let Some(background) = urgent_background {
        let separator_columns = if notice.is_empty() { 0 } else { 3 };
        let available = width.saturating_sub(display_width(notice) + separator_columns);
        let pending_columns = pending.map_or(0, |pending| display_width(pending) + 3);
        let available_with_pending = available.saturating_sub(pending_columns);
        let message = if display_width(&background.message) <= available_with_pending {
            background.message.as_str()
        } else {
            background
                .compact_message
                .as_deref()
                .unwrap_or(&background.message)
        };
        let available = if display_width(message) <= available_with_pending {
            available_with_pending
        } else {
            available
        };
        if !notice.is_empty() {
            line.push(" · ", Some(Role::MutedText));
        }
        line.spans.extend(background.line(available).spans);
    }
    if let Some(pending) = pending {
        let separator = if line.text().is_empty() { "" } else { " · " };
        if display_width(&line.text()) + display_width(separator) + display_width(pending) <= width
        {
            line.push(separator, Some(Role::MutedText));
            line.push(pending, Some(Role::MutedText));
        }
    }
    if let Some(background) = background.filter(|background| !background.necessary) {
        let separator = if line.text().is_empty() { "" } else { " · " };
        let available =
            width.saturating_sub(display_width(&line.text()) + display_width(separator));
        if available > 0 {
            line.push(separator, Some(Role::MutedText));
            line.spans.extend(background.line(available).spans);
        }
    }
    line.filled(Role::StatusBar)
}

/// Keeps frames off the terminal until dropped. Writes made meanwhile reach
/// the terminal as one frame, so a burst of them cannot repaint the screen
/// once per write.
pub(super) struct FrameHold {
    state: Arc<Mutex<State>>,
}

impl Drop for FrameHold {
    fn drop(&mut self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.held_frames = state.held_frames.saturating_sub(1);
        if state.held_frames == 0 && std::mem::take(&mut state.frame_deferred) {
            let _ = state.draw(&mut io::stdout().lock());
        }
    }
}

impl Console {
    #[cfg(test)]
    pub(super) fn test_for_chat(chat: Uuid) -> Self {
        let mut state = State::new(false, 140, 25);
        state.chat = Some(chat);
        state.prompt_enabled = true;
        state.clipboard = Clipboard::Captured {
            copies: Arc::new(Mutex::new(Vec::new())),
            image: None,
            remote: true,
        };
        tests::console_with_state(state)
    }

    #[cfg(test)]
    pub(super) fn test_interactive_for_chat(chat: Uuid) -> Self {
        let console = Self::test_for_chat(chat);
        console.state.lock().unwrap().interactive = true;
        console
    }

    #[cfg(test)]
    pub(super) fn test_status_text(&self) -> String {
        self.state.lock().unwrap().status.clone()
    }

    #[cfg(test)]
    pub(super) fn test_clipboard_contents(&self) -> Vec<String> {
        let state = self.state.lock().unwrap();
        let Clipboard::Captured { copies, .. } = &state.clipboard else {
            panic!("test clipboard is not isolated");
        };
        copies.lock().unwrap().clone()
    }

    pub(super) fn artifact_metadata_sink(&self) -> ArtifactMetadataSink {
        ArtifactMetadataSink {
            state: Arc::downgrade(&self.state),
        }
    }

    pub(super) fn show_artifacts_picker(
        &self,
        picker: super::artifacts_picker::ArtifactsPicker,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.install_artifact_picker(picker);
        state.rendered_region = None;
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }
    pub(super) fn artifacts_picker_is_open(&self) -> Result<bool, String> {
        Ok(self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .artifacts_picker
            .is_some())
    }
    pub(super) fn take_artifact_load(
        &self,
    ) -> Result<Option<super::artifacts_picker::ArtifactLoad>, String> {
        Ok(self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .artifacts_picker
            .as_mut()
            .and_then(|picker| picker.take_load()))
    }
    pub(super) fn artifact_page_loaded(
        &self,
        request: &super::artifacts_picker::ArtifactLoad,
        result: Result<(crate::artifacts::ArtifactPage, HashSet<String>, bool), String>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if let Some(picker) = state.artifact_picker_for_load(request.picker) {
            picker.loaded(request, result);
        }
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }
    pub(super) fn artifact_picker_matches(&self, id: Uuid) -> Result<bool, String> {
        let state = self.state.lock().map_err(|error| error.to_string())?;
        Ok(state
            .artifacts_picker
            .as_ref()
            .is_some_and(|picker| picker.id() == id && picker.chat == state.chat))
    }
    pub(super) fn artifact_picker_error(&self, error: String) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if let Some(picker) = &mut state.artifacts_picker {
            picker.report(error);
        }
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }
    pub(super) fn refresh_artifacts_picker(&self) -> Result<(), String> {
        if let Some(picker) = &mut self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .artifacts_picker
        {
            picker.refresh();
        }
        Ok(())
    }
    pub(super) fn select_artifact_tick(
        &self,
        picker: Uuid,
        chat: Uuid,
        id: &str,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.clear_artifact_picker(picker);
        let overlay = state.width + 1 < TICK_LIST_AUTO_MIN_COLUMNS
            || state.tick_list_preference == tickloop_agent::settings::TickListPreference::Hidden;
        let list = state.tick_list_for_chat_mut(chat);
        list.focus(overlay, Some(TickGroup::Artifacts));
        list.select(id);
        state.rendered_region = None;
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(super) fn update_artifact_ticks(
        &self,
        snapshot: &tickloop_agent::runtime::ChatSnapshot,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let list = if state.chat == Some(snapshot.id) {
            &mut state.tick_list
        } else {
            &mut state
                .view_cache
                .entry(Some(snapshot.id))
                .or_default()
                .tick_list
        };
        list.chat_snapshot = Some(snapshot.clone());
        list.rebuild();
        Ok(())
    }
    pub(super) fn update_artifact_metadata(
        &self,
        chat: Uuid,
        id: String,
        metadata: Option<crate::artifacts::ArtifactMetadata>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let list = if state.chat == Some(chat) {
            &mut state.tick_list
        } else {
            &mut state.view_cache.entry(Some(chat)).or_default().tick_list
        };
        list.metadata_failures.remove(&id);
        list.metadata.insert(id, metadata);
        list.rebuild();
        Ok(())
    }
    pub(super) fn copy_artifact_link(&self, url: &str) -> Result<(), String> {
        self.state
            .lock()
            .map_err(|error| error.to_string())?
            .clipboard
            .write(url, &mut io::stdout().lock())
    }
    pub(super) fn confirm_tick_action(
        &self,
        chat: Uuid,
        instance: String,
        action: Arc<dyn crate::tick_adapter::TickActionExecutor>,
        message: &str,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if state.chat != Some(chat) {
            return Ok(());
        }
        state.tick_list.confirmation = Some(super::tick_list::TickConfirmation {
            instance,
            action,
            prompt: message.into(),
        });
        state.status = message.into();
        Ok(())
    }
    pub(super) fn update_subscribed_ticks(
        &self,
        chat: Uuid,
        rows: &[TickSubscriptionStatus],
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let list = if state.chat == Some(chat) {
            &mut state.tick_list
        } else {
            &mut state.view_cache.entry(Some(chat)).or_default().tick_list
        };
        list.subscriptions = rows.to_vec();
        list.rebuild();
        Ok(())
    }
    pub(super) fn show_subscriptions_at(
        &self,
        chat: Uuid,
        rows: Vec<TickSubscriptionStatus>,
        selected: Uuid,
        cancel: bool,
    ) -> Result<(), String> {
        self.show_subscriptions(chat, rows)?;
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if let Some(surface) = &mut state.subscription_surface {
            surface.select(selected);
            if cancel {
                surface.apply(SubscriptionAction::RequestCancel);
            }
        }
        state.tick_list.focused = false;
        state.tick_list.overlay = false;
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(super) fn tick_list_has_entries(&self) -> Result<bool, String> {
        Ok(!self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .tick_list
            .entries()
            .is_empty())
    }
    pub(super) fn tick_list_is_visible(&self) -> Result<bool, String> {
        Ok(self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .tick_list_visible())
    }
    pub(super) fn set_tick_list_preference(
        &self,
        preference: tickloop_agent::settings::TickListPreference,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.tick_list_preference = preference;
        state.tick_list.overlay = false;
        state.tick_list.focused = false;
        state.rendered_region = None;
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }
    pub(super) fn open_tick_list(&self) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.tick_list.overlay =
            !state.tick_list.entries().is_empty() && state.width + 1 < TICK_LIST_AUTO_MIN_COLUMNS;
        state.rendered_region = None;
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }
    pub(super) fn update_shell_ticks(
        &self,
        snapshot: &tickloop_agent::runtime::ShellTaskSnapshot,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let list = state.tick_list_for_chat_mut(snapshot.chat_id);
        list.shell_snapshot = Some(snapshot.clone());
        list.rebuild();
        Ok(())
    }

    pub(super) fn poll_tick_detail(
        &self,
    ) -> Result<Option<(Uuid, super::tick_actions::PreparedTickAction)>, String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if !state.tick_list_visible() {
            return Ok(None);
        }
        let Some(detail) = state.tick_list.detail.as_mut() else {
            return Ok(None);
        };
        let chat = detail.chat();
        Ok(detail
            .poll(crate::private_fs::unix_time_milliseconds())
            .map(|action| (chat, action)))
    }
    pub(super) fn update_tick_detail<T: super::tick_actions::TickDetailView, R>(
        &self,
        chat: Uuid,
        panel: Uuid,
        update: impl FnOnce(&mut T) -> Option<R>,
    ) -> Result<Option<R>, String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let list = state.tick_list_for_chat_mut(chat);
        let Some(detail) = list
            .detail
            .as_mut()
            .filter(|detail| detail.id() == panel)
            .and_then(|detail| detail.as_any_mut().downcast_mut::<T>())
        else {
            return Ok(None);
        };
        let result = update(detail);
        if result.is_none() {
            list.detail = None;
        }
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())?;
        Ok(result)
    }
    pub(super) fn set_background_status_notice(
        &self,
        notice: Option<BackgroundStatusNotice>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.background_status_notice = notice.map(|mut notice| {
            notice.title = notice.title.map(|title| display_single_line(&title));
            notice.message = display_single_line(&notice.message);
            notice.compact_message = notice
                .compact_message
                .map(|message| display_single_line(&message));
            notice
        });
        Ok(())
    }

    pub(super) fn background_status_notice(
        &self,
        agent: Option<(Uuid, String)>,
        message: String,
        compact_message: Option<String>,
        necessary: bool,
        message_role: Role,
    ) -> Result<BackgroundStatusNotice, String> {
        let state = self.state.lock().map_err(|error| error.to_string())?;
        let ordinal = agent.as_ref().and_then(|(agent_id, _)| {
            state
                .status_area
                .ordinal(*agent_id)
                .or_else(|| state.overview.numbers.shared(*agent_id))
        });
        Ok(BackgroundStatusNotice {
            title: agent.map(|(_, title)| title),
            ordinal,
            message,
            message_role,
            compact_message,
            necessary,
        })
    }

    pub(super) fn set_status_lines(&self, lines: Option<Vec<String>>) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.configured_status_lines = lines;
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(super) fn set_connection_notice(&self, notice: Option<String>) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let notice = notice.map(|notice| display_single_line(&notice));
        if state.connection_notice == notice && state.execution_connection_notice.is_none() {
            return Ok(());
        }
        state.connection_notice = notice;
        state.execution_connection_notice = None;
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(super) fn set_execution_connection_notice(
        &self,
        notice: Option<ExecutionConnectionNotice>,
        cause: Option<String>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if notice.is_some()
            && let Some(chat) = state.chat
            && let Some(cause) = cause
        {
            state
                .last_connection_diagnostics
                .insert(chat, display_text(&cause));
        }
        if state.execution_connection_notice == notice && state.connection_notice.is_none() {
            return Ok(());
        }
        state.execution_connection_notice = notice;
        state.connection_notice = None;
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(super) fn record_execution_loss(
        &self,
        chat: Uuid,
        notice: ExecutionLossNotice,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let losses = state.execution_losses.entry(chat).or_default();
        if losses.iter().all(|loss| loss.run_id != notice.run_id) {
            losses.push(notice.clone());
        }
        if state.chat == Some(chat) && state.overview.projection.is_some() {
            if let Some(source) = state.overview_source_view.as_mut()
                && source.rendered_execution_losses.insert(notice.run_id)
            {
                source
                    .scrollback
                    .append_as(&notice.summary, Some(Role::Warning), None);
            }
        } else if state.chat == Some(chat) {
            if state.rendered_execution_losses.insert(notice.run_id) {
                state
                    .scrollback
                    .append_as(&notice.summary, Some(Role::Warning), None);
            }
            state
                .draw(&mut io::stdout().lock())
                .map_err(|error| error.to_string())?;
        } else if let Some(cached) = state.view_cache.get_mut(&Some(chat))
            && cached.rendered_execution_losses.insert(notice.run_id)
        {
            cached
                .scrollback
                .append_as(&notice.summary, Some(Role::Warning), None);
        }
        Ok(())
    }

    pub(super) fn replay_execution_losses(&self, chat: Uuid) -> Result<(), String> {
        let notices = self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .execution_losses
            .get(&chat)
            .cloned()
            .unwrap_or_default();
        for notice in notices {
            self.record_execution_loss(chat, notice)?;
        }
        Ok(())
    }
    pub(super) fn set_creation_notice(&self, notice: Option<String>) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let notice = notice.map(|notice| display_single_line(&notice));
        if state.creation_notice == notice {
            return Ok(());
        }
        state.creation_notice = notice;
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(super) fn set_global_overview(
        &self,
        projection: Option<AgentOverview>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let was_open = state.overview.projection.is_some();
        if !was_open && projection.is_some() {
            state.clear_prompt_suggestion();
            state.overview.current_draft = state.chat.is_none();
            let dispatch = std::mem::take(&mut state.overview_input);
            let source = state.replace_input(dispatch);
            state.overview_source_input = Some(source);
            let mut dispatch_view = ViewPresentation::default();
            if let Some(projection) = &projection {
                dispatch_view.history_project = projection.dispatch_directory.clone();
                dispatch_view.mention_roots =
                    Some((projection.dispatch_directory.clone(), Vec::new()));
            }
            dispatch_view.history_session = "overview-dispatch".into();
            state.overview_source_view = Some(state.replace_view(dispatch_view));
        } else if was_open
            && projection.is_none()
            && let Some(source) = state.overview_source_input.take()
        {
            state.overview_input = state.replace_input(source);
            if let Some(source) = state.overview_source_view.take() {
                state.replace_view(source);
            }
        }
        if !was_open {
            let heading_rows = if state.height >= agent_layout::TALL_LAYOUT_MIN_ROWS {
                2
            } else {
                1
            };
            state.overview.capacity = state
                .height
                .saturating_sub(agent_layout::bottom_capacity(state.height) + heading_rows);
        }
        state.overview.update(projection);
        if state.overview.projection.is_none() {
            state.overview.current_draft = false;
        }
        let index_directory = state
            .overview
            .projection
            .as_ref()
            .filter(|projection| {
                !was_open || state.history_project != projection.dispatch_directory
            })
            .map(|projection| projection.dispatch_directory.clone());
        if let Some(directory) = &index_directory {
            state.history_project = directory.clone();
            state.mention_roots = Some((directory.clone(), Vec::new()));
        }
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())?;
        drop(state);
        if let Some(directory) = index_directory {
            self.refresh_file_index(directory, Vec::new())?;
        }
        Ok(())
    }

    pub(super) fn command_scope(&self) -> CommandScope {
        self.state
            .lock()
            .map_or(CommandScope::Root, |state| state.command_scope())
    }

    /// Records the visible Agent's delegation from its snapshot, so command scope never depends
    /// on whether its parent is currently listed.
    pub(super) fn set_visible_delegation(&self, delegated: bool) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.visible_delegated = delegated;
        Ok(())
    }

    /// Names an Agent outside the shown conversation as its rows do: its title, plus this View's
    /// reading number when another Agent the View has listed reads alike.
    pub(super) fn agent_name(&self, agent_id: Uuid, title: &str) -> Result<String, String> {
        let state = self.state.lock().map_err(|error| error.to_string())?;
        let title = state
            .status_area
            .agent_title(agent_id)
            .or_else(|| {
                state
                    .overview
                    .projection
                    .as_ref()?
                    .rows
                    .iter()
                    .find(|row| row.agent_id == agent_id)
                    .map(|row| row.title.as_str())
            })
            .unwrap_or(title);
        let number = state
            .status_area
            .ordinal(agent_id)
            .or_else(|| state.overview.numbers.shared(agent_id));
        Ok(match number {
            Some(number) => format!("Agent {title} ({number})"),
            None => format!("Agent {title}"),
        })
    }

    /// The visible Agent's family; `visible` names the shown member, which also decides whether
    /// commands act on a child.
    pub(super) fn set_agent_family(
        &self,
        rows: Vec<AgentTreeRow>,
        visible: Option<Uuid>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let ordinals = state
            .overview
            .numbers
            .sharing(&rows, MIN_REQUEST_AGENT_TITLE_COLUMNS);
        state.status_area.update_agents(rows, visible, ordinals);
        let panels = state.status_area.available_panels().collect::<Vec<_>>();
        state.footer.update(panels);
        state.clear_footer_selection_if_hidden();
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(super) fn agent_request_is_open(&self) -> Result<bool, String> {
        Ok(self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .request_layout
            .is_some())
    }

    pub(super) fn dialog_chat(&self) -> Result<Option<Uuid>, String> {
        Ok(self.state.lock().map_err(|error| error.to_string())?.chat)
    }

    pub(super) fn dialog_menu_id(&self) -> Result<Option<Uuid>, String> {
        let state = self.state.lock().map_err(|error| error.to_string())?;
        Ok(state.menu.as_ref().map(|menu| menu.id).or_else(|| {
            state
                .conversation_picker
                .as_ref()
                .map(ConversationPicker::id)
        }))
    }

    pub(super) fn set_request_queue_count(&self, count: usize) -> Result<(), String> {
        self.state
            .lock()
            .map_err(|error| error.to_string())?
            .request_queue_count = count;
        Ok(())
    }

    pub(super) fn can_present_request(&self) -> Result<bool, String> {
        let state = self.state.lock().map_err(|error| error.to_string())?;
        Ok(state.overview.projection.is_none()
            && state.workspace_trust.is_none()
            && state.request_layout.is_none()
            && state.transcript.is_none()
            && state.diff_screen.is_none()
            && !state.menu_submission_is_pending()
            && (state.input_dialog_visible()
                || state.artifacts_picker.is_some()
                || state.conversation_picker.is_some()
                || state.task_surface.is_some()
                || state.subscription_surface.is_some()
                || state.permissions_surface.is_some()
                || state
                    .last_input_edit
                    .is_none_or(|edit| edit.elapsed() >= agent_layout::REQUEST_IDLE_DELAY)))
    }

    pub(super) fn set_request_layout(
        &self,
        request: Option<Uuid>,
    ) -> Result<RequestPresentation, String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if let (Some(current), Some(next)) = (state.request_layout, request)
            && current != next
            && state.menu.is_some()
        {
            return Err(
                "Finish or close the displayed request before opening another request.".into(),
            );
        }
        if request.is_some() && state.request_layout.is_none() {
            if state.menu_submission_is_pending()
                && state.menu_submission_finished != state.menu_submission_pending
                || state.branch_submission_owns_input()
            {
                return Ok(RequestPresentation::Deferred);
            }
            let count = state.request_queue_count;
            state.suspend_dialog();
            state.request_queue_count = count;
        }
        state.request_layout = request;
        if request.is_none() {
            state.request_menu = None;
        }
        Ok(RequestPresentation::Presented)
    }

    pub(super) fn hold_frames(&self) -> Result<FrameHold, String> {
        self.state
            .lock()
            .map_err(|error| error.to_string())?
            .held_frames += 1;
        Ok(FrameHold {
            state: Arc::clone(&self.state),
        })
    }

    pub fn for_message(&self, id: Option<String>) -> Self {
        let mut console = self.clone();
        console.message_id = id;
        console
    }

    pub fn auto_scroll(&self) -> Result<bool, String> {
        Ok(self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .scrollback
            .auto_scroll())
    }

    pub fn set_auto_scroll(&self, enabled: bool) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.scrollback.set_auto_scroll(enabled);
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(super) fn scroll_conversation_to_end(&self) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.scrollback.scroll(ScrollAction::Last);
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(super) fn theme(&self) -> Result<ThemeName, String> {
        Ok(self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .display
            .theme()
            .name())
    }

    /// Switches the palette for this terminal session only. The drawing surface
    /// is unchanged, so only the cached region is dropped: every row is written
    /// again in the new colours without scrolling a fresh screen into view.
    pub(super) fn set_theme(&self, name: ThemeName) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.display.set_theme(name);
        state.rendered_region = None;
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(super) fn display_preferences(&self) -> Result<DisplayPreferences, String> {
        Ok(self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .display
            .preferences())
    }

    pub(super) fn conversation_width(&self) -> Result<usize, String> {
        Ok(self.state.lock().map_err(|error| error.to_string())?.width)
    }

    pub(super) fn todo_refresh_pending(&self) -> Result<bool, String> {
        Ok(self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .status_area
            .todo_refresh_pending())
    }

    pub(super) fn set_display_preferences(
        &self,
        preferences: DisplayPreferences,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let previous = state.display;
        state.display.set_preferences(preferences);
        let next = state.display;
        state.scrollback.remap_display(previous, next);
        state.rendered_region = None;
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(super) fn display_profile(&self) -> Result<DisplayProfile, String> {
        Ok(self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .display)
    }

    pub fn set_footer_items(&self, items: Vec<FooterItemId>) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.footer.update(items);
        state.clear_footer_selection_if_hidden();
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(super) fn update_status_area(
        &self,
        mode: String,
        model: String,
        context: Option<(u64, usize)>,
        todos: Vec<TodoItem>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.status_area.update(mode, model, context, todos);
        let panels = state.status_area.available_panels().collect::<Vec<_>>();
        state.footer.update(panels);
        state.clear_footer_selection_if_hidden();
        Ok(())
    }

    /// `root` 为 `None` 时历史只保存在内存中。
    pub fn configure_input_history(
        &self,
        root: Option<&Path>,
        project: &Path,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let history = root.map_or_else(InputHistory::in_memory, InputHistory::open);
        state.input_history = Some(history);
        state.history_project = project.to_path_buf();
        state.shell_history_cache = None;
        Ok(())
    }

    /// Pasted images are cached under `root` for their `[Image #N]` links. `None`, for a
    /// session whose Chats are not saved, caches nothing: the cache expires with Transcripts.
    pub fn configure_image_cache(&self, root: Option<&Path>) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.image_cache = root.map(ImageCache::in_root);
        Ok(())
    }

    pub(super) fn image_links(&self) -> Result<ImageLinks, String> {
        Ok(self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .image_links())
    }

    pub fn set_input_context(
        &self,
        chat: Option<Uuid>,
        project: &Path,
        additional_directories: &[PathBuf],
    ) -> Result<(), String> {
        {
            let mut state = self.state.lock().map_err(|error| error.to_string())?;
            if state.overview.projection.is_some() {
                return Ok(());
            }
            let chat = chat.map(|chat| chat.to_string());
            if state.history_project != project
                || chat
                    .as_ref()
                    .is_some_and(|chat| *chat != state.history_session)
            {
                state.shell_history_cache = None;
            }
            state.history_project = project.to_path_buf();
            if let Some(chat) = chat {
                state.history_session = chat;
            }
            let roots = (project.to_path_buf(), additional_directories.to_vec());
            if state.mention_roots.as_ref() == Some(&roots) {
                return Ok(());
            }
            state.mention_roots = Some(roots);
        }
        self.refresh_file_index(project.to_path_buf(), additional_directories.to_vec())
    }

    pub fn record_prompt_history(&self, prompt: &str) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let session = state.history_session.clone();
        let project = state.history_project.clone();
        let pasted_texts = match state.submitted_pasted_texts.take() {
            Some((submitted, pasted_texts)) if submitted == prompt => pasted_texts,
            _ => Vec::new(),
        };
        if let Some(history) = &mut state.input_history {
            history
                .record(&session, &project, prompt, &pasted_texts)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn refresh_file_index(
        &self,
        project: PathBuf,
        additional_directories: Vec<PathBuf>,
    ) -> Result<(), String> {
        let (generation, owner) = {
            let mut state = self.state.lock().map_err(|error| error.to_string())?;
            (
                state.file_mentions.request_index_generation(),
                state.input_owner,
            )
        };
        let state = Arc::clone(&self.state);
        let stop = Arc::clone(&self._lifetime.stop);
        thread::Builder::new()
            .name("agent-file-index".into())
            .spawn(move || {
                let Ok(index) = FileMentionIndex::build(&project, &additional_directories) else {
                    return;
                };
                if stop.load(Ordering::Acquire) {
                    return;
                }
                let Ok(mut state) = state.lock() else {
                    return;
                };
                if state.replace_file_index(owner, generation, index) {
                    let draft = state.editor.draft().to_owned();
                    let cursor = state.editor.cursor();
                    state.file_mentions.update_at(&draft, cursor, 15);
                    let _ = state.draw(&mut io::stdout().lock());
                }
            })
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub fn select_chat(&self, chat: Option<Uuid>) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.switch_view_chat(chat);
        Ok(())
    }

    pub(super) fn select_created_chat(
        &self,
        chat: Uuid,
        startup_banner: &[(String, Option<Role>)],
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let is_new_view = state.chat != Some(chat) && !state.view_cache.contains_key(&Some(chat));
        let draft_history = (is_new_view && state.captures_creation_prefix())
            .then(|| std::mem::take(&mut state.scrollback));
        if let Some(history) = &draft_history {
            let prefix = state.creation_prefixes.remove(&None).unwrap_or_default();
            state.creation_prefixes.insert(Some(chat), prefix);
            state.scrollback.set_auto_scroll(history.auto_scroll());
            for (text, role) in startup_banner {
                state.scrollback.append_as(text, *role, None);
            }
            state.creation_prefixes.insert(
                None,
                startup_banner
                    .iter()
                    .map(|(text, role)| {
                        CreationPrefixEntry::Output(ConversationPiece::Styled(text.clone(), *role))
                    })
                    .collect(),
            );
        }
        state.switch_view_chat(Some(chat));
        if let Some(history) = draft_history {
            state.scrollback = history;
        } else if is_new_view && state.interactive {
            for (text, role) in startup_banner {
                state.scrollback.append_as(text, *role, None);
            }
            state.creation_prefixes.insert(
                Some(chat),
                startup_banner
                    .iter()
                    .map(|(text, role)| {
                        CreationPrefixEntry::Output(ConversationPiece::Styled(text.clone(), *role))
                    })
                    .collect(),
            );
        }
        Ok(())
    }

    pub(super) fn activate_agent(&self, chat: Uuid) -> Result<bool, String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let restored = state.chat == Some(chat) || state.view_cache.contains_key(&Some(chat));
        state.switch_view_chat(Some(chat));
        state.clear_footer_selection();
        state.conversation_focused = false;
        Ok(restored)
    }

    pub(super) fn clear_visible_agent(&self) -> Result<(), String> {
        self.state
            .lock()
            .map_err(|error| error.to_string())?
            .switch_view_chat(None);
        Ok(())
    }

    pub fn reset_chat(&self, chat: Uuid) -> Result<(), String> {
        #[cfg(test)]
        if let Some(error) = self
            .reset_chat_failure
            .lock()
            .map_err(|error| error.to_string())?
            .take()
        {
            return Err(error);
        }
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.reset_chat(chat);
        Ok(())
    }

    pub(super) fn commit_branch_and_reset_chat(&self, chat: Uuid) -> Result<(), String> {
        #[cfg(test)]
        if let Some(error) = self
            .reset_chat_failure
            .lock()
            .map_err(|error| error.to_string())?
            .take()
        {
            return Err(error);
        }
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.commit_branch_submission();
        state.reset_chat(chat);
        Ok(())
    }

    pub(super) fn cancel_branch_submission(
        &self,
        preserve_until_summary_finishes: bool,
    ) -> Result<(), String> {
        self.state
            .lock()
            .map_err(|error| error.to_string())?
            .cancel_branch_submission(preserve_until_summary_finishes);
        Ok(())
    }

    pub(super) fn commit_branch_submission(&self) -> Result<(), String> {
        #[cfg(test)]
        if let Some(error) = self
            .branch_commit_failure
            .lock()
            .map_err(|error| error.to_string())?
            .take()
        {
            return Err(error);
        }
        self.state
            .lock()
            .map_err(|error| error.to_string())?
            .commit_branch_submission();
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn fail_next_branch_commit(&self, error: impl Into<String>) -> Result<(), String> {
        *self
            .branch_commit_failure
            .lock()
            .map_err(|error| error.to_string())? = Some(error.into());
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn fail_next_reset_chat(&self, error: impl Into<String>) -> Result<(), String> {
        *self
            .reset_chat_failure
            .lock()
            .map_err(|error| error.to_string())? = Some(error.into());
        Ok(())
    }

    pub fn is_interactive(&self) -> bool {
        self.state
            .lock()
            .map_or(self._lifetime.interactive, |state| state.interactive)
    }

    pub fn transcript_id(&self) -> Result<Option<Uuid>, String> {
        Ok(self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .transcript
            .as_ref()
            .map(|(id, _)| *id))
    }

    pub fn start_transcript(&self) -> Result<Uuid, String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if !state.interactive {
            return Err("the transcript view requires a terminal".into());
        }
        state.hide_task_surface();
        state.hide_subscription_view();
        state.permissions_surface = None;
        let id = state.start_transcript();
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())?;
        Ok(id)
    }

    pub fn close_transcript(&self) -> Result<bool, String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let closed = state.transcript.take().is_some();
        if closed {
            state.rendered_region = None;
            state
                .draw(&mut io::stdout().lock())
                .map_err(|error| error.to_string())?;
        }
        Ok(closed)
    }

    pub fn update_transcript(
        &self,
        id: Uuid,
        mut document: TranscriptDocument,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if let Some(chat) = state.chat
            && let Some(losses) = state.execution_losses.get(&chat)
        {
            for loss in losses {
                document.lines.push(String::new());
                document.lines.push("Execution connection details".into());
                document
                    .lines
                    .extend(loss.technical.lines().map(display_text));
            }
        }
        if let Some(chat) = state.chat
            && let Some(cause) = state.last_connection_diagnostics.get(&chat)
        {
            document.lines.push(String::new());
            document
                .lines
                .push("Last connection diagnostic (this View)".into());
            document.lines.push(format!("Technical cause: {cause}"));
            document.lines.push(format!(
                "/resume {chat} loads saved history; continuing requires an explicit new input."
            ));
        }
        if let Some((current, transcript)) = &mut state.transcript
            && id == *current
        {
            transcript.update(document);
            state
                .draw(&mut io::stdout().lock())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub fn show_diff(&self, chat_id: Uuid, catalog: ChangeCatalog) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if !state.interactive {
            return Err("the diff viewer requires a terminal".into());
        }
        if state.chat != Some(chat_id) {
            return Ok(());
        }
        state.open_diff_screen(chat_id, catalog);
        state.hide_task_surface();
        state.hide_subscription_view();
        state.permissions_surface = None;
        state.clear_footer_selection();
        state.shell_background_chord_started_at = None;
        state.rendered_region = None;
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub fn show_tasks_at(
        &self,
        snapshot: TaskSurfaceSnapshot,
        selected_task: Option<Uuid>,
    ) -> Result<TaskSurfaceEffect, String> {
        self.show_task_surface(snapshot, selected_task)
    }

    fn show_task_surface(
        &self,
        snapshot: TaskSurfaceSnapshot,
        selected_task: Option<Uuid>,
    ) -> Result<TaskSurfaceEffect, String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if !state.interactive {
            return Err("the Shell task center requires a terminal".into());
        }
        let chat_id = snapshot.chat_id;
        if state.chat != Some(chat_id) {
            return Ok(TaskSurfaceEffect::None);
        }
        let (mut surface, mut effect) =
            if let Some(mut surface) = state.task_surface_cache.remove(&chat_id) {
                surface.clear_action_error();
                let effect = surface.reopen(snapshot);
                (surface, effect)
            } else {
                let mut surface = TaskSurface::open(snapshot, chat_id);
                let effect = surface.initial_effect();
                (surface, effect)
            };
        if let Some(task_id) = selected_task {
            let viewport = task_surface_content_viewport(state.width, state.height);
            effect = surface.open_detail(task_id, viewport);
        }
        if state.request_layout.is_some() {
            let mut dialog = input_dialog::DialogState::default();
            dialog.task_surface = Some(surface);
            state.install_user_dialog(Some(chat_id), dialog);
            return Ok(TaskSurfaceEffect::None);
        }
        state.task_surface = Some(surface);

        state.hide_subscription_view();
        state.permissions_surface = None;
        state.clear_footer_selection();
        state.transcript = None;
        state.hide_diff_screen();
        state.menu_focused = false;
        state.review_focused = false;
        state.shell_background_chord_started_at = None;
        state.rendered_region = None;
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())?;
        Ok(effect)
    }

    pub fn show_subscriptions(
        &self,
        chat_id: Uuid,
        rows: Vec<TickSubscriptionStatus>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if !state.interactive {
            return Err("the Tick Subscription view requires a terminal".into());
        }
        if state.chat != Some(chat_id) {
            return Ok(());
        }
        if state.request_layout.is_some() {
            let mut dialog = input_dialog::DialogState::default();
            dialog.subscription_surface = Some(SubscriptionSurface::open(chat_id, rows));
            state.install_user_dialog(Some(chat_id), dialog);
            return Ok(());
        }
        state.subscription_surface = Some(SubscriptionSurface::open(chat_id, rows));
        state.permissions_surface = None;
        state.hide_task_surface();
        state.clear_footer_selection();
        state.transcript = None;
        state.hide_diff_screen();
        state.menu_focused = false;
        state.review_focused = false;
        state.shell_background_chord_started_at = None;
        state.rendered_region = None;
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(super) fn show_permissions(
        &self,
        view: impl Into<PermissionsPage>,
        history: Result<tickloop_agent::runtime::ApprovalHistoryView, String>,
        return_to_settings: bool,
        auto_page: bool,
    ) -> Result<(), String> {
        self.show_permissions_at(
            view,
            Some(history),
            PermissionsReturn::ModePicker {
                settings: return_to_settings,
            },
            auto_page,
        )
    }

    fn show_permissions_at(
        &self,
        view: impl Into<PermissionsPage>,
        history: Option<Result<tickloop_agent::runtime::ApprovalHistoryView, String>>,
        destination: PermissionsReturn,
        auto_page: bool,
    ) -> Result<(), String> {
        let page = view.into();
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if !state.interactive {
            return Err("the Permissions view requires a terminal".into());
        }
        // A New Agent view belongs to the New Agent screen, which shows no Agent.
        if state.chat != page.subject.chat() {
            return Ok(());
        }
        let mut surface = PermissionsSurface::open(page, destination);
        if state.visible_delegated {
            surface.make_read_only(CHILD_AUTHORIZATION_FIXED);
        }
        if let Some(history) = history {
            match history {
                Ok(history) => surface.replace_denials(history),
                Err(error) => surface.denials_error(error),
            }
        }
        if auto_page {
            surface.open_auto();
        }
        if state.request_layout.is_some() {
            let mut dialog = input_dialog::DialogState::default();
            dialog.permissions_surface = Some(surface);
            let chat = state.chat;
            state.install_user_dialog(chat, dialog);
            return Ok(());
        }
        state.permissions_surface = Some(surface);
        state.hide_task_surface();
        state.hide_subscription_view();
        state.transcript = None;
        state.hide_diff_screen();
        state.clear_footer_selection();
        state.menu_focused = false;
        state.review_focused = false;
        state.rendered_region = None;
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(super) fn permissions_view_chat(&self) -> Result<Option<Uuid>, String> {
        Ok(self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .permissions_surface
            .as_ref()
            .and_then(PermissionsSurface::chat_id))
    }

    /// The Agent or New Agent screen the open Permissions view belongs to, at the revision read.
    pub(super) fn permissions_subject(&self) -> Result<Option<PermissionsSubject>, String> {
        Ok(self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .permissions_surface
            .as_ref()
            .map(|surface| surface.subject().clone()))
    }

    pub(super) fn permissions_saved(&self, view: impl Into<PermissionsPage>) -> Result<(), String> {
        let view = view.into();
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let surface = state
            .permissions_surfaces_mut()
            .find(|surface| surface.shows(&view));
        if let Some(surface) = surface {
            surface.replace_view(view);
            state
                .draw(&mut io::stdout().lock())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(super) fn permissions_refreshed(
        &self,
        view: impl Into<PermissionsPage>,
    ) -> Result<(), String> {
        let view = view.into();
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let surface = state
            .permissions_surfaces_mut()
            .find(|surface| surface.shows(&view));
        if let Some(surface) = surface {
            surface.refresh_view(view);
            state
                .draw(&mut io::stdout().lock())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(super) fn permissions_key_and_reviewer_saved(
        &self,
        view: impl Into<PermissionsPage>,
    ) -> Result<(), String> {
        let view = view.into();
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let surface = state
            .permissions_surfaces_mut()
            .find(|surface| surface.shows(&view));
        if let Some(surface) = surface {
            surface.key_and_reviewer_saved(view);
            state
                .draw(&mut io::stdout().lock())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(super) fn permissions_key_saved_choice_failed(
        &self,
        view: impl Into<PermissionsPage>,
        reason: String,
    ) -> Result<(), String> {
        let view = view.into();
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let surface = state
            .permissions_surfaces_mut()
            .find(|surface| surface.shows(&view));
        if let Some(surface) = surface {
            surface.key_saved_choice_failed(view, reason);
            state
                .draw(&mut io::stdout().lock())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(super) fn permissions_conflict(
        &self,
        view: impl Into<PermissionsPage>,
    ) -> Result<(), String> {
        let view = view.into();
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let surface = state
            .permissions_surfaces_mut()
            .find(|surface| surface.shows(&view));
        if let Some(surface) = surface {
            surface.conflict(view);
            state
                .draw(&mut io::stdout().lock())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(super) fn permissions_denials(
        &self,
        chat: Uuid,
        view: Result<tickloop_agent::runtime::ApprovalHistoryView, String>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let surface = state
            .permissions_surfaces_mut()
            .find(|surface| surface.chat_id() == Some(chat));
        if let Some(surface) = surface {
            match view {
                Ok(view) => surface.replace_denials(view),
                Err(error) => surface.denials_error(error),
            }
            state
                .draw(&mut io::stdout().lock())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(super) fn close_permissions(&self) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.permissions_surface = None;
        state.rendered_region = None;
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(super) fn permissions_notice(
        &self,
        chat: Option<Uuid>,
        notice: String,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let surface = state
            .permissions_surfaces_mut()
            .find(|surface| surface.chat_id() == chat);
        if let Some(surface) = surface {
            surface.notice(notice);
            state
                .draw(&mut io::stdout().lock())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(super) fn permissions_error(
        &self,
        chat: Option<Uuid>,
        message: String,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let surface = state
            .permissions_surfaces_mut()
            .find(|surface| surface.chat_id() == chat);
        if let Some(surface) = surface {
            surface.error(message);
            state
                .draw(&mut io::stdout().lock())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub fn subscription_view_chat(&self) -> Result<Option<Uuid>, String> {
        Ok(self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .subscription_surface
            .as_ref()
            .map(SubscriptionSurface::chat_id))
    }

    pub(super) fn subscription_dialog_chats(&self) -> Result<Vec<Uuid>, String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        Ok(state
            .subscription_surfaces_mut()
            .map(|surface| surface.chat_id())
            .collect())
    }

    pub fn update_subscriptions(
        &self,
        chat_id: Uuid,
        rows: Vec<TickSubscriptionStatus>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let Some(surface) = state
            .subscription_surfaces_mut()
            .find(|surface| surface.chat_id() == chat_id)
        else {
            return Ok(());
        };
        if !surface.update(rows) {
            return Ok(());
        }
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub fn subscription_change_failed(&self, chat: Uuid, message: String) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let Some(surface) = state
            .subscription_surfaces_mut()
            .find(|surface| surface.chat_id() == chat)
        else {
            return Ok(());
        };
        surface.report_error(message);
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub fn task_surface_chat(&self) -> Result<Option<Uuid>, String> {
        Ok(self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .task_surface
            .as_ref()
            .map(|surface| *surface.restore()))
    }

    pub(super) fn task_dialog_chats(&self) -> Result<Vec<Uuid>, String> {
        let state = self.state.lock().map_err(|error| error.to_string())?;
        let mut chats = state
            .task_surface
            .iter()
            .map(|surface| *surface.restore())
            .collect::<Vec<_>>();
        chats.extend(state.suspended_task_chats());
        chats.sort_unstable();
        chats.dedup();
        Ok(chats)
    }

    pub(super) fn take_resumed_task_effect(
        &self,
    ) -> Result<Option<(Uuid, TaskSurfaceEffect)>, String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        Ok(state.resumed_task_effect.take().filter(|(chat, _)| {
            state
                .task_surface
                .as_ref()
                .is_some_and(|surface| *surface.restore() == *chat)
        }))
    }

    pub fn update_tasks(&self, snapshot: TaskSurfaceSnapshot) -> Result<TaskSurfaceEffect, String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let chat_id = snapshot.chat_id;
        if let Some(surface) = state
            .task_surface
            .as_mut()
            .filter(|surface| *surface.restore() == chat_id)
        {
            let effect = surface.update_snapshot(snapshot);
            state
                .draw(&mut io::stdout().lock())
                .map_err(|error| error.to_string())?;
            return Ok(effect);
        }
        if let Some(surface) = state.hidden_task_surface_mut(chat_id) {
            surface.update_snapshot_hidden(snapshot);
        }
        Ok(TaskSurfaceEffect::None)
    }

    pub fn update_task_output(
        &self,
        chat: Uuid,
        update: TaskOutputUpdate,
    ) -> Result<TaskSurfaceEffect, String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let Some(surface) = state.task_surface_mut(chat) else {
            return Ok(TaskSurfaceEffect::None);
        };
        let effect = surface.update_output(update);
        let effect = if state
            .task_surface
            .as_ref()
            .is_some_and(|surface| *surface.restore() == chat)
        {
            effect
        } else {
            TaskSurfaceEffect::None
        };
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())?;
        Ok(effect)
    }

    pub fn task_action_failed(
        &self,
        chat: Uuid,
        task_id: Uuid,
        message: String,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let Some(surface) = state.task_surface_mut(chat) else {
            return Ok(());
        };
        surface.report_action_error(task_id, message);
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub fn clear_task_action_error(&self, chat: Uuid) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if let Some(surface) = state.task_surface_mut(chat) {
            surface.clear_action_error();
        }
        Ok(())
    }

    pub fn task_updates_failed(&self, chat_id: Uuid, message: String) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if let Some(surface) = state
            .task_surface
            .as_mut()
            .filter(|surface| *surface.restore() == chat_id)
        {
            surface.report_update_error(message);
        } else if let Some(surface) = state.hidden_task_surface_mut(chat_id) {
            surface.report_update_error(message);
        }
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    #[cfg(test)]
    pub fn start(interactive: bool) -> Result<(Self, mpsc::Receiver<InputEvent>), String> {
        Self::start_with_display(interactive, DisplayPreferences::default())
    }

    pub(super) fn start_with_display(
        interactive: bool,
        preferences: DisplayPreferences,
    ) -> Result<(Self, mpsc::Receiver<InputEvent>), String> {
        Self::start_with_startup(interactive, preferences, None)
    }

    pub(super) fn start_with_workspace_trust(
        preferences: DisplayPreferences,
        workdir: PathBuf,
    ) -> Result<(Self, mpsc::Receiver<InputEvent>), String> {
        Self::start_with_startup(true, preferences, Some(WorkspaceTrust::new(workdir)))
    }

    fn start_with_startup(
        interactive: bool,
        preferences: DisplayPreferences,
        workspace_trust: Option<WorkspaceTrust>,
    ) -> Result<(Self, mpsc::Receiver<InputEvent>), String> {
        let (width, height) = terminal::size().unwrap_or((80, 24));
        let choice =
            renderer::startup_choice(std::env::var(renderer::RENDERER_VARIABLE).ok().as_deref());
        // A console without a terminal draws nothing, so it stays on the surface
        // `State::new` starts from and never enters the alternate screen.
        let startup_renderer = if interactive {
            choice.renderer
        } else {
            Renderer::Inline
        };
        let mut initial = State::new_with_display(interactive, width, height, preferences);
        initial.workspace_trust = workspace_trust;
        if interactive {
            initial.renderer = startup_renderer;
            initial.surface = TerminalSurface::new(renderer::captures_mouse());
            initial.synchronized_output = renderer::supports_synchronized_output();
            initial.clipboard = Clipboard::detect();
            initial.terminal_shows_links = hyperlink::supported(|name| std::env::var(name).ok());
            initial.status = choice.notice.unwrap_or_default();
        }
        let surface = initial.surface.clone();
        let state = Arc::new(Mutex::new(initial));
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel(INPUT_CHANNEL_CAPACITY);
        if interactive {
            terminal::enable_raw_mode().map_err(|error| error.to_string())?;
            let mut output = io::stdout();
            if let Err(error) = surface
                .take(startup_renderer, &mut output)
                .and_then(|()| enable_input_reporting(&mut output))
            {
                let _ = restore_terminal(&mut output, &surface);
                return Err(error.to_string());
            }
        }
        let reader_state = Arc::clone(&state);
        if interactive
            && state
                .lock()
                .map_err(|error| error.to_string())?
                .workspace_trust
                .is_some()
        {
            let initial_draw = state
                .lock()
                .map_err(|error| error.to_string())?
                .draw(&mut io::stdout().lock());
            if let Err(error) = initial_draw {
                let _ = restore_terminal(&mut io::stdout(), &surface);
                return Err(error.to_string());
            }
        }
        let reader_stop = Arc::clone(&stop);
        let reader = thread::Builder::new()
            .name("agent-console-input".into())
            .spawn(move || {
                let result = if interactive {
                    read_keys(&reader_state, &reader_stop, &sender)
                } else {
                    read_lines(&reader_stop, &sender)
                };
                if let Err(error) = result {
                    let event = if error.kind() == io::ErrorKind::UnexpectedEof {
                        InputEvent::Eof
                    } else {
                        InputEvent::Error(error.to_string())
                    };
                    let _ = send_input(&sender, &reader_stop, event);
                }
            });
        let reader = match reader {
            Ok(reader) => reader,
            Err(error) => {
                if interactive {
                    let _ = restore_terminal(&mut io::stdout(), &surface);
                }
                return Err(error.to_string());
            }
        };
        Ok((
            Self {
                state: Arc::clone(&state),
                _lifetime: Arc::new(Lifetime {
                    state,
                    stop,
                    reader: Mutex::new(Some(reader)),
                    interactive,
                    surface,
                }),
                message_id: None,
                #[cfg(test)]
                reset_chat_failure: Arc::new(Mutex::new(None)),
                #[cfg(test)]
                branch_commit_failure: Arc::new(Mutex::new(None)),
            },
            receiver,
        ))
    }

    pub fn write(&self, text: &str) -> Result<(), String> {
        self.write_text(text, false)
    }

    pub(super) fn workspace_trust_id(&self) -> Result<Uuid, String> {
        self.state
            .lock()
            .map_err(|error| error.to_string())?
            .workspace_trust
            .as_ref()
            .map(|dialog| dialog.id)
            .ok_or_else(|| "Startup trust dialog is not active".into())
    }

    pub(super) fn workspace_trust_failed(&self, error: String) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if let Some(dialog) = &mut state.workspace_trust {
            let id = dialog.id;
            dialog.failed(error);
            state.begin_menu_submission(id);
            state.mark_menu_submission_finished(id);
        }
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(super) fn finish_workspace_trust(&self) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if let Some(dialog) = state.workspace_trust.take() {
            state.begin_menu_submission(dialog.id);
            state.mark_menu_submission_finished(dialog.id);
            state
                .draw(&mut io::stdout().lock())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Writes `text` as a line of its own; in plain output a line left open by streamed reply
    /// text is ended first, so a notice never joins it.
    pub fn write_line(&self, text: &str) -> Result<(), String> {
        self.write_text(&format!("{text}\n"), true)
    }

    fn write_text(&self, text: &str, own_line: bool) -> Result<(), String> {
        if text.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let mut output = io::stdout().lock();
        if !state.interactive {
            let separator = if own_line && state.plain_line_open && !text.starts_with('\n') {
                "\n"
            } else {
                ""
            };
            output
                .write_all(separator.as_bytes())
                .and_then(|()| output.write_all(text.as_bytes()))
                .and_then(|()| output.flush())
                .map_err(|error| error.to_string())?;
            state.plain_line_open = !text.ends_with('\n');
            return Ok(());
        }
        state.scrollback.append(text);
        if self.message_id.is_none() {
            state.capture_creation_output(&[ConversationPiece::Styled(text.into(), None)]);
        }
        if let Some(id) = &self.message_id {
            state.scrollback.note_message(id.clone());
        }
        if state.transcript.is_none() {
            state.draw(&mut output).map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Writes runs of text, each carrying the role it plays in the
    /// conversation. Non-interactive output receives the text alone.
    pub(super) fn write_roles(&self, spans: &[(String, Option<Role>)]) -> Result<(), String> {
        let pieces: Vec<ConversationPiece> = spans
            .iter()
            .map(|(text, role)| ConversationPiece::Styled(text.clone(), *role))
            .collect();
        self.write_pieces(&pieces)
    }

    /// Writes assistant reply text, shown as Markdown in the conversation.
    /// Non-interactive output receives the Markdown source.
    pub(super) fn write_markdown(&self, text: &str, starts_message: bool) -> Result<(), String> {
        self.write_pieces(&[ConversationPiece::Markdown {
            text: text.to_owned(),
            starts_message,
        }])
    }

    /// Writes a run's completion notice; in the conversation it replaces the
    /// previous completion notice.
    pub(super) fn write_completion_notice(
        &self,
        spans: &[(String, Option<Role>)],
    ) -> Result<(), String> {
        self.write_scrollback_spans(spans, |scrollback| {
            scrollback.append_completion_notice(spans)
        })
    }

    pub(super) fn write_tool_call(
        &self,
        run_id: Uuid,
        call_id: &str,
        spans: &[(String, Option<Role>)],
        state: ToolCallState,
    ) -> Result<(), String> {
        self.write_scrollback_spans(spans, |scrollback| {
            scrollback.set_tool_call(run_id, call_id, spans, state)
        })
    }

    fn write_scrollback_spans(
        &self,
        spans: &[(String, Option<Role>)],
        append: impl FnOnce(&mut Scrollback),
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if !state.interactive {
            drop(state);
            return self.write_roles(spans);
        }
        append(&mut state.scrollback);
        if let Some(id) = &self.message_id {
            state.scrollback.note_message(id.clone());
        }
        if state.transcript.is_none() {
            state
                .draw(&mut io::stdout().lock())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Ends one call or the remaining calls of the originating Run.
    pub(super) fn finish_tool_call(
        &self,
        run_id: Uuid,
        call_id: Option<&str>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        match call_id {
            Some(call_id) => state.scrollback.finish_tool_call(run_id, call_id),
            None => state.scrollback.finish_tool_calls(run_id),
        }
        Ok(())
    }

    /// Hides the previous run's completion notice when another run starts.
    pub(super) fn hide_completion_notice(&self) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if !state.interactive {
            return Ok(());
        }
        state.scrollback.hide_completion_notice();
        if state.transcript.is_none() {
            state
                .draw(&mut io::stdout().lock())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Shows or clears the live row below the conversation.
    pub(super) fn set_activity(
        &self,
        spans: Option<&[(String, Option<Role>)]>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if spans.is_none() {
            state.activity_interruptible = false;
        }
        if !state.interactive {
            return Ok(());
        }
        let activity = spans.map(|spans| {
            let mut line = RoleLine::default();
            for (text, role) in spans {
                line.push(display_text(text), *role);
            }
            line
        });
        if state.scrollback.set_activity(activity) && state.transcript.is_none() {
            state
                .draw(&mut io::stdout().lock())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub(super) fn set_activity_and_status(
        &self,
        spans: Option<&[(String, Option<Role>)]>,
        status: &str,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if spans.is_none() {
            state.activity_interruptible = false;
        }
        let activity = spans.map(|spans| {
            let mut line = RoleLine::default();
            for (text, role) in spans {
                line.push(display_text(text), *role);
            }
            line
        });
        state.scrollback.set_activity(activity);
        state.refresh_status(status);
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(super) fn set_activity_interruptible(&self, interruptible: bool) -> Result<(), String> {
        self.state
            .lock()
            .map_err(|error| error.to_string())?
            .activity_interruptible = interruptible;
        Ok(())
    }

    /// Shows why an action did nothing, kept across periodic status refreshes until the next key.
    pub(super) fn hold_status(&self, text: &str) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.hold_status(text);
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(super) fn write_pieces(&self, pieces: &[ConversationPiece]) -> Result<(), String> {
        if pieces.iter().all(|piece| piece.text().is_empty()) {
            return Ok(());
        }
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let mut output = io::stdout().lock();
        if !state.interactive {
            for piece in pieces {
                output
                    .write_all(piece.text().as_bytes())
                    .map_err(|error| error.to_string())?;
            }
            if let Some(last) = pieces
                .iter()
                .map(|piece| piece.text())
                .rfind(|text| !text.is_empty())
            {
                state.plain_line_open = !last.ends_with('\n');
            }
            return output.flush().map_err(|error| error.to_string());
        }
        if self.message_id.is_none() {
            state.capture_creation_output(pieces);
        }
        let display = state.display;
        for piece in pieces {
            piece.append_to(&mut state.scrollback, display);
        }
        if let Some(id) = &self.message_id {
            state.scrollback.note_message(id.clone());
        }
        if state.transcript.is_none() {
            state.draw(&mut output).map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Echoes a submitted line in the body; the returned echo names it in this presentation only.
    pub fn record_input(&self, line: &str) -> Result<Option<InputEcho>, String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if !state.interactive {
            return Ok(None);
        }
        let echo = state.record_input(line);
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())?;
        Ok(echo)
    }

    pub(super) fn input_is_echoed(&self, chat: Uuid, input: Uuid) -> Result<bool, String> {
        let state = self.state.lock().map_err(|error| error.to_string())?;
        Ok(if state.chat == Some(chat) {
            state.echoed_inputs.contains(&input)
        } else {
            state
                .view_cache
                .get(&Some(chat))
                .is_none_or(|cached| cached.echoed_inputs.contains(&input))
        })
    }

    pub(super) fn mark_input_echoes(
        &self,
        chat: Uuid,
        inputs: impl IntoIterator<Item = Uuid>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if state.chat == Some(chat) {
            for input in inputs {
                state.awaiting_input_echoes.remove(&input);
                state.echoed_inputs.insert(input);
            }
        } else if let Some(cached) = state.view_cache.get_mut(&Some(chat)) {
            for input in inputs {
                cached.awaiting_input_echoes.remove(&input);
                cached.echoed_inputs.insert(input);
            }
        }
        Ok(())
    }

    pub(super) fn defer_input_echo(&self, chat: Uuid, input: Uuid) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if state.chat == Some(chat) {
            if !state.echoed_inputs.contains(&input) {
                state.awaiting_input_echoes.insert(input);
            }
        } else if let Some(cached) = state.view_cache.get_mut(&Some(chat))
            && !cached.echoed_inputs.contains(&input)
        {
            cached.awaiting_input_echoes.insert(input);
        }
        Ok(())
    }

    pub(super) fn awaiting_input_echoes(&self, chat: Uuid) -> Result<Vec<Uuid>, String> {
        let state = self.state.lock().map_err(|error| error.to_string())?;
        Ok(if state.chat == Some(chat) {
            state.awaiting_input_echoes.iter().copied().collect()
        } else {
            state
                .view_cache
                .get(&Some(chat))
                .map(|cached| cached.awaiting_input_echoes.iter().copied().collect())
                .unwrap_or_default()
        })
    }

    pub(super) fn finish_deferred_input_echo(&self, chat: Uuid, input: Uuid) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if state.chat == Some(chat) {
            state.awaiting_input_echoes.remove(&input);
        } else if let Some(cached) = state.view_cache.get_mut(&Some(chat)) {
            cached.awaiting_input_echoes.remove(&input);
        }
        Ok(())
    }

    pub(super) fn record_consumed_input(
        &self,
        chat: Uuid,
        input: Uuid,
        line: &str,
    ) -> Result<Option<InputEcho>, String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if !state.interactive {
            return Ok(None);
        }
        if state.chat == Some(chat) {
            state.awaiting_input_echoes.remove(&input);
            if state.echoed_inputs.contains(&input) {
                return Ok(None);
            }
            let echo = state.record_input(line);
            if echo.is_some() {
                state.echoed_inputs.insert(input);
            }
            state
                .draw(&mut io::stdout().lock())
                .map_err(|error| error.to_string())?;
            Ok(echo)
        } else {
            let display = state.display;
            let links = state.image_links().in_text(chat, line);
            let Some(cached) = state.view_cache.get_mut(&Some(chat)) else {
                return Ok(None);
            };
            cached.awaiting_input_echoes.remove(&input);
            if cached.echoed_inputs.contains(&input) {
                return Ok(None);
            }
            let echo = append_input_echo(&mut cached.scrollback, line, display, &links);
            if echo.is_some() {
                cached.echoed_inputs.insert(input);
            }
            Ok(echo)
        }
    }

    /// Shows `current` as the continuation of `previous` in `chat`'s body when the two echoes are
    /// adjacent there; otherwise both stay separate blocks.
    pub(super) fn join_inputs(
        &self,
        chat: Uuid,
        previous: InputEcho,
        current: InputEcho,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let prompt = format!("{} ", state.display.glyph(Glyph::Prompt));
        let joined = if state.chat == Some(chat) {
            state.scrollback.join_inputs(previous, current, &prompt)
        } else {
            state
                .view_cache
                .get_mut(&Some(chat))
                .is_some_and(|cached| cached.scrollback.join_inputs(previous, current, &prompt))
        };
        if joined && state.chat == Some(chat) {
            state
                .draw(&mut io::stdout().lock())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Hides `echo` in `view`'s presentation (`None` is the New Agent draft): its input was not
    /// sent, or is shown in the created Agent's view instead.
    pub(super) fn withdraw_echo(&self, view: Option<Uuid>, echo: InputEcho) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if let Some(prefix) = state.creation_prefixes.get_mut(&view) {
            prefix.retain(|entry| {
                !matches!(entry, CreationPrefixEntry::Input { echo: recorded, .. } if *recorded == echo)
            });
        }
        if state.chat == view {
            state.scrollback.hide_echo(echo);
            state
                .draw(&mut io::stdout().lock())
                .map_err(|error| error.to_string())?;
        } else if let Some(cached) = state.view_cache.get_mut(&view) {
            cached.scrollback.hide_echo(echo);
        }
        Ok(())
    }

    pub fn show_menu(&self, menu: Menu) -> Result<(), String> {
        self.show_menu_with_comments(
            menu,
            None,
            Vec::new(),
            "Comment",
            None,
            MenuFocus::WhenInputEmpty,
        )
    }

    pub fn show_focused_menu(&self, menu: Menu) -> Result<(), String> {
        self.show_menu_with_comments(menu, None, Vec::new(), "Comment", None, MenuFocus::Menu)
    }

    pub(super) fn show_picker_menu(&self, menu: Menu, kind: PickerKind) -> Result<(), String> {
        self.show_menu_with_comments(
            menu,
            None,
            Vec::new(),
            "Comment",
            None,
            MenuFocus::Picker(
                kind,
                PickerPresentation {
                    headers: Vec::new(),
                    refresh: false,
                    hint: match kind {
                        PickerKind::Provider => {
                            "p Connection · f All/Configured · Enter models/setup · Esc close"
                        }
                        PickerKind::Model => "p Connection · Enter apply · Esc back",
                        PickerKind::Actions => "Enter open · Esc back",
                    }
                    .into(),
                    ..Default::default()
                },
            ),
        )
    }

    pub(super) fn picker_selection(&self, id: Uuid) -> Result<Option<usize>, String> {
        let state = self.state.lock().map_err(|error| error.to_string())?;
        Ok(state
            .menu
            .as_ref()
            .filter(|menu| menu.id == id)
            .map(|menu| menu.selected))
    }

    pub(super) fn show_presented_picker_menu(
        &self,
        menu: Menu,
        kind: PickerKind,
        presentation: PickerPresentation,
    ) -> Result<(), String> {
        self.show_menu_with_comments(
            menu,
            None,
            Vec::new(),
            "Comment",
            None,
            MenuFocus::Picker(kind, presentation),
        )
    }

    pub(super) fn show_secret_menu(&self, menu: Menu) -> Result<(), String> {
        self.show_secret_menu_with_value(menu, "")
    }

    pub(super) fn show_secret_menu_with_value(
        &self,
        mut menu: Menu,
        value: &str,
    ) -> Result<(), String> {
        menu.selected = 0;
        self.show_menu_with_comments(
            menu,
            Some(vec![0]),
            Vec::new(),
            "API key",
            None,
            MenuFocus::Secret(value.into()),
        )
    }

    pub(super) fn show_input_menu(
        &self,
        mut menu: Menu,
        label: &'static str,
        value: &str,
    ) -> Result<(), String> {
        menu.selected = 0;
        self.show_menu_with_comments(
            menu,
            Some(vec![0]),
            Vec::new(),
            label,
            None,
            MenuFocus::Input(value.into()),
        )
    }

    pub(super) fn show_login_menu(
        &self,
        menu: Menu,
        address: &str,
        callback: bool,
    ) -> Result<(), String> {
        {
            let mut state = self.state.lock().map_err(|error| error.to_string())?;
            if state.update_hidden_login(&menu, address, callback) {
                return state
                    .draw(&mut io::stdout().lock())
                    .map_err(|error| error.to_string());
            }
        }
        let link =
            valid_login_link(address).filter(|_| menu.context.iter().any(|line| line == address));
        self.show_menu_with_comments(
            menu,
            callback.then_some(vec![0]),
            Vec::new(),
            if callback { "Callback URL" } else { "Comment" },
            None,
            link.map_or(MenuFocus::Menu, MenuFocus::MenuWithLink),
        )
    }

    pub fn show_commentable_menu(
        &self,
        menu: Menu,
        comment_options: Vec<usize>,
    ) -> Result<(), String> {
        self.show_menu_with_comments(
            menu,
            Some(comment_options),
            Vec::new(),
            "Comment",
            None,
            MenuFocus::WhenInputEmpty,
        )
    }

    pub(super) fn show_plan_menu(&self, menu: Menu, feedback: bool) -> Result<(), String> {
        self.show_menu_with_comments(
            menu,
            feedback.then_some(vec![
                super::workflows::PlanApprovalChoice::KeepPlanning.option_index(),
            ]),
            Vec::new(),
            "Feedback",
            None,
            MenuFocus::Plan,
        )
    }

    pub fn show_expandable_commentable_menu(
        &self,
        menu: Menu,
        comment_options: Vec<usize>,
        expand_options: Vec<usize>,
    ) -> Result<(), String> {
        self.show_menu_with_comments(
            menu,
            Some(comment_options),
            expand_options,
            "Comment",
            None,
            MenuFocus::WhenInputEmpty,
        )
    }

    pub fn show_focused_commentable_menu(
        &self,
        menu: Menu,
        comment_options: Vec<usize>,
    ) -> Result<(), String> {
        self.show_menu_with_comments(
            menu,
            Some(comment_options),
            Vec::new(),
            "Guidance",
            None,
            MenuFocus::Menu,
        )
    }

    /// Shows one AskUserQuestion step; the "Other" row takes free text in an inline field.
    pub(super) fn show_question_menu(
        &self,
        menu: Menu,
        question: QuestionMenu,
    ) -> Result<(), String> {
        let comment_options = question.entry_option.map(|entry| vec![entry]);
        self.show_menu_with_comments(
            menu,
            comment_options,
            Vec::new(),
            QUESTION_ENTRY_LABEL,
            None,
            MenuFocus::Question(question),
        )
    }

    pub(super) fn prepare_command(&self, command: String) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if !state.editor.draft().is_empty() {
            return Err(
                "Input already has a draft. Keep or clear it before configuring a timer.".into(),
            );
        }
        let cursor = command.len();
        state.replace_draft(command, cursor);
        state.status =
            "Edit timer command · Enter save · Ctrl+G external editor · Ctrl+C cancel".into();
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub fn show_reviewable_menu(
        &self,
        menu: Menu,
        comment_options: Vec<usize>,
        review: Result<FileChangeReview, String>,
    ) -> Result<(), String> {
        self.show_menu_with_comments(
            menu,
            Some(comment_options),
            Vec::new(),
            "Comment",
            Some(review),
            MenuFocus::WhenInputEmpty,
        )
    }

    fn show_menu_with_comments(
        &self,
        mut menu: Menu,
        comment_options: Option<Vec<usize>>,
        expand_options: Vec<usize>,
        comment_label: &'static str,
        review: Option<Result<FileChangeReview, String>>,
        focus: MenuFocus,
    ) -> Result<(), String> {
        if menu.options.is_empty() {
            return Err("a selection menu needs at least one option".into());
        }
        if comment_options
            .as_ref()
            .is_some_and(|options| options.iter().any(|option| *option >= menu.options.len()))
        {
            return Err("a commentable menu option does not exist".into());
        }
        if expand_options
            .iter()
            .any(|option| *option >= menu.options.len())
        {
            return Err("an expandable menu option does not exist".into());
        }
        let secret = matches!(&focus, MenuFocus::Secret(_));
        let standalone_input = matches!(&focus, MenuFocus::Input(_));
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if !state.interactive {
            return Err("interactive menus require a terminal".into());
        }
        let picker_refresh = match &focus {
            MenuFocus::Picker(kind, _) => state
                .menu
                .as_ref()
                .filter(|current| current.id == menu.id && state.picker_kind == Some(*kind))
                .map(|current| {
                    (
                        current.selected,
                        state.menu_focused
                            && state.menu_is_visible()
                            && state.task_surface.is_none()
                            && state.subscription_surface.is_none()
                            && state.transcript.is_none()
                            && state.diff_screen.is_none()
                            && state.history_search.is_none(),
                    )
                }),
            _ => None,
        };
        menu.selected = picker_refresh
            .map_or(menu.selected, |(selected, _)| selected)
            .min(menu.options.len() - 1);
        let mut output = io::stdout().lock();
        state.menu_focused = picker_refresh.map_or_else(
            || {
                !matches!(focus, MenuFocus::Picker(..))
                    || state.request_layout.is_some()
                    || state.permissions_surface.is_some()
                    || (state.task_surface.is_none()
                        && state.subscription_surface.is_none()
                        && match &focus {
                            MenuFocus::WhenInputEmpty | MenuFocus::Question(_) => {
                                state.editor.draft().is_empty()
                            }
                            MenuFocus::Menu
                            | MenuFocus::Plan
                            | MenuFocus::MenuWithLink(_)
                            | MenuFocus::Secret(_)
                            | MenuFocus::Input(_)
                            | MenuFocus::Picker(..) => true,
                        })
            },
            |(_, focused)| focused,
        );
        state.review_focused = false;
        state.menu_rule_role = matches!(focus, MenuFocus::Plan).then_some(Role::PlanMode);
        state.menu_plan = matches!(focus, MenuFocus::Plan);
        state.request_menu = state.request_layout.map(|_| menu.id);
        state.shell_background_chord_started_at = None;
        state.completion.dismiss();
        state.clear_footer_selection();

        state.menu_comment = comment_options.map(|options| MenuCommentState {
            options,
            values: HashMap::new(),
            label: comment_label,
            editing: secret || standalone_input,
            cursor: 0,
            secret,
            standalone_input,
        });
        if let (Some(comment), MenuFocus::Input(value) | MenuFocus::Secret(value)) =
            (&mut state.menu_comment, &focus)
        {
            comment.cursor = value.len();
            comment.values.insert(0, value.clone());
        }
        if let (Some(comment), MenuFocus::Question(question)) = (&mut state.menu_comment, &focus)
            && let Some(entry) = question.entry_option
            && !question.entry_text.is_empty()
        {
            comment.values.insert(entry, question.entry_text.clone());
        }
        state.picker_kind = match &focus {
            MenuFocus::Picker(kind, _) => Some(*kind),
            MenuFocus::Secret(_) | MenuFocus::Input(_) | MenuFocus::MenuWithLink(_) => {
                Some(PickerKind::Actions)
            }
            _ => None,
        };
        state.picker_presentation = match &focus {
            MenuFocus::Picker(_, presentation) => presentation.clone(),
            _ => PickerPresentation {
                hint: "Enter confirm · Esc back".into(),
                ..Default::default()
            },
        };
        state.question_menu = match &focus {
            MenuFocus::Question(question) => Some(question.clone()),
            _ => None,
        };
        state.menu_expand_options = expand_options;
        state.menu_review = review.map(|review| MenuReviewState {
            review,
            navigation: DiffNavigation::default(),
            viewport: DiffViewport {
                width: 1,
                height: 1,
            },
        });
        state.menu_link = match focus {
            MenuFocus::MenuWithLink(address) => Some((menu.id, address)),
            MenuFocus::WhenInputEmpty
            | MenuFocus::Menu
            | MenuFocus::Plan
            | MenuFocus::Secret(_)
            | MenuFocus::Input(_)
            | MenuFocus::Picker(..)
            | MenuFocus::Question(_) => None,
        };
        state.menu = Some(menu);
        state.full_review = false;
        state.context_offset = 0;
        state.draw(&mut output).map_err(|error| error.to_string())
    }

    /// Shows the saved-conversation picker in the input dialog. Its choice and cancellation
    /// arrive as the menu `Choice` and `Cancel` of `picker.id()`.
    pub(super) fn show_conversation_picker(
        &self,
        picker: ConversationPicker,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if !state.interactive {
            return Err("interactive menus require a terminal".into());
        }
        state.shell_background_chord_started_at = None;
        state.completion.dismiss();
        state.clear_footer_selection();

        state.conversation_picker = Some(picker);
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub fn clear_menu(&self, id: Uuid) -> Result<(), String> {
        self.clear_menu_state(id)
    }

    pub fn finish_menu_submission(&self, id: Uuid) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.mark_menu_submission_finished(id);
        Ok(())
    }

    fn clear_menu_state(&self, id: Uuid) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if state.clear_menu(id) {
            let mut output = io::stdout().lock();
            state.draw(&mut output).map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub fn set_prompt_enabled(&self, enabled: bool) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if state.prompt_enabled != enabled {
            let mut output = io::stdout().lock();
            state.prompt_enabled = enabled;
            if !enabled {
                state.clear_prompt_suggestion();
                state.shell_background_chord_started_at = None;
            }
            state.draw(&mut output).map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub fn set_foreground_shell(&self, active: bool) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if state.foreground_shell != active {
            state.set_foreground_shell(active);
            state
                .draw(&mut io::stdout().lock())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub fn set_queued_inputs(&self, queued_inputs: Vec<QueuedInput>) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if state.queued_inputs != queued_inputs {
            state.queued_inputs = queued_inputs;
            state
                .draw(&mut io::stdout().lock())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Follows the shown Chat: whether its model takes images, and the highest pasted image
    /// already in its conversation, after which new `[Image #N]` numbers continue.
    pub fn set_chat_images(
        &self,
        chat: Uuid,
        accepts_images: bool,
        highest_pasted_image: Option<u32>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if state.chat != Some(chat) {
            return Ok(());
        }
        state.accepts_images = accepts_images;
        if let Some(id) = highest_pasted_image {
            state.paste_blocks.continue_after_image(id);
        }
        Ok(())
    }

    pub fn restore_queued_inputs(&self, entries: &[QueuedInput]) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let ids = entries
            .iter()
            .map(|entry| entry.id)
            .collect::<std::collections::HashSet<_>>();
        state.queued_inputs.retain(|entry| !ids.contains(&entry.id));
        if entries.len() == 1
            && entries[0].kind == QueuedInputKind::Shell
            && state.editor.draft().is_empty()
        {
            let action = state.shell_input.restore(entries[0].text.clone());
            state.apply_shell_action(action);
        } else if !entries.is_empty() {
            let mut restored = entries
                .iter()
                .map(|entry| match entry.kind {
                    QueuedInputKind::Message => entry.text.clone(),
                    QueuedInputKind::Command => entry.text.clone(),
                    QueuedInputKind::Shell => format!("!{}", entry.text),
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !state.editor.draft().is_empty() {
                restored.push('\n');
                restored.push_str(state.editor.draft());
            }
            let cursor = restored.len();
            state.replace_draft(restored, cursor);
        }
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub fn restore_rewind_draft(
        &self,
        prompt: &str,
        contexts: &[InputContext],
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if state.branch_submission_protects_editor() {
            state.finish_summary_input_transition();
            return Ok(());
        }
        let existing = state.editor.draft().to_owned();
        let history_session = state.history_session.clone();
        let history_project = state.history_project.clone();
        let pasted_texts = state.pasted_texts();
        if !existing.trim().is_empty()
            && let Some(history) = &mut state.input_history
        {
            history
                .record(&history_session, &history_project, &existing, &pasted_texts)
                .map_err(|error| error.to_string())?;
        }
        state.paste_blocks.clear();
        state.replace_draft(prompt.to_owned(), prompt.len());
        state.restore_saved_contexts(contexts);
        state.escape_confirmation = None;
        state.status = if contexts.is_empty() {
            "Checkpoint prompt restored for editing".into()
        } else {
            format!(
                "Checkpoint prompt restored with {} saved attachment{}",
                contexts.len(),
                if contexts.len() == 1 { "" } else { "s" }
            )
        };
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub fn clear_summary_draft(&self) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if state.branch_submission_protects_editor() {
            state.finish_summary_input_transition();
            return Ok(());
        }
        if state.editor.draft().is_empty() {
            state.restored_contexts.clear();
        } else {
            state.clear_draft_to_history();
            if !state.editor.draft().is_empty() {
                return Err(state.status.clone());
            }
        }
        state.escape_confirmation = None;
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(super) fn finish_summary_input_transition(&self) -> Result<(), String> {
        self.state
            .lock()
            .map_err(|error| error.to_string())?
            .finish_summary_input_transition();
        Ok(())
    }

    /// Puts a prompt the Agent refused back into the editor with its folded pastes, accepted
    /// `@` mentions and saved attachments, so it can be edited and sent again.
    pub fn restore_rejected_prompt(
        &self,
        contexts: Vec<InputContext>,
        status: &str,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.restore_rejected_prompt(contexts);
        state.status = display_text(status).replace('\n', " ");
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub fn restore_rejected_shell(&self, command: &str, status: &str) -> Result<(), String> {
        self.restore_unknown_shell(command, status, None)
    }

    /// Puts back a sent prompt whose acceptance is unknown. With `retry_of`, pressing Enter on the
    /// unchanged draft asks for that input's receipt instead of sending it again.
    pub(super) fn restore_unknown_prompt(
        &self,
        contexts: Vec<InputContext>,
        status: &str,
        retry_of: Option<Uuid>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.restore_unknown_prompt(contexts, retry_of);
        state.status = display_text(status).replace('\n', " ");
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(super) fn restore_unknown_shell(
        &self,
        command: &str,
        status: &str,
        retry_of: Option<Uuid>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if state.editor.draft().is_empty() {
            let action = state.shell_input.restore(command.to_owned());
            state.apply_shell_action(action);
            state.retry_of = retry_of;
        }
        state.status = display_single_line(status);
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    /// The unknown input the just-submitted, unchanged restored draft retries.
    pub(super) fn take_submitted_retry(&self) -> Result<Option<Uuid>, String> {
        Ok(self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .submitted_retry_of
            .take())
    }

    pub fn take_restored_contexts(&self, draft: &str) -> Result<Vec<InputContext>, String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state
            .restored_contexts
            .retain(|context| draft_keeps_context(draft, context));
        Ok(std::mem::take(&mut state.restored_contexts))
    }

    pub fn set_status(&self, text: &str) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.status = display_text(text).replace('\n', " ");
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    /// Current drawing surface, or `None` when this console is not attached to
    /// an interactive terminal.
    pub(super) fn renderer(&self) -> Result<Option<Renderer>, String> {
        let state = self.state.lock().map_err(|error| error.to_string())?;
        Ok(state.interactive.then_some(state.renderer))
    }

    /// Makes the next frame draw the whole screen after a hand-over that failed
    /// part-way. Stdout must not be locked by the caller: a frame takes the state
    /// lock before stdout.
    #[cfg(unix)]
    fn forget_rendered_rows(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.rendered_region = None;
        }
    }

    /// Moves the conversation to another drawing surface inside this process.
    /// Nothing about the session changes, so drafts, editor mode, scroll
    /// position and pending menus survive the move untouched.
    pub(super) fn switch_renderer(&self, target: Renderer) -> Result<RendererSwitch, String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if !state.interactive {
            return Ok(RendererSwitch::NotInteractive);
        }
        let previous = state.renderer;
        if previous == target {
            return Ok(RendererSwitch::AlreadyActive);
        }
        let mut output = io::stdout().lock();
        let surface = state.surface.clone();
        // Either failure leaves the screen showing a stale frame, so both
        // repaint before reporting. `release` keeps the surface flag set when
        // it fails, so a half-written sequence still gets cleaned up on exit.
        if let Err(error) = surface.release(previous, &mut output) {
            state.repaint_whole_screen(&mut output);
            return Err(format!(
                "cannot leave the {} renderer: {error}",
                previous.name()
            ));
        }
        if let Err(error) = surface.take(target, &mut output) {
            state.repaint_whole_screen(&mut output);
            return Err(format!(
                "cannot enter the {} renderer: {error}",
                target.name()
            ));
        }
        state.renderer = target;
        state.repaint_whole_screen(&mut output);
        Ok(RendererSwitch::Switched)
    }

    pub fn confirm_control_c_exit(&self) -> Result<bool, String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        let confirmed = state.request_exit('C');
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())?;
        Ok(confirmed)
    }

    #[cfg(unix)]
    pub fn suspend(&self) -> Result<(), String> {
        let (interactive, renderer, surface) = {
            let state = self.state.lock().map_err(|error| error.to_string())?;
            (state.interactive, state.renderer, state.surface.clone())
        };
        if !interactive {
            return Ok(());
        }

        let mut output = io::stdout().lock();
        // The alternate screen is left first so the suspend notice and the
        // shell the user returns to share the main buffer.
        let prepared = surface
            .release(renderer, &mut output)
            .and_then(|()| {
                write!(
                    output,
                    "{RESET_SCROLL_REGION}\r\nTick Loop Agent suspended. Run `fg` to resume.\r\n"
                )
            })
            .and_then(|()| disable_input_reporting(&mut output))
            .and_then(|()| execute!(output, Show))
            .and_then(|()| output.flush());
        if let Err(error) = prepared {
            drop(output);
            self.forget_rendered_rows();
            return Err(format!("cannot prepare terminal for suspend: {error}"));
        }
        if let Err(error) = terminal::disable_raw_mode() {
            let _ = surface.take(renderer, &mut output);
            let _ = enable_input_reporting(&mut output);
            let _ = execute!(output, Hide);
            drop(output);
            self.forget_rendered_rows();
            return Err(format!("cannot leave raw mode before suspend: {error}"));
        }
        drop(output);

        let stop_result = unsafe { libc::kill(libc::getpid(), libc::SIGSTOP) };
        let stop_error = (stop_result == -1)
            .then(|| format!("cannot suspend process: {}", io::Error::last_os_error()));

        let raw_result = terminal::enable_raw_mode();
        let mut output = io::stdout();
        let screen_result = surface.take(renderer, &mut output);
        let paste_result = enable_input_reporting(&mut output);
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.screen_initialized = false;
        state.rendered_region = None;
        if let Err(error) = raw_result.and(screen_result).and(paste_result) {
            return Err(format!("cannot restore terminal after resume: {error}"));
        }
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| format!("cannot redraw terminal after resume: {error}"))?;
        if let Some(error) = stop_error {
            return Err(error);
        }
        Ok(())
    }

    #[cfg(not(unix))]
    pub fn suspend(&self) -> Result<(), String> {
        Err("process suspension is available only on Unix".into())
    }
}

/// Restores every terminal mode this console turned on. Each step runs even
/// when an earlier one fails, so a single failure cannot leave the terminal
/// half restored; the first error is reported.
fn restore_terminal(output: &mut impl Write, surface: &TerminalSurface) -> io::Result<()> {
    let paste = disable_input_reporting(output);
    let screen = surface.release(surface.renderer(), output);
    let cursor = execute!(output, Show);
    let mode = terminal::disable_raw_mode();
    paste.and(screen).and(cursor).and(mode)
}

/// Bracketed paste and focus reporting go on and off together wherever the
/// console takes the terminal or hands it back.
fn enable_input_reporting(output: &mut impl Write) -> io::Result<()> {
    execute!(output, EnableBracketedPaste, EnableFocusChange)
}

fn disable_input_reporting(output: &mut impl Write) -> io::Result<()> {
    execute!(output, DisableBracketedPaste, DisableFocusChange)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RendererSwitch {
    Switched,
    AlreadyActive,
    NotInteractive,
}

impl Drop for Lifetime {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.clear_prompt_suggestion();
        }
        self.stop.store(true, Ordering::Release);
        if let Ok(reader) = self.reader.get_mut()
            && let Some(reader) = reader.take()
        {
            let _ = reader.join();
        }
        if let Ok(mut state) = self.state.lock()
            && let Some(history) = &mut state.input_history
        {
            history.flush();
        }
        if self.interactive {
            let renderer = self.surface.renderer();
            let mut reopen_hint = None;
            if let Ok(mut state) = self.state.lock() {
                let mut output = io::stdout().lock();
                reopen_hint = state
                    .chat
                    .map(|chat| format!("Chat {chat} · reopen it with /resume {chat}"));
                state.transcript = None;
                let _ = state.draw(&mut output);
                if !renderer.uses_alternate_screen() {
                    let _ = write!(output, "{RESET_SCROLL_REGION}");
                    let row = match state.rendered_region {
                        Some(RenderedRegion {
                            view: RenderedView::Conversation { body_rows },
                            ..
                        }) => body_rows,
                        _ => 0,
                    };
                    let _ = queue!(
                        output,
                        MoveTo(0, row as u16),
                        Clear(ClearType::FromCursorDown)
                    );
                }
            }
            let mut output = io::stdout().lock();
            let _ = restore_terminal(&mut output, &self.surface);
            // Printed once the terminal is restored, so the session stays
            // reachable after a surface that keeps nothing behind.
            if let Some(hint) = reopen_hint {
                let _ = writeln!(output, "{hint}");
            }
        }
    }
}

/// A run of conversation output: styled text, prompt text opening a link
/// (a cached `[Image #N]`), or assistant reply Markdown that starts a new
/// reply when `starts_message` is set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ConversationPiece {
    Styled(String, Option<Role>),
    Input {
        text: String,
        spans: Vec<RoleSpan>,
        is_user_message: bool,
    },
    Markdown {
        text: String,
        starts_message: bool,
    },
    ToolCall {
        run_id: Uuid,
        text: String,
        call_key: String,
        spans: Vec<(String, Option<Role>)>,
        state: ToolCallState,
    },
}

#[derive(Clone)]
enum CreationPrefixEntry {
    Input { echo: InputEcho, text: String },
    Output(ConversationPiece),
}

impl ConversationPiece {
    fn append_to(&self, scrollback: &mut Scrollback, display: DisplayProfile) {
        match self {
            Self::ToolCall {
                run_id,
                call_key,
                spans,
                state,
                ..
            } => scrollback.set_tool_call(*run_id, call_key, spans, *state),
            Self::Styled(text, role) => scrollback.append_as(text, *role, None),
            Self::Input {
                spans,
                is_user_message,
                ..
            } => {
                let first_line = scrollback.line_count();
                for span in spans {
                    scrollback.append_span(&span.text, span.role, None, span.link.as_ref());
                }
                if *is_user_message {
                    scrollback.mark_input(first_line..scrollback.line_count() - 1);
                }
            }
            Self::Markdown {
                text,
                starts_message,
            } => {
                let lead = format!("{} ", display.glyph(Glyph::Assistant));
                scrollback.append_markdown(text, &lead, *starts_message);
            }
        }
    }

    pub(super) fn text(&self) -> &str {
        match self {
            Self::Styled(text, _)
            | Self::Input { text, .. }
            | Self::Markdown { text, .. }
            | Self::ToolCall { text, .. } => text,
        }
    }
}

/// Finds how far the conversation region scrolled up between two frames, when
/// scrolling the terminal and repainting the rows that still differ writes
/// less than repainting every changed row in place.
fn body_scroll(previous: &[String], current: &[String]) -> Option<usize> {
    let rows_to_repaint = |shift: usize| {
        current
            .iter()
            .enumerate()
            .filter(|(row, rendered)| {
                previous
                    .get(row + shift)
                    .map_or(!rendered.is_empty(), |before| before != *rendered)
            })
            .count()
    };
    let in_place = rows_to_repaint(0);
    (1..previous.len())
        .filter(|&shift| previous[shift] == current[0])
        .map(|shift| (shift, rows_to_repaint(shift)))
        .min_by_key(|(_, count)| *count)
        .filter(|(_, count)| *count < in_place)
        .map(|(shift, _)| shift)
}

fn append_input_echo(
    scrollback: &mut Scrollback,
    line: &str,
    display: DisplayProfile,
    links: &[(std::ops::Range<usize>, hyperlink::Hyperlink)],
) -> Option<InputEcho> {
    let input = line.trim();
    if input.is_empty() {
        return None;
    }
    scrollback.append("\n");
    let first_line = scrollback.line_count() - 1;
    scrollback.append_as(
        &format!("{} ", display.glyph(Glyph::Prompt)),
        Some(Role::Subtle),
        Some(Role::UserMessageBg),
    );
    for (run, link) in hyperlink::runs(line, links) {
        scrollback.append_span(run, None, Some(Role::UserMessageBg), link);
    }
    scrollback.append("\n");
    let echo = scrollback.mark_input(first_line..scrollback.line_count() - 1);
    if !input.starts_with('/') {
        scrollback.note_message(Uuid::new_v4().to_string());
        if scrollback.auto_scroll() {
            scrollback.scroll(ScrollAction::Last);
        }
    }
    Some(echo)
}

fn pending_input_lines(entries: &[QueuedInput], width: usize, rows: usize) -> Vec<RoleLine> {
    if entries.is_empty() || rows == 0 {
        return Vec::new();
    }
    let title = format!("Messages to be submitted ({}) · ↑ edit", entries.len());
    let title = if display_width(&title) <= width {
        title
    } else {
        format!("Pending {} · ↑ edit", entries.len())
    };
    let mut lines = vec![RoleLine::role(title, Role::MutedText)];
    let available = rows.saturating_sub(1);
    let preview_count = if entries.len() > available && available > 1 {
        available - 1
    } else {
        available
    };
    for entry in entries.iter().take(preview_count) {
        let kind = match entry.kind {
            QueuedInputKind::Message => "message",
            QueuedInputKind::Shell => "shell",
            QueuedInputKind::Command => "command",
        };
        let target = if entry.loop_id.is_some() {
            String::new()
        } else {
            format!(" {NEW_LOOP_TARGET}")
        };
        let text = format!(
            "{kind}{target} · {}",
            display_text(&entry.text).replace('\n', " ↵ ")
        );
        let text = if available == 1 && entries.len() > 1 {
            let more = format!(" · … {} more", entries.len() - 1);
            format!(
                "{}{more}",
                truncate_width(&text, width.saturating_sub(display_width(&more)))
            )
        } else {
            text
        };
        lines.push(RoleLine::role(text, Role::MutedText));
    }
    if entries.len() > preview_count && available > 1 {
        lines.push(RoleLine::role(
            format!("… {} more", entries.len() - preview_count),
            Role::MutedText,
        ));
    }
    lines
}

impl State {
    fn set_foreground_shell(&mut self, active: bool) {
        self.foreground_shell = active;
        self.shell_background_chord_started_at = None;
        if active {
            self.clear_footer_selection();
        }
    }

    fn focus_lost(&mut self) {
        self.back_to_bottom_pressed = false;
        self.shell_background_chord_started_at = None;
        self.clear_footer_selection();
        self.clipboard_hint.focus_lost();
    }

    fn focus_gained(&mut self, now: Instant) {
        if self.workspace_trust.is_some() {
            return;
        }
        self.clipboard_hint.focus_gained(now);
    }

    /// Returns whether the clipboard image hint appeared or expired.
    fn poll_clipboard_hint(&mut self, now: Instant) -> bool {
        if self.workspace_trust.is_some() {
            return false;
        }
        self.clipboard_hint
            .poll(now, self.accepts_images, &self.clipboard)
    }

    fn expire_shell_background_chord(&mut self) {
        if self
            .shell_background_chord_started_at
            .is_some_and(|started_at| started_at.elapsed() > SHELL_BACKGROUND_CHORD_WINDOW)
        {
            self.shell_background_chord_started_at = None;
        }
    }

    fn record_input(&mut self, line: &str) -> Option<InputEcho> {
        let links = self
            .chat
            .map(|chat| self.image_links().in_text(chat, line))
            .unwrap_or_default();
        let echo = append_input_echo(&mut self.scrollback, line, self.display, &links);
        if self.captures_creation_prefix()
            && let Some(echo) = echo
        {
            self.creation_prefixes
                .entry(None)
                .or_default()
                .push(CreationPrefixEntry::Input {
                    echo,
                    text: line.into(),
                });
        }
        echo
    }

    fn captures_creation_prefix(&self) -> bool {
        self.interactive && self.chat.is_none() && self.overview.projection.is_none()
    }

    fn capture_creation_output(&mut self, pieces: &[ConversationPiece]) {
        if self.captures_creation_prefix() {
            self.creation_prefixes
                .entry(None)
                .or_default()
                .extend(pieces.iter().cloned().map(CreationPrefixEntry::Output));
        }
    }

    fn image_links(&self) -> ImageLinks {
        ImageLinks::new(self.image_cache.clone(), self.terminal_shows_links)
    }

    fn clear_history(&mut self) {
        let auto_scroll = self.scrollback.auto_scroll();
        self.scrollback = Scrollback::new();
        self.echoed_inputs.clear();
        self.awaiting_input_echoes.clear();
        self.rendered_execution_losses.clear();
        self.scrollback.set_auto_scroll(auto_scroll);
        self.rendered_region = None;
    }

    fn reset_chat(&mut self, chat: Uuid) {
        let preserve_editor_context = self.branch_submission_protects_editor();
        self.switch_view_chat(Some(chat));
        self.clear_history();
        if let Some(prefix) = self.creation_prefixes.get(&Some(chat)) {
            for entry in prefix {
                match entry {
                    CreationPrefixEntry::Input { text, .. } => {
                        let links = self.image_links().in_text(chat, text);
                        append_input_echo(&mut self.scrollback, text, self.display, &links);
                    }
                    CreationPrefixEntry::Output(piece) => {
                        piece.append_to(&mut self.scrollback, self.display);
                    }
                }
            }
        }
        self.reset_chat_surfaces(chat, preserve_editor_context);
    }

    fn switch_view_chat(&mut self, chat: Option<Uuid>) {
        if self.chat == chat {
            return;
        }
        self.clear_prompt_suggestion();
        self.visible_delegated = false;
        self.activity_interruptible = false;
        self.back_to_bottom_target = None;
        self.back_to_bottom_pressed = false;

        let previous_chat = self.chat;
        let previous_dialog = self.replace_dialog(input_dialog::DialogState::default());
        let suspended = self.suspended_dialog.take();
        self.dialog_cache
            .insert(previous_chat, (previous_dialog, suspended));
        self.status_area.reconcile_panel();
        let next = self.view_cache.remove(&chat).unwrap_or_else(|| {
            let mut next = ViewPresentation::default();
            next.scrollback
                .set_auto_scroll(self.scrollback.auto_scroll());
            next.history_project = self.history_project.clone();
            next.history_session = chat
                .map(|id| id.to_string())
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            next.mention_roots = self.mention_roots.clone();
            next
        });
        self.switch_editor_chat(chat);
        let previous = self.replace_view(next);
        self.view_cache.insert(previous_chat, previous);
        self.hide_diff_screen();
        self.hide_subscription_view();
        if let Some((dialog, suspended)) = self.dialog_cache.remove(&chat) {
            self.replace_dialog(dialog);
            self.suspended_dialog = suspended;
        }
        self.reconcile_resumed_task();
        self.shell_background_chord_started_at = None;
        self.rendered_region = None;
    }

    fn replace_view(&mut self, mut next: ViewPresentation) -> ViewPresentation {
        next.status_area.agents_collapsed = self.status_area.agents_collapsed;
        ViewPresentation {
            tick_list: std::mem::replace(&mut self.tick_list, next.tick_list),
            scrollback: std::mem::replace(&mut self.scrollback, next.scrollback),
            echoed_inputs: std::mem::replace(&mut self.echoed_inputs, next.echoed_inputs),
            awaiting_input_echoes: std::mem::replace(
                &mut self.awaiting_input_echoes,
                next.awaiting_input_echoes,
            ),
            footer: std::mem::replace(&mut self.footer, next.footer),
            status_area: std::mem::replace(&mut self.status_area, next.status_area),
            queued_inputs: std::mem::replace(&mut self.queued_inputs, next.queued_inputs),
            history_search: std::mem::replace(&mut self.history_search, next.history_search),
            history_project: std::mem::replace(&mut self.history_project, next.history_project),
            history_session: std::mem::replace(&mut self.history_session, next.history_session),
            mention_roots: std::mem::replace(&mut self.mention_roots, next.mention_roots),
            help_open: std::mem::replace(&mut self.help_open, next.help_open),
            help_offset: std::mem::replace(&mut self.help_offset, next.help_offset),
            last_input_edit: std::mem::replace(&mut self.last_input_edit, next.last_input_edit),
            footer_list_focused: std::mem::replace(
                &mut self.footer_list_focused,
                next.footer_list_focused,
            ),
            conversation_focused: std::mem::replace(
                &mut self.conversation_focused,
                next.conversation_focused,
            ),
            status: std::mem::replace(&mut self.status, next.status),
            execution_connection_notice: std::mem::replace(
                &mut self.execution_connection_notice,
                next.execution_connection_notice,
            ),
            rendered_execution_losses: std::mem::replace(
                &mut self.rendered_execution_losses,
                next.rendered_execution_losses,
            ),
        }
    }

    fn switch_editor_chat(&mut self, chat: Option<Uuid>) {
        if self.chat == chat {
            return;
        }
        let cached = self.editor_cache.remove(&chat);
        if self.chat.is_none() && chat.is_some() && cached.is_none() {
            self.chat = chat;
            return;
        }
        let previous = self.replace_input(cached.unwrap_or_default());
        self.editor_cache.insert(self.chat, previous);
        self.chat = chat;
    }

    fn replace_input(&mut self, next: ChatInput) -> ChatInput {
        self.retry_of = None;
        ChatInput {
            editor: std::mem::replace(&mut self.editor, next.editor),
            paste_blocks: std::mem::replace(&mut self.paste_blocks, next.paste_blocks),
            restored_contexts: std::mem::replace(
                &mut self.restored_contexts,
                next.restored_contexts,
            ),
            input_stash: std::mem::replace(&mut self.input_stash, next.input_stash),
            shell_input: std::mem::replace(&mut self.shell_input, next.shell_input),
            completion: std::mem::replace(&mut self.completion, next.completion),
            file_mentions: std::mem::replace(&mut self.file_mentions, next.file_mentions),
            owner: std::mem::replace(&mut self.input_owner, next.owner),
        }
    }

    fn replace_file_index(
        &mut self,
        owner: Uuid,
        generation: IndexGeneration,
        index: FileMentionIndex,
    ) -> bool {
        if self.input_owner == owner {
            return self.file_mentions.replace_index(generation, index)
                == IndexReplacement::Replaced;
        }
        let input = if self.overview_input.owner == owner {
            Some(&mut self.overview_input)
        } else if self
            .overview_source_input
            .as_ref()
            .is_some_and(|input| input.owner == owner)
        {
            self.overview_source_input.as_mut()
        } else {
            self.editor_cache
                .values_mut()
                .find(|input| input.owner == owner)
        };
        if let Some(input) = input {
            input.file_mentions.replace_index(generation, index);
        }
        false
    }

    fn reset_chat_surfaces(&mut self, chat: Uuid, preserve_editor_context: bool) {
        if !preserve_editor_context {
            self.restored_contexts.clear();
        }
        self.hide_diff_screen();
        self.hide_task_surface();
        self.hide_subscription_view();
        self.permissions_surface = None;
        self.clear_footer_selection();

        self.shell_background_chord_started_at = None;
        self.chat = Some(chat);
    }

    #[cfg(test)]
    fn new(interactive: bool, width: u16, height: u16) -> Self {
        Self::new_with_display(interactive, width, height, DisplayPreferences::default())
    }

    fn new_with_display(
        interactive: bool,
        width: u16,
        height: u16,
        preferences: DisplayPreferences,
    ) -> Self {
        Self {
            interactive,
            workspace_trust: None,
            plain_line_open: false,
            renderer: Renderer::Inline,
            surface: TerminalSurface::new(false),
            synchronized_output: false,
            clipboard: Clipboard::Terminal { native: false },
            external_editor: ExternalEditorLauncher::Terminal,
            rendered_screen: Vec::new(),
            rendered_rows: Vec::new(),
            held_frames: 0,
            frame_deferred: false,
            selection: None,
            display: DisplayProfile::new(preferences),
            colour_depth: ColourDepth::detect(
                interactive,
                std::env::var("COLORTERM").ok().as_deref(),
            ),
            prompt_enabled: false,
            prompt_suggestion_enabled: true,
            prompt_suggestion: None,
            prompt_suggestion_request: None,
            foreground_shell: false,
            shell_background_chord_started_at: None,
            width: usize::from(width).saturating_sub(1).max(2),
            height: usize::from(height).max(1),
            scrollback: Scrollback::new(),
            creation_prefixes: HashMap::new(),
            echoed_inputs: HashSet::new(),
            awaiting_input_echoes: HashSet::new(),
            chat: None,
            activity_interruptible: false,
            back_to_bottom_target: None,
            back_to_bottom_pressed: false,
            tick_toggle_target: None,
            tick_row_targets: Vec::new(),
            footer_bar_target: None,
            pointer_input: None,
            completion: Completion::default(),
            file_mentions: FileMentionState::default(),
            input_owner: Uuid::new_v4(),
            editor: Editor::new(),
            editor_cache: HashMap::new(),
            view_cache: HashMap::new(),
            paste_blocks: PasteBlocks::new(),
            restored_contexts: Vec::new(),
            input_stash: None,
            accepts_images: true,
            empty_paste_reads_clipboard_image: cfg!(target_os = "macos"),
            image_cache: None,
            terminal_shows_links: false,
            clipboard_hint: ClipboardImageHint::new(),
            shell_input: ShellInputState::default(),
            queued_inputs: Vec::new(),
            input_history: None,
            history_search: None,
            history_project: PathBuf::new(),
            history_session: Uuid::new_v4().to_string(),
            shell_history_cache: None,
            submitted_pasted_texts: None,
            submitted_prompt: None,
            retry_of: None,
            submitted_retry_of: None,
            mention_roots: None,
            suspended_dialog: None,
            dialog_cache: HashMap::new(),
            resumed_task_effect: None,
            dialog_cursor_visible: false,
            dialog_option_rows: Vec::new(),
            request_queue_count: 0,
            menu: None,
            menu_rule_role: None,
            picker_kind: None,
            picker_presentation: Default::default(),
            artifacts_picker: None,
            conversation_picker: None,
            menu_link: None,
            menu_review: None,
            full_review: false,
            menu_plan: false,
            menu_comment: None,
            menu_expand_options: Vec::new(),
            question_menu: None,
            request_layout: None,
            request_menu: None,
            menu_submission_pending: None,
            menu_submission_finished: None,
            branch_submission: BranchSubmission::Idle,
            menu_focused: false,
            review_focused: false,
            context_offset: 0,
            rendered_region: None,
            screen_initialized: false,
            transcript: None,
            diff_screen: None,
            diff_screen_cache: HashMap::new(),
            tick_list: TickList::default(),
            tick_list_preference: tickloop_agent::settings::TickListPreference::Auto,
            task_surface: None,
            task_surface_cache: HashMap::new(),
            subscription_surface: None,
            permissions_surface: None,
            footer: FooterState::default(),
            status_area: StatusArea::default(),
            overview: OverviewState::default(),
            overview_input: ChatInput::default(),
            overview_source_input: None,
            overview_source_view: None,
            help_open: false,
            help_offset: 0,
            last_input_edit: None,
            footer_list_focused: false,
            conversation_focused: false,
            status: String::new(),
            configured_status_lines: None,
            background_status_notice: None,
            connection_notice: None,
            execution_connection_notice: None,
            execution_losses: HashMap::new(),
            last_connection_diagnostics: HashMap::new(),
            rendered_execution_losses: HashSet::new(),
            creation_notice: None,
            visible_delegated: false,
            status_held: false,
            exit_confirmation: None,
            escape_confirmation: None,
        }
    }

    /// Pointer input scrolls the conversation, selects text and activates the return control.
    /// Other events are captured so the terminal does not translate the wheel into arrow keys.
    fn pointer(&mut self, event: MouseEvent, output: &mut impl Write) -> bool {
        let height = self.input_dialog_height();
        let start = self.height.saturating_sub(height);
        let body_height = self.input_dialog_body_height();
        let rule_rows = height - body_height;
        if let Some(dialog) = &mut self.workspace_trust {
            if event.kind == MouseEventKind::Down(MouseButton::Left)
                && usize::from(event.row) >= start + rule_rows
                && usize::from(event.row) < self.height
            {
                self.pointer_input = dialog
                    .click(
                        usize::from(event.row) - start - rule_rows,
                        self.width,
                        body_height,
                        self.display,
                    )
                    .map(|decision| match decision {
                        TrustDecision::Accept => InputEvent::Choice {
                            menu_id: dialog.id,
                            index: TRUST_ACCEPT_CHOICE_INDEX,
                            comment: None,
                        },
                        TrustDecision::Cancel => InputEvent::Cancel { menu_id: dialog.id },
                    });
            }
            return true;
        }
        self.pointer_input = None;
        if self.menu_submission_is_pending()
            || self.branch_submission_owns_input()
            || self.request_layout.is_some() && self.menu.is_none()
        {
            return false;
        }
        let at = Cell {
            row: usize::from(event.row),
            column: usize::from(event.column),
        };
        if self.full_review {
            match event.kind {
                MouseEventKind::ScrollUp => self.navigate_menu_review(DiffNavigationAction::Up),
                MouseEventKind::ScrollDown => self.navigate_menu_review(DiffNavigationAction::Down),
                _ => {}
            }
            return true;
        }
        if self.input_dialog_visible() {
            let start = self.height.saturating_sub(self.input_dialog_height());
            if at.row < start
                && matches!(
                    event.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                )
            {
                return false;
            }
            if at.row >= start {
                let row = at.row - start;
                let rule_rows = self.input_dialog_height() - self.input_dialog_body_height();
                if row < rule_rows {
                    return true;
                }
                let body_row = row - rule_rows;
                if self.menu.is_none()
                    && (self.artifacts_picker.is_some() || self.conversation_picker.is_some())
                {
                    match event.kind {
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                            let code = if event.kind == MouseEventKind::ScrollUp {
                                KeyCode::Up
                            } else {
                                KeyCode::Down
                            };
                            self.pointer_input = self.key(KeyEvent::new(code, KeyModifiers::NONE));
                        }
                        MouseEventKind::Down(MouseButton::Left) => {
                            let selected = if let Some(picker) = &mut self.artifacts_picker {
                                picker.select_at_row(body_row)
                            } else {
                                self.conversation_picker
                                    .as_mut()
                                    .is_some_and(|picker| picker.select_at_row(body_row))
                            };
                            if selected {
                                self.pointer_input =
                                    self.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                            }
                        }
                        _ => {}
                    }
                    return true;
                }
                if self.menu.is_none()
                    && (self.permissions_surface.is_some()
                        || self.subscription_surface.is_some()
                        || self.task_surface.is_some())
                {
                    match event.kind {
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                            let code = if event.kind == MouseEventKind::ScrollUp {
                                KeyCode::Up
                            } else {
                                KeyCode::Down
                            };
                            self.pointer_input = self.key(KeyEvent::new(code, KeyModifiers::NONE));
                        }
                        MouseEventKind::Down(MouseButton::Left) => {
                            let capacity = self.input_dialog_body_height();
                            if let Some(surface) = &mut self.permissions_surface {
                                let effect = surface.click(body_row, self.width, capacity);
                                if let PermissionsEffect::Close { .. } = effect {
                                    self.permissions_surface = None;
                                }
                                if !matches!(effect, PermissionsEffect::None) {
                                    self.pointer_input = Some(InputEvent::Permissions(effect));
                                }
                            } else {
                                let viewport =
                                    task_surface_content_viewport(self.width, self.height);
                                let capacity = self.input_dialog_body_height();
                                let selected = if let Some(surface) = &mut self.task_surface {
                                    body_row
                                        .checked_sub(1)
                                        .is_some_and(|row| surface.select_at_row(row, viewport))
                                } else {
                                    body_row.checked_sub(1).is_some_and(|row| {
                                        self.subscription_surface.as_mut().unwrap().select_at_row(
                                            row,
                                            capacity.saturating_sub(2).max(1),
                                            self.display,
                                        )
                                    })
                                };
                                if selected {
                                    self.pointer_input =
                                        self.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                                }
                            }
                        }
                        _ => {}
                    }
                    return true;
                }
                match event.kind {
                    MouseEventKind::ScrollUp => {
                        self.context_offset = self.context_offset.saturating_sub(1)
                    }
                    MouseEventKind::ScrollDown => {
                        self.context_offset = self.context_offset.saturating_add(1)
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some((_, index)) = self
                            .dialog_option_rows
                            .iter()
                            .rev()
                            .find(|(rows, _)| rows.contains(&(at.row - start)))
                            .cloned()
                        {
                            if let Some(menu) = &mut self.menu {
                                menu.selected = index;
                            }
                            self.pointer_input =
                                self.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                            return true;
                        }
                    }
                    _ => {}
                }
            }
            if !matches!(
                event.kind,
                MouseEventKind::Down(MouseButton::Left)
                    | MouseEventKind::Drag(MouseButton::Left)
                    | MouseEventKind::Up(MouseButton::Left)
            ) {
                return true;
            }
        }
        let at_return_control = self
            .back_to_bottom_target
            .as_ref()
            .is_some_and(|target| target.row == at.row && target.columns.contains(&at.column));
        if self.back_to_bottom_pressed {
            match event.kind {
                MouseEventKind::Drag(MouseButton::Left) => return true,
                MouseEventKind::Up(MouseButton::Left) => {
                    self.back_to_bottom_pressed = false;
                    if at_return_control {
                        self.scroll_conversation(ScrollAction::Last);
                    }
                    return true;
                }
                _ => {}
            }
        }
        match event.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_conversation(ScrollAction::LinesUp(renderer::WHEEL_SCROLL_LINES))
            }
            MouseEventKind::ScrollDown => {
                self.scroll_conversation(ScrollAction::LinesDown(renderer::WHEEL_SCROLL_LINES))
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if !self.input_dialog_visible()
                    && self.tick_list_visible()
                    && self.tick_list.confirmation.is_none()
                    && !self.menu_submission_is_pending()
                    && !self.branch_submission_owns_input()
                    && let Some(id) = self
                        .tick_row_targets
                        .iter()
                        .find(|target| {
                            Some(target.chat) == self.chat
                                && target.row == at.row
                                && target.columns.contains(&at.column)
                        })
                        .map(|target| target.id.clone())
                {
                    self.clear_selection();
                    self.clear_footer_selection();
                    self.conversation_focused = false;
                    self.tick_list.focus(self.tick_list.overlay, None);
                    self.tick_list.select(&id);
                    self.pointer_input =
                        self.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                    return true;
                }
                if self.tick_toggle_target.as_ref().is_some_and(|target| {
                    target.row == at.row && target.columns.contains(&at.column)
                }) {
                    self.pointer_input = Some(InputEvent::ToggleTickList {
                        show: !self.tick_list_visible(),
                    });
                    return true;
                }
                if at_return_control {
                    self.clear_selection();
                    self.back_to_bottom_pressed = true;
                    return true;
                }
                let category = self.footer_bar_target.as_ref().and_then(|target| {
                    (target.row == at.row)
                        .then(|| {
                            let column = at.column.checked_sub(target.inset)?;
                            target.categories.iter().find_map(|(columns, category)| {
                                columns.contains(&column).then_some(*category)
                            })
                        })
                        .flatten()
                });
                if let Some(panel) = category {
                    self.clear_selection();
                    if self.task_surface.is_some() {
                        if self.status_area.selected_panel() == Some(panel) {
                            return true;
                        }

                        self.hide_task_surface();
                        self.status_area.reconcile_panel();
                    }
                    self.footer.select(panel);
                    self.status_area.toggle_agents();
                    self.footer_list_focused = false;
                    return true;
                }
                self.back_to_bottom_pressed = false;
                let had_selection = self.selection.is_some();
                self.selection = Some(Selection::started_at(at));
                had_selection
            }
            MouseEventKind::Drag(MouseButton::Left) => match &mut self.selection {
                Some(selection) => {
                    selection.drag_to(at);
                    true
                }
                None => false,
            },
            MouseEventKind::Up(MouseButton::Left) => {
                // The release carries its own position, so a terminal that drops
                // the intermediate motion reports still selects the dragged span.
                if let Some(selection) = &mut self.selection {
                    selection.drag_to(at);
                }
                self.copy_selection(output)
            }
            _ => false,
        }
    }

    fn scroll_conversation(&mut self, action: ScrollAction) -> bool {
        if self.transcript.is_some()
            || self.task_surface.is_some()
            || self.subscription_surface.is_some()
            || self.diff_screen.is_some()
        {
            return false;
        }
        self.clear_selection();
        self.scrollback.scroll(action);
        if action == ScrollAction::Last {
            self.conversation_focused = false;
        }
        true
    }

    /// A click that never became a drag selects nothing, so it must not replace
    /// what the user already has on the clipboard.
    fn copy_selection(&mut self, output: &mut impl Write) -> bool {
        let Some(selection) = self.selection else {
            return false;
        };
        if selection.is_empty() {
            self.selection = None;
            return false;
        }
        let text = selection.text(&self.rendered_screen);
        if text.is_empty() {
            self.selection = None;
            return true;
        }
        if let Err(error) = self.clipboard.write(&text, output) {
            self.status = format!("Cannot copy the selection: {error}");
        }
        true
    }

    fn clear_selection(&mut self) -> bool {
        self.back_to_bottom_pressed = false;
        self.selection.take().is_some()
    }

    /// Takes the terminal's new size. On its way there the terminal may have
    /// reflowed or cut the rows, even when it ends at the size last drawn, so the
    /// next frame is drawn whole.
    fn resize(&mut self, width: u16, height: u16) {
        self.width = usize::from(width).saturating_sub(1).max(2);
        self.height = usize::from(height).max(1);
        self.rendered_region = None;
        self.clear_footer_selection_if_hidden();
    }

    /// Discards what the console believes is on screen and draws a fresh full
    /// frame. Used whenever the drawing surface changed underneath it.
    fn repaint_whole_screen(&mut self, output: &mut impl Write) {
        self.screen_initialized = false;
        self.rendered_region = None;
        let _ = self.draw(output);
    }

    fn draw(&mut self, output: &mut impl Write) -> io::Result<()> {
        if !self.interactive {
            return Ok(());
        }
        if self.input_dialog_visible() && self.prompt_suggestion_request.is_some() {
            self.clear_prompt_suggestion();
        }
        if self.held_frames > 0 {
            self.frame_deferred = true;
            return Ok(());
        }
        if !self.synchronized_output {
            return self.draw_frame(output);
        }
        renderer::write_synchronized(output, |output| self.draw_frame(output))
    }

    fn tick_list_visible(&self) -> bool {
        self.chat.is_some()
            && !self.tick_list.entries().is_empty()
            && self.overview.projection.is_none()
            && self.workspace_trust.is_none()
            && self.history_search.is_none()
            && (self.input_dialog_visible()
                || !self.help_open
                    && self.menu.is_none()
                    && self.conversation_picker.is_none()
                    && self.artifacts_picker.is_none()
                    && self.task_surface.is_none()
                    && self.permissions_surface.is_none()
                    && self.subscription_surface.is_none())
            && self.transcript.is_none()
            && self.diff_screen.is_none()
            && !self.full_review
            && (self.tick_list.overlay
                || self.width + 1 >= TICK_LIST_AUTO_MIN_COLUMNS
                    && self.tick_list_preference
                        == tickloop_agent::settings::TickListPreference::Auto)
    }

    #[cfg(test)]
    fn update_tick_entries(&mut self, chat: Uuid, group: TickGroup, entries: Vec<TickListEntry>) {
        if self.chat == Some(chat) {
            self.tick_list.replace(group, entries);
        } else {
            self.view_cache
                .entry(Some(chat))
                .or_default()
                .tick_list
                .replace(group, entries);
        }
    }

    fn tick_list_for_chat_mut(&mut self, chat: Uuid) -> &mut TickList {
        if self.chat == Some(chat) {
            &mut self.tick_list
        } else {
            &mut self.view_cache.entry(Some(chat)).or_default().tick_list
        }
    }

    fn content_columns(&self) -> usize {
        if self.tick_list_visible() && !self.tick_list.overlay {
            self.width.saturating_sub(TICK_LIST_COLUMNS).max(2)
        } else {
            self.width
        }
    }

    fn draw_frame(&mut self, output: &mut impl Write) -> io::Result<()> {
        self.tick_row_targets.clear();
        if self.full_review {
            let lines = self.full_review_frame();
            let rows = lines
                .iter()
                .map(|line| {
                    line.resolve(self.display.theme(), self.width)
                        .render(self.colour_depth)
                })
                .collect();
            self.initialize_screen(output)?;
            write!(output, "{RESET_SCROLL_REGION}")?;
            self.paint_rows(output, RenderedView::DiffScreen, rows, 0)?;
            self.rendered_screen = lines.iter().map(RoleLine::text).collect();
            queue!(output, Hide)?;
            return output.flush();
        }
        self.back_to_bottom_target = None;
        self.footer_bar_target = None;
        self.tick_toggle_target = None;
        if self.overview.projection.is_some() {
            let (bottom, row, column) = self.styled_frame();
            let top_height = self.height.saturating_sub(bottom.len());
            let mut lines = self.overview.lines(
                self.width,
                top_height,
                self.height,
                self.width + 1,
                self.help_open,
                self.display,
            );
            lines.extend(bottom);
            let rows: Vec<String> = lines
                .iter()
                .map(|line| {
                    line.resolve(self.display.theme(), self.width)
                        .render(self.colour_depth)
                })
                .collect();
            write!(output, "{RESET_SCROLL_REGION}")?;
            self.paint_rows(output, RenderedView::Overview, rows, 0)?;
            if self.overview.input_focused {
                queue!(
                    output,
                    MoveTo(column as u16, (top_height + row) as u16),
                    Show
                )?;
            } else {
                queue!(output, Hide)?;
            }
            self.rendered_screen = lines.iter().map(RoleLine::text).collect();
            return output.flush();
        }
        if let Some((_, transcript)) = &mut self.transcript {
            let mut frame = transcript.frame(self.width, self.height);
            if let Some(footer) = frame.lines.last_mut() {
                *footer = self.display.navigation_hint(footer);
            }
            if self.menu.is_some()
                && let Some(title) = frame.lines.first_mut()
            {
                *title = truncate_width(
                    &format!("{title} · Menu waiting in conversation"),
                    self.width,
                );
            }
            let rows: Vec<String> = frame
                .lines
                .iter()
                .enumerate()
                .map(|(row, line)| {
                    let links = frame
                        .links
                        .iter()
                        .filter(|link| link.line == row)
                        .map(|link| (link.range.clone(), link.link.clone()))
                        .collect::<Vec<_>>();
                    hyperlink::apply(line, &links)
                })
                .collect();
            write!(output, "{RESET_SCROLL_REGION}")?;
            self.paint_rows(output, RenderedView::Transcript, rows, 0)?;
            if let Some((column, row)) = frame.cursor {
                queue!(output, MoveTo(column as u16, row as u16), Show)?;
            } else {
                queue!(output, Hide)?;
            }
            self.rendered_screen = frame.lines;
            return output.flush();
        }
        if let Some(diff_screen) = self.diff_screen.as_mut().filter(|_| self.menu.is_none()) {
            let mut lines = diff_screen_lines(diff_screen, self.width, self.height, self.display);
            for line in &mut lines {
                *line = truncate_width(line, self.width);
            }
            write!(output, "{RESET_SCROLL_REGION}")?;
            self.paint_rows(output, RenderedView::DiffScreen, lines.clone(), 0)?;
            queue!(output, Hide)?;
            self.rendered_screen = lines;
            return output.flush();
        }
        let (lines, cursor_row, cursor_column) = self.styled_frame();
        let start = self.height - lines.len();
        let content_columns = self.content_columns();
        let sticky_prompt = (start >= 2 && self.workspace_trust.is_none())
            .then(|| {
                let text = self.scrollback.sticky_input_text(
                    self.display.glyph(Glyph::Prompt),
                    content_columns,
                    start,
                )?;
                let text = format!(
                    "{} {}",
                    self.display.glyph(Glyph::Prompt),
                    super::conversation_picker::compact_line(&display_single_line(&text))
                );
                Some(RoleLine::role(
                    super::conversation_picker::ellipsize(&text, content_columns),
                    Role::MutedText,
                ))
            })
            .flatten();
        let sticky_rows = usize::from(sticky_prompt.is_some());
        let body_height = start.saturating_sub(sticky_rows);
        let body = self.scrollback.frame_with_motion(
            content_columns,
            body_height,
            self.display.preferences().prefers_reduced_motion,
        );
        self.initialize_screen(output)?;
        write!(output, "{RESET_SCROLL_REGION}")?;
        let padding = body_height.saturating_sub(body.lines.len());
        let side_columns = TICK_LIST_COLUMNS.min(self.width);
        let tick_lines = self.tick_list_visible().then(|| {
            if let Some(detail) = &mut self.tick_list.detail {
                return detail
                    .lines(
                        side_columns.saturating_sub(2),
                        start,
                        crate::private_fs::unix_time_milliseconds(),
                    )
                    .into_iter()
                    .map(|line| line.line)
                    .collect();
            }
            let frame = self.tick_list.frame(
                side_columns.saturating_sub(2),
                start,
                self.display,
                crate::private_fs::unix_time_milliseconds(),
            );
            if let Some(chat) = self.chat {
                let columns = self.width.saturating_sub(side_columns) + 1..self.width;
                self.tick_row_targets = frame
                    .targets
                    .into_iter()
                    .map(|(row, id)| TickRowTarget {
                        chat,
                        row,
                        columns: columns.clone(),
                        id,
                    })
                    .collect();
            }
            frame.lines
        });
        let rows: Vec<String> = (0..start)
            .map(|row| {
                let pinned = if row == 0 {
                    sticky_prompt.as_ref()
                } else {
                    None
                };
                let pinned_text = pinned.map(RoleLine::text);
                let line_index = row
                    .checked_sub(sticky_rows)
                    .and_then(|row| row.checked_sub(padding));
                let line = pinned_text
                    .as_deref()
                    .or_else(|| {
                        line_index
                            .and_then(|index| body.lines.get(index))
                            .map(String::as_str)
                    })
                    .unwrap_or_default();
                if let Some(tick_lines) = &tick_lines {
                    let left_columns = self.width.saturating_sub(side_columns);
                    let mut left = pinned
                        .cloned()
                        .or_else(|| line_index.and_then(|index| body.styled.get(index)).cloned())
                        .unwrap_or_else(|| RoleLine::plain(line));
                    left.truncate(left_columns);
                    let left =
                        self.with_selection(row, left.resolve(self.display.theme(), left_columns));
                    let side = tick_lines.get(row).cloned().unwrap_or_default();
                    let right = side.resolve(self.display.theme(), side_columns.saturating_sub(2));
                    format!(
                        "{}\x1b[0m│ {}",
                        render_padded_line(&left, left_columns, self.colour_depth),
                        render_padded_line(
                            &right,
                            side_columns.saturating_sub(2),
                            self.colour_depth
                        )
                    )
                } else {
                    let resolved = pinned
                        .or_else(|| line_index.and_then(|index| body.styled.get(index)))
                        .map_or_else(
                            || StyledLine::plain(line),
                            |styled| styled.resolve(self.display.theme(), content_columns),
                        );
                    self.with_selection(row, resolved).render(self.colour_depth)
                }
            })
            .chain(lines.iter().enumerate().map(|(index, line)| {
                self.with_selection(
                    start + index,
                    line.resolve(self.display.theme(), self.width),
                )
                .render(self.colour_depth)
            }))
            .collect();
        let scroll_rows = if sticky_prompt.is_none() && !self.tick_list_visible() {
            start
        } else {
            0
        };
        self.paint_rows(
            output,
            RenderedView::Conversation { body_rows: start },
            rows,
            scroll_rows,
        )?;
        self.rendered_screen = (0..start)
            .map(|row| {
                let line = if row == 0
                    && let Some(prompt) = &sticky_prompt
                {
                    prompt.text()
                } else {
                    row.checked_sub(sticky_rows)
                        .and_then(|row| row.checked_sub(padding))
                        .and_then(|index| body.lines.get(index))
                        .cloned()
                        .unwrap_or_default()
                };
                if let Some(tick_lines) = &tick_lines {
                    let left_columns = self.width.saturating_sub(side_columns);
                    let left = truncate_width(&line, left_columns);
                    format!(
                        "{left}{}│ {}",
                        " ".repeat(left_columns.saturating_sub(display_width(&left))),
                        tick_lines.get(row).map(RoleLine::text).unwrap_or_default()
                    )
                } else {
                    line
                }
            })
            .chain(lines.iter().map(RoleLine::text))
            .collect();
        queue!(
            output,
            MoveTo(cursor_column as u16, (start + cursor_row) as u16)
        )?;
        if self.workspace_trust.is_some() {
            if self.dialog_cursor_visible {
                queue!(output, Show)?;
            } else {
                queue!(output, Hide)?;
            }
        } else if ((self.menu_focused || self.review_focused) && !self.menu_comment_is_editing())
            || self.footer.selected().is_some()
            || self.help_open
            || self.task_surface.is_some()
            || self.permissions_surface.is_some()
            || self.subscription_surface.is_some()
            || (self.artifacts_picker.is_some() || self.conversation_picker.is_some())
                && !self.dialog_cursor_visible
            || self.conversation_focused
            || self.tick_list.focused && self.tick_list_visible() && !self.input_dialog_visible()
        {
            queue!(output, Hide)?;
        } else {
            queue!(output, Show)?;
        }
        output.flush()
    }

    fn initialize_screen(&mut self, output: &mut impl Write) -> io::Result<()> {
        if !self.screen_initialized {
            if !self.renderer.uses_alternate_screen() {
                write!(output, "{RESET_SCROLL_REGION}")?;
                queue!(output, MoveTo(0, (self.height - 1) as u16))?;
                for _ in 0..self.height {
                    write!(output, "\r\n")?;
                }
            }
            self.screen_initialized = true;
        }
        Ok(())
    }

    /// Brings the screen to `rows`, one rendered row per screen row from the top.
    /// After a frame of the same view at the same size only rows whose output
    /// changed are written. When the history region also kept its size, the terminal can first
    /// shift its top `scroll_rows` rows if that leaves fewer to write. Other views or sizes clear
    /// the screen.
    fn paint_rows(
        &mut self,
        output: &mut impl Write,
        view: RenderedView,
        rows: Vec<String>,
        scroll_rows: usize,
    ) -> io::Result<()> {
        let region = RenderedRegion {
            width: self.width,
            height: self.height,
            view,
        };
        let previous_region = self.rendered_region.filter(|previous| {
            previous.same_view_and_size(region) && self.rendered_rows.len() == rows.len()
        });
        match previous_region {
            Some(previous_region) => {
                let mut previous = std::mem::take(&mut self.rendered_rows);
                // A resized conversation region must not shift editor or footer rows with history.
                let scroll_rows = if previous_region == region {
                    scroll_rows
                } else {
                    0
                };
                if let Some(shift) = body_scroll(&previous[..scroll_rows], &rows[..scroll_rows]) {
                    // The terminal shifts the conversation region itself, so a
                    // line appended to a full conversation writes one row
                    // instead of the whole region.
                    write!(output, "\x1b[1;{scroll_rows}r")?;
                    queue!(output, ScrollUp(shift as u16))?;
                    write!(output, "{RESET_SCROLL_REGION}")?;
                    previous[..scroll_rows].rotate_left(shift);
                    for vacated in &mut previous[scroll_rows - shift..scroll_rows] {
                        vacated.clear();
                    }
                }
                for (row, rendered) in rows.iter().enumerate() {
                    if previous[row] == *rendered {
                        continue;
                    }
                    queue!(output, MoveTo(0, row as u16), Clear(ClearType::CurrentLine))?;
                    write!(output, "{rendered}")?;
                }
            }
            None => {
                queue!(output, MoveTo(0, 0), Clear(ClearType::All))?;
                for (row, rendered) in rows.iter().enumerate() {
                    if rendered.is_empty() {
                        continue;
                    }
                    queue!(output, MoveTo(0, row as u16))?;
                    write!(output, "{rendered}")?;
                }
            }
        }
        self.rendered_rows = rows;
        self.rendered_region = Some(region);
        Ok(())
    }

    /// Paints the selection onto one screen row in the theme's readable
    /// selection colours; text outside the selected columns is untouched.
    fn with_selection(&self, screen_row: usize, line: StyledLine) -> StyledLine {
        let Some(selection) = self.selection.filter(|selection| !selection.is_empty()) else {
            return line;
        };
        let row_width = line.spans.iter().map(|span| line_width(&span.text)).sum();
        let Some((first, last)) = selection.columns_on(screen_row, row_width) else {
            return line;
        };
        let theme = self.display.theme();
        let mut highlighted = StyledLine {
            link: line.link.clone(),
            ..Default::default()
        };
        let mut column = 0;
        for span in line.spans {
            let width = line_width(&span.text);
            let (before, inside, after) = split_columns(
                &span.text,
                first.saturating_sub(column),
                last.saturating_sub(column),
            );
            highlighted.push(Span::styled(before, span.style).linked(span.link.clone()));
            highlighted
                .push(Span::styled(inside, theme.selected(span.style)).linked(span.link.clone()));
            highlighted.push(Span::styled(after, span.style).linked(span.link));
            column += width;
        }
        highlighted
    }

    #[cfg(test)]
    fn frame(&mut self) -> (Vec<String>, usize, usize) {
        let (lines, row, column) = self.styled_frame();
        (lines.iter().map(RoleLine::text).collect(), row, column)
    }

    /// Builds the fixed rows below the conversation, each row carrying the
    /// roles its parts play so the writer can apply the active theme.
    fn styled_frame(&mut self) -> (Vec<RoleLine>, usize, usize) {
        let todo_columns = self.content_columns();
        let todo_lines = self.status_area.todo_lines(
            todo_columns,
            self.height,
            self.display,
            self.scrollback.activity_line().is_some(),
            Instant::now(),
        );
        self.scrollback.set_activity_details(todo_lines);
        self.clear_footer_selection_if_hidden();
        if self.input_dialog_visible() {
            return self.input_dialog_frame();
        }
        if self.history_search.is_some() {
            return self.history_search_frame();
        }
        self.normal_styled_frame()
    }

    fn menu_group_header(&self, index: usize, window_start: usize) -> Option<&str> {
        self.picker_presentation
            .headers
            .iter()
            .rev()
            .find(|(first, _)| *first <= index)
            .filter(|(first, _)| *first == index || index == window_start)
            .map(|(_, header)| header.as_str())
    }

    fn request_styled_frame(&mut self, capacity: usize) -> (Vec<RoleLine>, usize, usize) {
        if !self.menu_is_visible() {
            let mut lines = vec![RoleLine::role(
                "Resize terminal to review · Esc cancel",
                Role::MutedText,
            )];
            lines.resize(capacity, RoleLine::default());
            for line in &mut lines {
                line.truncate(self.width);
            }
            return (lines, 0, 0);
        }
        let menu = self.menu.as_ref().expect("request menu is visible");
        let selected = menu.selected.min(menu.options.len().saturating_sub(1));
        let title_rows = usize::from(capacity > 1);
        let hint_rows = usize::from(capacity > 2);
        let input_field = self.input_menu_active();
        let comment = self.menu_comment.as_ref().filter(|comment| {
            comment.editing
                || input_field
                    && comment
                        .values
                        .get(&selected)
                        .is_some_and(|value| !value.is_empty())
        });
        let editing = comment.is_some_and(|comment| comment.editing);
        let (comment_lines, comment_cursor) = comment.map_or_else(
            || (Vec::new(), (0, 0)),
            |comment| {
                let value = comment
                    .values
                    .get(&selected)
                    .map(String::as_str)
                    .unwrap_or_default();
                let prefix = format!(
                    "{}{}: ",
                    if self.question_menu.is_some() {
                        ""
                    } else {
                        "  "
                    },
                    comment.label
                );
                let text = if comment.secret {
                    "*".repeat(value.chars().count())
                } else {
                    display_text(value)
                };
                let before_cursor = if comment.secret {
                    "*".repeat(value[..comment.cursor.min(value.len())].chars().count())
                } else {
                    display_text(&value[..comment.cursor.min(value.len())])
                };
                let cursor_lines = wrap_text(&format!("{prefix}{before_cursor}"), self.width);
                let column = display_width(cursor_lines.last().expect("the field has a line"));
                let cursor = if text
                    .chars()
                    .nth(before_cursor.chars().count())
                    .is_some_and(|next| column + next.width().unwrap_or(0) > self.width)
                {
                    (cursor_lines.len(), 0)
                } else {
                    (
                        cursor_lines.len() - 1,
                        column.min(self.width.saturating_sub(1)),
                    )
                };
                (wrap_text(&format!("{prefix}{text}"), self.width), cursor)
            },
        );
        if editing && capacity <= 3 {
            let start = comment_cursor.0.saturating_sub(capacity - 1);
            let mut lines = comment_lines
                .into_iter()
                .skip(start)
                .take(capacity)
                .map(RoleLine::plain)
                .collect::<Vec<_>>();
            lines.resize(capacity, RoleLine::default());
            for line in &mut lines {
                line.truncate(self.width);
            }
            return (lines, comment_cursor.0 - start, comment_cursor.1);
        }
        let comment_rows = comment_lines
            .len()
            .min(capacity.saturating_sub(title_rows + hint_rows + 1));
        let picker = self.picker_kind.is_some();
        let compact = picker && self.width < COMPACT_PICKER_COLUMNS;
        let minimum_option_rows =
            1 + usize::from(
                compact
                    && self
                        .picker_presentation
                        .compact_rows
                        .get(selected)
                        .is_some(),
            ) + usize::from(picker && !self.picker_presentation.headers.is_empty());
        let context_reserve = usize::from(!menu.context.is_empty() || self.menu_review.is_some())
            * capacity
                .saturating_sub(title_rows + hint_rows + comment_rows + minimum_option_rows)
                .min(2);
        let option_capacity =
            capacity.saturating_sub(title_rows + hint_rows + comment_rows + context_reserve);
        let options = menu
            .options
            .iter()
            .enumerate()
            .map(|(index, option)| {
                let marker = if index == selected {
                    self.display.glyph(Glyph::Prompt)
                } else {
                    " "
                };
                let ordinal =
                    if !picker && self.request_layout.is_none() && self.question_menu.is_none() {
                        format!("{}. ", index + 1)
                    } else {
                        String::new()
                    };
                let row = compact
                    .then(|| self.picker_presentation.compact_rows.get(index))
                    .flatten();
                let text = format!(
                    "{marker} {ordinal}{}",
                    display_text(row.map_or(option.as_str(), |row| row.label.as_str()))
                );
                if picker {
                    let mut lines = vec![truncate_width(&text, self.width)];
                    if let Some(row) = row {
                        lines.push(truncate_width(&format!("    {}", row.detail), self.width));
                    }
                    lines
                } else {
                    wrap_text(&text, self.width)
                }
            })
            .collect::<Vec<_>>();
        let show_headers = picker
            && !self.picker_presentation.headers.is_empty()
            && option_capacity > options[selected].len().min(option_capacity);
        let group_rows = |start: usize, end: usize| {
            if show_headers {
                (start..end)
                    .filter(|index| self.menu_group_header(*index, start).is_some())
                    .count()
            } else {
                0
            }
        };
        let mut option_rows = options[selected]
            .len()
            .min(option_capacity.saturating_sub(usize::from(show_headers)));
        let mut option_start = selected;
        let mut option_end = selected + 1;
        while option_start > 0
            && option_rows
                + options[option_start - 1].len()
                + group_rows(option_start - 1, option_end)
                <= option_capacity
        {
            option_start -= 1;
            option_rows += options[option_start].len();
        }
        while option_end < options.len()
            && option_rows + options[option_end].len() + group_rows(option_start, option_end + 1)
                <= option_capacity
        {
            option_rows += options[option_end].len();
            option_end += 1;
        }
        option_rows += group_rows(option_start, option_end);
        let context_capacity =
            capacity.saturating_sub(title_rows + hint_rows + option_rows + comment_rows);
        let mut lines = Vec::new();
        if capacity > 1 {
            let count = if self.request_layout.is_some() && self.request_queue_count > 1 {
                format!(" (1/{})", self.request_queue_count)
            } else {
                String::new()
            };
            let title = super::conversation_picker::ellipsize(
                &display_single_line(&menu.title),
                self.width.saturating_sub(display_width(&count)),
            );
            lines.push(RoleLine::role(
                truncate_width(&format!("{title}{count}"), self.width),
                Role::DialogTitle,
            ));
        }
        if context_capacity > 0 {
            if let Some(review) = &mut self.menu_review {
                let viewport = DiffViewport {
                    width: self.width.saturating_sub(2).max(1),
                    height: context_capacity,
                };
                let frame = match &review.review {
                    Ok(source) => render_diff(source, &review.navigation, viewport),
                    Err(error) => {
                        render_error("Review unavailable", error, &review.navigation, viewport)
                    }
                };
                review.navigation = frame.navigation;
                review.viewport = viewport;
                lines.extend(frame.lines.into_iter().take(context_capacity).map(|line| {
                    let text = format!("  {}", line.text);
                    match line.kind {
                        DiffLineKind::Added => RoleLine::plain(text).filled(Role::DiffAdded),
                        DiffLineKind::Removed => RoleLine::plain(text).filled(Role::DiffRemoved),
                        DiffLineKind::Error => RoleLine::role(text, Role::Error),
                        _ => RoleLine::role(text, Role::MutedText),
                    }
                }));
            } else {
                let context = menu
                    .context
                    .iter()
                    .flat_map(|line| {
                        let link = self
                            .menu_link
                            .as_ref()
                            .filter(|(id, address)| *id == menu.id && address == line)
                            .map(|(_, address)| address.clone());
                        wrap_text(&display_text(line), self.width.saturating_sub(2).max(1))
                            .into_iter()
                            .map(move |line| (line, link.clone()))
                    })
                    .collect::<Vec<_>>();
                self.context_offset = self
                    .context_offset
                    .min(context.len().saturating_sub(context_capacity));
                lines.extend(
                    context
                        .into_iter()
                        .skip(self.context_offset)
                        .take(context_capacity)
                        .map(|(line, link)| {
                            let text = format!("  {line}");
                            if let Some(address) = link {
                                RoleLine::linked(text, Role::Accent, &address)
                            } else {
                                RoleLine::role(text, Role::MutedText)
                            }
                        }),
                );
            }
        }
        let mut selected_row = lines.len();
        for (index, option_lines) in options
            .iter()
            .enumerate()
            .skip(option_start)
            .take(option_end - option_start)
        {
            if show_headers
                && let Some(header) = self
                    .menu_group_header(index, option_start)
                    .map(str::to_owned)
            {
                lines.push(RoleLine::role(
                    truncate_width(&header, self.width),
                    Role::MutedText,
                ));
            }
            let option_row = lines.len();
            if index == selected {
                selected_row = lines.len();
            }
            let remaining = capacity.saturating_sub(hint_rows + comment_rows + lines.len());
            lines.extend(option_lines.iter().take(remaining).map(|text| {
                let line = RoleLine::role(text.clone(), Role::Text);
                if index == selected {
                    line.filled(Role::Selection)
                } else {
                    line
                }
            }));
            self.dialog_option_rows
                .push((option_row..lines.len(), index));
        }
        let mut cursor = (selected_row, 0);
        if comment_rows > 0 {
            let start = comment_cursor.0.saturating_sub(comment_rows - 1);
            if editing {
                cursor = (lines.len() + comment_cursor.0 - start, comment_cursor.1);
            }
            lines.extend(
                comment_lines
                    .into_iter()
                    .skip(start)
                    .take(comment_rows)
                    .map(RoleLine::plain),
            );
        }
        lines.resize(capacity.saturating_sub(hint_rows), RoleLine::default());
        if hint_rows > 0 {
            let hint = if input_field && !editing {
                format!(
                    "Enter confirm · Esc cancel · Tab edit {}",
                    self.menu_comment.as_ref().unwrap().label
                )
            } else if let Some(comment) = comment {
                if self.input_menu_active() {
                    format!(
                        "{} · Enter save · Ctrl+G external editor · Esc close field",
                        comment.label
                    )
                } else if self.menu_link.is_some() {
                    format!("{} · Enter submit · Esc close field", comment.label)
                } else if self.question_menu.is_some() {
                    "Type your answer · Enter submit · Esc close field".into()
                } else {
                    if comment.label == "Guidance" {
                        "add context (optional) · Enter submit · Tab close · Esc close".into()
                    } else {
                        "Type a comment · Enter submit · Tab close · Esc close".into()
                    }
                }
            } else if picker && !self.review_focused {
                self.picker_hint().unwrap_or_default().into()
            } else if self.review_focused {
                "PageUp/Down review · Tab actions · Esc deny".into()
            } else if let Some(question) = &self.question_menu {
                question_menu_hint(question, selected)
            } else if menu.confirmation {
                if self.selected_menu_can_expand() {
                    "Enter/y exact · Tab prefixes · n/Esc deny".into()
                } else if self.selected_menu_accepts_comment() {
                    "Enter/y · n/Esc deny · Tab comment".into()
                } else {
                    "Enter/y · n/Esc deny".into()
                }
            } else {
                if self.selected_menu_accepts_comment() {
                    "Tab comment".into()
                } else {
                    String::new()
                }
            };
            let mut hints = Vec::new();
            if !hint.is_empty() {
                hints.push((hint, 10));
            }
            if picker && (option_start > 0 || option_end < options.len()) {
                let scroll = match (option_start > 0, option_end < options.len()) {
                    (true, true) => "More above/below",
                    (true, false) => "More above",
                    _ => "More below",
                };
                hints.push((scroll.into(), 7));
            }
            if self.menu_plan {
                hints.push(("v full plan".into(), 12));
                if !editing {
                    hints.push(("PgUp/PgDn plan".into(), 10));
                    if self.menu_comment.is_some() {
                        hints.push(("Tab feedback".into(), 10));
                    }
                }
            } else if self.menu_review.is_some() && comment.is_none() {
                hints.push(("v full Diff".into(), 12));
                hints.push(("Shift+Tab review".into(), 7));
            } else if menu
                .context
                .iter()
                .flat_map(|line| {
                    wrap_text(&display_text(line), self.width.saturating_sub(2).max(1))
                })
                .count()
                > context_capacity
            {
                hints.push(("PgUp/PgDn details".into(), 6));
            }
            let hint = super::conversation_picker::fit_hint(&hints, self.width, self.display);
            lines.push(RoleLine::role(hint, Role::MutedText));
        }
        for line in &mut lines {
            line.truncate(self.width);
        }
        (lines, cursor.0, cursor.1)
    }

    fn leading_status_notice(&self, width: usize) -> String {
        let creation_cue = (!self.help_open)
            .then(|| self.overview.orientation_cue(width))
            .flatten();
        let attach_unavailable = self.status.starts_with(CREATION_ATTACH_UNAVAILABLE_PREFIX);
        let execution_connection = self
            .execution_connection_notice
            .filter(|_| self.overview.projection.is_none());
        let full = [
            self.connection_notice
                .as_deref()
                .or(execution_connection.map(ExecutionConnectionNotice::text)),
            self.creation_notice.as_deref(),
            creation_cue.as_deref(),
            attach_unavailable.then_some(self.status.as_str()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        if display_width(&full) <= width {
            return full;
        }
        let mut notices = Vec::new();
        if let Some(connection) = execution_connection {
            notices.push(connection.compact().to_owned());
        }
        if let Some(connection) = &self.connection_notice {
            notices.push(if connection.starts_with("Disconnected") {
                "Disconnected/cached".to_owned()
            } else if connection.starts_with("View connection unavailable") {
                "No connection".to_owned()
            } else if connection.starts_with("View registration unavailable") {
                "No registration".to_owned()
            } else {
                connection.clone()
            });
        }
        let creation_index = self.creation_notice.as_ref().map(|creation| {
            let index = notices.len();
            notices.push(
                if creation.starts_with("Creation ") && creation.contains(" unknown") {
                    "Creation unknown".to_owned()
                } else {
                    creation.clone()
                },
            );
            index
        });
        let cue_index = creation_cue.as_ref().map(|_| {
            let index = notices.len();
            notices.push(self.overview.minimum_orientation_cue().unwrap());
            index
        });
        if attach_unavailable {
            notices.push(
                if cue_index.is_some() {
                    "no attach"
                } else {
                    "Created · no attach"
                }
                .into(),
            );
        }
        if display_width(&notices.join(" · ")) > width
            && self
                .connection_notice
                .as_deref()
                .is_some_and(|notice| notice.starts_with("Disconnected"))
        {
            notices[0] = "Offline/cached".into();
            if display_width(&notices.join(" · ")) > width {
                notices[0] = "Offline".into();
            }
        }
        let separator = if display_width(&notices.join(" · ")) > width {
            " "
        } else {
            " · "
        };
        if let Some(index) = creation_index {
            let creation = self.creation_notice.as_deref().unwrap();
            let factual_prefix = creation.split(';').next().unwrap_or(creation);
            let available = width.saturating_sub(
                display_width(&notices.join(separator))
                    .saturating_sub(display_width(&notices[index])),
            );
            if display_width(factual_prefix) <= available {
                notices[index] = factual_prefix.to_owned();
            }
        }
        if let Some(index) = cue_index {
            let available = width.saturating_sub(
                display_width(&notices.join(separator))
                    .saturating_sub(display_width(&notices[index])),
            );
            notices[index] = self.overview.orientation_cue(available).unwrap();
        }
        if attach_unavailable {
            let index = notices.len() - 1;
            let attachment = if cue_index.is_some() {
                "attach unavailable"
            } else {
                "Created · local connection unavailable"
            };
            let available = width.saturating_sub(
                display_width(&notices.join(separator))
                    .saturating_sub(display_width(&notices[index])),
            );
            if display_width(attachment) <= available {
                notices[index] = attachment.to_owned();
            }
        }
        notices.join(separator)
    }

    fn connection_status_line(
        &self,
        notice: &str,
        background: Option<&BackgroundStatusNotice>,
        pending: Option<&str>,
    ) -> RoleLine {
        let mut line = status_notice_line(notice, background, pending, self.width);
        if self.execution_connection_notice.is_some()
            && self.connection_notice.is_none()
            && self.overview.projection.is_none()
        {
            for span in line
                .spans
                .iter_mut()
                .filter(|span| span.role == Some(Role::StatusBar))
            {
                span.role = Some(Role::Warning);
            }
        }
        line
    }

    fn normal_styled_frame(&mut self) -> (Vec<RoleLine>, usize, usize) {
        let panels = self.status_area.available_panels().collect::<Vec<_>>();
        if self.status_area.is_configured() || !panels.is_empty() {
            self.footer.update(panels);
        }
        let overview = self.overview.projection.is_some();
        let short = self.height < agent_layout::NORMAL_LAYOUT_MIN_ROWS;
        let cap = agent_layout::bottom_capacity(self.height);
        let (draft, cursor) = self.input_display();
        let mut input_rows = agent_layout::editor_capacity(self.height);
        let input = InputView::frame_with_prompt(
            &draft,
            cursor,
            self.width,
            input_rows,
            self.display.glyph(Glyph::Prompt),
        );
        input_rows = input.lines.len().max(1);
        let mut candidates = self.candidate_rows(agent_layout::MAX_CANDIDATE_ROWS);
        let mut queue = if !overview {
            pending_input_lines(&self.queued_inputs, self.width, MAX_PENDING_INPUT_ROWS)
        } else {
            Vec::new()
        };
        let mut queue_merged = false;
        let mut footer = self.footer.frame();
        footer.list_focused = self.footer_list_focused;
        if !self.footer_list_focused && !short {
            footer.selected = None;
        }
        let footer_key_hint = (self.footer_items_are_visible()
            && self.footer.selected() == Some(FooterItemId::Agents))
        .then_some(if self.footer_list_focused {
            "Enter open · Space fold · Esc entry"
        } else if self.status_area.agents_collapsed {
            "Enter/Space expand · Esc input"
        } else {
            "Enter/Space hide · ↓ list · Esc input"
        });
        let mut agents_entry = (!short
            && self.footer_items_are_visible()
            && footer
                .items
                .iter()
                .any(|item| item.id == FooterItemId::Agents))
        .then(|| {
            format!(
                "{} {}",
                self.display.glyph(Glyph::Subagents),
                self.status_area.header_label(FooterItemId::Agents)
            )
        })
        .filter(|label| display_width(label) + DIVIDER_ENTRY_GAP_COLUMNS + 2 <= self.width);
        let mut agents_entry_width = agents_entry
            .as_ref()
            .map_or(0, |label| display_width(label) + DIVIDER_ENTRY_GAP_COLUMNS);
        let mut panel_rows = if !overview && !self.help_open && self.footer_items_are_visible() {
            if agents_entry.is_some() {
                self.status_area.row_count(self.height).saturating_sub(1)
            } else {
                self.status_area.row_count(self.height).max(1)
            }
        } else {
            0
        };
        let mut help = if self.help_open {
            agent_layout::help_lines(
                self.width,
                self.width + 1,
                self.overview.projection.as_ref(),
            )
        } else {
            Vec::new()
        };
        help.truncate((self.height / 3).clamp(3, 12));
        let configured_status = self
            .configured_status_lines
            .as_ref()
            .filter(|_| !self.help_open)
            .map(|lines| {
                let mut rows = lines
                    .iter()
                    .flat_map(|line| wrap_text(&display_text(line), self.width))
                    .take(4)
                    .collect::<Vec<_>>();
                if rows.len() > 3 {
                    rows.truncate(3);
                    rows[2] = format!(
                        "{} …",
                        truncate_width(&rows[2], self.width.saturating_sub(2))
                    );
                }
                rows
            });
        let mut leading_notice = self.leading_status_notice(self.width);
        if short && !overview && !self.queued_inputs.is_empty() {
            let pending_columns =
                display_width(&format!("Pending {}", self.queued_inputs.len())) + 3;
            let available = self.width.saturating_sub(pending_columns);
            let compact = self.leading_status_notice(available);
            if display_width(&compact) <= available {
                leading_notice = compact;
            }
        }
        let separate_background_notice = self
            .background_status_notice
            .as_ref()
            .filter(|notice| notice.necessary && !overview && !short)
            .filter(|notice| {
                !leading_notice.is_empty()
                    && display_width(&leading_notice)
                        + 3
                        + display_width(
                            notice.compact_message.as_deref().unwrap_or(&notice.message),
                        )
                        > self.width
            });
        let required_status = !self.status.is_empty()
            || self.confirmation_hint().is_some()
            || short && !overview && !self.queued_inputs.is_empty()
            || !overview
                && self
                    .background_status_notice
                    .as_ref()
                    .is_some_and(|notice| notice.necessary);
        let core_status = self
            .status_area
            .status_line_with_background(usize::MAX, "", None);
        let float_core_status = !short
            && !overview
            && !self.help_open
            && configured_status.is_none()
            && self.status_area.is_configured()
            && display_width(&core_status.text()) + 2 <= self.width;
        let edge_width = DIVIDER_EDGE_RULE_COLUMNS + DIVIDER_ENTRY_GAP_COLUMNS;
        let mut core_edge_width = if float_core_status
            && display_width(&core_status.text()) + edge_width + 2 <= self.width
        {
            edge_width
        } else {
            0
        };
        if float_core_status
            && display_width(&core_status.text()) + core_edge_width + agents_entry_width + 2
                > self.width
        {
            core_edge_width = 0;
        }
        if float_core_status
            && display_width(&core_status.text()) + agents_entry_width + 2 > self.width
        {
            agents_entry = None;
            agents_entry_width = 0;
            if !overview && !self.help_open && self.footer_items_are_visible() {
                panel_rows = self.status_area.row_count(self.height).max(1);
            }
        }
        let agents_edge_width = if agents_entry.is_some()
            && agents_entry_width
                + edge_width
                + if float_core_status {
                    display_width(&core_status.text()) + core_edge_width + 2
                } else {
                    1
                }
                <= self.width
        {
            edge_width
        } else {
            0
        };
        let mut status_rows = if self.help_open {
            usize::from(!leading_notice.is_empty() || required_status)
        } else {
            configured_status
                .as_ref()
                .map_or(usize::from(!float_core_status), Vec::len)
                .max(usize::from(!leading_notice.is_empty() || required_status))
        };
        if float_core_status {
            status_rows = status_rows.max(usize::from(
                self.background_status_notice.is_some() || self.foreground_shell,
            ));
        }
        if separate_background_notice.is_some() {
            status_rows = status_rows.max(2);
        }
        if self.height < agent_layout::TALL_LAYOUT_MIN_ROWS {
            status_rows = status_rows.min(if separate_background_notice.is_some() {
                2
            } else {
                1
            });
        }
        let mut borders = if short { 1 } else { 2 };
        let directory_rows = usize::from(overview && !short);
        if short {
            status_rows = status_rows.min(1);
            input_rows = 1;
            queue.clear();
            panel_rows = usize::from(panel_rows > 0);
            candidates.truncate(1);
            help.truncate(1);
            if !candidates.is_empty() {
                panel_rows = 0;
                help.clear();
            }
        }
        let total = |input_rows: usize,
                     panel_rows: usize,
                     queue: usize,
                     candidates: usize,
                     help: usize,
                     status_rows: usize,
                     borders: usize| {
            input_rows
                + panel_rows
                + queue
                + candidates
                + help
                + status_rows
                + borders
                + directory_rows
                + usize::from(footer_key_hint.is_some())
        };
        while total(
            input_rows,
            panel_rows,
            queue.len(),
            candidates.len(),
            help.len(),
            status_rows,
            borders,
        ) > cap
        {
            if status_rows > 1 && (status_rows > 2 || separate_background_notice.is_none()) {
                status_rows -= 1;
            } else if queue.len() > 1 {
                queue.pop();
            } else if !self.footer_list_focused && panel_rows > 2 {
                panel_rows -= 1;
            } else if input_rows > 1 {
                input_rows -= 1;
            } else if panel_rows > 2 {
                panel_rows -= 1;
            } else if candidates.len() > 1 {
                candidates.pop();
            } else if !queue.is_empty() {
                queue.clear();
                queue_merged = true;
                status_rows = status_rows.max(1);
            } else if help.len() > 2 {
                help.pop();
            } else if short && borders > 0 {
                borders -= 1;
            } else if short && status_rows > 0 {
                status_rows -= 1;
            } else if short && panel_rows > 0 {
                panel_rows = 0;
            } else if short && !help.is_empty() {
                help.clear();
            } else {
                break;
            }
        }
        queue = pending_input_lines(&self.queued_inputs, self.width, queue.len());
        let input = InputView::frame_with_prompt_and_marks(
            &draft,
            cursor,
            self.width,
            input_rows,
            self.display.glyph(Glyph::Prompt),
            &self.input_image_marks(),
        );
        if !candidates.is_empty() {
            candidates = self.candidate_rows(candidates.len());
        }
        if overview {
            self.overview.transient_rows = input.lines.len().saturating_sub(1) + candidates.len();
        }
        let body_rows = self.height.saturating_sub(total(
            input_rows,
            panel_rows,
            queue.len(),
            candidates.len(),
            help.len(),
            status_rows,
            borders,
        ));
        let mut lines = Vec::new();
        if directory_rows > 0
            && let Some(projection) = &self.overview.projection
        {
            lines.push(RoleLine::role(
                dispatch_directory_line(&projection.dispatch_directory, self.width),
                Role::MutedText,
            ));
        }
        lines.extend(queue);
        lines.extend(candidates);
        let border = if self.shell_input.kind() == InputKind::Shell {
            Role::ShellBorder
        } else {
            Role::PromptBorder
        };
        let mut return_control = None;
        let mut tick_control = None;
        if borders > 0 {
            let can_return = self.ordinary_input_is_visible() && !self.scrollback.is_following();
            let content_columns = self.content_columns();
            let sticky_rows = usize::from(
                can_return
                    && body_rows >= 2
                    && self
                        .scrollback
                        .sticky_input_text(
                            self.display.glyph(Glyph::Prompt),
                            content_columns,
                            body_rows,
                        )
                        .is_some(),
            );
            let reading = can_return
                && self
                    .scrollback
                    .has_hidden_tail(content_columns, body_rows.saturating_sub(sticky_rows));
            let label = reading.then(|| self.back_to_bottom_label()).flatten();
            if let Some(label) = label {
                let label_width = display_width(&label);
                let space = self.width.saturating_sub(label_width);
                let column = space / 2;
                let mut line = RoleLine::role("─".repeat(column), border);
                line.push(label, Some(Role::Permission));
                line.push("─".repeat(space - column), Some(border));
                return_control = Some((lines.len(), column..column + label_width));
                lines.push(line);
            } else {
                lines.push(RoleLine::role("─".repeat(self.width), border));
            }
            if self.chat.is_some()
                && self.overview.projection.is_none()
                && !self.help_open
                && !self.tick_list.entries().is_empty()
            {
                let label = format!(
                    "{} Ticks {}",
                    self.display.glyph(Glyph::ProtocolTick),
                    self.tick_list.entries().len()
                );
                let label_width = display_width(&label);
                if label_width + DIVIDER_ENTRY_GAP_COLUMNS + DIVIDER_EDGE_RULE_COLUMNS <= self.width
                {
                    let tick_edge_width = if label_width
                        + DIVIDER_ENTRY_GAP_COLUMNS
                        + edge_width
                        + DIVIDER_EDGE_RULE_COLUMNS
                        <= self.width
                    {
                        edge_width
                    } else {
                        0
                    };
                    let column =
                        self.width - label_width - tick_edge_width - DIVIDER_ENTRY_GAP_COLUMNS;
                    let line = lines.last_mut().unwrap();
                    line.truncate(column);
                    line.push(" ".repeat(DIVIDER_ENTRY_GAP_COLUMNS), None);
                    line.push(
                        label,
                        Some(if self.tick_list_visible() {
                            Role::Permission
                        } else {
                            Role::MutedText
                        }),
                    );
                    if tick_edge_width > 0 {
                        line.push(" ".repeat(DIVIDER_ENTRY_GAP_COLUMNS), None);
                        line.push("─".repeat(DIVIDER_EDGE_RULE_COLUMNS), Some(border));
                    }
                    tick_control = Some((
                        lines.len() - 1,
                        column + DIVIDER_ENTRY_GAP_COLUMNS..self.width - tick_edge_width,
                    ));
                    if return_control
                        .as_ref()
                        .is_some_and(|(_, columns)| columns.end > column)
                    {
                        return_control = None;
                    }
                }
            }
        }
        let cursor_row = lines.len() + input.cursor_row;
        lines.extend(if self.prompt_enabled {
            input.styled_lines
        } else {
            vec![RoleLine::default()]
        });
        let mut agents_divider_row = None;
        if borders > 1 {
            let mut divider = RoleLine::default();
            if let Some(label) = &agents_entry {
                agents_divider_row = Some(lines.len());
                if agents_edge_width > 0 {
                    divider.push("─".repeat(DIVIDER_EDGE_RULE_COLUMNS), Some(border));
                    divider.push(" ".repeat(DIVIDER_ENTRY_GAP_COLUMNS), None);
                }
                divider.push(
                    label,
                    Some(if self.footer.selected() == Some(FooterItemId::Agents) {
                        Role::Suggestion
                    } else if self.status_area.agents_collapsed {
                        Role::MutedText
                    } else {
                        Role::Permission
                    }),
                );
                divider.push(" ".repeat(DIVIDER_ENTRY_GAP_COLUMNS), None);
            }
            divider.push(
                "─".repeat(self.width.saturating_sub(
                    agents_entry_width
                        + agents_edge_width
                        + core_edge_width
                        + if float_core_status {
                            display_width(&core_status.text()) + 1
                        } else {
                            0
                        },
                )),
                Some(border),
            );
            if float_core_status {
                divider.push(" ", None);
                divider.spans.extend(core_status.spans);
                if core_edge_width > 0 {
                    divider.push(" ".repeat(DIVIDER_ENTRY_GAP_COLUMNS), None);
                    divider.push("─".repeat(DIVIDER_EDGE_RULE_COLUMNS), Some(border));
                }
            }
            lines.push(divider);
        }
        if status_rows > 0 {
            let status_start = lines.len();
            let status = if self.status.starts_with(CREATION_ATTACH_UNAVAILABLE_PREFIX) {
                ""
            } else {
                self.status.as_str()
            };
            let mut notice = self.status_notice(status);
            if !leading_notice.is_empty() {
                notice = if notice.is_empty() {
                    leading_notice.clone()
                } else {
                    format!("{leading_notice} · {notice}")
                };
            }
            let compact_pending =
                ((queue_merged || short) && !self.queued_inputs.is_empty() && !overview)
                    .then(|| format!("Pending {}", self.queued_inputs.len()));
            let background = self.background_status_notice.as_ref().filter(|background| {
                separate_background_notice.is_none()
                    && !overview
                    && (background.necessary || !self.help_open)
            });
            if let Some(configured_status) = &configured_status {
                let notice_line =
                    self.connection_status_line(&notice, background, compact_pending.as_deref());
                notice = notice_line.text();
                if configured_status.is_empty() {
                    lines.push(notice_line.clone());
                }
                for (index, configured) in configured_status
                    .iter()
                    .take(status_rows - usize::from(separate_background_notice.is_some()))
                    .enumerate()
                {
                    let mut line = if index == 0 && !notice.is_empty() {
                        if !leading_notice.is_empty()
                            || required_status
                            || compact_pending.is_some()
                        {
                            let mut line = notice_line.clone();
                            line.push(" · ", Some(Role::MutedText));
                            line.push(configured.clone(), Some(Role::StatusBar));
                            line
                        } else {
                            let mut line = RoleLine::role(configured.clone(), Role::StatusBar);
                            line.push(" · ", Some(Role::MutedText));
                            line.spans.extend(notice_line.clone().spans);
                            line
                        }
                    } else {
                        RoleLine::role(configured.clone(), Role::StatusBar)
                    };
                    if index + 1 == status_rows
                        && configured_status.len() > status_rows
                        && (index != 0 || display_width(&notice) <= self.width.saturating_sub(2))
                    {
                        line.truncate(self.width.saturating_sub(2));
                        line.push(" …", Some(Role::MutedText));
                    }
                    lines.push(line.filled(Role::StatusBar));
                }
            } else {
                lines.push(
                    if self.status_area.is_configured()
                        && !float_core_status
                        && !overview
                        && !self.help_open
                        && (leading_notice.is_empty()
                            || self.execution_connection_notice.is_some()
                                && self.connection_notice.is_none()
                                && self.creation_notice.is_none())
                        && compact_pending.is_none()
                    {
                        let mut line = self
                            .status_area
                            .status_line_with_background(self.width, &notice, background);
                        if self.execution_connection_notice.is_some() {
                            let hint = EXECUTION_CONNECTION_DETAILS_HINT;
                            if display_width(&line.text()) + 3 + display_width(hint) <= self.width {
                                line.push(" · ", Some(Role::MutedText));
                                line.push(hint, Some(Role::MutedText));
                            }
                        }
                        line
                    } else {
                        let mut notice_line = self.connection_status_line(
                            &notice,
                            background,
                            compact_pending.as_deref(),
                        );
                        notice = notice_line.text();
                        let overview_list_hint;
                        let return_hint;
                        let tick_hint;
                        let hints = if self.confirmation_hint().is_some() || self.help_open {
                            // Help is focused and draws its own close hint; only facts remain.
                            ""
                        } else if self.tick_list_visible() && self.tick_list.focused {
                            tick_hint = self.tick_list.detail.as_ref().map_or_else(|| "Esc input".into(), |detail| detail.hint());
                            tick_hint.as_str()
                        } else if self.foreground_shell {
                            "Shell running · Ctrl+B background · Ctrl+C stop"
                        } else if self.footer.selected().is_some() || float_core_status {
                            ""
                        } else if self.candidate_rows(1).is_empty()
                            && self.clipboard_hint.is_visible()
                        {
                            CLIPBOARD_IMAGE_HINT
                        } else if overview {
                            if !self.overview.input_focused {
                                overview_list_hint = format!(
                                    "? help · {} · Tab new Agent",
                                    self.overview.row_hints().join(" · ")
                                );
                                overview_list_hint.as_str()
                            } else if self.editor.draft().is_empty() {
                                "? help · Enter dispatch · ↑ list"
                            } else {
                                "Enter dispatch · Tab list"
                            }
                        } else if borders == 0
                            && self.ordinary_input_is_visible()
                            && !self.scrollback.is_at_bottom()
                        {
                            return_hint = self.back_to_bottom_label().unwrap_or_default();
                            return_hint.as_str()
                        } else if self.editor.draft().is_empty() {
                            if self.tick_list.entries().is_empty() { "← Agents · ? help · Ctrl+O details · Ctrl+G editor · Ctrl+C interrupt" } else { "← Agents · → Ticks · ? help · Ctrl+O details · Ctrl+G editor · Ctrl+C interrupt" }
                        } else {
                            "Enter send · Ctrl+G editor · Ctrl+C clear"
                        };
                        if notice.is_empty() {
                            RoleLine::role(hints, Role::StatusBar)
                        } else if hints.is_empty() {
                            notice_line
                        } else if overview && leading_notice.is_empty() && !self.status_held {
                            RoleLine::role(format!("{hints} · {notice}"), Role::StatusBar)
                        } else if overview {
                            RoleLine::role(
                                Some(format!("{notice} · {hints}"))
                                    .filter(|text| display_width(text) <= self.width)
                                    .unwrap_or(notice),
                                Role::StatusBar,
                            )
                        } else {
                            notice_line.push(" · ", Some(Role::MutedText));
                            notice_line.push(hints, Some(Role::StatusBar));
                            notice_line
                        }
                    },
                );
                if float_core_status
                    && self.execution_connection_notice.is_some()
                    && self.ordinary_input_is_visible()
                    && !self.conversation_focused
                    && let Some(line) = lines.last_mut()
                    && display_width(&line.text())
                        + 3
                        + display_width(EXECUTION_CONNECTION_DETAILS_HINT)
                        <= self.width
                {
                    line.push(" · ", Some(Role::MutedText));
                    line.push(EXECUTION_CONNECTION_DETAILS_HINT, Some(Role::MutedText));
                }
            }
            if let Some(background) = separate_background_notice {
                lines.insert(
                    status_start + 1,
                    background.line(self.width).filled(Role::StatusBar),
                );
            }
        }
        let mut footer_bar_row = None;
        if panel_rows > 0 {
            if self.status_area.is_configured() {
                if short
                    && panel_rows == 1
                    && footer.selected.is_some()
                    && self.footer_list_focused
                    && self.status_area.is_open()
                {
                    lines.extend(
                        self.status_area
                            .panel_lines_with_rows(&footer, self.width, 2, self.display)
                            .into_iter()
                            .skip(1)
                            .take(1),
                    );
                } else {
                    footer_bar_row = Some(lines.len());
                    let mut panel_footer = footer.clone();
                    if agents_entry.is_some() {
                        panel_footer
                            .items
                            .retain(|item| item.id != FooterItemId::Agents);
                    }
                    lines.extend(self.status_area.panel_lines_with_rows(
                        &panel_footer,
                        self.width,
                        panel_rows,
                        self.display,
                    ));
                }
            } else {
                lines.push(RoleLine::role(
                    footer_frame_line(&footer, self.display),
                    Role::MutedText,
                ));
            }
        }
        if let Some(hint) = footer_key_hint {
            let mut line = RoleLine::role(hint, Role::MutedText).filled(Role::StatusBar);
            line.truncate(self.width);
            lines.push(line);
        }
        if !help.is_empty() {
            let all = agent_layout::help_lines(
                self.width,
                self.width + 1,
                self.overview.projection.as_ref(),
            );
            if all.len() > help.len() && help.len() > 1 {
                // A scrolled help keeps its closing hint on the last row.
                let mut entries = all;
                let close = entries.pop();
                let rows = help.len() - 1;
                let offset = self.help_offset.min(entries.len().saturating_sub(rows));
                lines.extend(entries.into_iter().skip(offset).take(rows));
                lines.extend(close);
            } else {
                let offset = self.help_offset.min(all.len().saturating_sub(help.len()));
                lines.extend(all.into_iter().skip(offset).take(help.len()));
            }
        }
        for line in &mut lines {
            line.truncate(self.width);
        }
        self.tick_toggle_target = tick_control.map(|(row, columns)| BackToBottomTarget {
            row: self.height.saturating_sub(lines.len()) + row,
            columns,
        });
        self.back_to_bottom_target = return_control.map(|(row, columns)| BackToBottomTarget {
            row: self.height.saturating_sub(lines.len()) + row,
            columns,
        });
        self.footer_bar_target = agents_divider_row
            .or(footer_bar_row)
            .map(|row| FooterBarTarget {
                row: self.height.saturating_sub(lines.len()) + row,
                inset: 0,
                categories: if agents_divider_row.is_some() {
                    vec![(
                        agents_edge_width
                            ..agents_edge_width + agents_entry_width - DIVIDER_ENTRY_GAP_COLUMNS,
                        FooterItemId::Agents,
                    )]
                } else {
                    self.status_area.panel_targets(&footer, self.width)
                },
            });
        (
            lines,
            cursor_row.min(cap.saturating_sub(1)),
            input.cursor_column,
        )
    }

    fn candidate_rows(&self, capacity: usize) -> Vec<RoleLine> {
        let (selected, candidates) = if self.shell_input.active_completion().is_some() {
            let candidates = match self.shell_input.active_completion() {
                Some(ShellCompletionKind::History) => self
                    .shell_input
                    .history_candidates()
                    .iter()
                    .map(|row| (row.command.clone(), "history".to_owned()))
                    .collect::<Vec<_>>(),
                Some(ShellCompletionKind::Path) => self
                    .shell_input
                    .path_candidates()
                    .iter()
                    .map(|row| (row.display_path.clone(), "path".to_owned()))
                    .collect::<Vec<_>>(),
                None => Vec::new(),
            };
            (self.shell_input.selected_index(), candidates)
        } else if self.file_mentions.is_open() {
            (
                self.file_mentions.selected_index(),
                self.file_mentions
                    .candidates()
                    .iter()
                    .map(|row| (row.display_path.clone(), "file".to_owned()))
                    .collect(),
            )
        } else if self.completion.is_open() {
            (
                self.completion.selected_index(),
                self.completion.described_candidates(),
            )
        } else {
            return Vec::new();
        };
        let start = selected.map_or(0, |index| index.saturating_sub(capacity.saturating_sub(1)));
        candidates
            .into_iter()
            .enumerate()
            .skip(start)
            .take(capacity)
            .map(|(index, (text, description))| {
                candidate_line(selected == Some(index), &text, &description, self.display)
            })
            .collect()
    }

    fn input_image_marks(&self) -> Vec<(std::ops::Range<usize>, Role)> {
        if let Some(text) = self.visible_prompt_suggestion() {
            return vec![(0..text.len(), Role::MutedText)];
        }
        let offset = usize::from(self.shell_input.kind() == InputKind::Shell);
        self.paste_blocks
            .projection(
                self.editor.draft(),
                self.editor.cursor(),
                self.width.saturating_sub(2),
            )
            .blocks
            .into_iter()
            .filter(|block| block.is_image)
            .map(|block| {
                (
                    block.display_range.start + offset..block.display_range.end + offset,
                    Role::Accent,
                )
            })
            .collect()
    }

    fn input_display(&self) -> (String, usize) {
        if let Some(text) = self.visible_prompt_suggestion() {
            return (text.into(), 0);
        }
        let projection = self.paste_blocks.projection(
            self.editor.draft(),
            self.editor.cursor(),
            self.width.saturating_sub(2),
        );
        if self.shell_input.kind() == InputKind::Shell {
            debug_assert_eq!(self.shell_input.command(), self.editor.draft());
            debug_assert_eq!(self.shell_input.cursor(), self.editor.cursor());
            (format!("!{}", projection.text), projection.cursor + 1)
        } else {
            (projection.text, projection.cursor)
        }
    }

    fn history_search_frame(&self) -> (Vec<RoleLine>, usize, usize) {
        let search = self.history_search.as_ref().unwrap();
        let match_capacity = self.height.saturating_sub(4).max(1);
        let selected = search.selected();
        let start = selected
            .map(|index| index.saturating_sub(match_capacity.saturating_sub(1)))
            .unwrap_or(0);
        let mut lines = vec![RoleLine::role(
            format!("Search history · {}", search.scope().label()),
            Role::DialogTitle,
        )];
        for (index, candidate) in search
            .matches()
            .iter()
            .enumerate()
            .skip(start)
            .take(match_capacity)
        {
            let marker = if selected == Some(index) {
                self.display.glyph(Glyph::Prompt)
            } else {
                " "
            };
            let prompt = display_text(candidate.display()).replace('\n', " ↵ ");
            let source = if search.scope() == HistoryScope::AllProjects {
                format!("  {}", candidate.project())
            } else {
                String::new()
            };
            let mut line = RoleLine::default();
            line.push(
                format!("{marker} {prompt}"),
                (selected == Some(index)).then_some(Role::Suggestion),
            );
            line.push(source, Some(Role::MutedText));
            line.truncate(self.width);
            lines.push(line);
        }
        while lines.len() < self.height.saturating_sub(2) {
            lines.push(RoleLine::default());
        }
        let mut query = RoleLine::plain(format!("Search: {}", display_text(search.query())));
        query.truncate(self.width);
        lines.push(query);
        let mut hint = RoleLine::role(
            self.display
                .navigation_hint("Ctrl+S scope · Enter/Tab insert · Esc cancel"),
            Role::MutedText,
        );
        hint.truncate(self.width);
        lines.push(hint);
        let cursor_row = lines.len().saturating_sub(2);
        let cursor_column = 8usize
            .saturating_add(
                search
                    .query()
                    .chars()
                    .map(|character| character.width().unwrap_or(0))
                    .sum::<usize>(),
            )
            .min(self.width.saturating_sub(1));
        (lines, cursor_row, cursor_column)
    }

    fn insert(&mut self, text: &str) {
        self.last_input_edit = Some(Instant::now());
        self.edit(EditorCommand::Insert(text.into()));
    }

    /// Pasted text and image placeholders keep their own undo snapshot instead of
    /// joining the typing group around them.
    fn insert_pasted(&mut self, text: &str) {
        self.edit(EditorCommand::InsertPasted(text.into()));
    }

    /// Claude Code `handleImagePaste`: the clipboard image becomes an `[Image #N]` block at
    /// the cursor, numbered with the folded pastes.
    fn paste_clipboard_image(&mut self) {
        if !self.accepts_images {
            self.status = IMAGE_INPUT_UNSUPPORTED.into();
            return;
        }
        match self.clipboard.read_image() {
            Ok(Some(image)) => self.attach_image(image),
            Ok(None) if self.clipboard.is_remote() => {
                self.status = NO_CLIPBOARD_IMAGE_OVER_SSH.into();
            }
            Ok(None) => self.status = NO_CLIPBOARD_IMAGE.into(),
            Err(error) => self.status = format!("Cannot paste image: {error}"),
        }
    }

    fn attach_image(&mut self, image: Image) {
        let id = self.paste_blocks.next_id();
        let placeholder = match PasteBlocks::image_placeholder(id) {
            Ok(placeholder) => placeholder,
            Err(error) => {
                self.status = format!("Cannot attach image: {error}");
                return;
            }
        };
        // Claude Code `storeImage`: best effort, a failed write only costs the link.
        if let (Some(cache), Some(chat), Ok(image_id)) =
            (&self.image_cache, self.chat, u32::try_from(id.get()))
        {
            let _ = cache.store(chat, image_id, &image);
        }
        let start = self.editor.cursor();
        self.insert_pasted(&placeholder);
        if let Err(error) = self.paste_blocks.register_image(
            self.editor.draft(),
            start..self.editor.cursor(),
            id,
            image,
        ) {
            self.status = format!("Cannot attach image: {error}");
        }
    }

    /// Only an empty editor is filled, so nothing typed after sending is overwritten; the
    /// refused text is in the input history either way.
    /// Puts the last sent prompt back unless something was typed since; returns whether it did.
    fn restore_rejected_prompt(&mut self, contexts: Vec<InputContext>) -> bool {
        let Some(prompt) = self.submitted_prompt.take() else {
            return false;
        };
        if !self.editor.draft().is_empty() {
            return false;
        }
        let cursor = prompt.draft.len();
        self.replace_draft(prompt.draft, cursor);
        self.paste_blocks.restore(prompt.paste_blocks);
        self.file_mentions
            .restore_accepted(self.editor.draft(), prompt.mentions);
        self.file_mentions.dismiss();
        self.completion.dismiss();
        self.restore_saved_contexts(&contexts);
        true
    }

    pub(super) fn hold_status(&mut self, text: &str) {
        self.status = display_text(text).replace('\n', " ");
        self.status_held = true;
    }

    /// The periodic status, unless it would erase why the last action did nothing.
    fn refresh_status(&mut self, status: &str) {
        if !self.status_held {
            self.status = display_text(status).replace('\n', " ");
        }
    }

    /// Puts back a prompt whose acceptance is unknown; the unchanged draft then retries
    /// `retry_of` instead of being a new input.
    fn restore_unknown_prompt(&mut self, contexts: Vec<InputContext>, retry_of: Option<Uuid>) {
        if self.restore_rejected_prompt(contexts) {
            self.retry_of = retry_of;
        }
    }

    /// Saved pasted images go back onto their `[Image #N]` placeholders as image blocks;
    /// the other attachments wait in `restored_contexts` until the prompt is sent.
    fn restore_saved_contexts(&mut self, contexts: &[InputContext]) {
        self.restored_contexts.clear();
        for context in contexts {
            match context {
                InputContext::Image(ImageContext {
                    origin: ImageOrigin::Pasted { id },
                    image,
                }) => {
                    self.paste_blocks
                        .restore_image(self.editor.draft(), *id, image.clone());
                }
                _ => self.restored_contexts.push(context.clone()),
            }
        }
    }

    fn replace_draft(&mut self, draft: String, cursor: usize) {
        let before = self.editor.draft().to_owned();
        self.editor.replace_draft(draft, cursor);
        if self.editor.draft() != before {
            self.retry_of = None;
        }
        self.paste_blocks.reconcile(&before, self.editor.draft());
        self.refresh_completion();
    }

    fn snap_cursor_out_of_pasted_block(&mut self, previous_cursor: usize) {
        let cursor = self.editor.cursor();
        let Some(id) = self.paste_blocks.block_at_raw_cursor(cursor) else {
            return;
        };
        let Some(start) = self.paste_blocks.cursor_before_block(id) else {
            return;
        };
        let Some(end) = self.paste_blocks.cursor_after_block(id) else {
            return;
        };
        if cursor == start || cursor == end {
            return;
        }
        let target = if cursor >= previous_cursor {
            end
        } else {
            start
        };
        self.editor
            .replace_draft(self.editor.draft().to_owned(), target);
    }

    fn move_across_pasted_block(&mut self, forward: bool) -> bool {
        let cursor = self.editor.cursor();
        let Some(id) = self.paste_blocks.block_at_raw_cursor(cursor) else {
            return false;
        };
        let target = if forward {
            self.paste_blocks.cursor_after_block(id)
        } else {
            self.paste_blocks.cursor_before_block(id)
        };
        let Some(target) = target.filter(|target| *target != cursor) else {
            return false;
        };
        self.editor
            .replace_draft(self.editor.draft().to_owned(), target);
        self.refresh_completion();
        true
    }

    fn delete_pasted_block(&mut self, backward: bool) -> bool {
        let cursor = self.editor.cursor();
        let Some(id) = self.paste_blocks.block_at_raw_cursor(cursor) else {
            return false;
        };
        let is_boundary = if backward {
            self.paste_blocks.cursor_after_block(id) == Some(cursor)
        } else {
            self.paste_blocks.cursor_before_block(id) == Some(cursor)
        };
        if !is_boundary {
            return false;
        }
        match self.paste_blocks.delete(self.editor.draft(), id) {
            Ok(Some(deletion)) => {
                self.editor.replace_draft(deletion.draft, deletion.cursor);
                self.refresh_completion();
                true
            }
            Ok(None) => false,
            Err(error) => {
                self.status = format!("Cannot delete pasted text: {error}");
                true
            }
        }
    }

    fn apply_shell_action(&mut self, action: ShellInputAction) -> Option<InputEvent> {
        match action {
            ShellInputAction::Ignored => None,
            ShellInputAction::Consumed => None,
            ShellInputAction::EnterShell { command, cursor }
            | ShellInputAction::ReplaceCommand { command, cursor } => {
                self.replace_draft(command, cursor);
                None
            }
            ShellInputAction::ExitShell => {
                self.replace_draft(String::new(), 0);
                self.refresh_completion();
                None
            }
            ShellInputAction::Submit(submission) => {
                self.submitted_retry_of = self.retry_of.take();
                self.editor.replace_draft(String::new(), 0);
                self.paste_blocks.clear();
                self.refresh_completion();
                Some(InputEvent::UserShellSubmitted(submission))
            }
        }
    }

    fn apply_shell_completion(&mut self, key: ShellCompletionKey) -> bool {
        let action = self.shell_input.handle_completion_key(key);
        if action == ShellInputAction::Ignored {
            return false;
        }
        self.apply_shell_action(action);
        true
    }

    fn refresh_completion(&mut self) {
        self.restored_contexts
            .retain(|context| draft_keeps_context(self.editor.draft(), context));
        if self.shell_input.kind() == InputKind::Shell {
            self.completion.dismiss();
            self.file_mentions.dismiss();
            self.refresh_shell_history_cache();
            let history = self
                .shell_history_cache
                .as_ref()
                .map_or(&[][..], |cache| cache.commands.as_slice());
            let cwd = self
                .mention_roots
                .as_ref()
                .map(|(workdir, _)| workdir.as_path())
                .unwrap_or(self.history_project.as_path());
            let home = std::env::var_os("HOME").map(PathBuf::from);
            if let Err(error) = self.shell_input.refresh_candidates(
                self.editor.draft(),
                self.editor.cursor(),
                history,
                cwd,
                home.as_deref(),
                15,
            ) {
                self.status = format!("Cannot complete shell path: {error}");
            }
            return;
        }
        self.shell_history_cache = None;
        if self.prompt_enabled && self.menu.is_none() && self.transcript.is_none() {
            let scope = self.command_scope();
            self.completion
                .update_at(self.editor.draft(), self.editor.cursor(), scope);
            self.file_mentions
                .update_at(self.editor.draft(), self.editor.cursor(), 15);
        } else {
            self.completion.dismiss();
            self.file_mentions.dismiss();
        }
    }

    /// 进入 `!` 模式时载入，之后按 Claude Code 的 60 秒 TTL 复用；离开 `!` 模式（含提交）
    /// 时丢弃，下次进入会带上刚提交的命令。读取失败与 Claude Code 一样缓存为空，过期后再试。
    fn refresh_shell_history_cache(&mut self) {
        if self
            .shell_history_cache
            .as_ref()
            .is_some_and(|cache| cache.loaded_at.elapsed() < SHELL_HISTORY_CACHE_TTL)
        {
            return;
        }
        let commands = self
            .input_history
            .as_ref()
            .and_then(|history| {
                history
                    .recent_shell_commands(&self.history_session, &self.history_project)
                    .ok()
            })
            .unwrap_or_default();
        self.shell_history_cache = Some(ShellHistoryCache {
            commands,
            loaded_at: Instant::now(),
        });
    }

    fn apply_completion(&mut self, key: CompletionKey) -> bool {
        match self.completion.handle_key(key) {
            CompletionAction::Ignored => false,
            CompletionAction::Consumed => true,
            CompletionAction::ReplaceDraft { draft, cursor } => {
                self.replace_draft(draft, cursor);
                self.completion.dismiss();
                true
            }
        }
    }

    fn apply_file_mention(&mut self, key: FileMentionKey) -> bool {
        match self.file_mentions.handle_key(key) {
            FileMentionAction::Ignored => false,
            FileMentionAction::Consumed => true,
            FileMentionAction::ReplaceDraft { draft, cursor } => {
                self.replace_draft(draft, cursor);
                true
            }
        }
    }

    fn stash_input(&mut self) {
        self.last_input_edit = Some(Instant::now());
        if !self.editor.draft().is_empty() {
            self.input_stash = Some(InputStash {
                paste_blocks: self.paste_blocks.take(),
                restored_contexts: std::mem::take(&mut self.restored_contexts),
                accepted_mentions: self.file_mentions.take_accepted(),
                shell_input: std::mem::take(&mut self.shell_input),
            });
            self.edit(EditorCommand::Stash);
        } else if let Some(stash) = self.input_stash.take() {
            self.edit(EditorCommand::Stash);
            self.paste_blocks.restore(stash.paste_blocks);
            self.restored_contexts = stash.restored_contexts;
            self.file_mentions
                .restore_accepted(self.editor.draft(), stash.accepted_mentions);
            self.shell_input = stash.shell_input;
            self.refresh_completion();
        }
    }

    fn edit(&mut self, command: EditorCommand) -> EditorOutcome {
        self.last_input_edit = Some(Instant::now());
        if let EditorCommand::MoveUp { wrap_width } = command {
            self.load_up_arrow_history(wrap_width);
        }
        let before = self.editor.draft().to_owned();
        let previous_cursor = self.editor.cursor();
        let outcome = self
            .editor
            .apply_keeping_whole(command, &self.paste_blocks.ranges());
        if outcome == EditorOutcome::Changed {
            self.clear_prompt_suggestion();
            self.retry_of = None;
            self.paste_blocks.reconcile(&before, self.editor.draft());
            self.snap_cursor_out_of_pasted_block(previous_cursor);
            self.refresh_completion();
        }
        outcome
    }

    fn menu_is_visible(&self) -> bool {
        if self.menu.as_ref().is_some_and(|menu| menu.confirmation)
            && self.request_layout.is_none()
            && self.input_dialog_body_height() < 3
        {
            return false;
        }
        self.input_dialog_visible() || self.height >= MIN_MENU_TERMINAL_ROWS
    }

    fn menu_submission_is_pending(&self) -> bool {
        self.menu_submission_pending.is_some()
    }

    fn commit_branch_submission(&mut self) {
        if self.branch_submission != BranchSubmission::Pending {
            return;
        }
        let EditorOutcome::Submitted(submitted) = self.editor.apply(EditorCommand::Submit) else {
            unreachable!()
        };
        self.editor.forget_latest_history(&submitted);
        self.paste_blocks.clear();
        self.restored_contexts.clear();
        self.completion.dismiss();
        self.file_mentions.update_at("", 0, 15);
        self.branch_submission = BranchSubmission::FinishedCommitted;
    }

    fn cancel_branch_submission(&mut self, preserve_until_summary_finishes: bool) {
        if self.branch_submission == BranchSubmission::Pending {
            self.branch_submission = if preserve_until_summary_finishes {
                BranchSubmission::FinishedCancelledForSummary
            } else {
                BranchSubmission::FinishedCancelled
            };
        }
    }

    fn clear_finished_branch_submission(&mut self) -> bool {
        if !matches!(
            self.branch_submission,
            BranchSubmission::FinishedCommitted
                | BranchSubmission::FinishedCancelled
                | BranchSubmission::FinishedCancelledForSummary
        ) {
            return false;
        }
        self.branch_submission =
            if self.branch_submission == BranchSubmission::FinishedCancelledForSummary {
                BranchSubmission::PreservedCancelled
            } else {
                BranchSubmission::Idle
            };
        true
    }

    fn branch_submission_owns_input(&self) -> bool {
        matches!(
            self.branch_submission,
            BranchSubmission::Pending
                | BranchSubmission::FinishedCommitted
                | BranchSubmission::FinishedCancelled
                | BranchSubmission::FinishedCancelledForSummary
        )
    }

    fn branch_submission_protects_editor(&self) -> bool {
        matches!(
            self.branch_submission,
            BranchSubmission::Pending
                | BranchSubmission::FinishedCancelledForSummary
                | BranchSubmission::PreservedCancelled
        )
    }

    fn finish_summary_input_transition(&mut self) {
        self.branch_submission = match self.branch_submission {
            BranchSubmission::FinishedCancelledForSummary => BranchSubmission::FinishedCancelled,
            BranchSubmission::PreservedCancelled => BranchSubmission::Idle,
            state => state,
        };
    }

    fn clear_menu(&mut self, id: Uuid) -> bool {
        if self.clear_hidden_menu(id) {
            return true;
        }
        if self.clear_conversation_picker(id) {
            if self.menu_submission_pending != Some(id) {
                self.menu_submission_pending = None;
                self.menu_submission_finished = None;
            }
            return true;
        }
        if self.menu.as_ref().is_none_or(|menu| menu.id != id) {
            return false;
        }
        self.reset_active_menu();
        if self.menu_submission_pending != Some(id) {
            self.menu_submission_pending = None;
            self.menu_submission_finished = None;
        }
        self.resume_dialog();
        self.refresh_completion();
        true
    }

    fn clear_conversation_picker(&mut self, id: Uuid) -> bool {
        if self
            .conversation_picker
            .as_ref()
            .is_none_or(|picker| picker.id() != id)
        {
            return false;
        }
        self.conversation_picker = None;
        self.rendered_region = None;
        true
    }

    fn begin_menu_submission(&mut self, id: Uuid) {
        self.menu_submission_pending = Some(id);
        self.menu_submission_finished = None;
    }

    fn mark_menu_submission_finished(&mut self, id: Uuid) {
        if self.menu_submission_pending == Some(id) {
            self.menu_submission_finished = Some(id);
        }
    }

    /// 释放已完成的菜单与 Branch 提交对输入的占用，返回界面是否变化。
    fn release_finished_submissions(&mut self) -> bool {
        let menu_changed = self.clear_finished_menu_submission();
        let branch_finished = self.clear_finished_branch_submission();
        menu_changed || branch_finished
    }

    fn clear_finished_menu_submission(&mut self) -> bool {
        let Some(id) = self.menu_submission_finished else {
            return false;
        };
        self.menu_submission_pending = None;
        self.menu_submission_finished = None;
        if self.clear_hidden_menu(id) {
            return true;
        }
        if self.clear_conversation_picker(id) {
            return true;
        }
        if self.menu.as_ref().is_none_or(|menu| menu.id != id) {
            return false;
        }
        self.reset_active_menu();
        self.resume_dialog();
        self.refresh_completion();
        true
    }

    fn footer_items_are_visible(&self) -> bool {
        let frame = self.footer.frame();
        self.height >= MIN_FOOTER_ITEMS_TERMINAL_ROWS
            && self.menu.is_none()
            && self.conversation_picker.is_none()
            && self.overview.projection.is_none()
            && !self.help_open
            && self.history_search.is_none()
            && self.transcript.is_none()
            && self.diff_screen.is_none()
            && self.task_surface.is_none()
            && self.subscription_surface.is_none()
            && !self.foreground_shell
            && self.shell_input.kind() == InputKind::Prompt
            && !frame.items.is_empty()
    }

    fn can_enter_footer(&self) -> bool {
        if !self.footer_can_own_focus()
            || !self.scrollback.is_at_bottom()
            || self.editor.is_browsing_history()
        {
            return false;
        }
        let (display_draft, display_cursor) = self.input_display();
        let input = InputView::frame_with_prompt(
            &display_draft,
            display_cursor,
            self.width,
            agent_layout::editor_capacity(self.height),
            self.display.glyph(Glyph::Prompt),
        );
        input.first_visual_row + input.cursor_row + 1 == input.total_visual_rows
    }

    fn footer_can_own_focus(&self) -> bool {
        self.footer_items_are_visible()
            && self.shell_input.active_completion().is_none()
            && !self.file_mentions.is_open()
            && !self.completion.is_open()
    }

    fn clear_footer_selection(&mut self) {
        if self.footer.selected().is_some() {
            self.footer.apply(FooterAction::Clear);
        }
        self.footer_list_focused = false;
    }

    fn clear_footer_selection_if_hidden(&mut self) {
        if !self.help_open && !self.footer_can_own_focus() {
            self.clear_footer_selection();
        }
    }

    fn apply_footer_action(&mut self, action: FooterAction) -> Option<InputEvent> {
        let was_open = self.status_area.is_open();
        let effect = if action == FooterAction::Down
            && self.footer.selected().is_none()
            && let Some(panel) = self.status_area.selected_panel()
        {
            self.footer.select(panel);
            FooterEffect::None
        } else {
            self.footer.apply(action)
        };
        if matches!(action, FooterAction::Clear) && self.status_area.is_configured() {
            self.footer_list_focused = false;
        }
        if (was_open
            || matches!(
                action,
                FooterAction::Down | FooterAction::Previous | FooterAction::Next
            ))
            && let Some(selected) = self.footer.selected()
        {
            self.status_area.select(selected);
            if matches!(action, FooterAction::Previous | FooterAction::Next) {
                self.footer_list_focused = false;
            }
        }
        match effect {
            FooterEffect::None
            | FooterEffect::ReturnToEditor
            | FooterEffect::Open(FooterItemId::Agents) => None,
        }
    }

    fn footer_key(&mut self, key: KeyEvent) -> Option<Option<InputEvent>> {
        if !self.status_area.is_open() {
            self.footer_list_focused = false;
        }
        if self.footer.selected() == Some(FooterItemId::Agents) {
            if key.code == KeyCode::Enter && key.modifiers != KeyModifiers::NONE {
                return Some(None);
            }
            if !self.footer_list_focused
                && key.modifiers == KeyModifiers::NONE
                && matches!(key.code, KeyCode::Enter | KeyCode::Char(' '))
            {
                self.status_area.toggle_agents();
                return Some(None);
            }
            if self.footer_list_focused && key.code == KeyCode::Esc {
                self.footer_list_focused = false;
                return Some(None);
            }
        }
        if self.status_area.is_configured()
            && self.status_area.is_open()
            && let Some(selected) = self.footer.selected()
        {
            let amount = match (key.code, key.modifiers) {
                (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
                | (KeyCode::Char('n'), KeyModifiers::CONTROL) => Some(1),
                (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
                | (KeyCode::Char('p'), KeyModifiers::CONTROL) => Some(-1),
                _ => None,
            };
            if let Some(amount) = amount {
                if !self.footer_list_focused {
                    if amount > 0 {
                        self.footer_list_focused = self.status_area.is_open();
                    } else {
                        self.apply_footer_action(FooterAction::Clear);
                    }
                } else if amount < 0 && self.status_area.selection_at_start() {
                    self.footer_list_focused = false;
                } else {
                    let capacity = agent_layout::panel_capacity(self.height);
                    self.status_area.move_agent_selection(amount, capacity);
                }
                return Some(None);
            }
            if key.code == KeyCode::Enter && !self.footer_list_focused {
                self.footer_list_focused = true;
                return Some(None);
            }
            if key.code == KeyCode::Enter && selected == FooterItemId::Agents {
                return Some(self.status_area.selected_agent().map(InputEvent::OpenAgent));
            }
            if key.code == KeyCode::Char(' ')
                && selected == FooterItemId::Agents
                && self.footer_list_focused
            {
                self.status_area.toggle_selected_agent();
                return Some(None);
            }
        }
        if self.status_area.is_configured()
            && let Some(selected) = self.footer.selected()
        {
            match (key.code, key.modifiers) {
                (KeyCode::Enter, _) if !self.status_area.is_open() => {
                    self.status_area.select(selected);
                    return Some(None);
                }
                (KeyCode::Char(' '), KeyModifiers::NONE) => {
                    self.status_area.select(selected);
                    self.clear_footer_selection();
                    return Some(None);
                }
                _ => {}
            }
        }
        let scroll = match (key.code, key.modifiers) {
            (KeyCode::PageUp | KeyCode::Char('k'), KeyModifiers::NONE)
            | (KeyCode::Char('p'), KeyModifiers::CONTROL) => Some(-1),
            (KeyCode::PageDown | KeyCode::Char('j'), KeyModifiers::NONE)
            | (KeyCode::Char('n'), KeyModifiers::CONTROL) => Some(1),
            _ => None,
        };
        if self.status_area.is_configured()
            && self.status_area.is_open()
            && self.footer_list_focused
            && let Some(amount) = scroll
        {
            let capacity = self.status_area.row_count(self.height).saturating_sub(1);
            self.status_area.scroll(amount, capacity);
            return Some(None);
        }
        let action = match (key.code, key.modifiers) {
            (KeyCode::Left | KeyCode::Char('h'), KeyModifiers::NONE) => {
                Some(FooterAction::Previous)
            }
            (KeyCode::Right | KeyCode::Char('l'), KeyModifiers::NONE) => Some(FooterAction::Next),
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
            | (KeyCode::Char('p'), KeyModifiers::CONTROL) => Some(FooterAction::Up),
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
            | (KeyCode::Char('n'), KeyModifiers::CONTROL) => Some(FooterAction::Down),
            (KeyCode::Enter, _) => Some(FooterAction::Open),
            (KeyCode::Esc, _) => Some(FooterAction::Clear),
            _ => None,
        };
        if let Some(action) = action {
            return Some(self.apply_footer_action(action));
        }
        if !key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char(_))
        {
            self.clear_footer_selection();
            return None;
        }
        Some(None)
    }

    fn hide_diff_screen(&mut self) {
        if let Some(screen) = self.diff_screen.take() {
            self.diff_screen_cache.insert(screen.chat_id, screen.dialog);
            self.rendered_region = None;
        }
    }

    fn open_diff_screen(&mut self, chat_id: Uuid, catalog: ChangeCatalog) {
        self.hide_diff_screen();
        let dialog = self.diff_screen_cache.remove(&chat_id).unwrap_or_default();
        self.diff_screen = Some(DiffScreen {
            chat_id,
            catalog,
            dialog,
        });
    }

    fn menu_comment_is_editing(&self) -> bool {
        self.menu_comment
            .as_ref()
            .is_some_and(|comment| comment.editing)
    }

    fn input_menu_active(&self) -> bool {
        self.menu.is_some()
            && self
                .menu_comment
                .as_ref()
                .is_some_and(|comment| comment.secret || comment.standalone_input)
    }

    fn navigate_menu_review(&mut self, action: DiffNavigationAction) {
        if self.menu_plan {
            let page = self.height.saturating_sub(3).max(1);
            self.context_offset = match action {
                DiffNavigationAction::Up => self.context_offset.saturating_sub(1),
                DiffNavigationAction::Down => self.context_offset.saturating_add(1),
                DiffNavigationAction::PageUp => self.context_offset.saturating_sub(page),
                DiffNavigationAction::PageDown => self.context_offset.saturating_add(page),
                DiffNavigationAction::Home => 0,
                DiffNavigationAction::End => usize::MAX,
            };
            return;
        }
        let Some(review_state) = &mut self.menu_review else {
            return;
        };
        review_state.navigation = match &review_state.review {
            Ok(review) => navigate_diff(
                review,
                &review_state.navigation,
                review_state.viewport,
                action,
            ),
            Err(error) => navigate_error(
                "Review unavailable",
                error,
                &review_state.navigation,
                review_state.viewport,
                action,
            ),
        };
    }

    fn diff_key(&mut self, key: KeyEvent) {
        let action = match (key.code, key.modifiers) {
            (KeyCode::Left, _) => Some(DiffDialogAction::Left),
            (KeyCode::Right, _) => Some(DiffDialogAction::Right),
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE) => Some(DiffDialogAction::Up),
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE) => {
                Some(DiffDialogAction::Down)
            }
            (KeyCode::PageUp, _) => Some(DiffDialogAction::PageUp),
            (KeyCode::PageDown, _) => Some(DiffDialogAction::PageDown),
            (KeyCode::Home, _) => Some(DiffDialogAction::Home),
            (KeyCode::End, _) => Some(DiffDialogAction::End),
            (KeyCode::Enter, _) => Some(DiffDialogAction::Enter),
            (KeyCode::Esc, _) => Some(DiffDialogAction::Escape),
            _ => None,
        };
        let Some(action) = action else {
            return;
        };
        let screen = self.diff_screen.as_mut().unwrap();
        let viewport = DiffViewport {
            width: self.width,
            height: self.height.saturating_sub(2),
        };
        if screen.dialog.apply(&screen.catalog, viewport, action) == DiffDialogOutcome::Close {
            self.hide_diff_screen();
        }
    }

    fn subscription_key(&mut self, key: KeyEvent) -> Option<InputEvent> {
        let focus = self.subscription_surface.as_ref()?.focus();
        let action = match focus {
            SubscriptionFocus::List => match (key.code, key.modifiers) {
                (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
                | (KeyCode::Char('p'), KeyModifiers::CONTROL) => SubscriptionAction::Previous,
                (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
                | (KeyCode::Char('n'), KeyModifiers::CONTROL) => SubscriptionAction::Next,
                (KeyCode::Enter, _) => SubscriptionAction::EditCadence,
                (KeyCode::Char('d'), KeyModifiers::NONE) => SubscriptionAction::RequestCancel,
                (KeyCode::Left | KeyCode::Esc, _) => SubscriptionAction::Escape,
                _ => return None,
            },
            SubscriptionFocus::Cadence => match (key.code, key.modifiers) {
                (KeyCode::Char(digit), KeyModifiers::NONE | KeyModifiers::SHIFT)
                    if digit.is_ascii_digit() =>
                {
                    SubscriptionAction::TypeDigit(digit)
                }
                (KeyCode::Backspace, _) => SubscriptionAction::DeleteDigit,
                (KeyCode::Enter, _) => SubscriptionAction::Confirm,
                (KeyCode::Esc, _) => SubscriptionAction::Escape,
                _ => return None,
            },
            SubscriptionFocus::CancelConfirmation => match (key.code, key.modifiers) {
                (KeyCode::Enter, _) | (KeyCode::Char('y'), KeyModifiers::NONE) => {
                    SubscriptionAction::Confirm
                }
                (KeyCode::Esc, _) | (KeyCode::Char('n'), KeyModifiers::NONE) => {
                    SubscriptionAction::Escape
                }
                _ => return None,
            },
        };
        let effect = self.subscription_surface.as_mut()?.apply(action);
        match effect {
            SubscriptionEffect::None => None,
            SubscriptionEffect::Close => {
                self.hide_subscription_view();
                self.menu_focused = self.menu.is_some() && self.editor.draft().is_empty();
                None
            }
            effect => Some(InputEvent::Subscription(effect)),
        }
    }

    fn task_surface_key(&mut self, key: KeyEvent) -> Option<InputEvent> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
            self.hide_task_surface();
            let id = self.start_transcript();
            return Some(InputEvent::OpenTranscript { id });
        }
        let focus = self.task_surface.as_ref()?.focus();
        let action = match focus {
            TaskSurfaceFocus::List => match (key.code, key.modifiers) {
                (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
                | (KeyCode::Char('p'), KeyModifiers::CONTROL) => TaskSurfaceAction::Previous,
                (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
                | (KeyCode::Char('n'), KeyModifiers::CONTROL) => TaskSurfaceAction::Next,
                (KeyCode::Enter, _) => TaskSurfaceAction::Open,
                (KeyCode::Left | KeyCode::Esc, _) => TaskSurfaceAction::Close,
                (KeyCode::Char('b'), KeyModifiers::NONE) => TaskSurfaceAction::Background,
                (KeyCode::Char('x'), KeyModifiers::NONE) => TaskSurfaceAction::Stop,
                _ => return None,
            },
            TaskSurfaceFocus::Detail { .. } => match (key.code, key.modifiers) {
                (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
                | (KeyCode::Char('p'), KeyModifiers::CONTROL) => TaskSurfaceAction::ScrollLines(-1),
                (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
                | (KeyCode::Char('n'), KeyModifiers::CONTROL) => TaskSurfaceAction::ScrollLines(1),
                (KeyCode::PageUp, _) => TaskSurfaceAction::Page(-1),
                (KeyCode::PageDown, _) => TaskSurfaceAction::Page(1),
                (KeyCode::Home, _) => TaskSurfaceAction::Home,
                (KeyCode::End, _) => TaskSurfaceAction::End,
                (KeyCode::Left, _) => TaskSurfaceAction::Back,
                (KeyCode::Esc | KeyCode::Enter, _) | (KeyCode::Char(' '), KeyModifiers::NONE) => {
                    TaskSurfaceAction::Close
                }
                (KeyCode::Char('b'), KeyModifiers::NONE) => TaskSurfaceAction::Background,
                (KeyCode::Char('x'), KeyModifiers::NONE) => TaskSurfaceAction::Stop,
                _ => return None,
            },
        };
        let viewport = task_surface_content_viewport(self.width, self.height);
        let effect = self.task_surface.as_mut()?.apply(action, viewport);
        match effect {
            TaskSurfaceEffect::None => None,
            TaskSurfaceEffect::Close => {
                self.hide_task_surface();
                self.rendered_region = None;
                self.menu_focused = self.menu.is_some() && self.editor.draft().is_empty();
                None
            }
            effect => Some(InputEvent::TaskSurface(effect)),
        }
    }

    fn hide_subscription_view(&mut self) {
        self.subscription_surface = None;
    }

    fn hide_task_surface(&mut self) {
        if let Some(surface) = self.task_surface.take() {
            self.task_surface_cache.insert(*surface.restore(), surface);
            self.status_area.reconcile_panel();
            self.footer.update(self.status_area.available_panels());
        }
    }

    /// Handles the keys a question menu adds; `Some` carries the event for a handled key.
    fn question_menu_key(&mut self, key: KeyEvent) -> Option<Option<InputEvent>> {
        let (entry_option, multi_select, navigable) =
            self.question_menu.as_ref().map(|question| {
                (
                    question.entry_option,
                    question.multi_select,
                    question.navigable,
                )
            })?;
        let (menu_id, selected) = self.menu.as_ref().map(|menu| (menu.id, menu.selected))?;
        if key.modifiers != KeyModifiers::NONE {
            return None;
        }
        let numbered_option = match key.code {
            KeyCode::Char(digit) => digit
                .to_digit(10)
                .and_then(|number| usize::try_from(number).ok())
                .and_then(|number| number.checked_sub(1)),
            _ => None,
        };
        match key.code {
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Right | KeyCode::Char('l')
                if navigable =>
            {
                let next = matches!(key.code, KeyCode::Right | KeyCode::Char('l'));
                self.begin_menu_submission(menu_id);
                Some(Some(InputEvent::SwitchQuestion { menu_id, next }))
            }
            KeyCode::Enter | KeyCode::Char(' ') if entry_option == Some(selected) => {
                self.toggle_menu_comment();
                Some(None)
            }
            KeyCode::Char(_) if numbered_option.is_some() && numbered_option == entry_option => {
                if let (Some(menu), Some(entry)) = (self.menu.as_mut(), entry_option) {
                    menu.selected = entry;
                }
                self.toggle_menu_comment();
                Some(None)
            }
            KeyCode::Char(' ') if multi_select => {
                self.begin_menu_submission(menu_id);
                Some(Some(InputEvent::Choice {
                    menu_id,
                    index: selected,
                    comment: None,
                }))
            }
            _ => None,
        }
    }

    fn selected_menu_accepts_comment(&self) -> bool {
        let Some(selected) = self.menu.as_ref().map(|menu| menu.selected) else {
            return false;
        };
        self.menu_comment
            .as_ref()
            .is_some_and(|comment| comment.options.contains(&selected))
    }

    fn picker_hint(&self) -> Option<&str> {
        self.picker_kind.map(|_| {
            self.menu
                .as_ref()
                .and_then(|menu| self.picker_presentation.row_hints.get(menu.selected))
                .map_or(self.picker_presentation.hint.as_str(), String::as_str)
        })
    }

    fn selected_menu_can_expand(&self) -> bool {
        self.menu
            .as_ref()
            .is_some_and(|menu| self.menu_expand_options.contains(&menu.selected))
    }

    fn toggle_menu_comment(&mut self) -> bool {
        let Some(selected) = self.menu.as_ref().map(|menu| menu.selected) else {
            return false;
        };
        let Some(comment) = self
            .menu_comment
            .as_mut()
            .filter(|comment| comment.options.contains(&selected))
        else {
            return false;
        };
        comment.editing = !comment.editing;
        comment.cursor = comment
            .values
            .get(&selected)
            .map_or(0, std::string::String::len);
        true
    }

    fn selected_menu_comment(&self) -> Option<String> {
        let selected = self.menu.as_ref()?.selected;
        self.menu_comment
            .as_ref()?
            .values
            .get(&selected)
            .cloned()
            .filter(|comment| !comment.trim().is_empty())
    }

    fn edit_menu_comment(&mut self, key: KeyEvent) -> bool {
        let Some(selected) = self.menu.as_ref().map(|menu| menu.selected) else {
            return false;
        };
        let Some(comment) = self.menu_comment.as_mut().filter(|comment| comment.editing) else {
            return false;
        };
        let value = comment.values.entry(selected).or_default();
        if comment.standalone_input && key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('a') => comment.cursor = 0,
                KeyCode::Char('e') => comment.cursor = value.len(),
                KeyCode::Char('b') => comment.cursor = previous_boundary(value, comment.cursor),
                KeyCode::Char('f') => comment.cursor = next_boundary(value, comment.cursor),
                KeyCode::Char('u') => {
                    value.drain(..comment.cursor);
                    comment.cursor = 0;
                }
                KeyCode::Char('k') => {
                    value.truncate(comment.cursor);
                }
                _ => {}
            }
            return true;
        }
        match key.code {
            KeyCode::Left => {
                comment.cursor = previous_boundary(value, comment.cursor);
            }
            KeyCode::Right => {
                comment.cursor = next_boundary(value, comment.cursor);
            }
            KeyCode::Home => comment.cursor = 0,
            KeyCode::End => comment.cursor = value.len(),
            KeyCode::Backspace => {
                let start = previous_boundary(value, comment.cursor);
                value.replace_range(start..comment.cursor, "");
                comment.cursor = start;
            }
            KeyCode::Delete => {
                let end = next_boundary(value, comment.cursor);
                value.replace_range(comment.cursor..end, "");
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                value.insert(comment.cursor, character);
                comment.cursor += character.len_utf8();
            }
            _ => {}
        }
        true
    }

    fn paste_menu_comment(&mut self, text: &str) -> bool {
        let Some(selected) = self.menu.as_ref().map(|menu| menu.selected) else {
            return false;
        };
        let Some(comment) = self.menu_comment.as_mut().filter(|comment| comment.editing) else {
            return false;
        };
        let text: String = if comment.secret {
            let trimmed = text.trim();
            if trimmed.chars().any(char::is_control) {
                self.status = SECRET_PASTE_ERROR.into();
                return true;
            }
            if self.status == SECRET_PASTE_ERROR {
                self.status.clear();
            }
            trimmed.to_owned()
        } else {
            text.replace(['\r', '\n'], " ")
                .chars()
                .map(|character| {
                    if character.is_control() && character != '\t' {
                        '�'
                    } else {
                        character
                    }
                })
                .collect()
        };
        let value = comment.values.entry(selected).or_default();
        value.insert_str(comment.cursor, &text);
        comment.cursor += text.len();
        true
    }

    fn paste(&mut self, text: &str) {
        if self.ordinary_input_is_visible() {
            self.clear_prompt_suggestion();
        }
        if self.request_layout.is_some() && self.menu.is_none()
            || self.workspace_trust.is_some()
            || self.full_review
            || self.input_dialog_visible()
                && self.menu.is_none()
                && self.artifacts_picker.is_none()
                && self.conversation_picker.is_none()
                && self.permissions_surface.is_none()
        {
            return;
        }
        if self.input_dialog_visible() && self.menu.is_some() && !self.review_focused {
            self.menu_focused = true;
        }
        self.last_input_edit = Some(Instant::now());
        if self.branch_submission_owns_input() {
            return;
        }
        self.clear_footer_selection();
        if self.menu_submission_is_pending() {
            return;
        }
        if self.menu.is_none()
            && let Some(picker) = &mut self.artifacts_picker
        {
            picker.paste(text);
            return;
        }
        if self.menu.is_none()
            && let Some(picker) = &mut self.conversation_picker
        {
            picker.paste(text);
            return;
        }
        if !self.input_dialog_visible() && self.tick_list.focused && self.tick_list_visible() {
            return;
        }
        if self.input_menu_active() {
            self.paste_menu_comment(text);
            return;
        }
        if self.menu.is_none() && self.foreground_shell {
            self.shell_background_chord_started_at = None;
            return;
        }
        if self.menu.is_none()
            && let Some(surface) = &mut self.permissions_surface
        {
            surface.paste(text);
            return;
        }
        if self.menu.is_none()
            && (self.task_surface.is_some() || self.subscription_surface.is_some())
        {
            return;
        }
        if let Some((_, transcript)) = &mut self.transcript {
            transcript.paste(text);
            return;
        }
        if self.menu.is_none() && self.history_search.is_some() {
            self.apply_history_search(HistorySearchInput::Insert(text.into()));
            return;
        }
        if self.paste_menu_comment(text) {
            return;
        }
        if self.menu_focused {
            return;
        }
        if self.prompt_enabled {
            self.menu_focused = false;
            let text: String = text
                .replace("\r\n", "\n")
                .replace('\r', "\n")
                .chars()
                .map(|character| {
                    if character.is_control() && !matches!(character, '\n' | '\t') {
                        '�'
                    } else {
                        character
                    }
                })
                .collect();
            let mut refused_image_path = false;
            if self.shell_input.kind() == InputKind::Prompt {
                if text.is_empty() {
                    if self.empty_paste_reads_clipboard_image {
                        self.paste_clipboard_image_after_empty_paste();
                    }
                    return;
                }
                if self.accepts_images {
                    if self.paste_with_image_paths(&text) {
                        return;
                    }
                } else {
                    refused_image_path = find_image_paths(&text, self.working_directory())
                        .iter()
                        .any(|candidate| candidate.path.is_file());
                }
            }
            let action = if self.menu.is_none() {
                self.shell_input
                    .try_tty_paste(self.editor.draft(), self.editor.cursor(), &text)
            } else {
                ShellInputAction::Ignored
            };
            if action == ShellInputAction::Ignored {
                self.insert_pasted_text(&text);
            } else {
                self.apply_shell_action(action);
                if PasteBlocks::is_large_paste(self.editor.draft()) {
                    match self
                        .paste_blocks
                        .register(self.editor.draft(), 0..self.editor.draft().len())
                    {
                        Ok(PasteRegistration::Folded(id)) => {
                            self.status = format!("Pasted text #{} folded", id.get());
                        }
                        Ok(PasteRegistration::Inline) => {}
                        Err(error) => self.status = format!("Cannot fold pasted text: {error}"),
                    }
                }
            }
            if refused_image_path {
                self.status = IMAGE_INPUT_UNSUPPORTED.into();
            }
        }
    }

    fn insert_pasted_text(&mut self, text: &str) {
        let start = self.editor.cursor();
        self.insert_pasted(text);
        let end = self.editor.cursor();
        match self
            .paste_blocks
            .register(self.editor.draft(), start.min(end)..start.max(end))
        {
            Ok(PasteRegistration::Folded(id)) => {
                self.status = format!("Pasted text #{} folded", id.get());
            }
            Ok(PasteRegistration::Inline) => {}
            Err(error) => self.status = format!("Cannot fold pasted text: {error}"),
        }
    }

    /// Claude Code `usePasteHandler`: pasted or dragged image files become `[Image #N]` blocks
    /// where their paths were, and the text around them is pasted as usual. A path that is
    /// missing, unreadable or not an image stays text. Returns false when no image was
    /// attached, leaving the whole paste to the plain text path.
    fn paste_with_image_paths(&mut self, text: &str) -> bool {
        let mut images = Vec::new();
        for candidate in find_image_paths(text, self.working_directory()) {
            match read_pasted_image(&candidate.path) {
                Some(Ok(image)) => images.push((candidate.range, image)),
                Some(Err(error)) => {
                    self.status = format!(
                        "Cannot attach {} as an image: {error}",
                        candidate.path.display()
                    );
                }
                None => {}
            }
        }
        if images.is_empty() {
            return false;
        }
        let mut pasted_through = 0;
        for (range, image) in images {
            if range.start > pasted_through {
                self.insert_pasted_text(&text[pasted_through..range.start]);
            }
            self.attach_image(image);
            pasted_through = range.end;
        }
        if pasted_through < text.len() {
            self.insert_pasted_text(&text[pasted_through..]);
        }
        true
    }

    /// Unlike `Ctrl+V`, an empty paste is silent when the clipboard holds no image: the user
    /// only pasted, and nothing was there to paste.
    fn paste_clipboard_image_after_empty_paste(&mut self) {
        match self.clipboard.read_image() {
            Ok(Some(_)) if !self.accepts_images => self.status = IMAGE_INPUT_UNSUPPORTED.into(),
            Ok(Some(image)) => self.attach_image(image),
            Ok(None) => {}
            Err(error) => self.status = format!("Cannot paste image: {error}"),
        }
    }

    fn command_scope(&self) -> CommandScope {
        if self.overview.projection.is_some() {
            CommandScope::Overview
        } else if self.visible_delegated {
            CommandScope::Child
        } else {
            CommandScope::Root
        }
    }

    /// Shift+Tab is the key form of `/mode` and follows its scope rule: refused in the overview,
    /// which shows no Agent, and on a child, whose mode is fixed. Returns whether it was refused.
    fn refuse_mode_cycle(&mut self) -> bool {
        let Some((_, reason)) = unavailable_command("/mode", self.command_scope()) else {
            return false;
        };
        self.hold_status(&super::completion::command_refusal(
            self.command_scope(),
            "Shift+Tab mode",
            reason,
        ));
        true
    }

    /// Where relative paths in the input resolve: the Chat's working directory.
    fn working_directory(&self) -> &Path {
        self.mention_roots
            .as_ref()
            .map_or(self.history_project.as_path(), |(workdir, _)| {
                workdir.as_path()
            })
    }

    fn start_transcript(&mut self) -> Uuid {
        let id = Uuid::new_v4();
        self.shell_background_chord_started_at = None;
        self.clear_footer_selection();

        self.transcript = Some((
            id,
            Transcript::new(vec!["Loading conversation…".into()].into()),
        ));
        self.completion.dismiss();
        self.file_mentions.dismiss();
        id
    }

    /// 即将从草稿进入上箭头历史时，从持久化历史重新载入列表。
    fn load_up_arrow_history(&mut self, wrap_width: usize) {
        if self.editor.is_browsing_history() || !self.editor.is_on_first_visual_row(wrap_width) {
            return;
        }
        let Some(history) = &self.input_history else {
            return;
        };
        match history.recent_prompts(&self.history_session, &self.history_project) {
            Ok(prompts) => self
                .editor
                .replace_history(prompts.into_iter().rev().collect()),
            Err(error) => self.status = format!("Cannot read input history: {error}"),
        }
    }

    /// Folded text only: an image placeholder stays literal in the input history and its
    /// image is not saved there.
    fn pasted_texts(&self) -> Vec<PastedText> {
        self.paste_blocks
            .projection(self.editor.draft(), 0, usize::MAX)
            .blocks
            .into_iter()
            .filter(|block| !block.is_image)
            .map(|block| PastedText {
                id: block.id.get(),
                range: block.raw_range,
            })
            .collect()
    }

    fn open_history_search(&mut self) {
        self.clear_footer_selection();
        let Some(history) = &self.input_history else {
            self.status = "Input history is unavailable".into();
            return;
        };
        match HistorySearch::open(
            history,
            &self.history_session,
            &self.history_project,
            self.editor.draft(),
            self.editor.cursor(),
            "",
            100,
        ) {
            Ok(search) => {
                self.history_search = Some(search);
                self.completion.dismiss();
                self.file_mentions.dismiss();
            }
            Err(error) => self.status = format!("Cannot search input history: {error}"),
        }
    }

    fn apply_history_search(&mut self, input: HistorySearchInput) {
        let (Some(mut search), Some(history)) =
            (self.history_search.take(), self.input_history.as_ref())
        else {
            return;
        };
        match search.apply(history, input) {
            Ok(HistorySearchOutcome::Open) => self.history_search = Some(search),
            Ok(HistorySearchOutcome::Accepted(draft)) => {
                self.replace_draft(draft.text, draft.cursor);
            }
            Ok(HistorySearchOutcome::Cancelled(_)) => self.refresh_completion(),
            Err(error) => {
                self.status = format!("Cannot search input history: {error}");
                self.history_search = Some(search);
            }
        }
    }

    fn edit_in_external_editor(&mut self) {
        let menu_input = self.input_menu_active()
            && self.menu_comment.as_ref().is_some_and(|comment| {
                (comment.standalone_input || comment.secret) && comment.editing
            });
        let draft = if menu_input {
            self.selected_menu_comment().unwrap_or_default()
        } else {
            self.editor.draft().to_owned()
        };
        let Some(outcome) = self.run_external_editor(&draft) else {
            return;
        };
        match outcome {
            ExternalEditorOutcome::Updated { content } => {
                if menu_input {
                    let content = content.trim();
                    if content.chars().any(char::is_control) {
                        self.status = "Enter a single value without control characters.".into();
                        return;
                    }
                    let selected = self.menu.as_ref().unwrap().selected;
                    let comment = self.menu_comment.as_mut().unwrap();
                    comment.values.insert(selected, content.into());
                    comment.cursor = content.len();
                    self.status = format!("{} updated by external editor", comment.label);
                } else {
                    self.replace_draft(content.clone(), content.len());
                    self.status = "Draft updated by external editor".into();
                }
            }
            ExternalEditorOutcome::Unchanged => {
                self.status = "External editor left the draft unchanged".into();
            }
            ExternalEditorOutcome::Cancelled { signal } => {
                self.status = format!("External editor cancelled · signal {signal:?}");
            }
            ExternalEditorOutcome::Unavailable => {
                self.status = "No external editor found; set VISUAL or EDITOR".into();
            }
            ExternalEditorOutcome::Failed(error) => {
                self.status = format!("External editor failed: {error}");
            }
        }
    }

    /// Returns `None` when the terminal could not be handed over or taken back;
    /// the status line then carries the reason.
    fn run_external_editor(&mut self, text: &str) -> Option<ExternalEditorOutcome> {
        match &self.external_editor {
            ExternalEditorLauncher::Terminal => self.edit_text_in_terminal(text),
            #[cfg(test)]
            ExternalEditorLauncher::Scripted {
                opened_drafts,
                outcome,
            } => {
                opened_drafts.lock().unwrap().push(text.to_owned());
                Some(outcome.clone())
            }
        }
    }

    /// Returns `None` when the terminal could not be handed over or taken back;
    /// the status line then carries the reason.
    fn edit_text_in_terminal(&mut self, draft: &str) -> Option<ExternalEditorOutcome> {
        let working_directory = self
            .mention_roots
            .as_ref()
            .map(|(workdir, _)| workdir.clone())
            .or_else(|| {
                (!self.history_project.as_os_str().is_empty()).then(|| self.history_project.clone())
            })
            .unwrap_or_else(|| PathBuf::from("."));
        let mut output = io::stdout().lock();
        // The editor gets the user's own main buffer, not the screen this
        // console is drawing on.
        if let Err(error) = self
            .surface
            .release(self.renderer, &mut output)
            .and_then(|()| write!(output, "{RESET_SCROLL_REGION}"))
            .and_then(|()| disable_input_reporting(&mut output))
            .and_then(|()| execute!(output, Show))
            .and_then(|()| output.flush())
        {
            // Part of the hand-over may have reached the terminal.
            self.rendered_region = None;
            self.status = format!("Cannot prepare terminal for editor: {error}");
            return None;
        }
        if let Err(error) = terminal::disable_raw_mode() {
            let _ = self.surface.take(self.renderer, &mut output);
            let _ = enable_input_reporting(&mut output);
            // Taking the alternate screen again starts it blank.
            self.rendered_region = None;
            self.status = format!("Cannot leave raw mode for editor: {error}");
            return None;
        }
        let outcome = edit_prompt(draft, &working_directory);
        let raw_result = terminal::enable_raw_mode();
        let screen_result = self.surface.take(self.renderer, &mut output);
        let paste_result = enable_input_reporting(&mut output);
        self.screen_initialized = false;
        self.rendered_region = None;
        if let Err(error) = raw_result.and(screen_result).and(paste_result) {
            self.status = format!("Cannot restore terminal after editor: {error}");
            return None;
        }
        Some(outcome)
    }

    fn can_edit_in_external_editor(&self) -> bool {
        self.prompt_enabled
            && self.menu.is_none()
            && self.conversation_picker.is_none()
            && self.transcript.is_none()
            && self.history_search.is_none()
            && !self.foreground_shell
    }

    /// Every key belongs to the open picker, so none reaches the draft beneath it.
    fn conversation_picker_key(&mut self, key: KeyEvent) -> Option<InputEvent> {
        self.escape_confirmation = None;
        self.exit_confirmation = None;
        #[cfg(unix)]
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('z') {
            return Some(InputEvent::Suspend);
        }
        let picker = self.conversation_picker.as_mut()?;
        let menu_id = picker.id();
        match picker.key(key) {
            ConversationPickerEffect::None => None,
            ConversationPickerEffect::EditQuery => {
                self.edit_picker_query_in_external_editor();
                None
            }
            ConversationPickerEffect::Open(index) => {
                self.begin_menu_submission(menu_id);
                Some(InputEvent::Choice {
                    menu_id,
                    index,
                    comment: None,
                })
            }
            ConversationPickerEffect::Cancel => {
                self.begin_menu_submission(menu_id);
                Some(InputEvent::Cancel { menu_id })
            }
        }
    }

    fn edit_picker_query_in_external_editor(&mut self) {
        let Some(query) = self
            .conversation_picker
            .as_ref()
            .map(|picker| picker.query().to_owned())
        else {
            return;
        };
        let outcome = self.run_external_editor(&query);
        let Some(picker) = self.conversation_picker.as_mut() else {
            return;
        };
        match outcome {
            Some(ExternalEditorOutcome::Updated { content }) => picker.replace_query_text(&content),
            Some(ExternalEditorOutcome::Unchanged) => {}
            Some(ExternalEditorOutcome::Cancelled { signal }) => {
                picker.set_notice(format!("External editor cancelled · signal {signal:?}"));
            }
            Some(ExternalEditorOutcome::Unavailable) => {
                picker.set_notice("No external editor found; set VISUAL or EDITOR".into());
            }
            Some(ExternalEditorOutcome::Failed(error)) => {
                picker.set_notice(format!("External editor failed: {error}"));
            }
            None => picker.set_notice(self.status.clone()),
        }
    }

    fn history_search_key(&mut self, key: KeyEvent) {
        let input = if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('s') => Some(HistorySearchInput::CycleScope),
                KeyCode::Char('p') => Some(HistorySearchInput::Previous),
                KeyCode::Char('n') => Some(HistorySearchInput::Next),
                _ => None,
            }
        } else {
            match key.code {
                KeyCode::Up => Some(HistorySearchInput::Previous),
                KeyCode::Down => Some(HistorySearchInput::Next),
                KeyCode::Enter => Some(HistorySearchInput::AcceptEnter),
                KeyCode::Tab => Some(HistorySearchInput::AcceptTab),
                KeyCode::Esc => Some(HistorySearchInput::Cancel),
                KeyCode::Backspace => Some(HistorySearchInput::Backspace),
                KeyCode::Char(character) => Some(HistorySearchInput::Insert(character.to_string())),
                _ => None,
            }
        };
        if let Some(input) = input {
            self.apply_history_search(input);
        }
    }

    fn pending_exit_control(&self) -> Option<char> {
        self.exit_confirmation
            .filter(|confirmation| confirmation.requested_at.elapsed() <= EXIT_CONFIRMATION_WINDOW)
            .map(|confirmation| confirmation.control)
    }

    fn request_exit(&mut self, control: char) -> bool {
        if self.pending_exit_control() == Some(control) {
            self.exit_confirmation = None;
            true
        } else {
            self.exit_confirmation = Some(ExitConfirmation {
                control,
                requested_at: Instant::now(),
            });
            false
        }
    }

    fn pending_escape_action(&self) -> Option<EscapeAction> {
        self.escape_confirmation
            .filter(|confirmation| {
                confirmation.requested_at.elapsed() <= ESCAPE_CONFIRMATION_WINDOW
            })
            .map(|confirmation| confirmation.action)
    }

    /// The key confirmation in progress. It stays visible whichever footer is
    /// drawn, since a press without a visible effect reads as no reaction.
    fn confirmation_hint(&self) -> Option<String> {
        if let Some(control) = self.pending_exit_control() {
            return Some(if control == 'C' {
                "Press Ctrl+C again to exit".into()
            } else {
                "Press Ctrl+D again to exit".into()
            });
        }
        self.pending_escape_action().map(|action| match action {
            EscapeAction::ClearDraft => "Press Esc again to clear".into(),
            EscapeAction::OpenRewind => "Press Esc again to rewind".into(),
        })
    }

    fn status_notice(&self, status: &str) -> String {
        let Some(hint) = self.confirmation_hint() else {
            return status.to_owned();
        };
        let full = if status.is_empty() {
            hint.clone()
        } else {
            format!("{status} · {hint}")
        };
        let available = if self.status_area.is_configured() {
            self.status_area.notice_columns(self.width)
        } else {
            self.width
        };
        if display_width(&full) <= available {
            return full;
        }
        let compact_hint = if let Some(control) = self.pending_exit_control() {
            format!("Ctrl+{control} again exit")
        } else if let Some(action) = self.pending_escape_action() {
            match action {
                EscapeAction::ClearDraft => "Esc again clear".into(),
                EscapeAction::OpenRewind => "Esc again rewind".into(),
            }
        } else {
            "Opening panel · Esc input".into()
        };
        if status.is_empty() {
            return compact_hint;
        }
        let status = if status.starts_with("Not saved:") {
            "Not saved"
        } else {
            status
        };
        let (available, compact_hint) = if display_width(status) + 3 + display_width(&compact_hint)
            > available
            && let Some(control) = self.pending_exit_control()
        {
            (self.width, format!("Ctrl+{control} exit"))
        } else {
            (available, compact_hint)
        };
        let status = truncate_width(
            status,
            available.saturating_sub(display_width(&compact_hint) + 3),
        );
        if status.is_empty() {
            compact_hint
        } else {
            format!("{status} · {compact_hint}")
        }
    }

    /// Drops key confirmations whose window has passed. Returns whether one
    /// did, so its hint leaves the screen.
    fn expire_key_confirmations(&mut self) -> bool {
        let exit_expired =
            self.exit_confirmation.is_some() && self.pending_exit_control().is_none();
        if exit_expired {
            self.exit_confirmation = None;
        }
        let escape_expired =
            self.escape_confirmation.is_some() && self.pending_escape_action().is_none();
        if escape_expired {
            self.escape_confirmation = None;
        }
        exit_expired || escape_expired
    }

    fn request_escape(&mut self, action: EscapeAction) -> bool {
        if self.pending_escape_action() == Some(action) {
            self.escape_confirmation = None;
            true
        } else {
            self.escape_confirmation = Some(EscapeConfirmation {
                action,
                requested_at: Instant::now(),
            });
            false
        }
    }

    fn clear_draft_to_history(&mut self) {
        let draft = self.editor.draft().to_owned();
        let pasted_texts = self.pasted_texts();
        if !draft.trim().is_empty()
            && let Some(history) = &mut self.input_history
            && let Err(error) = history.record(
                &self.history_session,
                &self.history_project,
                &draft,
                &pasted_texts,
            )
        {
            self.status = format!("Cannot save cleared draft: {error}");
            return;
        }
        let EditorOutcome::Submitted(_) = self.editor.apply(EditorCommand::Submit) else {
            unreachable!()
        };
        self.paste_blocks.clear();
        self.restored_contexts.clear();
        self.completion.dismiss();
        self.file_mentions.update_at("", 0, 15);
        self.status = "Draft cleared · Up restores it".into();
    }

    fn main_escape(&mut self) -> Option<InputEvent> {
        if self.ordinary_input_is_visible() {
            if !self.scrollback.is_at_bottom() {
                self.scroll_conversation(ScrollAction::Last);
                self.escape_confirmation = None;
                return None;
            }
            if self.activity_interruptible && self.scrollback.activity_line().is_some() {
                self.escape_confirmation = None;
                return Some(InputEvent::InterruptActivity);
            }
        }
        let action = if self.editor.draft().is_empty() {
            EscapeAction::OpenRewind
        } else {
            EscapeAction::ClearDraft
        };
        if !self.request_escape(action) {
            return Some(InputEvent::Escape);
        }
        match action {
            EscapeAction::ClearDraft => {
                self.clear_draft_to_history();
                None
            }
            EscapeAction::OpenRewind => Some(InputEvent::OpenRewind),
        }
    }

    fn ordinary_input_is_visible(&self) -> bool {
        self.prompt_enabled
            && self.overview.projection.is_none()
            && self.menu.is_none()
            && self.conversation_picker.is_none()
            && self.transcript.is_none()
            && self.task_surface.is_none()
            && self.diff_screen.is_none()
            && self.subscription_surface.is_none()
            && self.permissions_surface.is_none()
            && self.history_search.is_none()
            && !self.help_open
            && !self.foreground_shell
            && !self.review_focused
            && self.footer.selected().is_none()
            && self.shell_input.kind() == InputKind::Prompt
            && self.shell_input.active_completion().is_none()
            && !self.file_mentions.is_open()
            && !self.completion.is_open()
    }

    fn back_to_bottom_label(&self) -> Option<String> {
        let arrow = self.display.glyph(Glyph::ScrollDown);
        [
            format!("{arrow} Back to bottom · Esc"),
            format!("{arrow} Bottom · Esc"),
            format!("{arrow} Esc"),
            "Esc".into(),
        ]
        .into_iter()
        .find(|label| display_width(label) <= self.width)
    }

    fn overview_key(&mut self, key: KeyEvent) -> Option<Option<InputEvent>> {
        self.overview.projection.as_ref()?;
        if self.overview.input_focused
            && (self.shell_input.active_completion().is_some()
                || self.completion.is_open()
                || self.file_mentions.is_open())
        {
            return None;
        }
        if key.code == KeyCode::Esc {
            return Some(Some(InputEvent::CloseOverview));
        }
        if self.overview.input_focused {
            if key.code == KeyCode::Tab || key.code == KeyCode::Up && self.editor.draft().is_empty()
            {
                self.overview.input_focused = false;
                return Some(None);
            }
            return None;
        }
        let page = self.overview.capacity.max(1) as isize;
        match (key.code, key.modifiers) {
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
            | (KeyCode::Char('n'), KeyModifiers::CONTROL) => self.overview.move_selection(1),
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
            | (KeyCode::Char('p'), KeyModifiers::CONTROL) => self.overview.move_selection(-1),
            (KeyCode::PageDown, _) => self.overview.move_selection(page),
            (KeyCode::PageUp, _) => self.overview.move_selection(-page),
            (KeyCode::Enter | KeyCode::Right, _) => match self.overview.selected {
                Some(OverviewTarget::Agent(id)) => return Some(Some(InputEvent::OpenAgent(id))),
                Some(OverviewTarget::CurrentDraft) => return Some(Some(InputEvent::CloseOverview)),
                Some(OverviewTarget::Heading(_) | OverviewTarget::Disclosure(_)) => {
                    self.overview.toggle_selected()
                }
                None => {}
            },
            (KeyCode::Char(' '), KeyModifiers::NONE) => self.overview.toggle_selected(),
            (KeyCode::Char('p'), KeyModifiers::NONE) => self.overview.cycle_filter(true),
            (KeyCode::Char('w'), KeyModifiers::NONE) => self.overview.cycle_filter(false),
            (KeyCode::Char('r'), KeyModifiers::NONE) => {
                return Some(Some(InputEvent::Line("/resume".into())));
            }
            (KeyCode::Tab, _) => self.overview.input_focused = true,
            (KeyCode::BackTab, _) => {
                self.refuse_mode_cycle();
            }
            _ => {}
        }
        Some(None)
    }

    fn key(&mut self, key: KeyEvent) -> Option<InputEvent> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if self.ordinary_input_is_visible() && matches!(key.code, KeyCode::Char(_)) {
            self.clear_prompt_suggestion();
        }
        let dialog_height = self.input_dialog_body_height();
        if let Some(dialog) = &mut self.workspace_trust {
            if self.menu_submission_pending.is_some() {
                return None;
            }
            return dialog
                .key(key, self.width, dialog_height, self.display)
                .map(|decision| match decision {
                    TrustDecision::Accept => InputEvent::Choice {
                        menu_id: dialog.id,
                        index: TRUST_ACCEPT_CHOICE_INDEX,
                        comment: None,
                    },
                    TrustDecision::Cancel => InputEvent::Cancel { menu_id: dialog.id },
                });
        }
        if self.full_review {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('v'))
                && key.modifiers == KeyModifiers::NONE
            {
                self.full_review = false;
            } else if let Some(action) = Self::review_navigation_key(key) {
                self.navigate_menu_review(action);
            }
            return None;
        }
        if key.code == KeyCode::Char('v')
            && key.modifiers == KeyModifiers::NONE
            && (self.menu_review.is_some() || self.menu_plan)
            && !self.menu_comment_is_editing()
        {
            self.full_review = true;
            return None;
        }
        self.status_held = false;
        // Legacy terminals send Ctrl+H as 0x08; crossterm reports Ctrl+h, which Readline treats as Backspace.
        let key = if key.code == KeyCode::Char('h') && key.modifiers == KeyModifiers::CONTROL {
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)
        } else {
            key
        };
        if self.menu_submission_is_pending()
            || self.branch_submission_owns_input()
            || self.request_layout.is_some() && self.menu.is_none()
        {
            return None;
        }
        if self.input_dialog_visible() && self.menu.is_some() && !self.review_focused {
            self.menu_focused = true;
        }
        if self.menu.is_none()
            && let Some(picker) = &mut self.artifacts_picker
        {
            return match picker.key(key) {
                super::artifacts_picker::ArtifactPickerEffect::None => None,
                super::artifacts_picker::ArtifactPickerEffect::Choice(choice) => {
                    Some(InputEvent::ArtifactChoice(choice))
                }
                super::artifacts_picker::ArtifactPickerEffect::Close => {
                    self.artifacts_picker = None;
                    self.rendered_region = None;
                    None
                }
                super::artifacts_picker::ArtifactPickerEffect::EditQuery => {
                    let draft = picker.external_query();
                    match edit_prompt(&draft, &self.history_project) {
                        ExternalEditorOutcome::Updated { content: text } => {
                            self.artifacts_picker.as_mut().unwrap().replace_query(&text)
                        }
                        ExternalEditorOutcome::Failed(error) => self
                            .artifacts_picker
                            .as_mut()
                            .unwrap()
                            .report(error.to_string()),
                        _ => {}
                    }
                    None
                }
            };
        }
        if self.menu.is_none() && self.conversation_picker.is_some() {
            return self.conversation_picker_key(key);
        }
        if self.menu.is_none()
            && self.input_dialog_body_height() < 3
            && key.code == KeyCode::Enter
            && (self.tick_list.confirmation.is_some()
                || self
                    .tick_list
                    .detail
                    .as_ref()
                    .is_some_and(|detail| detail.confirmation().is_some()))
        {
            return None;
        }
        if self.menu.is_none() && self.tick_list.confirmation.is_some() {
            if key.code == KeyCode::Esc {
                self.tick_list.confirmation = None;
            } else if key.code == KeyCode::Enter {
                let confirmation = self.tick_list.confirmation.take().unwrap();
                let chat = self.chat?;
                match confirmation
                    .action
                    .prepare(&mut super::tick_actions::TickActionUi { state: self })
                {
                    Ok(action) => {
                        debug_assert_eq!(action.instance, confirmation.instance);
                        return Some(InputEvent::TickAction { chat, action });
                    }
                    Err(error) => self.hold_status(&error),
                }
            }
            return None;
        }
        if (!self.input_dialog_visible()
            || self
                .tick_list
                .detail
                .as_ref()
                .is_some_and(|detail| detail.confirmation().is_some()))
            && self.menu.is_none()
            && !self.help_open
            && self.tick_list_visible()
            && self.tick_list.focused
        {
            let width = TICK_LIST_COLUMNS.min(self.width).saturating_sub(2);
            let height = self.height.saturating_sub(self.styled_frame().0.len());
            if let Some(detail) = self.tick_list.detail.as_mut() {
                let chat = detail.chat();
                let result = detail.key(key, width, height);
                return match result {
                    super::tick_actions::TickDetailKey::None => None,
                    super::tick_actions::TickDetailKey::Back => {
                        self.tick_list.detail = None;
                        self.rendered_region = None;
                        None
                    }
                    super::tick_actions::TickDetailKey::Action(action) => {
                        Some(InputEvent::TickAction { chat, action })
                    }
                };
            }
            match (key.code, key.modifiers) {
                (KeyCode::Char('r'), KeyModifiers::NONE) => {
                    let chat = self.chat?;
                    let executor = self.tick_list.refresh_action();
                    let action = executor
                        .prepare(&mut super::tick_actions::TickActionUi { state: self })
                        .ok()?;
                    return Some(InputEvent::TickAction { chat, action });
                }
                (KeyCode::Esc | KeyCode::Left, _) => {
                    self.tick_list.focused = false;
                    self.tick_list.overlay = false;
                    self.rendered_region = None;
                }
                (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
                | (KeyCode::Char('n'), KeyModifiers::CONTROL) => self.tick_list.move_selection(1),
                (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
                | (KeyCode::Char('p'), KeyModifiers::CONTROL) => self.tick_list.move_selection(-1),
                (KeyCode::Enter, _) | (KeyCode::Char(_), KeyModifiers::NONE) => {
                    if let Some(entry) = self.tick_list.selected().cloned() {
                        let action_key = match key.code {
                            KeyCode::Enter => 'o',
                            KeyCode::Char(key) => key,
                            _ => unreachable!(),
                        };
                        let action = entry
                            .actions
                            .iter()
                            .find(|action| action.key == action_key)?;
                        match &action.operation {
                            crate::tick_adapter::TickListOperation::Execute(executor) => {
                                let chat = self.chat?;
                                match executor
                                    .prepare(&mut super::tick_actions::TickActionUi { state: self })
                                {
                                    Ok(action) => {
                                        return Some(InputEvent::TickAction { chat, action });
                                    }
                                    Err(error) => self.hold_status(&error),
                                }
                            }
                            crate::tick_adapter::TickListOperation::RemoveLocal => {
                                self.tick_list.remove_local(&entry.id)
                            }
                            crate::tick_adapter::TickListOperation::Notify(message) => {
                                self.hold_status(message)
                            }
                        }
                    }
                }
                _ => {}
            }
            return None;
        }
        if self.visible_prompt_suggestion().is_some()
            && key.modifiers == KeyModifiers::NONE
            && matches!(key.code, KeyCode::Tab | KeyCode::Right | KeyCode::Enter)
        {
            let text = self
                .prompt_suggestion
                .take()
                .expect("visible suggestion exists");
            self.clear_prompt_suggestion();
            self.insert(&text);
            if key.code != KeyCode::Enter {
                return None;
            }
        }
        if self.chat.is_some()
            && !self.tick_list.entries().is_empty()
            && self.ordinary_input_is_visible()
            && !self.conversation_focused
            && !self.footer_list_focused
            && (self.editor.draft().is_empty() || key.modifiers == KeyModifiers::SHIFT)
            && !self.completion.is_open()
            && !self.file_mentions.is_open()
            && ((key.code == KeyCode::Right && key.modifiers == KeyModifiers::NONE)
                || matches!(
                    (key.code, key.modifiers),
                    (KeyCode::Up | KeyCode::Down, KeyModifiers::SHIFT)
                ))
        {
            let overlay = self.width + 1 < TICK_LIST_AUTO_MIN_COLUMNS
                || self.tick_list_preference
                    == tickloop_agent::settings::TickListPreference::Hidden;
            let group = (key.modifiers == KeyModifiers::SHIFT).then_some(TickGroup::Shells);
            self.tick_list.focus(overlay, group);
            let chat = self.chat?;
            let executor = self.tick_list.refresh_action();
            let action = executor
                .prepare(&mut super::tick_actions::TickActionUi { state: self })
                .ok()?;
            return Some(InputEvent::TickAction { chat, action });
        }
        if self.menu.is_none()
            && self.transcript.is_none()
            && self.task_surface.is_none()
            && self.diff_screen.is_none()
            && self.subscription_surface.is_none()
            && self.permissions_surface.is_none()
            && self.history_search.is_none()
        {
            if self.help_open {
                match (key.code, key.modifiers) {
                    (KeyCode::Esc | KeyCode::Char('?'), _) => self.help_open = false,
                    (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
                    | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                        self.help_offset = self.help_offset.saturating_add(1)
                    }
                    (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
                    | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                        self.help_offset = self.help_offset.saturating_sub(1)
                    }
                    _ => {}
                }
                return None;
            }
            if key.code == KeyCode::Char('?')
                && key.modifiers == KeyModifiers::NONE
                && (self.overview.projection.is_some() && !self.overview.input_focused
                    || self.editor.draft().is_empty()
                        && self.shell_input.kind() == InputKind::Prompt)
                && self.candidate_rows(1).is_empty()
            {
                self.help_open = true;
                self.help_offset = 0;
                return None;
            }
            if let Some(event) = self.overview_key(key) {
                return event;
            }
            if self.conversation_focused {
                match (key.code, key.modifiers) {
                    (KeyCode::Esc, _) => {}
                    (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
                    | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                        self.scrollback.scroll(ScrollAction::LinesDown(1));
                        return None;
                    }
                    (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
                    | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                        self.scrollback.scroll(ScrollAction::LinesUp(1));
                        return None;
                    }
                    (KeyCode::PageUp | KeyCode::PageDown, _)
                    | (KeyCode::End, KeyModifiers::CONTROL) => {}
                    _ => return None,
                }
            }
        }
        if self.input_menu_active() {
            self.menu_focused = true;
            self.review_focused = false;
            if key.code == KeyCode::BackTab {
                if self.menu_comment_is_editing() {
                    self.toggle_menu_comment();
                }
                return None;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && self.menu_comment.as_ref().is_some_and(|comment| {
                    (comment.standalone_input || comment.secret) && comment.editing
                })
            {
                if key.code == KeyCode::Char('g') {
                    self.edit_in_external_editor();
                } else {
                    self.edit_menu_comment(key);
                }
                return None;
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && !matches!(key.code, KeyCode::Char('n' | 'p'))
            {
                return None;
            }
        }
        self.expire_shell_background_chord();
        if self.shell_background_chord_started_at.is_some()
            && (!self.foreground_shell
                || self.menu.is_some()
                || self.diff_screen.is_some()
                || self.task_surface.is_some()
                || self.subscription_surface.is_some()
                || self.permissions_surface.is_some()
                || self.transcript.is_some()
                || self.history_search.is_some())
        {
            self.shell_background_chord_started_at = None;
        }
        self.clear_footer_selection_if_hidden();
        if let Some(started_at) = self.shell_background_chord_started_at.take() {
            if started_at.elapsed() <= SHELL_BACKGROUND_CHORD_WINDOW
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && key.code == KeyCode::Char('b')
            {
                return Some(InputEvent::BackgroundForegroundShell);
            }
            return None;
        }
        if self.menu.is_none() && self.diff_screen.is_some() {
            self.escape_confirmation = None;
            self.diff_key(key);
            return None;
        }
        #[cfg(unix)]
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('z') {
            return Some(InputEvent::Suspend);
        }
        if self.menu.is_none() && self.subscription_surface.is_some() {
            self.escape_confirmation = None;
            return self.subscription_key(key);
        }
        if self.menu.is_none() && self.permissions_surface.is_some() {
            self.escape_confirmation = None;
            let effect = self.permissions_surface.as_mut().unwrap().key(key);
            return match effect {
                PermissionsEffect::None => None,
                PermissionsEffect::Close { destination } => {
                    self.permissions_surface = None;
                    Some(InputEvent::Permissions(PermissionsEffect::Close {
                        destination,
                    }))
                }
                effect => Some(InputEvent::Permissions(effect)),
            };
        }
        if self.menu.is_none() && self.task_surface.is_some() {
            self.escape_confirmation = None;
            return self.task_surface_key(key);
        }
        if let Some((_, transcript)) = &mut self.transcript {
            self.escape_confirmation = None;
            if transcript.key(key) == TranscriptEvent::Close {
                self.transcript = None;
                self.conversation_focused = false;
                self.rendered_region = None;
            }
            return None;
        }
        if self.menu.is_none() && self.history_search.is_some() {
            self.escape_confirmation = None;
            self.history_search_key(key);
            return None;
        }
        if self.review_focused {
            let action = match (key.code, key.modifiers) {
                (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE) => {
                    Some(DiffNavigationAction::Up)
                }
                (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE) => {
                    Some(DiffNavigationAction::Down)
                }
                (KeyCode::PageUp, _) => Some(DiffNavigationAction::PageUp),
                (KeyCode::PageDown, _) => Some(DiffNavigationAction::PageDown),
                (KeyCode::Home, _) => Some(DiffNavigationAction::Home),
                (KeyCode::End, _) => Some(DiffNavigationAction::End),
                _ => None,
            };
            if let Some(action) = action.filter(|_| self.menu_is_visible()) {
                self.navigate_menu_review(action);
                return None;
            }
            if !matches!(key.code, KeyCode::Tab | KeyCode::BackTab | KeyCode::Esc) {
                return None;
            }
        }
        if key.code != KeyCode::Esc {
            self.escape_confirmation = None;
        }
        if !(key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'd')))
        {
            self.exit_confirmation = None;
        }
        if (self.menu_focused || self.review_focused)
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('b' | 'c' | 'd' | 'x'))
        {
            return None;
        }
        if self.menu_focused && self.menu_comment_is_editing() {
            self.escape_confirmation = None;
            match key.code {
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Esc => {
                    self.toggle_menu_comment();
                    return None;
                }
                KeyCode::Enter if self.menu_is_visible() => {
                    let comment = self.selected_menu_comment();
                    let menu = self.menu.as_ref().unwrap();
                    let (menu_id, index) = (menu.id, menu.selected);
                    self.begin_menu_submission(menu_id);
                    return Some(InputEvent::Choice {
                        menu_id,
                        index,
                        comment,
                    });
                }
                _ => {
                    self.edit_menu_comment(key);
                    return None;
                }
            }
        }
        if self.menu.is_none() && self.foreground_shell {
            return match (key.code, key.modifiers) {
                (KeyCode::Char('c'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                    Some(InputEvent::Interrupt)
                }
                (KeyCode::Char('b'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                    Some(InputEvent::BackgroundForegroundShell)
                }
                (KeyCode::Char('x'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.shell_background_chord_started_at = Some(Instant::now());
                    None
                }
                _ => None,
            };
        }
        if self.footer.selected().is_some()
            && let Some(event) = self.footer_key(key)
        {
            return event;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('g')
            && self.can_edit_in_external_editor()
        {
            self.edit_in_external_editor();
            return None;
        }
        let scroll = match key.code {
            KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(ScrollAction::First)
            }
            KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(ScrollAction::Last)
            }
            KeyCode::PageUp if !self.menu_focused && !self.review_focused => {
                Some(ScrollAction::HalfPageUp)
            }
            KeyCode::PageDown if !self.menu_focused && !self.review_focused => {
                Some(ScrollAction::HalfPageDown)
            }
            _ => None,
        };
        if let Some(action) = scroll {
            self.scrollback.scroll(action);
            self.conversation_focused = action != ScrollAction::Last;
            return None;
        }
        let menu_is_visible = self.menu_is_visible();
        if self.menu_focused
            && menu_is_visible
            && let Some(event) = self.question_menu_key(key)
        {
            return event;
        }
        if self.menu_focused
            && menu_is_visible
            && let Some(menu) = &mut self.menu
        {
            let picker_action = match (self.picker_kind, key.code, key.modifiers) {
                (
                    Some(PickerKind::Model | PickerKind::Provider),
                    KeyCode::Char('p'),
                    KeyModifiers::NONE,
                ) if self
                    .picker_presentation
                    .connection_options
                    .is_none_or(|count| menu.selected < count) =>
                {
                    Some(PickerAction::ConfigureProvider)
                }
                (Some(PickerKind::Provider), KeyCode::Char('f'), KeyModifiers::NONE) => {
                    Some(PickerAction::ToggleConnections)
                }
                (Some(PickerKind::Model), KeyCode::Char('r'), KeyModifiers::NONE)
                    if self.picker_presentation.refresh =>
                {
                    Some(PickerAction::RefreshAccountModels)
                }
                (
                    Some(PickerKind::Model),
                    KeyCode::Left | KeyCode::Char('h'),
                    KeyModifiers::NONE,
                ) => Some(PickerAction::ThinkingPrevious),
                (
                    Some(PickerKind::Model),
                    KeyCode::Right | KeyCode::Char('l'),
                    KeyModifiers::NONE,
                ) => Some(PickerAction::ThinkingNext),
                _ => None,
            };
            if let Some(action) = picker_action {
                return Some(InputEvent::PickerAction {
                    menu_id: menu.id,
                    index: menu.selected,
                    action,
                });
            }
            let direction = match (key.code, key.modifiers) {
                (KeyCode::Char('p'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                    Some(false)
                }
                (KeyCode::Char('n'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
                    Some(true)
                }
                (KeyCode::Char('k'), KeyModifiers::NONE) => Some(false),
                (KeyCode::Char('j'), KeyModifiers::NONE) => Some(true),
                _ => None,
            };
            if let Some(down) = direction {
                if down {
                    menu.selected = (menu.selected + 1).min(menu.options.len() - 1);
                } else {
                    menu.selected = menu.selected.saturating_sub(1);
                }
                return None;
            }
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if matches!(key.code, KeyCode::Char('n' | 'p'))
                && self.apply_shell_completion(if key.code == KeyCode::Char('n') {
                    ShellCompletionKey::Next
                } else {
                    ShellCompletionKey::Previous
                })
            {
                return None;
            }
            if key.code == KeyCode::Char('u') && self.shell_input.kind() == InputKind::Shell {
                let action = self
                    .shell_input
                    .handle_empty_control(ShellEmptyControl::Clear);
                if action != ShellInputAction::Ignored {
                    return self.apply_shell_action(action);
                }
            }
            if self.menu.is_none()
                && !self.editor.is_browsing_history()
                && matches!(key.code, KeyCode::Char('n' | 'p'))
                && (self.apply_file_mention(if key.code == KeyCode::Char('n') {
                    FileMentionKey::Next
                } else {
                    FileMentionKey::Previous
                }) || self.apply_completion(if key.code == KeyCode::Char('n') {
                    CompletionKey::Next
                } else {
                    CompletionKey::Previous
                }))
            {
                return None;
            }
            return match key.code {
                KeyCode::Char('r') if self.prompt_enabled && self.menu.is_none() => {
                    self.open_history_search();
                    None
                }
                KeyCode::Char('o') if self.overview.projection.is_none() => {
                    let id = self.start_transcript();
                    Some(InputEvent::OpenTranscript { id })
                }
                KeyCode::Char('c') if !self.prompt_enabled => Some(InputEvent::Interrupt),
                KeyCode::Char('c') if !self.editor.draft().is_empty() => {
                    self.edit(EditorCommand::Clear);
                    None
                }
                KeyCode::Char('c') => Some(InputEvent::Interrupt),
                // Ctrl+B backgrounds only a running foreground Shell; otherwise it moves the cursor back.
                KeyCode::Char('b') if self.prompt_enabled && !self.menu_focused => {
                    if !self.move_across_pasted_block(false) {
                        self.edit(EditorCommand::MoveLeft);
                    }
                    None
                }
                KeyCode::Char('f') if self.prompt_enabled && !self.menu_focused => {
                    if !self.move_across_pasted_block(true) {
                        self.edit(EditorCommand::MoveRight);
                    }
                    None
                }
                KeyCode::Left if self.prompt_enabled && !self.menu_focused => {
                    self.edit(EditorCommand::MoveWordBackward);
                    None
                }
                KeyCode::Right if self.prompt_enabled && !self.menu_focused => {
                    self.edit(EditorCommand::MoveWordForward);
                    None
                }
                KeyCode::Backspace if self.prompt_enabled && !self.menu_focused => {
                    self.edit(EditorCommand::KillWordBackward);
                    None
                }
                KeyCode::Char('j') if self.prompt_enabled && !self.menu_focused => {
                    self.edit(EditorCommand::Newline);
                    None
                }
                KeyCode::Char('u') if self.prompt_enabled && !self.menu_focused => {
                    self.edit(EditorCommand::KillToLineStart);
                    None
                }
                KeyCode::Char('d') if self.editor.draft().is_empty() => {
                    self.request_exit('D').then_some(InputEvent::Eof)
                }
                KeyCode::Char('d') if self.prompt_enabled && !self.menu_focused => {
                    self.edit(EditorCommand::DeleteForward);
                    None
                }
                KeyCode::Char('a') if self.prompt_enabled && !self.menu_focused => {
                    self.edit(EditorCommand::MoveLineStart);
                    None
                }
                KeyCode::Char('e') if self.prompt_enabled && !self.menu_focused => {
                    self.edit(EditorCommand::MoveLineEnd);
                    None
                }
                KeyCode::Char('k') if self.prompt_enabled && !self.menu_focused => {
                    self.edit(EditorCommand::KillToLineEnd);
                    None
                }
                KeyCode::Char('w') if self.prompt_enabled && !self.menu_focused => {
                    self.edit(EditorCommand::KillToPreviousWhitespace);
                    None
                }
                KeyCode::Char('y') if self.prompt_enabled && !self.menu_focused => {
                    self.edit(EditorCommand::Yank);
                    None
                }
                KeyCode::Char('v')
                    if self.prompt_enabled
                        && !self.menu_focused
                        && self.shell_input.kind() == InputKind::Prompt =>
                {
                    self.paste_clipboard_image();
                    None
                }
                // Claude Code binds both Ctrl+_ and Ctrl+Shift+- to undo. Legacy
                // terminals send 0x1F for either, which crossterm reports as Ctrl+7;
                // terminals with an enhanced keyboard protocol report `-` or `_`.
                KeyCode::Char(character @ ('_' | '7' | '-'))
                    if self.prompt_enabled
                        && !self.menu_focused
                        && (character != '-' || key.modifiers.contains(KeyModifiers::SHIFT)) =>
                {
                    self.edit(EditorCommand::Undo);
                    None
                }
                KeyCode::Char('s') if self.prompt_enabled && !self.menu_focused => {
                    self.stash_input();
                    None
                }
                KeyCode::Char('p') if self.prompt_enabled && !self.menu_focused => {
                    self.edit(EditorCommand::MoveUp {
                        wrap_width: self.width.saturating_sub(2).max(1),
                    });
                    None
                }
                KeyCode::Char('n') if self.prompt_enabled && !self.menu_focused => {
                    self.edit(EditorCommand::MoveDown {
                        wrap_width: self.width.saturating_sub(2).max(1),
                    });
                    None
                }
                _ => None,
            };
        }
        if key.code == KeyCode::Enter
            && key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT)
        {
            if self.prompt_enabled && !self.menu_focused {
                self.insert("\n");
            }
            return None;
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) && self.menu.is_some() {
            if self.menu_focused && key.code == KeyCode::Tab && self.toggle_menu_comment() {
                return None;
            }
            if self.menu_focused && key.code == KeyCode::Tab && self.selected_menu_can_expand() {
                let menu = self.menu.as_ref().unwrap();
                return Some(InputEvent::ExpandChoice {
                    menu_id: menu.id,
                    index: menu.selected,
                });
            }
            if self.menu_focused {
                if self.input_dialog_visible() {
                    self.review_focused =
                        key.code == KeyCode::BackTab && self.menu_review.is_some();
                    self.menu_focused = !self.review_focused;
                    return None;
                }
                self.menu_focused = false;
                self.review_focused = key.code == KeyCode::BackTab && self.menu_review.is_some();
            } else if self.review_focused {
                self.review_focused = false;
                self.menu_focused = true;
            } else {
                self.menu_focused = true;
            }
            return None;
        }
        let selected_comment = self.selected_menu_comment();
        if self.menu_plan
            && key.code == KeyCode::Enter
            && self.selected_menu_accepts_comment()
            && !self.menu_comment_is_editing()
            && selected_comment.is_none()
        {
            if let Some(comment) = &mut self.menu_comment {
                comment.editing = true;
            }
            return None;
        }
        let direct_number_choices =
            self.picker_kind.is_none() && self.menu.as_ref().is_some_and(|menu| !menu.confirmation);
        let approval_review_available = self
            .menu_review
            .as_ref()
            .is_none_or(|review| review.review.is_ok());
        if let Some(menu) = &mut self.menu {
            match key.code {
                KeyCode::PageUp if menu_is_visible => {
                    self.context_offset = self.context_offset.saturating_sub(1);
                    return None;
                }
                KeyCode::PageDown if menu_is_visible => {
                    self.context_offset = self.context_offset.saturating_add(1);
                    return None;
                }
                KeyCode::Esc => {
                    self.escape_confirmation = None;
                    let menu_id = menu.id;
                    self.begin_menu_submission(menu_id);
                    return Some(InputEvent::Cancel { menu_id });
                }
                _ => {}
            }
            if self.menu_focused {
                let choice = match key.code {
                    KeyCode::Up if menu_is_visible => {
                        menu.selected = menu.selected.saturating_sub(1);
                        None
                    }
                    KeyCode::Down if menu_is_visible => {
                        menu.selected = (menu.selected + 1).min(menu.options.len() - 1);
                        None
                    }
                    KeyCode::Char(digit)
                        if direct_number_choices
                            && menu_is_visible
                            && key.modifiers == KeyModifiers::NONE
                            && digit
                                .to_digit(10)
                                .and_then(|number| usize::try_from(number).ok())
                                .and_then(|number| number.checked_sub(1))
                                .is_some_and(|index| index < menu.options.len()) =>
                    {
                        let index = digit
                            .to_digit(10)
                            .and_then(|number| usize::try_from(number).ok())
                            .and_then(|number| number.checked_sub(1))
                            .expect("validated menu number");
                        Some(InputEvent::Choice {
                            menu_id: menu.id,
                            index,
                            comment: None,
                        })
                    }
                    KeyCode::Enter
                        if menu_is_visible
                            && (approval_review_available
                                || (menu.confirmation
                                    && menu.selected == menu.options.len().saturating_sub(1))) =>
                    {
                        Some(InputEvent::Choice {
                            menu_id: menu.id,
                            index: menu.selected,
                            comment: selected_comment,
                        })
                    }
                    KeyCode::Char('y' | 'Y')
                        if menu.confirmation && menu_is_visible && approval_review_available =>
                    {
                        Some(InputEvent::Choice {
                            menu_id: menu.id,
                            index: 0,
                            comment: None,
                        })
                    }
                    KeyCode::Char('n' | 'N') if menu.confirmation && menu_is_visible => {
                        Some(InputEvent::Cancel { menu_id: menu.id })
                    }
                    _ => None,
                };
                if choice.is_none()
                    && menu.confirmation
                    && menu_is_visible
                    && !approval_review_available
                    && (key.code == KeyCode::Enter || matches!(key.code, KeyCode::Char('y' | 'Y')))
                {
                    self.status =
                        "Cannot approve: the file change review is unavailable · Esc or n denies"
                            .into();
                }
                if choice.is_some() {
                    let menu_id = menu.id;
                    self.begin_menu_submission(menu_id);
                }
                return choice;
            }
        }
        if self.shell_input.active_completion().is_some() {
            if key.code == KeyCode::BackTab {
                return None;
            }
            let shell_key = match key.code {
                KeyCode::Up => Some(ShellCompletionKey::Previous),
                KeyCode::Down => Some(ShellCompletionKey::Next),
                KeyCode::Tab => Some(ShellCompletionKey::Complete),
                KeyCode::Esc => Some(ShellCompletionKey::Cancel),
                _ => None,
            };
            if shell_key.is_some_and(|key| self.apply_shell_completion(key)) {
                return None;
            }
        }
        if self.file_mentions.is_open() || self.completion.is_open() {
            if key.code == KeyCode::BackTab {
                return None;
            }
            let file_key = match key.code {
                KeyCode::Up if !self.editor.is_browsing_history() => Some(FileMentionKey::Previous),
                KeyCode::Down if !self.editor.is_browsing_history() => Some(FileMentionKey::Next),
                KeyCode::Tab => Some(FileMentionKey::Complete),
                KeyCode::Enter => Some(FileMentionKey::Choose),
                KeyCode::Esc => Some(FileMentionKey::Cancel),
                _ => None,
            };
            let completion_key = match key.code {
                KeyCode::Up if !self.editor.is_browsing_history() => Some(CompletionKey::Previous),
                KeyCode::Down if !self.editor.is_browsing_history() => Some(CompletionKey::Next),
                KeyCode::Tab => Some(CompletionKey::Complete),
                KeyCode::Enter => Some(CompletionKey::Choose),
                KeyCode::Esc => Some(CompletionKey::Cancel),
                _ => None,
            };
            if file_key.is_some_and(|key| self.apply_file_mention(key))
                || completion_key.is_some_and(|key| self.apply_completion(key))
            {
                self.escape_confirmation = None;
                return None;
            }
        }
        if key.code == KeyCode::BackTab && self.prompt_enabled {
            if self.refuse_mode_cycle() {
                return None;
            }
            return Some(InputEvent::CycleExecutionMode);
        }
        if key.code == KeyCode::Esc
            && self.ordinary_input_is_visible()
            && (self.prompt_suggestion.is_some() || self.prompt_suggestion_request.is_some())
        {
            self.clear_prompt_suggestion();
            self.escape_confirmation = None;
            return None;
        }
        if key.code == KeyCode::Down && self.can_enter_footer() {
            self.apply_footer_action(FooterAction::Down);
            return None;
        }
        if key.code == KeyCode::Esc && self.shell_input.kind() == InputKind::Shell {
            let action = self
                .shell_input
                .handle_empty_control(ShellEmptyControl::Escape);
            if action != ShellInputAction::Ignored {
                self.escape_confirmation = None;
                return self.apply_shell_action(action);
            }
        }
        if key.code == KeyCode::Esc {
            return self.main_escape();
        }
        if !self.prompt_enabled {
            return None;
        }
        match key.code {
            KeyCode::Enter => {
                if self.shell_input.kind() == InputKind::Shell {
                    let action = self.shell_input.submit(ShellExecution::Foreground);
                    return self.apply_shell_action(action);
                }
                let source_text = self
                    .paste_blocks
                    .source_text(self.editor.draft())
                    .to_owned();
                let scope = self.command_scope();
                if scope == CommandScope::Overview && source_text.trim() == "/help" {
                    self.editor.apply(EditorCommand::Clear);
                    self.completion.dismiss();
                    self.help_open = true;
                    self.help_offset = 0;
                    return None;
                }
                if let Some((command, reason)) = unavailable_command(&source_text, scope) {
                    self.completion.dismiss();
                    self.hold_status(&super::completion::command_refusal(scope, command, reason));
                    return None;
                }
                if crate::terminal::is_branch_command(source_text.trim()) {
                    self.branch_submission = BranchSubmission::Pending;
                    return Some(InputEvent::Prompt {
                        text: source_text,
                        file_references: Vec::new(),
                        images: Vec::new(),
                    });
                }
                let file_references = match self
                    .file_mentions
                    .revalidate_all_for_submission(self.editor.draft())
                {
                    Ok(mentions) => mentions
                        .into_iter()
                        .map(|mention| mention.canonical_path)
                        .collect(),
                    Err(error) => {
                        self.hold_status(&format!("Cannot attach referenced path: {error}"));
                        return None;
                    }
                };
                let pasted_texts = self.pasted_texts();
                let draft = self.editor.draft().to_owned();
                let images = self.paste_blocks.images(&draft);
                let EditorOutcome::Submitted(_) = self.editor.apply(EditorCommand::Submit) else {
                    unreachable!()
                };
                let line = source_text;
                self.submitted_pasted_texts = Some((line.clone(), pasted_texts));
                self.submitted_retry_of = self.retry_of.take();
                self.submitted_prompt = Some(SubmittedPrompt {
                    draft,
                    paste_blocks: self.paste_blocks.take(),
                    mentions: self.file_mentions.take_accepted(),
                });
                self.completion.dismiss();
                self.file_mentions.update_at("", 0, 15);
                if line.trim() == "/details" && self.overview.projection.is_none() {
                    Some(InputEvent::OpenTranscript {
                        id: self.start_transcript(),
                    })
                } else {
                    Some(InputEvent::Prompt {
                        text: line,
                        file_references,
                        images,
                    })
                }
            }
            KeyCode::Up => {
                if !self.queued_inputs.is_empty()
                    && !self.editor.is_browsing_history()
                    && self
                        .editor
                        .is_on_first_visual_row(self.width.saturating_sub(2).max(1))
                {
                    return Some(InputEvent::TakeBackQueued {
                        draft_empty: self.editor.draft().is_empty(),
                    });
                }
                self.edit(EditorCommand::MoveUp {
                    wrap_width: self.width.saturating_sub(2).max(1),
                });
                None
            }
            KeyCode::Down => {
                self.edit(EditorCommand::MoveDown {
                    wrap_width: self.width.saturating_sub(2).max(1),
                });
                None
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::ALT) => {
                if character == '?'
                    && self.menu.is_none()
                    && self.editor.draft().is_empty()
                    && self.shell_input.kind() == InputKind::Prompt
                    && self.paste_blocks.images("").is_empty()
                {
                    self.help_open = true;
                    self.help_offset = 0;
                    return None;
                }
                let action = if self.menu.is_none() {
                    self.shell_input.try_tty_character(
                        self.editor.draft(),
                        self.editor.cursor(),
                        character,
                    )
                } else {
                    ShellInputAction::Ignored
                };
                if action != ShellInputAction::Ignored {
                    return self.apply_shell_action(action);
                }
                self.insert(&character.to_string());
                None
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => {
                self.edit(EditorCommand::MoveWordBackward);
                None
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => {
                self.edit(EditorCommand::MoveWordForward);
                None
            }
            KeyCode::Delete if key.modifiers.contains(KeyModifiers::ALT) => {
                self.edit(EditorCommand::KillToLineEnd);
                None
            }
            KeyCode::Left => {
                if key.modifiers == KeyModifiers::NONE
                    && self.menu.is_none()
                    && self.shell_input.kind() == InputKind::Prompt
                    && self.editor.draft().is_empty()
                    && self.restored_contexts.is_empty()
                    && self.paste_blocks.images("").is_empty()
                    && !self.completion.is_open()
                    && !self.file_mentions.is_open()
                {
                    return Some(InputEvent::OpenOverview);
                }
                if self.move_across_pasted_block(false) {
                    return None;
                }
                self.edit(EditorCommand::MoveLeft);
                None
            }
            KeyCode::Right => {
                if self.move_across_pasted_block(true) {
                    return None;
                }
                self.edit(EditorCommand::MoveRight);
                None
            }
            KeyCode::Home => {
                self.edit(EditorCommand::MoveLineStart);
                None
            }
            KeyCode::End => {
                self.edit(EditorCommand::MoveLineEnd);
                None
            }
            KeyCode::Backspace if !key.modifiers.contains(KeyModifiers::ALT) => {
                if self.shell_input.kind() == InputKind::Shell {
                    let action = self
                        .shell_input
                        .handle_empty_control(ShellEmptyControl::Backspace);
                    if action != ShellInputAction::Ignored {
                        return self.apply_shell_action(action);
                    }
                }
                if self.delete_pasted_block(true) {
                    return None;
                }
                self.edit(EditorCommand::DeleteBackward);
                None
            }
            KeyCode::Delete => {
                if self.delete_pasted_block(false) {
                    return None;
                }
                self.edit(EditorCommand::DeleteForward);
                None
            }
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.edit(EditorCommand::MoveWordBackward);
                None
            }
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.edit(EditorCommand::MoveWordForward);
                None
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.edit(EditorCommand::KillWordForward);
                None
            }
            KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.edit(EditorCommand::YankPrevious);
                None
            }
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => {
                self.edit(EditorCommand::KillWordBackward);
                None
            }
            _ => None,
        }
    }
}

fn read_keys(
    state: &Arc<Mutex<State>>,
    stop: &AtomicBool,
    sender: &mpsc::Sender<InputEvent>,
) -> io::Result<()> {
    while !stop.load(Ordering::Acquire) {
        let input_was_idle = !event::poll(Duration::ZERO)?;
        if input_was_idle {
            let mut state = state
                .lock()
                .map_err(|error| io::Error::other(error.to_string()))?;
            if state.release_finished_submissions() {
                state.draw(&mut io::stdout().lock())?;
            }
        }
        if !event::poll(INPUT_POLL_INTERVAL)? {
            let mut state = state
                .lock()
                .map_err(|error| io::Error::other(error.to_string()))?;
            state.expire_shell_background_chord();
            let confirmation_expired = state.expire_key_confirmations();
            if state.poll_clipboard_hint(Instant::now()) || confirmation_expired {
                state.draw(&mut io::stdout().lock())?;
            }
            continue;
        }
        let event = event::read()?;
        // 照 Claude Code `processKeysInBatch`：按键、粘贴与鼠标操作是用户输入，推迟后台清理；
        // 没有按下按钮的指针移动不算。
        if matches!(&event, Event::Key(_) | Event::Paste(_))
            || matches!(&event, Event::Mouse(mouse) if mouse.kind != MouseEventKind::Moved)
        {
            crate::housekeeping::UserActivity::process().record();
        }
        let mut state = state
            .lock()
            .map_err(|error| io::Error::other(error.to_string()))?;
        // 提交期间已排队的按键仍被丢弃；输入排空后才到达的事件晚于期间完成的提交，
        // 应作用于提交后可见的界面，否则菜单关闭后立即输入的按键会丢失。
        let submission_released = input_was_idle && state.release_finished_submissions();
        let mut output = io::stdout().lock();
        let mut redraw = true;
        let input = match event {
            Event::Key(key) => {
                state.clear_selection();
                state.key(key)
            }
            Event::Paste(text) => {
                state.paste(&text);
                None
            }
            Event::Resize(width, height) => {
                state.resize(width, height);
                None
            }
            Event::Mouse(mouse) => {
                // Pointer input that changes nothing must not cost a frame.
                redraw = state.pointer(mouse, &mut output);
                state.pointer_input.take()
            }
            Event::FocusLost => {
                state.focus_lost();
                None
            }
            Event::FocusGained => {
                state.focus_gained(Instant::now());
                None
            }
        };
        if redraw || submission_released {
            state.draw(&mut output)?;
        }
        drop(output);
        drop(state);
        if let Some(input) = input {
            let ended = matches!(input, InputEvent::Eof);
            if !send_input(sender, stop, input) || ended {
                break;
            }
        }
    }
    Ok(())
}

fn read_lines(stop: &AtomicBool, sender: &mpsc::Sender<InputEvent>) -> io::Result<()> {
    let mut pending = Vec::new();
    let mut buffer = [0_u8; 4096];
    while !stop.load(Ordering::Acquire) {
        let mut descriptor = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let available =
            unsafe { libc::poll(&mut descriptor, 1, INPUT_POLL_INTERVAL.as_millis() as i32) };
        if available < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if available == 0 {
            continue;
        }
        let bytes =
            unsafe { libc::read(libc::STDIN_FILENO, buffer.as_mut_ptr().cast(), buffer.len()) };
        if bytes < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        let bytes = bytes as usize;
        if bytes == 0 {
            if !pending.is_empty()
                && !send_input(
                    sender,
                    stop,
                    InputEvent::Line(String::from_utf8_lossy(&pending).into_owned()),
                )
            {
                return Ok(());
            }
            send_input(sender, stop, InputEvent::Eof);
            return Ok(());
        }
        pending.extend_from_slice(&buffer[..bytes]);
        while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
            let line = String::from_utf8_lossy(&pending[..end])
                .trim_end_matches('\r')
                .to_owned();
            pending.drain(..=end);
            if !send_input(sender, stop, InputEvent::Line(line)) {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn send_input(sender: &mpsc::Sender<InputEvent>, stop: &AtomicBool, mut input: InputEvent) -> bool {
    loop {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        match sender.try_send(input) {
            Ok(()) => return true,
            Err(mpsc::error::TrySendError::Closed(_)) => return false,
            Err(mpsc::error::TrySendError::Full(returned)) => {
                input = returned;
                thread::sleep(INPUT_POLL_INTERVAL);
            }
        }
    }
}

fn task_surface_content_viewport(width: usize, height: usize) -> TaskViewport {
    TaskViewport {
        width: width.saturating_sub(2).max(1),
        height: input_dialog::body_height(input_dialog::height_for_terminal(height))
            .saturating_sub(TASK_SURFACE_HEADER_FOOTER_ROWS),
    }
}

fn subscription_surface_lines(
    surface: &mut SubscriptionSurface,
    height: usize,
    display: DisplayProfile,
) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    let frame = surface.frame(
        SubscriptionViewport {
            height: height
                .saturating_sub(SUBSCRIPTION_SURFACE_HEADER_ROWS + SUBSCRIPTION_SURFACE_FOOTER_ROWS)
                .max(1),
        },
        display,
    );
    let mut lines = vec!["Tick Subscriptions".to_owned()];
    lines.extend(frame.lines.into_iter().map(|line| {
        format!(
            "{} {}",
            if line.selected {
                display.glyph(Glyph::Prompt)
            } else {
                " "
            },
            line.text
        )
    }));
    if height > SUBSCRIPTION_SURFACE_FOOTER_ROWS {
        if lines.len() >= height {
            lines.truncate(height - SUBSCRIPTION_SURFACE_FOOTER_ROWS);
        }
        lines.push(frame.footer);
    }
    lines
}

fn task_surface_lines(
    surface: &mut TaskSurface<Uuid>,
    width: usize,
    height: usize,
    pending_menu: Option<&str>,
    display: DisplayProfile,
) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    let focus = surface.focus();
    let viewport = TaskViewport {
        width: if focus == TaskSurfaceFocus::List {
            width.saturating_sub(2).max(1)
        } else {
            width.max(1)
        },
        height: height.saturating_sub(2),
    };
    let frame = surface.frame(viewport);
    let action_error = frame.action_error.clone();
    let selected_capabilities = frame.selected_capabilities;
    let title = match frame.focus {
        TaskSurfaceFocus::List => "Shell tasks".to_owned(),
        TaskSurfaceFocus::Detail { .. } => "Shell task".to_owned(),
    };
    let mut header = title;
    if let Some(menu) = pending_menu {
        header.push_str(" · ");
        header.push_str(&display_single_line(menu));
        header.push_str(" waiting in Chat");
    }
    let mut lines = vec![header];
    lines.extend(frame.lines.into_iter().map(|line| {
        if frame.focus == TaskSurfaceFocus::List {
            format!(
                "{} {}",
                if line.selected {
                    display.glyph(Glyph::Prompt)
                } else {
                    " "
                },
                line.text
            )
        } else {
            line.text
        }
    }));
    if height > 1 {
        let footer = if let Some(error) = action_error {
            format!("Error: {} · Esc close", display_single_line(&error))
        } else {
            match frame.focus {
                TaskSurfaceFocus::List => {
                    let mut footer = "Enter open".to_owned();
                    if selected_capabilities.can_background {
                        footer.push_str(" · b background");
                    }
                    if selected_capabilities.can_stop {
                        footer.push_str(" · x stop");
                    }
                    footer.push_str(" · Esc close");
                    footer
                }
                TaskSurfaceFocus::Detail { .. } => {
                    let unread = if frame.unread_bytes == 0 {
                        String::new()
                    } else {
                        format!(" · {} unread bytes", frame.unread_bytes)
                    };
                    let mut footer = "← list · Esc/Enter/Space close".to_owned();
                    if selected_capabilities.can_stop {
                        footer.push_str(" · x stop");
                    }
                    if selected_capabilities.can_background {
                        footer.push_str(" · b background");
                    }
                    footer.push_str(" · PgUp/PgDn · Home/End");
                    footer.push_str(&unread);
                    footer
                }
            }
        };
        if lines.len() >= height {
            lines.truncate(height - 1);
        }
        lines.push(footer);
    }
    lines
}

fn diff_screen_lines(
    screen: &mut DiffScreen,
    width: usize,
    height: usize,
    display: DisplayProfile,
) -> Vec<String> {
    let viewport = DiffViewport {
        width,
        height: height.saturating_sub(2),
    };
    let frame = screen.dialog.frame(&screen.catalog, viewport);
    let mut lines = Vec::new();
    let source_label = frame.source.map_or_else(
        || "No sources".to_owned(),
        |source| match source.id {
            ChangeSourceId::Current => "Current · working tree".into(),
            ChangeSourceId::AgentRun(run_id) => {
                let run_id = run_id.to_string();
                let prompt = source
                    .user_prompt
                    .as_deref()
                    .map(display_single_line)
                    .unwrap_or_else(|| "Agent turn in this process".into());
                format!("Agent · {} · {prompt}", &run_id[..8])
            }
        },
    );
    let source_position = frame.source_index.map_or_else(String::new, |index| {
        format!(" · {}/{}", index + 1, frame.source_count)
    });
    lines.push(format!("Diff review · {source_label}{source_position}"));

    if frame.mode == DiffDialogMode::Detail {
        if let Some(detail) = frame.detail {
            lines.extend(detail.lines.into_iter().map(|line| line.text));
            if detail.total_rows > viewport.height {
                lines.push(format!(
                    "Rows {}–{}/{}",
                    detail.top_row + 1,
                    (detail.top_row + viewport.height).min(detail.total_rows),
                    detail.total_rows
                ));
            }
        }
    } else if let Some(source) = frame.source {
        match &source.state {
            ChangeSourceState::Ready => {}
            ChangeSourceState::Clean => lines.push("Working tree is clean.".into()),
            ChangeSourceState::NonGit => {
                lines.push("Current directory is not a Git repository.".into())
            }
            ChangeSourceState::GitError { message } => {
                lines.push(format!("Git error: {}", display_single_line(message)))
            }
        }
        for (index, file) in source.files[frame.visible_files.clone()].iter().enumerate() {
            let actual_index = frame.visible_files.start + index;
            let marker = if frame.selected_file == Some(actual_index) {
                display.glyph(Glyph::Prompt)
            } else {
                " "
            };
            let stats = file.summary.as_ref().map_or_else(
                || "unavailable".into(),
                |summary| format!("+{} -{}", summary.added_lines, summary.removed_lines),
            );
            let availability = match &file.detail {
                CatalogChangeDetail::Available(_) => String::new(),
                CatalogChangeDetail::Unavailable { message, .. } => {
                    format!(" · {}", display_single_line(message))
                }
            };
            let current_status = file
                .current_status
                .map(current_file_status_label)
                .unwrap_or_default();
            lines.push(format!(
                "{marker} {:?} · {} · {stats}{current_status}{availability}",
                file.action,
                display_single_line(&file.path.to_string_lossy())
            ));
        }
        if source.files.is_empty() && matches!(source.state, ChangeSourceState::Ready) {
            lines.push("No file changes in this source.".into());
        }
    }

    let footer = if frame.mode == DiffDialogMode::Detail {
        "PgUp/PgDn · Home/End · Esc back"
    } else {
        "Enter details · Esc close"
    };
    let content_rows = height.saturating_sub(1);
    lines.truncate(content_rows);
    while lines.len() < content_rows {
        lines.push(String::new());
    }
    if height > 0 {
        lines.push(footer.into());
    }
    lines
}

fn current_file_status_label(status: CurrentFileStatus) -> String {
    if status.untracked {
        return " · untracked".into();
    }
    let mut parts = Vec::new();
    if status.index != GitFileState::Unmodified {
        parts.push(format!("index {}", git_file_state_label(status.index)));
    }
    if status.worktree != GitFileState::Unmodified {
        parts.push(format!(
            "worktree {}",
            git_file_state_label(status.worktree)
        ));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" · {}", parts.join(" · "))
    }
}

fn git_file_state_label(state: GitFileState) -> String {
    match state {
        GitFileState::Unmodified => "unmodified".into(),
        GitFileState::Modified => "modified".into(),
        GitFileState::Added => "added".into(),
        GitFileState::Deleted => "deleted".into(),
        GitFileState::TypeChanged => "type changed".into(),
        GitFileState::Unmerged => "unmerged".into(),
        GitFileState::Unknown(value) => format!("status {value}"),
    }
}

pub(super) fn display_single_line(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            '\n' => '␤',
            '\r' => '␍',
            '\t' => '⇥',
            character if character.is_control() => '�',
            character => character,
        })
        .collect()
}

fn question_menu_hint(question: &QuestionMenu, selected: usize) -> String {
    let mut hint = String::from(if question.entry_option == Some(selected) {
        "Enter type answer"
    } else if question.multi_select {
        "Space/Enter toggle"
    } else {
        "Enter select"
    });
    hint.push_str(" · Esc answer later");
    hint
}

fn display_text(text: &str) -> String {
    text.chars()
        .filter_map(|character| match character {
            '\r' => None,
            '\t' => Some(' '),
            '\n' => Some(character),
            character if character.is_control() => Some('�'),
            character => Some(character),
        })
        .collect()
}

fn valid_login_link(address: &str) -> Option<String> {
    if address.chars().any(char::is_control) {
        return None;
    }
    let url = reqwest::Url::parse(address).ok()?;
    let secure = url.scheme() == "https";
    #[cfg(test)]
    let secure = secure
        || url.scheme() == "http"
            && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"));
    (secure && url.host_str().is_some() && url.username().is_empty() && url.password().is_none())
        .then(|| address.to_owned())
}

fn footer_frame_line(frame: &FooterFrame, display: DisplayProfile) -> String {
    frame
        .items
        .iter()
        .map(|item| {
            let marker = if item.selected {
                display.glyph(Glyph::Prompt)
            } else {
                " "
            };
            format!("{marker} {}", item.id.label())
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// A completion or picker row: the focused candidate in the suggestion colour,
/// its kind or description dimmed, as Claude Code's suggestion list does.
fn candidate_line(
    selected: bool,
    candidate: &str,
    detail: &str,
    display: DisplayProfile,
) -> RoleLine {
    let marker = if selected {
        display.glyph(Glyph::Prompt)
    } else {
        " "
    };
    let mut line = RoleLine::default();
    line.push(
        format!("{marker} {candidate}"),
        selected.then_some(Role::Suggestion),
    );
    line.push(format!("  {detail}"), Some(Role::MutedText));
    line
}

fn previous_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .char_indices()
        .last()
        .map_or(0, |(index, _)| index)
}

fn next_boundary(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .char_indices()
        .nth(1)
        .map_or(text.len(), |(index, _)| cursor + index)
}

fn display_width(text: &str) -> usize {
    text.chars()
        .map(|character| character.width().unwrap_or(0))
        .sum()
}

pub(super) fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    let mut column = 0;
    for character in text.chars() {
        if character == '\n' {
            lines.push(String::new());
            column = 0;
            continue;
        }
        let character_width = character.width().unwrap_or(0);
        if column + character_width > width && column > 0 {
            lines.push(String::new());
            column = 0;
        }
        lines
            .last_mut()
            .expect("there is always one display line")
            .push(character);
        column += character_width;
    }
    lines
}

fn render_padded_line(line: &StyledLine, width: usize, depth: ColourDepth) -> String {
    let columns = line
        .spans
        .iter()
        .map(|span| display_width(&span.text))
        .sum::<usize>();
    format!(
        "{}{}",
        line.render(depth),
        " ".repeat(width.saturating_sub(columns))
    )
}

pub(super) fn truncate_width(text: &str, width: usize) -> String {
    let mut result = String::new();
    let mut column = 0;
    for character in text.chars() {
        let character_width = character.width().unwrap_or(0);
        if column + character_width > width {
            break;
        }
        result.push(character);
        column += character_width;
    }
    result
}

fn dispatch_directory_line(directory: &Path, width: usize) -> String {
    let prefix = "New Agent · cwd ";
    let directory = display_single_line(&directory.to_string_lossy());
    let available = width.saturating_sub(display_width(prefix));
    format!("{prefix}{}", front_truncate_width(&directory, available))
}

fn front_truncate_width(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.into();
    }
    if width == 0 {
        return String::new();
    }
    let mut columns = 0;
    let mut suffix = Vec::new();
    for character in text.chars().rev() {
        let next = character.width().unwrap_or(0);
        if columns + next > width.saturating_sub(1) {
            break;
        }
        suffix.push(character);
        columns += next;
    }
    format!("…{}", suffix.into_iter().rev().collect::<String>())
}

fn truncate_path_width(path: &str, width: usize) -> String {
    if display_width(path) <= width {
        return path.into();
    }
    let Some((tail_start, _)) = path
        .rmatch_indices('/')
        .filter(|(index, _)| index + 1 < path.len())
        .nth(1)
    else {
        return front_truncate_width(path, width);
    };
    let tail = &path[tail_start..];
    let tail_columns = display_width(tail) + display_width("…");
    if tail_columns >= width {
        return front_truncate_width(path, width);
    }
    format!(
        "{}…{tail}",
        truncate_width(&path[..tail_start], width - tail_columns)
    )
}

fn draft_keeps_context(draft: &str, context: &InputContext) -> bool {
    context
        .path()
        .is_some_and(|path| draft_mentions_path(draft, path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_agent::file_change::{
        FileChangeAction, FileChangeHunk, FileChangeLine, FileChangeLineKind, FileVersion,
    };
    use tickloop_agent::permissions::{Decision, ExecutionMode, PermissionRule};
    use tickloop_agent::runtime::{
        EventKind, ShellTask, ShellTaskCapabilities, ShellTaskOrigin, ShellTaskOwner,
        ShellTaskSnapshot, ShellTaskState, TextContext,
    };

    use crate::terminal::diff_catalog::{
        CatalogFileChange, CatalogFileChangeId, ChangeSource, CurrentFileStatus, GitFileState,
    };
    use crate::terminal::execution::ConsoleExecution as Agent;
    use crate::terminal::{branch_from_terminal, test_support as support};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn set_cursor(state: &mut State, cursor: usize) {
        let draft = state.editor.draft().to_owned();
        state.editor.replace_draft(draft, cursor);
        state.refresh_completion();
    }

    fn menu() -> Menu {
        Menu {
            id: Uuid::new_v4(),
            title: "Run this command?".into(),
            context: vec!["find crates docs .scratch -maxdepth 4 | head -100".into()],
            options: vec![
                "Allow once".into(),
                "Allow for this session".into(),
                "Deny".into(),
            ],
            selected: 0,
            confirmation: true,
        }
    }

    #[tokio::test]
    async fn a_delegated_agents_permissions_open_read_only_without_editing_hints_or_forms() {
        let directory = tempfile::tempdir().unwrap();
        let agent = Agent::new_embedded(
            support::config(),
            Arc::new(support::ScriptedModel::new(Vec::new())),
        );
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Auto)
            .await
            .unwrap();
        let view = agent.permissions_view(chat).await.unwrap();
        let mut state = State::new(true, 100, 30);
        state.chat = Some(chat);
        let console = InteractiveConsoleFixture::new(state);
        console.set_visible_delegation(true).unwrap();
        console
            .show_permissions(view, Err("no activity".into()), false, true)
            .unwrap();
        let mut state = console.state.lock().unwrap();
        let frame = |state: &mut State| {
            let lines = state
                .permissions_surface
                .as_mut()
                .expect("the surface stays open")
                .lines(100, 24)
                .join("\n");
            assert!(lines.contains("Read-only"), "{lines}");
            for editing in [
                "a add",
                "e edit",
                "d remove",
                "Enter edit/preview",
                "Ctrl+S save",
                "Enter save",
            ] {
                assert!(!lines.contains(editing), "{editing}: {lines}");
            }
            lines
        };
        let press = |state: &mut State, code: KeyCode| {
            let event = state.key(key(code));
            assert!(
                !matches!(
                    event,
                    Some(InputEvent::Permissions(PermissionsEffect::Apply(_)))
                ),
                "a read-only surface never applies a change"
            );
        };
        let refuse_editing = |state: &mut State| {
            let before = frame(state);
            for code in [KeyCode::Char('a'), KeyCode::Char('e'), KeyCode::Char('d')] {
                press(state, code);
                assert_eq!(frame(state), before, "no editing form opens");
            }
        };
        refuse_editing(&mut state);
        press(&mut state, KeyCode::Enter);
        assert!(
            frame(&mut state).contains("❯ Reviewer"),
            "the reviewer stays as it is"
        );
        for _ in 0..3 {
            press(&mut state, KeyCode::Char('j'));
        }
        press(&mut state, KeyCode::Enter);
        let group = frame(&mut state);
        assert!(
            group.contains("Enter details"),
            "Auto rules stay browsable: {group}"
        );
        refuse_editing(&mut state);
        press(&mut state, KeyCode::Esc);
        assert!(frame(&mut state).contains("❯ Soft deny"));
    }

    #[tokio::test]
    async fn a_permission_menu_suspends_the_permissions_draft_until_dismissed() {
        let directory = tempfile::tempdir().unwrap();
        let agent = Agent::new_embedded(
            support::config(),
            Arc::new(support::ScriptedModel::new(Vec::new())),
        );
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Auto)
            .await
            .unwrap();
        let view = agent.permissions_view(chat).await.unwrap();
        let mut state = State::new(true, 100, 30);
        state.chat = Some(chat);
        state.permissions_surface = Some(PermissionsSurface::open(
            view,
            PermissionsReturn::ModePicker { settings: false },
        ));
        state.key(key(KeyCode::Right));
        for _ in 0..5 {
            state.key(key(KeyCode::Char('j')));
        }
        state.key(key(KeyCode::Enter));
        state.key(key(KeyCode::Enter));
        state.key(key(KeyCode::Char('w')));
        state.editor.replace_draft("chat draft".into(), 10);
        let console = InteractiveConsoleFixture::new(state);
        let prompt = menu();
        let menu_id = prompt.id;
        console.show_menu(prompt).unwrap();
        {
            let mut state = console.state.lock().unwrap();
            assert!(state.menu_focused);
            state.draw_frame(&mut Vec::new()).unwrap();
            assert!(
                state
                    .rendered_screen
                    .iter()
                    .any(|line| line.contains("Run this command?"))
            );
            assert!(
                !state
                    .rendered_screen
                    .iter()
                    .any(|line| line.contains("Replace Environment"))
            );
            state.key(key(KeyCode::Char('j')));
            assert_eq!(state.menu.as_ref().unwrap().selected, 1);
            state.paste("must stay out of the permission draft");
            assert_eq!(state.editor.draft(), "chat draft");
            let permission_lines = state.permissions_surface.as_mut().unwrap().lines(80, 12);
            assert!(
                permission_lines.iter().any(|line| line.contains("Text  ")
                    && line.replace(['▏', '▉'], "").trim_end().ends_with('w')),
                "{permission_lines:?}"
            );

            state.clear_menu(menu_id);
            state.key(key(KeyCode::Char('x')));
            assert!(
                state
                    .permissions_surface
                    .as_mut()
                    .unwrap()
                    .lines(80, 12)
                    .iter()
                    .any(|line| line.contains("Text  ")
                        && line.replace(['▏', '▉'], "").trim_end().ends_with("wx"))
            );
            state.draw_frame(&mut Vec::new()).unwrap();
            assert!(
                state
                    .rendered_screen
                    .iter()
                    .any(|line| line.contains("Replace Environment"))
            );
        }
        agent.shutdown().await;
    }

    #[test]
    fn login_link_wraps_without_changing_visible_text_or_target() {
        let address =
            "https://auth.openai.com/authorize?client_id=tickloop&state=long-challenge-value";
        let mut state = State::new(true, 32, 30);
        let id = Uuid::new_v4();
        state.menu = Some(Menu {
            id,
            title: "Codex login".into(),
            context: vec!["Open this link:".into(), address.into()],
            options: vec!["Cancel".into()],
            selected: 0,
            confirmation: false,
        });
        state.menu_link = Some((id, address.into()));
        let (lines, _, _) = state.styled_frame();
        let linked: Vec<_> = lines.iter().filter(|line| line.link.is_some()).collect();
        assert!(linked.len() > 1);
        assert_eq!(
            linked
                .iter()
                .map(|line| line.text().trim_start().to_owned())
                .collect::<String>(),
            address
        );
        for line in linked {
            assert!(display_width(&line.text()) <= state.width);
            let rendered = line
                .resolve(state.display.theme(), state.width)
                .render(ColourDepth::None);
            assert!(rendered.starts_with(&format!("\x1b]8;;{address}\x1b\\")));
            assert!(rendered.ends_with("\x1b]8;;\x1b\\"));
        }
        assert!(state.clear_menu(id));
        assert!(state.menu_link.is_none());
    }

    #[test]
    fn login_link_rejects_control_characters_and_non_web_targets() {
        assert!(valid_login_link("https://auth.openai.com/authorize").is_some());
        assert!(valid_login_link("https://auth.openai.com/\x1b]8;;evil").is_none());
        assert!(valid_login_link("file:///tmp/fake-login").is_none());
        assert!(valid_login_link("https://user:password@auth.openai.com/").is_none());
    }

    pub(super) fn task_snapshot(chat_id: Uuid, count: usize) -> ShellTaskSnapshot {
        ShellTaskSnapshot {
            chat_id,
            live_revision: 1,
            tasks: (0..count)
                .map(|index| ShellTask {
                    updated_at_ms: None,
                    id: Uuid::from_u128(index as u128 + 1),
                    chat_id,
                    origin: ShellTaskOrigin::User,
                    owner: ShellTaskOwner::User { chat_id },
                    command: format!("printf task-{index}"),
                    cwd: PathBuf::from("/tmp"),
                    state: ShellTaskState::Running,
                    pid: Some(1000 + index as u32),
                    exit_code: None,
                    signal: None,
                    reason: None,
                    output_bytes: 0,
                    was_background: false,
                    capabilities: ShellTaskCapabilities {
                        can_background: true,
                        can_stop: true,
                        can_read_output: false,
                    },
                })
                .collect(),
        }
    }

    pub(super) fn subscription_row(instance: u64) -> TickSubscriptionStatus {
        TickSubscriptionStatus {
            title: None,
            updated_at_ms: None,
            kind: None,
            id: Uuid::from_u128(u128::from(instance)),
            loop_id: Uuid::from_u128(900),
            loop_state: tickloop_agent::runtime::LoopState::WaitingTrigger,
            instance: tick_connect::instance::InstanceRef::native(format!(
                "fixture-run-{instance}"
            ))
            .unwrap(),
            every_seconds: 60,
            accumulated: 0,
            last_delivery_ms: None,
            last_delivery: None,
            link: tickloop_agent::runtime::TickLinkState::Live,
        }
    }

    fn subscription_view(rows: usize) -> State {
        let chat_id = Uuid::from_u128(77);
        let mut state = State::new(true, 80, 24);
        state.chat = Some(chat_id);
        state.subscription_surface = Some(SubscriptionSurface::open(
            chat_id,
            (0..rows)
                .map(|index| subscription_row(index as u64 + 3))
                .collect(),
        ));
        state
    }

    fn control(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn the_subscription_view_moves_with_j_k_keys_and_closes_on_escape() {
        let mut state = subscription_view(3);

        state.key(key(KeyCode::Char('j')));
        state.key(control(KeyCode::Char('n')));
        state.draw(&mut Vec::new()).unwrap();
        assert!(
            state
                .rendered_screen
                .iter()
                .any(|line| line.contains("❯ ◉ Tick Instance herdr:fixture-run-5")),
            "{:?}",
            state.rendered_screen
        );

        state.key(key(KeyCode::Char('k')));
        state.key(control(KeyCode::Char('p')));
        state.draw(&mut Vec::new()).unwrap();
        assert!(
            state
                .rendered_screen
                .iter()
                .any(|line| line.contains("❯ ◉ Tick Instance herdr:fixture-run-3")),
            "{:?}",
            state.rendered_screen
        );

        assert!(state.key(key(KeyCode::Esc)).is_none());
        assert!(state.subscription_surface.is_none());
    }

    #[test]
    fn the_subscription_view_edits_a_cadence_and_cancels_with_a_confirmation() {
        let mut state = subscription_view(2);

        state.key(key(KeyCode::Enter));
        for _ in 0..2 {
            state.key(key(KeyCode::Backspace));
        }
        state.key(key(KeyCode::Char('9')));
        state.key(key(KeyCode::Char('0')));
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Subscription(SubscriptionEffect::Change {
                subscription,
                change: tickloop_agent::runtime::TickSubscriptionChange::Cadence(90),
            })) if subscription == Uuid::from_u128(3)
        ));

        state.key(key(KeyCode::Char('d')));
        state.draw(&mut Vec::new()).unwrap();
        assert!(
            state
                .rendered_screen
                .last()
                .is_some_and(|line| line.contains("Cancel the subscription")),
            "{:?}",
            state.rendered_screen.last()
        );
        assert!(
            state.key(key(KeyCode::Esc)).is_none(),
            "Esc keeps the subscription"
        );
        assert!(state.subscription_surface.is_some());

        state.key(key(KeyCode::Char('d')));
        assert!(matches!(
            state.key(key(KeyCode::Char('y'))),
            Some(InputEvent::Subscription(SubscriptionEffect::Change {
                subscription,
                change: tickloop_agent::runtime::TickSubscriptionChange::Cancel,
            })) if subscription == Uuid::from_u128(3)
        ));
    }

    #[test]
    fn a_hidden_subscription_view_consumes_no_input() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;

        for character in ['d', 'j', 'k'] {
            state.key(key(KeyCode::Char(character)));
        }

        assert_eq!(state.editor.draft(), "djk");
    }

    fn diff_catalog(line_count: u64) -> ChangeCatalog {
        let before = FileVersion {
            exists: true,
            identity: None,
            byte_len: line_count,
            line_count,
            ends_with_newline: true,
            sha256: "before".into(),
        };
        let after = FileVersion {
            sha256: "after".into(),
            ..before.clone()
        };
        let review = FileChangeReview {
            version: 1,
            action: FileChangeAction::Edit,
            path: PathBuf::from("src/lib.rs"),
            before,
            after,
            hunks: vec![FileChangeHunk {
                old_start: 1,
                old_len: line_count,
                new_start: 1,
                new_len: line_count,
                lines: (1..=line_count)
                    .map(|line_number| FileChangeLine {
                        kind: FileChangeLineKind::Added,
                        old_line: None,
                        new_line: Some(line_number),
                        text: format!("changed line {line_number}"),
                        has_newline: true,
                    })
                    .collect(),
            }],
        };
        let summary = review.summary();
        ChangeCatalog {
            sources: vec![ChangeSource {
                id: ChangeSourceId::Current,
                state: ChangeSourceState::Ready,
                user_prompt: None,
                files: vec![CatalogFileChange {
                    id: CatalogFileChangeId::CurrentPath(PathBuf::from("src/lib.rs")),
                    path: PathBuf::from("src/lib.rs"),
                    action: FileChangeAction::Edit,
                    current_status: Some(CurrentFileStatus {
                        index: GitFileState::Unmodified,
                        worktree: GitFileState::Modified,
                        untracked: false,
                    }),
                    summary: Some(summary),
                    detail: CatalogChangeDetail::Available(Box::new(review)),
                }],
            }],
        }
    }

    pub(super) fn console_with_state(state: State) -> Console {
        let surface = state.surface.clone();
        let state = Arc::new(Mutex::new(state));
        Console {
            state: Arc::clone(&state),
            _lifetime: Arc::new(Lifetime {
                state,
                stop: Arc::new(AtomicBool::new(false)),
                reader: Mutex::new(None),
                interactive: false,
                surface,
            }),
            message_id: None,
            reset_chat_failure: Arc::new(Mutex::new(None)),
            branch_commit_failure: Arc::new(Mutex::new(None)),
        }
    }

    fn saved_file_context(path: &str, content: &str) -> InputContext {
        InputContext::File(TextContext {
            path: PathBuf::from(path),
            content: content.into(),
            truncated: false,
        })
    }

    fn pending_branch_console(chat: Option<Uuid>, command: &str) -> (Console, InputContext, usize) {
        let context = saved_file_context("branch-context", "saved context");
        let mut state = State::new(false, 80, 24);
        state.prompt_enabled = true;
        state.switch_editor_chat(chat);
        state.insert(command);
        state.completion.dismiss();
        state.file_mentions.dismiss();
        state.restored_contexts = vec![context.clone()];
        let cursor = state.editor.cursor();
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Prompt { text, .. }) if text == command
        ));
        (console_with_state(state), context, cursor)
    }

    fn assert_pre_save_branch_input_preserved(
        console: &Console,
        chat: Option<Uuid>,
        command: &str,
        context: &InputContext,
        cursor: usize,
        preserve_until_summary_finishes: bool,
    ) {
        let mut state = console.state.lock().unwrap();
        assert_eq!(state.chat, chat);
        assert_eq!(
            state.branch_submission,
            if preserve_until_summary_finishes {
                BranchSubmission::FinishedCancelledForSummary
            } else {
                BranchSubmission::FinishedCancelled
            }
        );
        assert_eq!(state.editor.draft(), command);
        assert_eq!(state.editor.cursor(), cursor);
        assert_eq!(
            state.restored_contexts.as_slice(),
            std::slice::from_ref(context)
        );
        for character in "TRAIL".chars() {
            assert!(state.key(key(KeyCode::Char(character))).is_none());
        }
        state.paste(" ignored tail paste");
        assert_eq!(state.editor.draft(), command);
        assert_eq!(
            state.restored_contexts.as_slice(),
            std::slice::from_ref(context)
        );
        assert!(state.clear_finished_branch_submission());
        assert_eq!(
            state.branch_submission,
            if preserve_until_summary_finishes {
                BranchSubmission::PreservedCancelled
            } else {
                BranchSubmission::Idle
            }
        );
    }

    pub(super) struct InteractiveConsoleFixture(Console);

    impl InteractiveConsoleFixture {
        pub(super) fn new(state: State) -> Self {
            assert!(state.interactive);
            let mut console = console_with_state(state);
            Arc::get_mut(&mut console._lifetime).unwrap().interactive = true;
            Self(console)
        }
    }

    impl std::ops::Deref for InteractiveConsoleFixture {
        type Target = Console;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl Drop for InteractiveConsoleFixture {
        fn drop(&mut self) {
            // This fixture exercises interactive routing without owning the process terminal.
            Arc::get_mut(&mut self.0._lifetime).unwrap().interactive = false;
        }
    }

    fn attach_permission_comments(state: &mut State) {
        state.menu_comment = Some(MenuCommentState {
            options: vec![0, 2],
            values: HashMap::new(),
            label: "Comment",
            editing: false,
            cursor: 0,
            secret: false,
            standalone_input: false,
        });
    }

    fn state_with_history(rows: usize) -> State {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.scrollback.append(
            &(0..rows)
                .map(|index| format!("history-{index}\n"))
                .collect::<String>(),
        );
        state.draw(&mut Vec::new()).unwrap();
        state
    }

    fn mouse(kind: MouseEventKind, row: u16, column: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn state_with_capturing_clipboard() -> (State, Arc<Mutex<Vec<String>>>) {
        let mut state = state_with_history(100);
        let copies = Arc::new(Mutex::new(Vec::new()));
        state.clipboard = Clipboard::Captured {
            copies: Arc::clone(&copies),
            image: None,
            remote: false,
        };
        (state, copies)
    }

    fn drag(state: &mut State, from: (u16, u16), to: (u16, u16)) {
        let mut output = Vec::new();
        state.pointer(
            mouse(MouseEventKind::Down(MouseButton::Left), from.0, from.1),
            &mut output,
        );
        state.pointer(
            mouse(MouseEventKind::Drag(MouseButton::Left), to.0, to.1),
            &mut output,
        );
        state.pointer(
            mouse(MouseEventKind::Up(MouseButton::Left), to.0, to.1),
            &mut output,
        );
    }

    #[test]
    fn dragging_over_the_conversation_copies_what_was_selected() {
        let (mut state, copies) = state_with_capturing_clipboard();
        let row = state
            .rendered_screen
            .iter()
            .position(|line| line.starts_with("history-"))
            .expect("the conversation should be on screen");
        let expected = state.rendered_screen[row][.."history-9".len()].to_owned();

        drag(
            &mut state,
            (row as u16, 0),
            (row as u16, "history-9".len() as u16),
        );

        assert_eq!(copies.lock().unwrap().as_slice(), [expected]);
    }

    #[test]
    fn dragging_over_the_draft_copies_it_without_disturbing_the_editor() {
        let (mut state, copies) = state_with_capturing_clipboard();
        state.replace_draft("可以选中的草稿".into(), "可以选中的草稿".len());
        state.draw(&mut Vec::new()).unwrap();
        let cursor = state.editor.cursor();
        let row = state
            .rendered_screen
            .iter()
            .position(|line| line.contains("可以选中的草稿"))
            .expect("the draft should be on screen");
        let byte = state.rendered_screen[row].find("可以选中的草稿").unwrap();
        let column = display_width(&state.rendered_screen[row][..byte]);

        drag(
            &mut state,
            (row as u16, column as u16),
            (row as u16, (column + 8) as u16),
        );

        assert_eq!(copies.lock().unwrap().as_slice(), ["可以选中"]);
        assert_eq!(state.editor.draft(), "可以选中的草稿");
        assert_eq!(state.editor.cursor(), cursor);
    }

    #[test]
    fn a_drag_highlights_exactly_the_copied_text_with_the_selection_background() {
        let (mut state, copies) = state_with_capturing_clipboard();
        let row = state
            .rendered_screen
            .iter()
            .position(|line| line.starts_with("history-"))
            .expect("the conversation should be on screen");
        let width = "history-9".len();
        let mut output = Vec::new();
        state.pointer(
            mouse(MouseEventKind::Down(MouseButton::Left), row as u16, 0),
            &mut output,
        );
        state.pointer(
            mouse(
                MouseEventKind::Drag(MouseButton::Left),
                row as u16,
                width as u16,
            ),
            &mut output,
        );

        let line = state.rendered_screen[row].clone();
        let highlighted = state.with_selection(row, StyledLine::plain(line.as_str()));
        let background = state.display.theme().style(Role::Selection).background;
        let selected: String = highlighted
            .spans
            .iter()
            .filter(|span| span.style.background == background)
            .map(|span| span.text.as_str())
            .collect();
        assert_eq!(selected, &line[..width]);
        assert_eq!(
            highlighted
                .spans
                .iter()
                .map(|span| span.text.as_str())
                .collect::<String>(),
            line,
            "highlighting must not change the text"
        );
        assert!(
            state
                .with_selection(row + 1, StyledLine::plain("other row"))
                .spans
                .iter()
                .all(|span| span.style.background != background),
            "rows outside the selection stay unhighlighted"
        );

        state.pointer(
            mouse(
                MouseEventKind::Up(MouseButton::Left),
                row as u16,
                width as u16,
            ),
            &mut output,
        );
        assert_eq!(copies.lock().unwrap().as_slice(), [&line[..width]]);
        assert!(
            state.selection.is_some(),
            "the highlight stays after copying"
        );

        state.clear_selection();
        assert_eq!(
            state.with_selection(row, StyledLine::plain(line.as_str())),
            StyledLine::plain(line.as_str())
        );
    }

    #[test]
    fn dragged_text_stays_readable_in_every_theme_and_colour_depth() {
        let (mut state, _) = state_with_capturing_clipboard();
        let row = state
            .rendered_screen
            .iter()
            .position(|line| line.starts_with("history-"))
            .expect("the conversation should be on screen");
        drag(&mut state, (row as u16, 0), (row as u16, 20));
        let styled = RoleLine::role("history ", Role::MutedText)
            .with("failed", Role::Error)
            .with(" │ quoted", Role::QuoteBar);
        for name in ThemeName::ALL {
            state.display.set_theme(name);
            let theme = state.display.theme();
            let selection = theme
                .style(Role::Selection)
                .background
                .expect("every theme paints the selection");
            for line in [
                styled.resolve(theme, state.width),
                StyledLine::plain("history-0 in the terminal colour"),
            ] {
                let highlighted = state.with_selection(row, line);
                let selected: Vec<_> = highlighted
                    .spans
                    .iter()
                    .filter(|span| span.style.background == Some(selection))
                    .collect();
                assert!(!selected.is_empty(), "{name:?} should highlight the drag");
                for span in selected {
                    assert!(!span.style.dim && !span.style.reversed, "{:?}", span.text);
                    let foreground = span
                        .style
                        .foreground
                        .expect("selected text never keeps the terminal default colour");
                    assert_eq!(
                        Some(foreground),
                        theme.style(Role::Text).foreground,
                        "selected text uses the theme's Text role"
                    );
                    for depth in [ColourDepth::Indexed256, ColourDepth::TrueColour] {
                        let ratio =
                            super::super::style::emitted_contrast(foreground, selection, depth);
                        assert!(
                            ratio >= 4.5,
                            "{name:?} {depth:?} {:?}: {ratio:.2}",
                            span.text
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_release_away_from_the_press_selects_even_without_motion_reports() {
        let (mut state, copies) = state_with_capturing_clipboard();
        let row = state
            .rendered_screen
            .iter()
            .position(|line| line.starts_with("history-"))
            .expect("the conversation should be on screen");
        let width = "history-9".len();
        let expected = state.rendered_screen[row][..width].to_owned();
        let mut output = Vec::new();
        state.pointer(
            mouse(MouseEventKind::Down(MouseButton::Left), row as u16, 0),
            &mut output,
        );
        state.pointer(
            mouse(
                MouseEventKind::Up(MouseButton::Left),
                row as u16,
                width as u16,
            ),
            &mut output,
        );
        assert_eq!(copies.lock().unwrap().as_slice(), [expected]);
    }

    #[test]
    fn a_click_without_a_drag_leaves_the_clipboard_alone() {
        let (mut state, copies) = state_with_capturing_clipboard();
        let mut output = Vec::new();
        state.pointer(
            mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
            &mut output,
        );
        state.pointer(
            mouse(MouseEventKind::Up(MouseButton::Left), 3, 4),
            &mut output,
        );
        assert!(copies.lock().unwrap().is_empty());
        assert!(state.selection.is_none());
    }

    #[test]
    fn scrolling_drops_a_selection_that_no_longer_matches_the_screen() {
        let (mut state, _copies) = state_with_capturing_clipboard();
        let mut output = Vec::new();
        state.pointer(
            mouse(MouseEventKind::Down(MouseButton::Left), 3, 0),
            &mut output,
        );
        state.pointer(
            mouse(MouseEventKind::Drag(MouseButton::Left), 4, 6),
            &mut output,
        );
        assert!(state.selection.is_some());

        state.pointer(mouse(MouseEventKind::ScrollUp, 3, 0), &mut output);
        assert!(state.selection.is_none());
    }

    #[test]
    fn dragging_without_a_press_first_selects_nothing() {
        let (mut state, copies) = state_with_capturing_clipboard();
        let mut output = Vec::new();
        assert!(!state.pointer(
            mouse(MouseEventKind::Drag(MouseButton::Left), 3, 4),
            &mut output
        ));
        assert!(state.selection.is_none());
        assert!(copies.lock().unwrap().is_empty());
    }

    #[test]
    fn the_wheel_scrolls_the_conversation_instead_of_the_input_history() {
        let mut state = state_with_history(100);
        let draft = state.editor.draft().to_owned();
        let at_tail = state.scrollback.frame(state.width, 21).lines;

        assert!(state.scroll_conversation(ScrollAction::LinesUp(renderer::WHEEL_SCROLL_LINES)));
        let scrolled_up = state.scrollback.frame(state.width, 21).lines;
        assert_ne!(
            scrolled_up, at_tail,
            "the wheel should move the conversation"
        );
        assert_eq!(
            state.editor.draft(),
            draft,
            "the wheel must not reach the input editor"
        );

        assert!(state.scroll_conversation(ScrollAction::LinesDown(renderer::WHEEL_SCROLL_LINES)));
        assert_eq!(
            state.scrollback.frame(state.width, 21).lines,
            at_tail,
            "scrolling back down should return to where it started"
        );
    }

    #[test]
    fn only_the_wheel_is_acted_on_and_only_over_the_conversation() {
        let mut state = state_with_history(100);
        let at_tail = state.scrollback.frame(state.width, 21).lines;

        for ignored in [
            MouseEventKind::Moved,
            MouseEventKind::Down(MouseButton::Right),
        ] {
            assert!(
                !state.pointer(mouse(ignored, 3, 4), &mut Vec::new()),
                "{ignored:?} is captured only so the wheel survives; it must not cost a frame"
            );
            assert_eq!(
                state.scrollback.frame(state.width, 21).lines,
                at_tail,
                "{ignored:?} must not move the conversation"
            );
        }

        state.diff_screen = Some(DiffScreen {
            chat_id: Uuid::new_v4(),
            catalog: ChangeCatalog {
                sources: vec![ChangeSource {
                    id: ChangeSourceId::Current,
                    state: ChangeSourceState::Clean,
                    user_prompt: None,
                    files: Vec::new(),
                }],
            },
            dialog: DiffDialog::new(),
        });
        assert!(
            !state.pointer(mouse(MouseEventKind::ScrollUp, 3, 4), &mut Vec::new()),
            "an overlay owns the screen; the wheel must not scroll behind it"
        );
        assert_eq!(state.scrollback.frame(state.width, 21).lines, at_tail);
    }

    #[test]
    fn a_poisoned_state_lock_still_leaves_the_alternate_screen() {
        let state = Arc::new(Mutex::new(State::new(true, 80, 24)));
        let surface = state.lock().unwrap().surface.clone();
        surface.take(Renderer::Fullscreen, &mut Vec::new()).unwrap();

        let poisoned = Arc::clone(&state);
        let _ = thread::spawn(move || {
            let _guard = poisoned.lock().unwrap();
            panic!("poison the state lock");
        })
        .join();
        assert!(state.lock().is_err(), "the state lock should be poisoned");

        // What `Lifetime::drop` is left with once the state is unreadable.
        let mut output = Vec::new();
        let _ = restore_terminal(&mut output, &surface);
        assert!(
            String::from_utf8(output).unwrap().contains("\x1b[?1049l"),
            "a panic while the state was locked must not strand the terminal"
        );
    }

    #[test]
    fn a_non_interactive_console_reports_no_renderer_and_refuses_to_switch() {
        let console = console_with_state(State::new(false, 80, 24));
        assert_eq!(console.renderer().unwrap(), None);
        assert_eq!(
            console.switch_renderer(Renderer::Fullscreen).unwrap(),
            RendererSwitch::NotInteractive
        );
        assert_eq!(
            console.state.lock().unwrap().renderer,
            Renderer::Inline,
            "a refused switch must not change the surface"
        );
    }

    #[test]
    fn the_first_inline_frame_scrolls_a_screen_into_view_and_fullscreen_does_not() {
        let mut inline = State::new(true, 80, 24);
        inline.prompt_enabled = true;
        let mut output = Vec::new();
        inline.draw(&mut output).unwrap();
        let scrolled = String::from_utf8(output).unwrap().matches("\r\n").count();
        assert!(
            scrolled >= inline.height,
            "the inline renderer has to carve its screen out of the main buffer, saw {scrolled}"
        );

        let mut fullscreen = State::new(true, 80, 24);
        fullscreen.renderer = Renderer::Fullscreen;
        fullscreen.prompt_enabled = true;
        let mut output = Vec::new();
        fullscreen.draw(&mut output).unwrap();
        assert!(
            !String::from_utf8(output).unwrap().contains("\r\n"),
            "the alternate screen already is a blank full screen"
        );
    }

    #[test]
    fn switching_renderers_keeps_the_draft_and_the_scroll_position() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.replace_draft("草稿".into(), "草稿".len());
        state.scrollback.auto_scroll();
        let draft = state.editor.draft().to_owned();
        let auto_scroll = state.scrollback.auto_scroll();

        state.renderer = Renderer::Fullscreen;
        state.repaint_whole_screen(&mut Vec::new());

        assert_eq!(state.editor.draft(), draft);
        assert_eq!(state.scrollback.auto_scroll(), auto_scroll);
    }

    #[test]
    fn synchronized_output_wraps_a_frame_in_one_pair_of_markers() {
        let mut state = State::new(true, 80, 24);
        state.synchronized_output = true;
        state.prompt_enabled = true;
        let mut output = Vec::new();
        state.draw(&mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with("\x1b[?2026h"), "{output:?}");
        assert!(output.ends_with("\x1b[?2026l"), "{output:?}");
        assert_eq!(output.matches("\x1b[?2026h").count(), 1);
        assert_eq!(output.matches("\x1b[?2026l").count(), 1);
    }

    #[test]
    fn a_non_interactive_console_emits_no_frame_and_no_markers() {
        let mut state = State::new(false, 80, 24);
        state.synchronized_output = true;
        state.renderer = Renderer::Fullscreen;
        let mut output = Vec::new();
        state.draw(&mut output).unwrap();
        assert!(output.is_empty(), "{output:?}");
    }

    #[test]
    fn backtab_on_a_child_explains_its_fixed_mode_until_the_next_key() {
        let mut state = State::new(true, 100, 24);
        state.prompt_enabled = true;
        state.visible_delegated = true;
        state.insert("keep this draft");

        assert!(state.key(key(KeyCode::BackTab)).is_none());
        assert!(state.status.contains("Shift+Tab mode is unavailable here"));
        state.refresh_status("Ready");
        assert!(
            state.status.contains("Shift+Tab mode is unavailable here"),
            "a periodic refresh keeps the reason"
        );
        assert_eq!(state.editor.draft(), "keep this draft");
        state.key(key(KeyCode::Char('!')));
        state.refresh_status("Ready");
        assert_eq!(state.status, "Ready");
    }

    #[test]
    fn backtab_cycles_execution_mode_only_from_the_plain_prompt_surface() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("keep this draft");
        let draft = state.editor.draft().to_owned();

        assert!(matches!(
            state.key(key(KeyCode::BackTab)),
            Some(InputEvent::CycleExecutionMode)
        ));
        assert_eq!(state.editor.draft(), draft);

        state.menu = Some(menu());
        assert!(state.key(key(KeyCode::BackTab)).is_none());
        assert!(state.menu_focused);
    }

    #[test]
    fn diff_focus_owns_j_k_navigation_and_cannot_approve_a_permission() {
        let mut state = State::new(true, 40, 18);
        state.prompt_enabled = true;
        state.menu = Some(menu());
        let catalog = diff_catalog(80);
        let CatalogChangeDetail::Available(review) = &catalog.sources[0].files[0].detail else {
            unreachable!()
        };
        state.menu_review = Some(MenuReviewState {
            review: Ok(review.as_ref().clone()),
            navigation: DiffNavigation::default(),
            viewport: DiffViewport {
                width: 1,
                height: 1,
            },
        });
        state.review_focused = false;
        state.menu_focused = true;
        state.key(key(KeyCode::BackTab));
        assert!(state.review_focused);
        assert!(!state.menu_focused);
        let _ = state.frame();
        let before = state.menu_review.as_ref().unwrap().navigation.clone();

        assert!(state.key(key(KeyCode::Char('j'))).is_none());
        assert_ne!(
            state.menu_review.as_ref().unwrap().navigation,
            before,
            "j should scroll the focused diff"
        );
        assert!(state.key(key(KeyCode::Enter)).is_none());
        assert!(state.key(key(KeyCode::Char('y'))).is_none());
        assert!(state.menu.is_some());

        state.key(key(KeyCode::Tab));
        assert!(!state.review_focused);
        assert!(state.menu_focused);
        assert!(matches!(
            state.key(key(KeyCode::Char('y'))),
            Some(InputEvent::Choice { index: 0, .. })
        ));
    }

    #[test]
    fn unavailable_file_review_cannot_be_approved_but_remains_deniable() {
        let mut state = State::new(true, 80, 18);
        let permission = menu();
        let permission_id = permission.id;
        state.menu = Some(permission);
        state.menu_review = Some(MenuReviewState {
            review: Err("review artifact is missing".into()),
            navigation: DiffNavigation::default(),
            viewport: DiffViewport {
                width: 1,
                height: 1,
            },
        });
        state.menu_focused = true;

        assert!(state.key(key(KeyCode::Char('y'))).is_none());
        assert!(state.key(key(KeyCode::Enter)).is_none());
        assert!(state.menu.is_some());
        assert!(state.status.contains("Cannot approve"));
        assert!(matches!(
            state.key(key(KeyCode::Char('n'))),
            Some(InputEvent::Cancel { menu_id }) if menu_id == permission_id
        ));
    }

    #[test]
    fn two_choice_permission_can_be_declined_when_file_review_is_unavailable() {
        let mut state = State::new(true, 80, 18);
        let mut permission = menu();
        permission.options = vec!["Yes, once".into(), "No".into()];
        permission.selected = 1;
        let permission_id = permission.id;
        state.menu = Some(permission);
        state.menu_review = Some(MenuReviewState {
            review: Err("review artifact is missing".into()),
            navigation: DiffNavigation::default(),
            viewport: DiffViewport {
                width: 1,
                height: 1,
            },
        });
        state.menu_focused = true;
        let _ = state.frame();

        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Choice {
                menu_id,
                index: 1,
                ..
            }) if menu_id == permission_id
        ));
    }

    #[test]
    fn tiny_diff_review_blocks_hidden_permission_choices() {
        let mut state = State::new(true, 40, 4);
        state.menu = Some(menu());
        state.menu_review = Some(MenuReviewState {
            review: Err("review unavailable".into()),
            navigation: DiffNavigation::default(),
            viewport: DiffViewport {
                width: 1,
                height: 1,
            },
        });
        state.review_focused = true;

        assert!(state.key(key(KeyCode::Enter)).is_none());
        assert!(state.key(key(KeyCode::Char('y'))).is_none());
        assert!(state.menu.is_some());
        assert!(state.frame().0[0].contains("Resize"));
    }

    #[test]
    fn diff_screen_owns_keys_and_clean_state_closes_with_escape() {
        let mut state = State::new(true, 60, 12);
        state.prompt_enabled = true;
        state.diff_screen = Some(DiffScreen {
            chat_id: Uuid::new_v4(),
            catalog: ChangeCatalog {
                sources: vec![ChangeSource {
                    id: ChangeSourceId::Current,
                    state: ChangeSourceState::Clean,
                    user_prompt: None,
                    files: Vec::new(),
                }],
            },
            dialog: DiffDialog::new(),
        });

        let mut output = Vec::new();
        state.draw(&mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Current · working tree"));
        assert!(output.contains("Working tree is clean."));
        assert!(state.key(key(KeyCode::Char('x'))).is_none());
        assert!(state.editor.draft().is_empty());
        assert!(state.diff_screen.is_some());
        assert!(state.key(key(KeyCode::Esc)).is_none());
        assert!(state.diff_screen.is_none());
    }

    #[test]
    fn permission_menu_overlays_diff_and_returns_to_the_saved_anchor() {
        let mut state = State::new(true, 40, 10);
        state.prompt_enabled = true;
        state.diff_screen = Some(DiffScreen {
            chat_id: Uuid::new_v4(),
            catalog: diff_catalog(40),
            dialog: DiffDialog::new(),
        });
        state.key(key(KeyCode::Enter));
        state.key(key(KeyCode::End));
        let viewport = DiffViewport {
            width: state.width,
            height: state.height.saturating_sub(2),
        };
        let before = state
            .diff_screen
            .as_mut()
            .unwrap()
            .dialog
            .frame(&diff_catalog(40), viewport)
            .detail
            .unwrap()
            .top_row;

        let permission = menu();
        let permission_id = permission.id;
        state.menu = Some(permission);
        state.menu_focused = true;
        assert!(matches!(
            state.key(key(KeyCode::Char('y'))),
            Some(InputEvent::Choice { menu_id, index: 0, .. }) if menu_id == permission_id
        ));
        assert!(state.diff_screen.is_some());
        state.menu = None;
        state.menu_focused = false;

        let screen = state.diff_screen.as_mut().unwrap();
        let after = screen
            .dialog
            .frame(&screen.catalog, viewport)
            .detail
            .unwrap()
            .top_row;
        assert_eq!(after, before);
        assert!(after > 0);
        assert!(state.editor.draft().is_empty());
    }

    #[test]
    fn changing_chat_caches_the_previous_chat_diff_view() {
        let first_chat = Uuid::new_v4();
        let second_chat = Uuid::new_v4();
        let mut state = State::new(false, 80, 24);
        state.chat = Some(first_chat);
        state.diff_screen = Some(DiffScreen {
            chat_id: first_chat,
            catalog: diff_catalog(1),
            dialog: DiffDialog::new(),
        });
        let console = console_with_state(state);

        console.select_chat(Some(second_chat)).unwrap();
        {
            let state = console.state.lock().unwrap();
            assert!(state.diff_screen.is_none());
            assert!(state.diff_screen_cache.contains_key(&first_chat));
        }

        console.state.lock().unwrap().diff_screen = Some(DiffScreen {
            chat_id: second_chat,
            catalog: diff_catalog(1),
            dialog: DiffDialog::new(),
        });
        console.reset_chat(second_chat).unwrap();
        assert!(console.state.lock().unwrap().diff_screen.is_none());
    }

    #[test]
    fn branch_input_is_not_recalled_and_chat_editor_histories_are_isolated() {
        let source_chat = Uuid::new_v4();
        let child_chat = Uuid::new_v4();
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.switch_editor_chat(Some(source_chat));

        state.insert("source prompt");
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Prompt { text, .. }) if text == "source prompt"
        ));
        state.insert("/branch child");
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Prompt { text, .. }) if text == "/branch child"
        ));
        assert_eq!(state.branch_submission, BranchSubmission::Pending);
        assert_eq!(state.editor.draft(), "/branch child");

        state.commit_branch_submission();
        state.reset_chat(child_chat);
        assert_eq!(state.branch_submission, BranchSubmission::FinishedCommitted);
        for character in "TRAIL".chars() {
            assert!(state.key(key(KeyCode::Char(character))).is_none());
        }
        state.paste("ignored paste");
        assert!(state.editor.draft().is_empty());
        assert!(state.clear_finished_branch_submission());
        assert_eq!(state.branch_submission, BranchSubmission::Idle);
        assert!(state.key(key(KeyCode::Up)).is_none());
        assert!(state.editor.draft().is_empty());
        state.insert("child prompt");
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Prompt { text, .. }) if text == "child prompt"
        ));

        state.reset_chat(source_chat);
        assert!(state.key(key(KeyCode::Up)).is_none());
        assert_eq!(state.editor.draft(), "source prompt");
        state.reset_chat(child_chat);
        assert!(state.key(key(KeyCode::Up)).is_none());
        assert_eq!(state.editor.draft(), "child prompt");
    }

    #[test]
    fn keys_typed_before_a_chat_switch_stay_in_the_original_agent_draft() {
        let first_chat = Uuid::new_v4();
        let second_chat = Uuid::new_v4();
        let resumed_chat = Uuid::new_v4();
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.switch_editor_chat(Some(first_chat));
        state.insert("first prompt");
        state.key(key(KeyCode::Enter));
        state.insert("/clear");
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Prompt { text, .. }) if text == "/clear"
        ));
        for character in "abc".chars() {
            assert!(state.key(key(KeyCode::Char(character))).is_none());
        }

        state.reset_chat(second_chat);
        for character in "def".chars() {
            assert!(state.key(key(KeyCode::Char(character))).is_none());
        }
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Prompt { text, .. }) if text == "def"
        ));
        assert!(state.key(key(KeyCode::Up)).is_none());
        assert_eq!(state.editor.draft(), "def");
        state.key(key(KeyCode::Down));

        state.insert("/resume");
        state.key(key(KeyCode::Enter));
        state.insert("乙");
        state.reset_chat(resumed_chat);
        state.insert("会话");
        assert_eq!(state.editor.draft(), "会话");

        state.editor.replace_draft(String::new(), 0);
        state.reset_chat(first_chat);
        assert_eq!(state.editor.draft(), "abc");
        state.editor.replace_draft(String::new(), 0);
        assert!(state.key(key(KeyCode::Up)).is_none());
        assert_eq!(state.editor.draft(), "/clear");
    }

    #[test]
    fn rejected_branch_keeps_editor_context_focus_and_paused_scroll() {
        let source_chat = Uuid::new_v4();
        let command = "/branch @src/lib.rs";
        let saved_context = saved_file_context("src/lib.rs", "checkpoint contents");
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.switch_editor_chat(Some(source_chat));
        state.scrollback.append(
            &(0..100)
                .map(|index| format!("history-{index}\n"))
                .collect::<String>(),
        );
        state.draw(&mut Vec::new()).unwrap();
        state.scrollback.scroll(ScrollAction::First);
        let before_scroll = state.scrollback.frame(state.width, 21).lines;

        state.menu_focused = false;
        state.context_offset = 3;
        state.insert(command);
        state.restored_contexts = vec![saved_context.clone()];
        state.completion.dismiss();
        state.file_mentions.dismiss();
        let original_cursor = state.editor.cursor();

        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Prompt { text, file_references, .. })
                if text == command && file_references.is_empty()
        ));
        assert_eq!(state.branch_submission, BranchSubmission::Pending);
        assert_eq!(state.editor.draft(), command);
        assert_eq!(state.editor.cursor(), original_cursor);
        assert_eq!(
            state.restored_contexts.as_slice(),
            std::slice::from_ref(&saved_context)
        );

        assert!(state.key(key(KeyCode::Char('x'))).is_none());
        assert!(state.key(key(KeyCode::PageDown)).is_none());
        state.paste(" ignored while branching");
        state.record_input(command);
        state.scrollback.append("Failed to branch conversation\n");
        assert_eq!(state.scrollback.frame(state.width, 21).lines, before_scroll);
        assert_eq!(state.editor.draft(), command);
        assert_eq!(state.editor.cursor(), original_cursor);
        assert_eq!(
            state.restored_contexts.as_slice(),
            std::slice::from_ref(&saved_context)
        );
        assert!(state.menu.is_none());
        assert!(!state.menu_focused);
        assert!(!state.review_focused);
        assert_eq!(state.context_offset, 3);

        state.cancel_branch_submission(false);
        assert_eq!(state.branch_submission, BranchSubmission::FinishedCancelled);
        for character in "TRAIL".chars() {
            assert!(state.key(key(KeyCode::Char(character))).is_none());
        }
        state.paste(" ignored after rejection");
        assert_eq!(state.editor.draft(), command);
        assert_eq!(state.restored_contexts, [saved_context]);
        assert!(state.clear_finished_branch_submission());
        assert_eq!(state.branch_submission, BranchSubmission::Idle);
        assert!(state.key(key(KeyCode::Left)).is_none());
        assert!(state.editor.cursor() < original_cursor);
    }

    #[test]
    fn summary_success_cannot_replace_a_branch_draft_after_busy_rejection() {
        for drain_before_summary in [false, true] {
            for restore_checkpoint_draft in [false, true] {
                let source_chat = Uuid::new_v4();
                let command = "/branch @branch-context";
                let context = saved_file_context("branch-context", "saved context");
                let replacement_context = saved_file_context("checkpoint", "replacement context");
                let mut state = State::new(false, 80, 24);
                state.prompt_enabled = true;
                state.switch_editor_chat(Some(source_chat));
                state.insert(command);
                state.completion.dismiss();
                state.file_mentions.dismiss();
                state.restored_contexts = vec![context.clone()];
                let cursor = state.editor.cursor();
                assert!(matches!(
                    state.key(key(KeyCode::Enter)),
                    Some(InputEvent::Prompt { text, .. }) if text == command
                ));
                state.cancel_branch_submission(true);
                if drain_before_summary {
                    assert!(state.clear_finished_branch_submission());
                    assert_eq!(
                        state.branch_submission,
                        BranchSubmission::PreservedCancelled
                    );
                }
                let console = console_with_state(state);

                console.reset_chat(source_chat).unwrap();
                if restore_checkpoint_draft {
                    console
                        .restore_rewind_draft(
                            "checkpoint draft that must not replace /branch",
                            std::slice::from_ref(&replacement_context),
                        )
                        .unwrap();
                } else {
                    console.clear_summary_draft().unwrap();
                }

                let mut state = console.state.lock().unwrap();
                assert_eq!(state.editor.draft(), command);
                assert_eq!(state.editor.cursor(), cursor);
                assert_eq!(state.restored_contexts, [context]);
                if drain_before_summary {
                    assert_eq!(state.branch_submission, BranchSubmission::Idle);
                } else {
                    assert_eq!(state.branch_submission, BranchSubmission::FinishedCancelled);
                    for character in "TRAIL".chars() {
                        assert!(state.key(key(KeyCode::Char(character))).is_none());
                    }
                    state.paste("ignored trailing paste");
                    assert_eq!(state.editor.draft(), command);
                    assert!(state.clear_finished_branch_submission());
                    assert_eq!(state.branch_submission, BranchSubmission::Idle);
                }
            }
        }
    }

    #[test]
    fn summary_failure_and_observer_completion_release_branch_protection_without_editing() {
        for drain_before_completion in [false, true] {
            for _completion in ["failure", "observer-stopped"] {
                let source_chat = Uuid::new_v4();
                let command = "/branch @branch-context";
                let context = saved_file_context("branch-context", "saved context");
                let (console, _, cursor) = pending_branch_console(Some(source_chat), command);
                {
                    let mut state = console.state.lock().unwrap();
                    state.cancel_branch_submission(true);
                    if drain_before_completion {
                        assert!(state.clear_finished_branch_submission());
                        assert_eq!(
                            state.branch_submission,
                            BranchSubmission::PreservedCancelled
                        );
                    }
                }

                console.finish_summary_input_transition().unwrap();

                let mut state = console.state.lock().unwrap();
                assert_eq!(state.editor.draft(), command);
                assert_eq!(state.editor.cursor(), cursor);
                assert_eq!(state.restored_contexts, [context]);
                if drain_before_completion {
                    assert_eq!(state.branch_submission, BranchSubmission::Idle);
                } else {
                    assert_eq!(state.branch_submission, BranchSubmission::FinishedCancelled);
                    state.paste("ignored trailing paste");
                    assert_eq!(state.editor.draft(), command);
                    assert!(state.clear_finished_branch_submission());
                }
            }
        }
    }

    #[tokio::test]
    async fn summary_completion_before_branch_dispatch_preserves_later_failure_input() {
        let directory = tempfile::tempdir().unwrap();
        let agent = Agent::new_embedded(
            support::config(),
            Arc::new(support::ScriptedModel::new(Vec::new())),
        );
        let source_chat = agent.create_chat(directory.path().into()).await.unwrap();
        let command = "/branch summary-finished-first";
        let (console, context, cursor) = pending_branch_console(Some(source_chat), command);

        console.reset_chat(source_chat).unwrap();
        console.clear_summary_draft().unwrap();
        {
            let state = console.state.lock().unwrap();
            assert_eq!(state.branch_submission, BranchSubmission::Pending);
            assert_eq!(state.editor.draft(), command);
            assert_eq!(state.restored_contexts, std::slice::from_ref(&context));
        }

        let error = branch_from_terminal(&console, &agent, Some(source_chat), None, command, false)
            .await
            .unwrap_err();

        assert!(error.contains("there is no conversation to branch"));
        assert_pre_save_branch_input_preserved(
            &console,
            Some(source_chat),
            command,
            &context,
            cursor,
            false,
        );
        agent.shutdown().await;
    }

    #[test]
    fn post_save_reset_failure_commits_the_branch_input_without_switching_chat() {
        let source_chat = Uuid::new_v4();
        let child_chat = Uuid::new_v4();
        let command = "/branch @recoverable-child";
        let saved_context = saved_file_context("recoverable-child", "saved context");
        let mut state = State::new(false, 80, 24);
        state.prompt_enabled = true;
        state.switch_editor_chat(Some(source_chat));
        state.insert(command);
        state.completion.dismiss();
        state.file_mentions.dismiss();
        state.restored_contexts = vec![saved_context.clone()];
        let original_cursor = state.editor.cursor();
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Prompt { text, .. }) if text == command
        ));
        let console = console_with_state(state);
        console.fail_next_reset_chat("renderer failed").unwrap();

        assert_eq!(
            console
                .commit_branch_and_reset_chat(child_chat)
                .unwrap_err(),
            "renderer failed"
        );
        {
            let state = console.state.lock().unwrap();
            assert_eq!(state.chat, Some(source_chat));
            assert_eq!(state.branch_submission, BranchSubmission::Pending);
            assert_eq!(state.editor.draft(), command);
            assert_eq!(state.editor.cursor(), original_cursor);
            assert_eq!(state.restored_contexts, [saved_context]);
        }

        console.commit_branch_submission().unwrap();
        let mut state = console.state.lock().unwrap();
        assert_eq!(state.chat, Some(source_chat));
        assert_eq!(state.branch_submission, BranchSubmission::FinishedCommitted);
        assert!(state.editor.draft().is_empty());
        assert!(state.restored_contexts.is_empty());
        for character in "TRAIL".chars() {
            assert!(state.key(key(KeyCode::Char(character))).is_none());
        }
        state.paste("ignored paste");
        assert!(state.editor.draft().is_empty());
        assert!(state.clear_finished_branch_submission());
        assert!(state.key(key(KeyCode::Up)).is_none());
        assert!(state.editor.draft().is_empty());
    }

    #[tokio::test]
    async fn terminal_pre_save_branch_errors_preserve_the_retryable_input() {
        let directory = tempfile::tempdir().unwrap();
        let agent = Agent::new_embedded(
            support::config(),
            Arc::new(support::ScriptedModel::new(Vec::new())),
        );
        let empty_chat = agent.create_chat(directory.path().into()).await.unwrap();
        let command = "/branch @branch-context";

        let (console, context, cursor) = pending_branch_console(None, command);
        let error = branch_from_terminal(&console, &agent, None, None, command, false)
            .await
            .unwrap_err();
        assert!(error.contains("No Chat is selected"));
        assert_pre_save_branch_input_preserved(&console, None, command, &context, cursor, false);

        let (console, context, cursor) = pending_branch_console(Some(empty_chat), command);
        let error = branch_from_terminal(
            &console,
            &agent,
            Some(empty_chat),
            None,
            "/branching",
            false,
        )
        .await
        .unwrap_err();
        assert!(error.contains("use /branch [NAME]"));
        assert_pre_save_branch_input_preserved(
            &console,
            Some(empty_chat),
            command,
            &context,
            cursor,
            false,
        );

        let (console, context, cursor) = pending_branch_console(Some(empty_chat), command);
        let error = branch_from_terminal(&console, &agent, Some(empty_chat), None, command, false)
            .await
            .unwrap_err();
        assert!(error.contains("there is no conversation to branch"));
        assert_pre_save_branch_input_preserved(
            &console,
            Some(empty_chat),
            command,
            &context,
            cursor,
            false,
        );
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn terminal_busy_branch_rejection_is_immediate_and_keeps_the_input() {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(support::ScriptedModel::new(vec![
            support::text("source answer"),
            support::call(
                "write",
                "Write",
                serde_json::json!({"file_path":"busy.txt","content":"pending"}),
            ),
        ]));
        let agent = Agent::new_embedded(support::config(), model);
        let source_chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_permission_rules(
                source_chat,
                vec![PermissionRule {
                    id: Uuid::new_v4(),
                    decision: Decision::Ask,
                    tool: "Write".into(),
                    specifier: None,
                    input: None,
                    loop_id: None,
                    persistent: false,
                }],
            )
            .await
            .unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(source_chat, None, "source prompt".into())
            .await
            .unwrap();
        loop {
            match support::next_event(&mut events).await.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("source fixture failed: {error}"),
                _ => {}
            }
        }
        let waiting = agent
            .submit(source_chat, None, "request a write".into())
            .await
            .unwrap();
        loop {
            match support::next_event(&mut events).await.kind {
                EventKind::PermissionRequested { .. } => break,
                EventKind::Failed(error) => panic!("busy fixture failed: {error}"),
                _ => {}
            }
        }
        let command = "/branch @branch-context";
        let (console, context, cursor) = pending_branch_console(Some(source_chat), command);

        let error = branch_from_terminal(
            &console,
            &agent,
            Some(source_chat),
            Some(waiting.loop_id),
            command,
            true,
        )
        .await
        .unwrap_err();

        assert_eq!(error, "Failed to branch conversation: chat is busy");
        assert_pre_save_branch_input_preserved(
            &console,
            Some(source_chat),
            command,
            &context,
            cursor,
            true,
        );
        console.finish_summary_input_transition().unwrap();
        assert_eq!(
            console.state.lock().unwrap().branch_submission,
            BranchSubmission::Idle
        );
        assert_eq!(agent.list_chats().await.len(), 1);
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn terminal_summary_tracker_rejects_branch_after_runtime_becomes_quiescent() {
        let directory = tempfile::tempdir().unwrap();
        let agent = Agent::new_embedded(
            support::config(),
            Arc::new(support::ScriptedModel::new(vec![support::text(
                "source answer",
            )])),
        );
        let source_chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        let source = agent
            .submit(source_chat, None, "source prompt".into())
            .await
            .unwrap();
        loop {
            match support::next_event(&mut events).await.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("source fixture failed: {error}"),
                _ => {}
            }
        }
        let command = "/branch completion-ready-summary";
        let (console, context, cursor) = pending_branch_console(Some(source_chat), command);

        let error = branch_from_terminal(
            &console,
            &agent,
            Some(source_chat),
            Some(source.loop_id),
            command,
            true,
        )
        .await
        .unwrap_err();

        assert_eq!(error, "Failed to branch conversation: chat is busy");
        assert_pre_save_branch_input_preserved(
            &console,
            Some(source_chat),
            command,
            &context,
            cursor,
            true,
        );
        assert_eq!(agent.list_chats().await.len(), 1);
        console.finish_summary_input_transition().unwrap();
        assert_eq!(
            console.state.lock().unwrap().branch_submission,
            BranchSubmission::Idle
        );
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn post_save_switch_failure_consumes_input_and_reports_the_durable_child() {
        let directory = tempfile::tempdir().unwrap();
        let agent = Agent::new_embedded(
            support::config(),
            Arc::new(support::ScriptedModel::new(vec![support::text(
                "source answer",
            )])),
        );
        let source_chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        let source = agent
            .submit(source_chat, None, "source prompt".into())
            .await
            .unwrap();
        loop {
            match support::next_event(&mut events).await.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("source fixture failed: {error}"),
                _ => {}
            }
        }
        let command = "/branch @branch-context";
        let (console, _, _) = pending_branch_console(Some(source_chat), command);
        console.fail_next_reset_chat("renderer failed").unwrap();

        let error = branch_from_terminal(
            &console,
            &agent,
            Some(source_chat),
            Some(source.loop_id),
            command,
            false,
        )
        .await
        .unwrap_err();
        let child = agent
            .list_chats()
            .await
            .into_iter()
            .find(|snapshot| snapshot.id != source_chat)
            .expect("the durable child was not published");

        assert!(error.contains(&format!("Branch {} was saved", child.id)));
        assert!(error.contains("Use /resume to find it"));
        assert!(error.contains(&format!("/resume {}", child.id)));
        {
            let mut state = console.state.lock().unwrap();
            assert_eq!(state.chat, Some(source_chat));
            assert_eq!(state.branch_submission, BranchSubmission::FinishedCommitted);
            assert!(state.editor.draft().is_empty());
            assert!(state.restored_contexts.is_empty());
            for character in "TRAIL".chars() {
                assert!(state.key(key(KeyCode::Char(character))).is_none());
            }
            state.paste("ignored paste");
            assert!(state.editor.draft().is_empty());
            assert!(state.clear_finished_branch_submission());
        }
        agent.shutdown().await;
    }

    #[test]
    fn reopening_diff_restores_the_chat_dialog_with_a_fresh_catalog() {
        let chat_id = Uuid::new_v4();
        let mut state = State::new(false, 80, 24);
        state.chat = Some(chat_id);
        state.open_diff_screen(chat_id, diff_catalog(40));
        state.diff_key(key(KeyCode::Enter));
        state.diff_key(key(KeyCode::End));
        let viewport = DiffViewport {
            width: state.width,
            height: state.height.saturating_sub(2),
        };
        let before = {
            let screen = state.diff_screen.as_mut().unwrap();
            assert_eq!(screen.dialog.mode(), DiffDialogMode::Detail);
            screen
                .dialog
                .frame(&screen.catalog, viewport)
                .detail
                .unwrap()
                .top_row
        };
        assert!(before > 0);
        state.hide_diff_screen();

        let mut fresh_catalog = diff_catalog(40);
        fresh_catalog.sources[0].user_prompt = Some("fresh catalog".into());
        state.open_diff_screen(chat_id, fresh_catalog);

        let screen = state.diff_screen.as_mut().unwrap();
        assert_eq!(
            screen.catalog.sources[0].user_prompt.as_deref(),
            Some("fresh catalog")
        );
        assert_eq!(screen.dialog.mode(), DiffDialogMode::Detail);
        let after = screen
            .dialog
            .frame(&screen.catalog, viewport)
            .detail
            .unwrap()
            .top_row;
        assert_eq!(after, before);
    }

    #[test]
    fn diff_list_keeps_paths_and_source_labels_on_one_terminal_row() {
        let mut catalog = diff_catalog(1);
        catalog.sources[0].id = ChangeSourceId::AgentRun(Uuid::from_u128(7));
        catalog.sources[0].user_prompt = Some("first line\nsecond\tpart".into());
        catalog.sources[0].files[0].path = PathBuf::from("line\nbreak.txt");
        catalog.sources[0].files[0].current_status = Some(CurrentFileStatus {
            index: GitFileState::Added,
            worktree: GitFileState::Modified,
            untracked: false,
        });
        let mut screen = DiffScreen {
            chat_id: Uuid::new_v4(),
            catalog,
            dialog: DiffDialog::new(),
        };

        let lines = diff_screen_lines(&mut screen, 100, 12, DisplayProfile::default());
        assert!(lines.iter().all(|line| !line.contains('\n')));
        assert!(lines[0].contains("first line␤second⇥part"));
        assert!(lines.iter().any(|line| line.contains("line␤break.txt")));
        assert!(
            lines
                .iter()
                .any(|line| { line.contains("index added") && line.contains("worktree modified") })
        );
    }

    #[test]
    fn permission_comment_tab_round_trip_preserves_unicode_and_does_not_expand_options() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.menu = Some(menu());
        state.menu_focused = true;
        attach_permission_comments(&mut state);

        assert!(state.key(key(KeyCode::Tab)).is_none());
        assert!(state.menu_comment_is_editing());
        state.key(key(KeyCode::Char('说')));
        state.paste("明\n保留");
        assert_eq!(state.selected_menu_comment().as_deref(), Some("说明 保留"));
        assert!(state.key(key(KeyCode::Tab)).is_none());
        assert!(!state.menu_comment_is_editing());

        state.key(key(KeyCode::Down));
        assert_eq!(state.menu.as_ref().unwrap().selected, 1);
        state.key(key(KeyCode::Tab));
        assert!(state.menu_focused);
        assert!(!state.menu_comment_is_editing());
        state.key(key(KeyCode::Tab));
        state.key(key(KeyCode::Up));
        state.key(key(KeyCode::Tab));
        assert!(state.menu_comment_is_editing());
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Choice { index: 0, comment: Some(comment), .. }) if comment == "说明 保留"
        ));
    }

    fn question_menu(options: &[&str]) -> Menu {
        Menu {
            id: Uuid::new_v4(),
            title: "Which report?".into(),
            context: vec!["☐ Report".into()],
            options: options.iter().map(|option| (*option).into()).collect(),
            selected: 0,
            confirmation: false,
        }
    }

    #[test]
    fn question_other_row_opens_a_text_field_where_j_and_k_type() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        let console = InteractiveConsoleFixture::new(state);
        let menu = question_menu(&["Weekly", "Monthly", "Other"]);
        let menu_id = menu.id;
        console
            .show_question_menu(
                menu,
                QuestionMenu {
                    entry_option: Some(2),
                    entry_text: String::new(),
                    multi_select: false,
                    navigable: false,
                },
            )
            .unwrap();

        let mut state = console.state.lock().unwrap();
        assert!(state.menu_focused);
        assert!(state.frame().0.join("\n").contains("Enter select"));
        assert!(state.key(key(KeyCode::Right)).is_none());
        assert!(state.key(key(KeyCode::Char('l'))).is_none());
        assert!(state.key(key(KeyCode::Char(' '))).is_none());
        state.key(key(KeyCode::Char('j')));
        state.key(key(KeyCode::Char('j')));
        assert_eq!(state.menu.as_ref().unwrap().selected, 2);
        assert!(state.key(key(KeyCode::Enter)).is_none());
        assert!(state.menu_comment_is_editing());
        state.key(key(KeyCode::Char('j')));
        state.key(key(KeyCode::Char('k')));
        let frame = state.frame().0.join("\n");
        assert!(frame.contains("Answer: jk"), "{frame}");
        assert!(frame.contains("Enter submit"));
        assert!(state.key(key(KeyCode::Esc)).is_none());
        assert!(!state.menu_comment_is_editing());
        assert!(state.key(key(KeyCode::Enter)).is_none());
        assert!(state.menu_comment_is_editing());
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Choice { menu_id: chosen, index: 2, comment: Some(comment) })
                if chosen == menu_id && comment == "jk"
        ));
    }

    #[test]
    fn multi_select_question_menus_toggle_with_space_and_switch_questions() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        let console = InteractiveConsoleFixture::new(state);
        let show = || {
            let menu = question_menu(&["Summary", "Risks", "Charts", "Other", "Next"]);
            let menu_id = menu.id;
            console
                .show_question_menu(
                    menu,
                    QuestionMenu {
                        entry_option: Some(3),
                        entry_text: "Appendix".into(),
                        multi_select: true,
                        navigable: true,
                    },
                )
                .unwrap();
            menu_id
        };

        let menu_id = show();
        {
            let mut state = console.state.lock().unwrap();
            assert!(state.frame().0.join("\n").contains("Space/Enter toggle"));
            assert!(matches!(
                state.key(key(KeyCode::Char(' '))),
                Some(InputEvent::Choice { menu_id: chosen, index: 0, comment: None })
                    if chosen == menu_id
            ));
            state.mark_menu_submission_finished(menu_id);
            state.release_finished_submissions();
        }

        let menu_id = show();
        {
            let mut state = console.state.lock().unwrap();
            assert!(state.key(key(KeyCode::Char('4'))).is_none());
            assert_eq!(state.menu.as_ref().unwrap().selected, 3);
            assert!(state.menu_comment_is_editing());
            assert_eq!(state.selected_menu_comment().as_deref(), Some("Appendix"));
            assert!(state.key(key(KeyCode::Char('l'))).is_none());
            assert_eq!(state.selected_menu_comment().as_deref(), Some("Appendixl"));
            assert!(state.key(key(KeyCode::Esc)).is_none());
            assert!(matches!(
                state.key(key(KeyCode::Right)),
                Some(InputEvent::SwitchQuestion { menu_id: chosen, next: true }) if chosen == menu_id
            ));
            state.mark_menu_submission_finished(menu_id);
            state.release_finished_submissions();
        }

        let menu_id = show();
        let mut state = console.state.lock().unwrap();
        assert!(matches!(
            state.key(key(KeyCode::Char('h'))),
            Some(InputEvent::SwitchQuestion { menu_id: chosen, next: false }) if chosen == menu_id
        ));
    }

    #[tokio::test]
    async fn the_question_dialog_answers_every_question_through_menu_keys() {
        use crate::terminal::menus::Menus;
        use tickloop_agent::{model::Content, runtime::ASK_USER_QUESTION_TOOL_NAME};

        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(support::ScriptedModel::new(vec![
            support::call(
                "clarify",
                ASK_USER_QUESTION_TOOL_NAME,
                serde_json::json!({"questions":[
                    {"question":"Which report?","header":"Report","options":[
                        {"label":"Weekly","description":"The Monday summary"},
                        {"label":"Monthly","description":"The board pack"}
                    ]},
                    {"question":"Which sections?","header":"Sections","multiSelect":true,"options":[
                        {"label":"Summary","description":"Opening"},
                        {"label":"Risks","description":"Open risks"},
                        {"label":"Charts","description":"Every chart"}
                    ]}
                ]}),
            ),
            support::text("Done."),
        ]));
        let agent = Agent::new_embedded(support::config(), model.clone());
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, None, "Shorten the report".into())
            .await
            .unwrap();
        let request = loop {
            if let EventKind::QuestionAsked { request } =
                support::next_event(&mut events).await.kind
            {
                break request;
            }
        };
        let mut state = State::new(true, 100, 40);
        state.prompt_enabled = true;
        let console = InteractiveConsoleFixture::new(state);
        let mut menus = Menus::default();
        let questions = HashMap::from([(request.id, request.clone())]);
        menus
            .refresh_permissions(&agent, &console, Some(chat), &HashMap::new(), &questions)
            .await
            .unwrap();

        async fn press(
            agent: &Agent,
            console: &Console,
            menus: &mut Menus,
            keys: &[KeyCode],
        ) -> Option<Uuid> {
            let mut answered = None;
            for code in keys {
                let event = console.state.lock().unwrap().key(key(*code));
                match event {
                    Some(InputEvent::Choice {
                        menu_id,
                        index,
                        comment,
                    }) => {
                        answered = menus
                            .choose(agent, console, menu_id, Some(index), comment)
                            .await
                            .unwrap()
                            .question;
                    }
                    Some(InputEvent::SwitchQuestion { menu_id, next }) => {
                        menus.switch_question(console, menu_id, next).unwrap();
                    }
                    Some(other) => panic!("unexpected event {other:?}"),
                    None => {}
                }
                console.state.lock().unwrap().release_finished_submissions();
            }
            answered
        }
        let title = |console: &Console| {
            console
                .state
                .lock()
                .unwrap()
                .menu
                .as_ref()
                .unwrap()
                .title
                .clone()
        };

        assert_eq!(title(&console), "Which report?");
        press(
            &agent,
            &console,
            &mut menus,
            &[KeyCode::Char('j'), KeyCode::Enter],
        )
        .await;
        assert_eq!(title(&console), "Which sections?");
        press(
            &agent,
            &console,
            &mut menus,
            &[
                KeyCode::Char('l'),
                KeyCode::Char('h'),
                KeyCode::Char('l'),
                KeyCode::Char(' '),
                KeyCode::Char('j'),
                KeyCode::Char('j'),
                KeyCode::Char(' '),
                KeyCode::Char('j'),
                KeyCode::Enter,
            ],
        )
        .await;
        assert_eq!(title(&console), "Which sections?");
        press(
            &agent,
            &console,
            &mut menus,
            &"jk".chars().map(KeyCode::Char).collect::<Vec<_>>(),
        )
        .await;
        press(&agent, &console, &mut menus, &[KeyCode::Enter]).await;
        let frame = console.state.lock().unwrap().frame().0.join("\n");
        assert!(frame.contains("[✓] Summary"), "{frame}");
        assert!(frame.contains("[✓] Charts"), "{frame}");
        assert!(frame.contains("[✓] Other"), "{frame}");
        press(
            &agent,
            &console,
            &mut menus,
            &[KeyCode::Char('j'), KeyCode::Enter],
        )
        .await;
        assert_eq!(title(&console), "Review your answers");
        let frame = console.state.lock().unwrap().frame().0.join("\n");
        assert!(frame.contains("→ Monthly"), "{frame}");
        assert!(frame.contains("→ Summary, Charts, jk"), "{frame}");
        let answered = press(&agent, &console, &mut menus, &[KeyCode::Enter]).await;
        assert_eq!(answered, Some(request.id));
        assert!(console.state.lock().unwrap().menu.is_none());

        loop {
            match support::next_event(&mut events).await.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        let requests = model.requests.lock().unwrap().clone();
        let result = requests[1]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|block| match block {
                Content::ToolResult { content, .. } => Some(content.text()),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            result,
            "User has answered your questions: \"Which report?\"=\"Monthly\", \"Which sections?\"=\"Summary, Charts, jk\". You can now continue with the user's answers in mind."
        );
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn a_requester_reads_alike_with_the_same_number_as_its_background_notices() {
        use crate::agent_console::AttentionGroup;
        use crate::terminal::menus::Menus;
        const TITLE: &str = "Review the shared plan";
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(support::ScriptedModel::new(vec![support::call(
            "shell",
            "Bash",
            serde_json::json!({"command": "printf checked"}),
        )]));
        let agent = Agent::new_embedded(support::config(), model);
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let twin = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_permission_rules(
                chat,
                vec![PermissionRule {
                    id: Uuid::new_v4(),
                    decision: Decision::Ask,
                    tool: "Bash".into(),
                    specifier: None,
                    input: None,
                    loop_id: None,
                    persistent: false,
                }],
            )
            .await
            .unwrap();
        let console = InteractiveConsoleFixture::new(State::new(true, 100, 30));
        let row = |agent_id: Uuid| AgentTreeRow {
            agent_id,
            parent_id: None,
            depth: 0,
            title: TITLE.into(),
            status: "Running".into(),
            project: "/work".into(),
            workspace: None,
            approvals: 0,
            questions: 0,
            group: AttentionGroup::Working,
            glyph: status_glyph(AttentionGroup::Working, false),
            preview: None,
            updated_at: None,
        };
        let requester = agent.reference(chat).await.unwrap().agent_id;
        let other = agent.reference(twin).await.unwrap().agent_id;
        console
            .set_global_overview(Some(AgentOverview {
                rows: vec![row(requester), row(other)],
                dispatch_directory: directory.path().into(),
                created: None,
                visible: None,
            }))
            .unwrap();
        console.set_global_overview(None).unwrap();
        let notice_name = console.agent_name(requester, TITLE).unwrap();
        assert!(notice_name.ends_with(')'), "{notice_name}");
        let mut menus = Menus::default();
        let mut events = agent.subscribe();
        agent.submit(chat, None, TITLE.into()).await.unwrap();
        let request = loop {
            if let EventKind::PermissionRequested { request } =
                support::next_event(&mut events).await.kind
            {
                break request;
            }
        };
        menus
            .refresh_permissions(
                &agent,
                &console,
                Some(chat),
                &HashMap::from([(request.id, request.run_id)]),
                &HashMap::new(),
            )
            .await
            .unwrap();
        let frame = console.state.lock().unwrap().frame().0;
        let context = frame
            .iter()
            .find(|line| line.contains("Goal:"))
            .unwrap_or_else(|| panic!("the request names its goal: {frame:#?}"));
        assert!(
            context.contains(&format!("{notice_name} · Goal: {TITLE}")),
            "the requester reads as its background notices do ({notice_name}): {context}"
        );
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn a_permission_request_names_its_agent_and_goal_without_a_loop_hash() {
        use crate::terminal::menus::Menus;
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(support::ScriptedModel::new(vec![support::call(
            "shell",
            "Bash",
            serde_json::json!({"command": "printf checked"}),
        )]));
        let agent = Agent::new_embedded(support::config(), model);
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_permission_rules(
                chat,
                vec![PermissionRule {
                    id: Uuid::new_v4(),
                    decision: Decision::Ask,
                    tool: "Bash".into(),
                    specifier: None,
                    input: None,
                    loop_id: None,
                    persistent: false,
                }],
            )
            .await
            .unwrap();
        let console = InteractiveConsoleFixture::new(State::new(true, 100, 30));
        let mut menus = Menus::default();
        let mut events = agent.subscribe();
        let receipt = agent
            .submit(chat, None, "Inspect commit deadbeef".into())
            .await
            .unwrap();
        let request = loop {
            if let EventKind::PermissionRequested { request } =
                support::next_event(&mut events).await.kind
            {
                break request;
            }
        };
        menus
            .refresh_permissions(
                &agent,
                &console,
                Some(chat),
                &HashMap::from([(request.id, request.run_id)]),
                &HashMap::new(),
            )
            .await
            .unwrap();
        let frame = console.state.lock().unwrap().frame().0;
        let context = frame
            .iter()
            .find(|line| line.contains("Goal:"))
            .unwrap_or_else(|| panic!("the request names its goal: {frame:#?}"));
        let loop_prefix = &receipt.loop_id.to_string()[..8];
        assert!(
            context.contains("Agent Inspect commit deadbeef")
                && context.contains("Goal: Inspect commit deadbeef")
                && !context.contains(loop_prefix),
            "the requester and goal are named by text, the Loop by nothing generated: {context}"
        );
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn the_enter_plan_mode_dialog_declines_on_escape_and_enters_on_confirmation() {
        use crate::terminal::menus::Menus;
        use tickloop_agent::runtime::ENTER_PLAN_MODE_TOOL_NAME;

        let directory = tempfile::tempdir().unwrap();
        let enter = |id| support::call(id, ENTER_PLAN_MODE_TOOL_NAME, serde_json::json!({}));
        let model = Arc::new(support::ScriptedModel::new(vec![
            enter("declined"),
            enter("accepted"),
            support::text("Planning first."),
        ]));
        let agent = Agent::new_embedded(support::config(), model.clone());
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Auto)
            .await
            .unwrap();
        let console = InteractiveConsoleFixture::new(State::new(true, 100, 30));
        let mut menus = Menus::default();
        let mut events = agent.subscribe();

        async fn show_request(
            agent: &Agent,
            console: &Console,
            menus: &mut Menus,
            chat: Uuid,
            events: &mut tokio::sync::broadcast::Receiver<tickloop_agent::runtime::Event>,
        ) -> Uuid {
            let request = loop {
                if let EventKind::PermissionRequested { request } =
                    support::next_event(events).await.kind
                {
                    break request;
                }
            };
            menus
                .refresh_permissions(
                    agent,
                    console,
                    Some(chat),
                    &HashMap::from([(request.id, request.run_id)]),
                    &HashMap::new(),
                )
                .await
                .unwrap();
            request.id
        }
        async fn finished(
            events: &mut tokio::sync::broadcast::Receiver<tickloop_agent::runtime::Event>,
        ) -> EventKind {
            loop {
                let kind = support::next_event(events).await.kind;
                if matches!(kind, EventKind::Completed { .. } | EventKind::Interrupted) {
                    return kind;
                }
            }
        }

        let receipt = agent
            .submit(chat, None, "Add caching".into())
            .await
            .unwrap();
        let request = show_request(&agent, &console, &mut menus, chat, &mut events).await;
        {
            let mut state = console.state.lock().unwrap();
            let menu = state.menu.as_ref().unwrap();
            assert_eq!(menu.title, "Enter plan mode?");
            assert_eq!(
                menu.options,
                ["Yes, enter plan mode", "No, start implementing now"]
            );
            assert_eq!(
                state.styled_frame().0[0].spans[0].role,
                Some(Role::PlanMode)
            );
            assert!(state.menu_comment.is_none());
            let frame = state.frame().0.join("\n");
            assert!(
                frame.contains(
                    "The Agent wants to enter plan mode to explore and design an implementation approach."
                ),
                "{frame}"
            );
            assert!(
                frame.contains("No changes will be made until you approve the plan."),
                "{frame}"
            );
            assert!(state.key(key(KeyCode::Char('j'))).is_none());
            assert_eq!(state.menu.as_ref().unwrap().selected, 1);
            assert!(state.key(key(KeyCode::Char('k'))).is_none());
            assert_eq!(state.menu.as_ref().unwrap().selected, 0);
            assert!(matches!(
                state.key(key(KeyCode::Esc)),
                Some(InputEvent::Cancel { menu_id }) if menu_id == request
            ));
        }
        let outcome = menus
            .choose(&agent, &console, request, None, None)
            .await
            .unwrap();
        assert_eq!(outcome.permission, Some(request));
        console.state.lock().unwrap().release_finished_submissions();
        assert!(matches!(
            finished(&mut events).await,
            EventKind::Interrupted
        ));
        assert_eq!(
            agent.snapshot(chat).await.unwrap().policy.mode,
            ExecutionMode::Auto
        );

        agent
            .submit(chat, Some(receipt.loop_id), "Plan it first".into())
            .await
            .unwrap();
        let request = show_request(&agent, &console, &mut menus, chat, &mut events).await;
        let choice = console.state.lock().unwrap().key(key(KeyCode::Enter));
        let Some(InputEvent::Choice {
            menu_id,
            index,
            comment,
        }) = choice
        else {
            panic!("Enter must confirm the dialog: {choice:?}");
        };
        assert_eq!((menu_id, index), (request, 0));
        menus
            .choose(&agent, &console, menu_id, Some(index), comment)
            .await
            .unwrap();
        assert!(matches!(
            finished(&mut events).await,
            EventKind::Completed { .. }
        ));
        assert_eq!(
            agent.snapshot(chat).await.unwrap().policy.mode,
            ExecutionMode::Plan
        );
        assert_eq!(model.requests.lock().unwrap().len(), 3);
        agent.shutdown().await;
    }

    #[test]
    fn file_path_input_uses_normal_editing_and_external_editor_without_touching_prompt() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("preserved draft");
        let console = InteractiveConsoleFixture::new(state);
        let mut selection = menu();
        selection.options = vec!["Save service account".into(), "Cancel".into()];
        selection.confirmation = false;
        console
            .show_input_menu(selection, "JSON file path", "/old.json")
            .unwrap();
        let mut state = console.state.lock().unwrap();
        assert!(state.menu_comment_is_editing());
        state.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for character in "/newjk.json".chars() {
            state.key(key(KeyCode::Char(character)));
        }
        assert_eq!(
            state.selected_menu_comment().as_deref(),
            Some("/newjk.json")
        );
        let opened = scripted_external_editor(&mut state, "/edited path.json\n");
        state.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert_eq!(opened.lock().unwrap().as_slice(), ["/newjk.json"]);
        assert_eq!(
            state.selected_menu_comment().as_deref(),
            Some("/edited path.json")
        );
        assert_eq!(state.editor.draft(), "preserved draft");
        assert!(
            matches!(state.key(key(KeyCode::Enter)), Some(InputEvent::Choice { index: 0, comment: Some(value), .. }) if value == "/edited path.json")
        );
    }

    #[test]
    fn secret_menu_masks_rendered_value_and_submits_exact_key() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("preserved draft");
        let console = InteractiveConsoleFixture::new(state);
        let mut selection = menu();
        selection.title = "Set API key".into();
        selection.context.clear();
        selection.options = vec!["Save API key".into(), "Cancel".into()];
        selection.selected = 1;
        selection.confirmation = false;
        console.show_secret_menu(selection).unwrap();

        let mut state = console.state.lock().unwrap();
        assert!(state.menu_focused);
        assert!(state.menu_comment_is_editing());
        assert_eq!(state.menu.as_ref().unwrap().selected, 0);
        let key_text = "sk-水é-secret";
        state.paste(key_text);
        let (frame, cursor_row, cursor_column) = state.frame();
        assert!(frame.join("\n").contains(&format!(
            "API key: {}",
            "*".repeat(key_text.chars().count())
        )));
        assert!(!frame.join("\n").contains(key_text));
        assert_eq!(
            cursor_column,
            display_width("  API key: ") + key_text.chars().count()
        );
        assert!(frame[cursor_row].contains("API key:"));
        let mut output = Vec::new();
        state.draw(&mut output).unwrap();
        assert!(!String::from_utf8_lossy(&output).contains(key_text));
        assert!(
            state
                .rendered_screen
                .iter()
                .all(|line| !line.contains(key_text))
        );
        let copies = Arc::new(Mutex::new(Vec::new()));
        state.clipboard = Clipboard::Captured {
            copies: Arc::clone(&copies),
            image: None,
            remote: false,
        };
        let row = state
            .rendered_screen
            .iter()
            .position(|line| line.contains("API key: "))
            .unwrap();
        let column = display_width("  API key: ");
        drag(
            &mut state,
            (row as u16, column as u16),
            (row as u16, (column + key_text.chars().count()) as u16),
        );
        assert_eq!(
            copies.lock().unwrap().as_slice(),
            ["*".repeat(key_text.chars().count())]
        );
        assert_eq!(state.editor.draft(), "preserved draft");
        let submission = state.key(key(KeyCode::Enter));
        assert!(!format!("{submission:?}").contains(key_text));
        assert!(matches!(
            submission,
            Some(InputEvent::Choice { index: 0, comment: Some(comment), .. }) if comment == key_text
        ));
    }

    #[test]
    fn cancelling_secret_menu_discards_key_without_touching_prompt_history() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.input_history = Some(InputHistory::in_memory());
        let console = InteractiveConsoleFixture::new(state);
        let mut selection = menu();
        selection.title = "Set API key".into();
        selection.context.clear();
        selection.options = vec!["Save API key".into(), "Cancel".into()];
        selection.confirmation = false;
        let menu_id = selection.id;
        console.show_secret_menu(selection).unwrap();

        let mut state = console.state.lock().unwrap();
        state.paste("cancelled-secret");
        assert!(state.key(key(KeyCode::Esc)).is_none());
        assert!(!state.menu_comment_is_editing());
        assert!(!state.frame().0.join("\n").contains("cancelled-secret"));
        assert!(matches!(
            state.key(key(KeyCode::Esc)),
            Some(InputEvent::Cancel { menu_id: cancelled }) if cancelled == menu_id
        ));
        assert!(state.clear_menu(menu_id));
        assert!(state.menu_comment.is_none());
        assert!(state.editor.draft().is_empty());
        assert!(
            state
                .input_history
                .as_ref()
                .unwrap()
                .recent_prompts(&state.history_session, &state.history_project)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn secret_menu_cannot_route_paste_to_prompt_or_transcript_after_leaving_field() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.input_history = Some(InputHistory::in_memory());
        let console = InteractiveConsoleFixture::new(state);
        let mut selection = menu();
        selection.title = "Set API key".into();
        selection.context.clear();
        selection.options = vec!["Save API key".into(), "Cancel".into()];
        selection.confirmation = false;
        console.show_secret_menu(selection).unwrap();

        let mut state = console.state.lock().unwrap();
        assert!(state.key(key(KeyCode::Esc)).is_none());
        assert!(state.key(key(KeyCode::BackTab)).is_none());
        assert!(state.menu_focused);
        state.paste("must-not-become-a-prompt");
        assert!(state.key(key(KeyCode::Char('x'))).is_none());
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert!(state.transcript.is_none());
        assert!(state.editor.draft().is_empty());
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Choice {
                index: 0,
                comment: None,
                ..
            })
        ));
        assert!(
            state
                .input_history
                .as_ref()
                .unwrap()
                .recent_prompts(&state.history_session, &state.history_project)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn secret_paste_trims_outer_whitespace_and_rejects_internal_newlines() {
        let mut state = State::new(true, 80, 24);
        state.menu = Some(menu());
        state.menu_focused = true;
        state.menu_comment = Some(MenuCommentState {
            options: vec![0],
            values: HashMap::new(),
            label: "API key",
            editing: true,
            cursor: 0,
            secret: true,
            standalone_input: false,
        });

        state.paste("other\nkey");
        assert!(state.selected_menu_comment().is_none());
        assert_eq!(state.status, SECRET_PASTE_ERROR);
        state.paste(" \r\n  sk-valid  \n\t");
        assert_eq!(state.selected_menu_comment().as_deref(), Some("sk-valid"));
        assert!(state.status.is_empty());
        state.paste("other\nkey");
        assert_eq!(state.selected_menu_comment().as_deref(), Some("sk-valid"));
        assert_eq!(state.status, SECRET_PASTE_ERROR);
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Choice { comment: Some(comment), .. }) if comment == "sk-valid"
        ));
    }

    #[test]
    fn permission_rule_tab_expands_while_enter_submits_the_exact_choice() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        let mut permission = menu();
        permission.selected = 1;
        let menu_id = permission.id;
        state.menu = Some(permission);
        state.menu_focused = true;
        state.menu_expand_options = vec![1];

        assert!(matches!(
            state.key(key(KeyCode::Tab)),
            Some(InputEvent::ExpandChoice {
                menu_id: expanded_menu,
                index: 1,
            }) if expanded_menu == menu_id
        ));
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Choice {
                menu_id: selected_menu,
                index: 1,
                comment: None,
            }) if selected_menu == menu_id
        ));
    }

    #[test]
    fn summary_guidance_uses_its_own_label_and_numbers_submit_without_guidance() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        let mut summary = menu();
        summary.title = "Rewind actions".into();
        summary.options = vec![
            "Restore conversation".into(),
            "Summarize from here".into(),
            "Summarize up to here".into(),
            "Never mind".into(),
        ];
        summary.selected = 1;
        summary.confirmation = false;
        state.menu = Some(summary);
        state.menu_focused = true;
        state.menu_comment = Some(MenuCommentState {
            options: vec![1, 2],
            values: HashMap::new(),
            label: "Guidance",
            editing: false,
            cursor: 0,
            secret: false,
            standalone_input: false,
        });

        assert!(state.key(key(KeyCode::Tab)).is_none());
        for character in "focus on API boundaries".chars() {
            state.key(key(KeyCode::Char(character)));
        }
        let frame = state.frame().0.join("\n");
        assert!(frame.contains("Guidance: focus on API boundaries"));
        assert!(frame.contains("add context (optional)"));
        assert!(state.key(key(KeyCode::Tab)).is_none());
        assert!(matches!(
            state.key(key(KeyCode::Char('2'))),
            Some(InputEvent::Choice {
                index: 1,
                comment: None,
                ..
            })
        ));
    }

    #[test]
    fn summary_guidance_enter_submits_the_context() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        let mut summary = menu();
        summary.title = "Rewind actions".into();
        summary.options = vec![
            "Restore conversation".into(),
            "Summarize from here".into(),
            "Summarize up to here".into(),
            "Never mind".into(),
        ];
        summary.selected = 2;
        summary.confirmation = false;
        state.menu = Some(summary);
        state.menu_focused = true;
        state.menu_comment = Some(MenuCommentState {
            options: vec![1, 2],
            values: HashMap::new(),
            label: "Guidance",
            editing: false,
            cursor: 0,
            secret: false,
            standalone_input: false,
        });

        assert!(state.key(key(KeyCode::Tab)).is_none());
        for character in "preserve the API decisions".chars() {
            state.key(key(KeyCode::Char(character)));
        }
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Choice {
                index: 2,
                comment: Some(comment),
                ..
            }) if comment == "preserve the API decisions"
        ));
    }

    #[test]
    fn non_confirmation_menu_numbers_choose_the_displayed_option() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        let mut rewind = menu();
        rewind.title = "Rewind".into();
        rewind.options = vec![
            "Before · first prompt".into(),
            "Before · second prompt".into(),
            "Current point".into(),
        ];
        rewind.confirmation = false;
        let menu_id = rewind.id;
        state.menu = Some(rewind);
        state.menu_focused = true;

        assert!(matches!(
            state.key(key(KeyCode::Char('2'))),
            Some(InputEvent::Choice {
                menu_id: selected_menu,
                index: 1,
                comment: None,
            }) if selected_menu == menu_id
        ));
    }

    #[test]
    fn summary_modal_blocks_edits_but_keeps_cancel_and_transcript_controls() {
        let mut state = State::new(true, 80, 24);
        state.replace_draft("preserved draft".into(), "preserved draft".len());
        state.prompt_enabled = false;

        assert!(state.key(key(KeyCode::Char('x'))).is_none());
        assert_eq!(state.editor.draft(), "preserved draft");
        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(InputEvent::Interrupt)
        ));
        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Some(InputEvent::OpenTranscript { .. })
        ));
        assert_eq!(state.editor.draft(), "preserved draft");
    }

    #[test]
    fn escape_closes_a_permission_comment_before_it_cancels_the_menu() {
        let mut state = State::new(true, 80, 24);
        state.menu = Some(menu());
        state.menu_focused = true;
        attach_permission_comments(&mut state);
        state.key(key(KeyCode::Down));
        state.key(key(KeyCode::Down));
        state.key(key(KeyCode::Tab));
        state.key(key(KeyCode::Char('n')));

        assert!(state.key(key(KeyCode::Esc)).is_none());
        assert!(state.menu.is_some());
        assert!(!state.menu_comment_is_editing());
        assert!(matches!(
            state.key(key(KeyCode::Esc)),
            Some(InputEvent::Cancel { .. })
        ));
    }

    #[test]
    fn permission_dialog_captures_focus_and_preserves_an_existing_input_draft() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("看看这个目录");
        let console = console_with_state(state);
        console.show_commentable_menu(menu(), vec![0, 2]).unwrap();
        let mut state = console.state.lock().unwrap();
        assert!(state.menu_focused);
        assert!(
            !state
                .frame()
                .0
                .iter()
                .any(|line| line.contains("看看这个目录"))
        );
        state.key(key(KeyCode::Down));
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Choice { index: 1, .. })
        ));
        assert_eq!(state.editor.draft(), "看看这个目录");
    }

    #[test]
    fn focused_menu_owns_paste_and_shell_control_keys_even_when_too_small_to_render() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.paste("!printf ready");
        assert_eq!(state.shell_input.kind(), InputKind::Shell);
        let draft = state.editor.draft().to_owned();
        state.menu = Some(menu());
        state.menu_focused = true;

        state.paste(" ignored");
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert_eq!(state.editor.draft(), draft);
        assert!(state.menu.is_some());
        assert!(state.menu_focused);

        state.height = MIN_MENU_TERMINAL_ROWS.saturating_sub(1);
        state.paste(" still ignored");
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert_eq!(state.editor.draft(), draft);
        assert!(state.menu.is_some());
        assert!(state.menu_focused);

        let mut empty = State::new(true, 80, 24);
        empty.prompt_enabled = true;
        empty.menu = Some(menu());
        empty.menu_focused = true;
        for _ in 0..2 {
            assert!(
                empty
                    .key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
                    .is_none()
            );
        }
        assert_eq!(empty.pending_exit_control(), None);
    }

    #[test]
    fn submitted_permission_menu_keeps_owning_queued_keys_until_it_is_cleared() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        let permission = menu();
        let permission_id = permission.id;
        state.menu = Some(permission);
        state.menu_focused = true;

        assert!(matches!(
            state.key(key(KeyCode::Char('n'))),
            Some(InputEvent::Cancel { menu_id }) if menu_id == permission_id
        ));
        state.mark_menu_submission_finished(permission_id);
        for character in "/branch leaked".chars() {
            assert!(state.key(key(KeyCode::Char(character))).is_none());
        }
        assert!(state.key(key(KeyCode::Enter)).is_none());
        state.paste("/branch pasted");

        assert!(state.menu.is_some());
        assert!(state.editor.draft().is_empty());
        assert!(state.clear_finished_menu_submission());
        assert!(state.key(key(KeyCode::Char('x'))).is_none());
        assert_eq!(state.editor.draft(), "x");
    }

    #[test]
    fn keys_arriving_after_idle_input_follow_a_finished_menu_submission() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        let settings = menu();
        let settings_id = settings.id;
        state.menu = Some(settings);
        state.menu_focused = true;
        assert!(matches!(
            state.key(key(KeyCode::Esc)),
            Some(InputEvent::Cancel { menu_id }) if menu_id == settings_id
        ));
        assert!(!state.release_finished_submissions());
        assert!(state.key(key(KeyCode::Char('q'))).is_none());

        state.mark_menu_submission_finished(settings_id);
        assert!(state.release_finished_submissions());
        for character in "abc".chars() {
            assert!(state.key(key(KeyCode::Char(character))).is_none());
        }
        assert!(state.menu.is_none());
        assert_eq!(state.editor.draft(), "abc");
    }

    #[test]
    fn submitted_parent_menu_owns_queued_keys_across_a_submenu_transition() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        let parent = menu();
        let parent_id = parent.id;
        state.menu = Some(parent);
        state.menu_focused = true;

        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Choice { menu_id, .. }) if menu_id == parent_id
        ));
        let mut child = menu();
        child.selected = 0;
        let child_id = child.id;
        state.menu = Some(child);
        state.mark_menu_submission_finished(parent_id);

        for code in [KeyCode::Down, KeyCode::Char('n'), KeyCode::Enter] {
            assert!(state.key(key(code)).is_none());
        }
        state.paste("2");
        assert_eq!(state.menu.as_ref().unwrap().id, child_id);
        assert_eq!(state.menu.as_ref().unwrap().selected, 0);
        assert!(!state.clear_finished_menu_submission());

        assert!(state.key(key(KeyCode::Down)).is_none());
        assert_eq!(state.menu.as_ref().unwrap().selected, 1);
    }

    #[tokio::test]
    async fn stale_parent_choice_releases_gate_without_dismissing_replacement_menu() {
        use crate::terminal::menus::Menus;
        use tickloop_agent::model::{HttpModelClient, InputModality, ModelConfig, Protocol};

        let config = ModelConfig {
            provider: crate::model::ModelProvider::Api,
            auth_source: crate::model::ModelAuthSource::ApiKeyEnv,
            name: "menu gate fixture".into(),
            protocol: Protocol::Messages,
            base_url: "http://127.0.0.1".into(),
            model: "fixture".into(),
            api_key_env: String::new(),
            max_output_tokens: 128,
            provider_id: String::new(),
            provider_name: String::new(),
            model_id: String::new(),
            connection: None,
            card: None,
            input_modalities: vec![InputModality::Text],
        };
        let agent = Agent::new_embedded(
            config.clone(),
            Arc::new(HttpModelClient::new(config).unwrap()),
        );
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        let console = InteractiveConsoleFixture::new(state);
        let mut menus = Menus::default();
        menus.help(&console, None).unwrap();
        let stale_choice = {
            let mut state = console.state.lock().unwrap();
            state.key(key(KeyCode::Enter)).unwrap()
        };
        let InputEvent::Choice {
            menu_id: stale_menu_id,
            index,
            comment,
        } = stale_choice
        else {
            panic!("expected the parent menu choice");
        };

        menus.help(&console, Some(0)).unwrap();
        let replacement_menu_id = console.state.lock().unwrap().menu.as_ref().unwrap().id;
        menus
            .choose(&agent, &console, stale_menu_id, Some(index), comment)
            .await
            .unwrap();

        let mut state = console.state.lock().unwrap();
        for code in [KeyCode::Down, KeyCode::Char('n'), KeyCode::Enter] {
            assert!(state.key(key(code)).is_none());
        }
        state.paste("2");
        assert_eq!(state.menu.as_ref().unwrap().id, replacement_menu_id);
        assert_eq!(state.menu.as_ref().unwrap().selected, 0);
        assert!(!state.clear_finished_menu_submission());
        assert!(state.key(key(KeyCode::Char('j'))).is_none());
        assert_eq!(state.menu.as_ref().unwrap().selected, 1);
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Choice {
                menu_id,
                index: 1,
                ..
            }) if menu_id == replacement_menu_id
        ));
    }

    #[test]
    fn prepared_timer_commands_use_normal_editing_and_preserve_existing_drafts() {
        let mut state = State::new(true, 100, 32);
        state.prompt_enabled = true;
        let console = InteractiveConsoleFixture::new(state);
        let command = "/loops schedule 00000000-0000-0000-0000-000000000001 60 review";
        console.prepare_command(command.into()).unwrap();
        assert!(console.prepare_command("replacement".into()).is_err());
        let mut state = console.state.lock().unwrap();
        assert_eq!(state.editor.draft(), command);
        assert!(state.can_edit_in_external_editor());
        state.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(state.editor.draft(), "");
        state.key(key(KeyCode::Char('j')));
        state.key(key(KeyCode::Char('k')));
        assert_eq!(state.editor.draft(), "jk");
    }

    #[test]
    fn slash_completion_keeps_submission_and_selection_distinct() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("/mo");
        assert!(state.completion.is_open());
        assert_eq!(state.completion.selected_index(), None);
        assert!(
            matches!(state.key(key(KeyCode::Enter)), Some(InputEvent::Prompt { text, file_references, .. }) if text == "/mo" && file_references.is_empty())
        );

        state.insert("/mo");
        assert!(state.key(key(KeyCode::Tab)).is_none());
        assert_eq!(state.editor.draft(), "/model");
        assert!(!state.completion.is_open());

        state.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        state.insert("/mo");
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert_eq!(state.completion.selected_index(), Some(0));
        assert!(state.key(key(KeyCode::Enter)).is_none());
        assert_eq!(state.editor.draft(), "/model");
    }

    #[test]
    fn mid_prompt_completion_replaces_only_the_command_token() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("先检查 /com 然后继续");
        set_cursor(&mut state, "先检查 /com".len());
        state.refresh_completion();
        assert!(state.completion.is_open());
        assert!(state.key(key(KeyCode::Tab)).is_none());
        assert_eq!(state.editor.draft(), "先检查 /compact 然后继续");
        assert_eq!(state.editor.cursor(), "先检查 /compact ".len());
        assert!(
            matches!(state.key(key(KeyCode::Enter)), Some(InputEvent::Prompt { text, file_references, .. }) if text == "先检查 /compact 然后继续" && file_references.is_empty())
        );
    }

    #[test]
    fn blocking_menu_suppresses_completion_without_losing_the_draft() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("/mo");
        assert!(state.completion.is_open());
        let menu = menu();
        let menu_id = menu.id;
        state.menu_focused = false;
        state.completion.dismiss();
        state.menu = Some(menu);

        assert!(state.key(key(KeyCode::Down)).is_none());
        assert_eq!(state.editor.draft(), "/mo");
        assert!(state.completion.selected_index().is_none());
        assert!(
            matches!(state.key(key(KeyCode::Enter)), Some(InputEvent::Choice { menu_id: id, .. }) if id == menu_id)
        );
        assert_eq!(state.editor.draft(), "/mo");
        assert_eq!(state.menu.as_ref().map(|menu| menu.id), Some(menu_id));
        assert!(!state.completion.is_open());
    }

    #[test]
    fn completion_rows_stay_above_the_bottom_input() {
        let mut state = State::new(true, 80, 12);
        state.prompt_enabled = true;
        state.insert("/");
        let first_completion = state.completion.candidates().next().unwrap().syntax();
        let (frame, cursor_row, _) = state.frame();
        assert!(frame.len() <= agent_layout::bottom_capacity(state.height));
        assert_eq!(cursor_row, frame.len() - 2);
        assert!(
            frame[..cursor_row]
                .iter()
                .any(|line| line.contains(&first_completion))
        );
        assert!(frame[cursor_row].contains("❯\u{a0}/"));
    }

    #[test]
    fn accepted_file_candidates_submit_structured_references_but_plain_mentions_do_not() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("notes.md");
        std::fs::write(&path, "context").unwrap();
        let index = FileMentionIndex::build(directory.path(), &[]).unwrap();
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.file_mentions = FileMentionState::with_index(index.clone());

        state.insert("Review @not");
        assert!(state.file_mentions.is_open());
        assert!(state.key(key(KeyCode::Tab)).is_none());
        assert_eq!(state.editor.draft(), "Review @notes.md");
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Prompt { text, file_references, .. })
                if text == "Review @notes.md" && file_references == [path.canonicalize().unwrap()]
        ));

        state.file_mentions = FileMentionState::with_index(index);
        state.insert("Keep @notes.md as ordinary text");
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Prompt { text, file_references, .. })
                if text == "Keep @notes.md as ordinary text" && file_references.is_empty()
        ));
    }

    #[test]
    fn a_restored_file_mention_refusal_survives_periodic_status_until_the_next_key() {
        let directory = tempfile::tempdir().unwrap();
        let old_root = directory.path().join("old");
        let new_root = directory.path().join("new");
        std::fs::create_dir(&old_root).unwrap();
        std::fs::create_dir(&new_root).unwrap();
        std::fs::write(old_root.join("notes.md"), "old-root context").unwrap();
        let mut state = State::new(true, 120, 30);
        state.prompt_enabled = true;
        state.file_mentions =
            FileMentionState::with_index(FileMentionIndex::build(&old_root, &[]).unwrap());
        state.insert("Review @not");
        state.key(key(KeyCode::Tab));
        let draft = state.editor.draft().to_owned();
        state.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let generation = state.file_mentions.request_index_generation();
        state
            .file_mentions
            .replace_index(generation, FileMentionIndex::build(&new_root, &[]).unwrap());
        state.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert_eq!(state.editor.draft(), draft);
        assert!(state.key(key(KeyCode::Enter)).is_none());
        assert!(state.status.contains("outside the allowed directories"));
        let refusal = state.status.clone();
        state.refresh_status("Ready");
        assert_eq!(
            state.status, refusal,
            "The reason for the rejected submission must remain visible"
        );
        assert_eq!(state.editor.draft(), draft);
        assert!(
            state
                .frame()
                .0
                .join("\n")
                .contains("Cannot attach referenced path:")
        );
        state.key(key(KeyCode::Char('!')));
        state.refresh_status("Ready");
        assert_eq!(state.status, "Ready");
    }

    #[test]
    fn a_refused_prompt_returns_to_the_editor_with_its_mentions_pastes_and_attachments() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chart.png");
        std::fs::write(&path, "image bytes").unwrap();
        let index = FileMentionIndex::build(directory.path(), &[]).unwrap();
        let mut state = State::new(true, 100, 24);
        state.prompt_enabled = true;
        state.file_mentions = FileMentionState::with_index(index);
        let paste = "first\nsecond\nthird\nfourth";
        let saved_context = saved_file_context("notes.md", "checkpoint notes");
        state.insert("Compare @cha");
        assert!(state.key(key(KeyCode::Tab)).is_none());
        state.insert(" with @notes.md and ");
        state.paste(paste);
        let draft = state.editor.draft().to_owned();
        let prompt = format!("Compare @chart.png with @notes.md and {paste}");
        let references = vec![path.canonicalize().unwrap()];
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Prompt { text, file_references, .. })
                if text == prompt && file_references == references
        ));
        assert!(state.editor.draft().is_empty());

        state.restore_rejected_prompt(vec![saved_context.clone()]);

        assert_eq!(state.editor.draft(), draft);
        assert_eq!(
            state.restored_contexts.as_slice(),
            std::slice::from_ref(&saved_context)
        );
        let (lines, _, _) = state.frame();
        assert!(
            lines
                .iter()
                .any(|line| line.contains("[Pasted text #1 +3 lines]"))
        );
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Prompt { text, file_references, .. })
                if text == prompt && file_references == references
        ));
    }

    #[test]
    fn a_refused_prompt_never_replaces_text_typed_after_sending() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("first prompt");
        assert!(state.key(key(KeyCode::Enter)).is_some());
        state.insert("typed meanwhile");

        state.restore_rejected_prompt(Vec::new());

        assert_eq!(state.editor.draft(), "typed meanwhile");
    }

    #[test]
    fn accepted_file_mention_in_branch_name_is_text_and_never_revalidated() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("notes.md");
        std::fs::write(&path, "context").unwrap();
        let index = FileMentionIndex::build(directory.path(), &[]).unwrap();
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.file_mentions = FileMentionState::with_index(index);

        state.insert("/branch @not");
        assert!(state.file_mentions.is_open());
        assert!(state.key(key(KeyCode::Tab)).is_none());
        assert_eq!(state.editor.draft(), "/branch @notes.md");
        std::fs::remove_file(path).unwrap();

        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Prompt { text, file_references, .. })
                if text == "/branch @notes.md" && file_references.is_empty()
        ));
        assert_eq!(state.branch_submission, BranchSubmission::Pending);
        assert!(!state.status.starts_with("Cannot attach referenced path:"));
    }

    #[test]
    fn control_r_opens_history_search_that_accepts_into_the_editor_or_cancels_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut history = InputHistory::open(root.path());
        history
            .record("session", project.path(), "first prompt", &[])
            .unwrap();
        history
            .record("session", project.path(), "second prompt", &[])
            .unwrap();
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.input_history = Some(history);
        state.history_project = project.path().into();
        state.history_session = "session".into();

        state.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert!(state.history_search.is_some());
        state.key(key(KeyCode::Char('f')));
        state.key(key(KeyCode::Enter));
        assert!(state.history_search.is_none());
        assert_eq!(state.editor.draft(), "first prompt");

        state.key(key(KeyCode::Left));
        let cursor = state.editor.cursor();
        state.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert!(state.history_search.is_some());
        state.key(key(KeyCode::Char('s')));
        state.key(key(KeyCode::Esc));
        assert!(state.history_search.is_none());
        assert_eq!(state.editor.draft(), "first prompt");
        assert_eq!(state.editor.cursor(), cursor);
    }

    #[test]
    fn up_arrow_reads_project_history_with_the_current_chat_first() {
        let root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        {
            let mut other_agent = InputHistory::open(root.path());
            other_agent
                .record("other-chat", project.path(), "其他 Chat 的输入", &[])
                .unwrap();
            other_agent
                .record("other-chat", elsewhere.path(), "other project", &[])
                .unwrap();
        }
        let mut history = InputHistory::open(root.path());
        history
            .record("current-chat", project.path(), "current older", &[])
            .unwrap();
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.input_history = Some(history);
        state.history_project = project.path().into();
        state.history_session = "current-chat".into();
        let console = console_with_state(state);
        console.record_prompt_history("current newer").unwrap();

        let mut state = console.state.lock().unwrap();
        state.insert("unsent");
        state.key(key(KeyCode::Up));
        assert_eq!(state.editor.draft(), "current newer");
        state.key(key(KeyCode::Up));
        assert_eq!(state.editor.draft(), "current older");
        state.key(key(KeyCode::Up));
        assert_eq!(state.editor.draft(), "其他 Chat 的输入");
        state.key(key(KeyCode::Up));
        assert_eq!(state.editor.draft(), "其他 Chat 的输入");
        for _ in 0..3 {
            state.key(key(KeyCode::Down));
        }
        assert_eq!(state.editor.draft(), "unsent");
    }

    #[test]
    fn shell_history_completion_filters_cached_commands_without_rereading_a_large_history() {
        let root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let project_key = project
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let history_line = |display: &str, project: &str| {
            serde_json::json!({
                "display": display,
                "pastedContents": {},
                "timestamp": 1,
                "project": project,
                "chatId": "other-chat",
            })
            .to_string()
                + "\n"
        };
        let mut contents = history_line("!cargo test --workspace", &project_key);
        let other_project_line = history_line(&format!("other {}", "x".repeat(200)), "/elsewhere");
        while contents.len() < 20 * 1024 * 1024 {
            contents.push_str(&other_project_line);
        }
        let history_file = root.path().join("history.jsonl");
        std::fs::write(&history_file, contents).unwrap();

        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.input_history = Some(InputHistory::open(root.path()));
        state.history_project = project.path().into();
        state.history_session = "current-chat".into();
        let shell_history_commands = |state: &State| {
            state
                .shell_input
                .history_candidates()
                .iter()
                .map(|candidate| candidate.command.clone())
                .collect::<Vec<_>>()
        };

        state.key(key(KeyCode::Char('!')));
        assert_eq!(state.shell_input.kind(), InputKind::Shell);
        // 进入 `!` 模式时已载入缓存；删除文件后连续按键仍能补全，说明按键过滤不再读文件。
        std::fs::remove_file(&history_file).unwrap();
        for character in "cargo".chars() {
            state.key(key(KeyCode::Char(character)));
        }
        assert_eq!(shell_history_commands(&state), ["cargo test --workspace"]);

        state.replace_draft(String::new(), 0);
        state.key(key(KeyCode::Backspace));
        assert_eq!(state.shell_input.kind(), InputKind::Prompt);
        state.key(key(KeyCode::Char('!')));
        for character in "cargo".chars() {
            state.key(key(KeyCode::Char(character)));
        }
        assert!(shell_history_commands(&state).is_empty());
    }

    #[test]
    fn a_large_folded_paste_is_restored_completely_from_history_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let pasted = format!("第一行\n{}\n最后一行", "长文本".repeat(600));
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.input_history = Some(InputHistory::open(root.path()));
        state.history_project = project.path().into();
        state.history_session = "paste-chat".into();
        state.insert("Summarize ");
        state.paste(&pasted);
        state.insert(" now");
        assert!(
            state
                .input_display()
                .0
                .contains("[Pasted text #1 +2 lines]")
        );
        let Some(InputEvent::Prompt { text, .. }) = state.key(key(KeyCode::Enter)) else {
            panic!("Enter should submit the prompt");
        };
        assert_eq!(text, format!("Summarize {pasted} now"));
        let console = console_with_state(state);
        console.record_prompt_history(&text).unwrap();
        drop(console);

        let mut restarted = State::new(true, 80, 24);
        restarted.prompt_enabled = true;
        restarted.input_history = Some(InputHistory::open(root.path()));
        restarted.history_project = project.path().into();
        restarted.history_session = "new-chat".into();
        restarted.key(key(KeyCode::Up));
        assert_eq!(restarted.editor.draft(), text);

        restarted.replace_draft(String::new(), 0);
        restarted.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        restarted.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert_eq!(
            restarted.history_search.as_ref().unwrap().scope(),
            HistoryScope::ThisProject
        );
        restarted.key(key(KeyCode::Enter));
        assert_eq!(restarted.editor.draft(), text);
    }

    #[test]
    fn rewind_draft_archives_existing_input() {
        let project = tempfile::tempdir().unwrap();
        let mut state = State::new(false, 80, 24);
        state.prompt_enabled = true;
        state.input_history = Some(InputHistory::in_memory());
        state.history_project = project.path().into();
        state.history_session = "rewind-session".into();
        let existing_draft = "newer unsent draft";
        state.replace_draft(existing_draft.into(), existing_draft.len());
        let console = console_with_state(state);

        let checkpoint_prompt = "Review @notes.md";
        console
            .restore_rewind_draft(checkpoint_prompt, &[])
            .unwrap();

        let state = console.state.lock().unwrap();
        assert_eq!(state.editor.draft(), checkpoint_prompt);
        assert_eq!(state.editor.cursor(), checkpoint_prompt.len());
        let history = state
            .input_history
            .as_ref()
            .unwrap()
            .search(
                &state.history_session,
                &state.history_project,
                HistoryScope::ThisSession,
                "",
                10,
            )
            .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].display(), existing_draft);
    }

    #[test]
    fn removing_a_restored_path_token_discards_its_saved_context() {
        let context = saved_file_context("notes with space.md", "checkpoint contents");
        let checkpoint_prompt = "Review @notes\\ with\\ space.md";
        let mut state = State::new(false, 80, 24);
        state.prompt_enabled = true;
        let console = console_with_state(state);
        console
            .restore_rewind_draft(checkpoint_prompt, std::slice::from_ref(&context))
            .unwrap();

        let submitted_text = {
            let mut state = console.state.lock().unwrap();
            assert_eq!(state.restored_contexts, [context]);
            let edited_prompt = "Review without the saved attachment";
            state.replace_draft(edited_prompt.into(), edited_prompt.len());
            assert!(state.restored_contexts.is_empty());
            match state.key(key(KeyCode::Enter)) {
                Some(InputEvent::Prompt {
                    text,
                    file_references,
                    ..
                }) => {
                    assert!(file_references.is_empty());
                    text
                }
                _ => panic!("edited rewind draft should submit as a prompt"),
            }
        };

        assert!(
            console
                .take_restored_contexts(&submitted_text)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn submitting_a_restored_path_token_returns_its_saved_context() {
        let context = saved_file_context("src/lib.rs", "checkpoint contents");
        let checkpoint_prompt = "Review @src/lib.rs";
        let mut state = State::new(false, 80, 24);
        state.prompt_enabled = true;
        let console = console_with_state(state);
        console
            .restore_rewind_draft(checkpoint_prompt, std::slice::from_ref(&context))
            .unwrap();

        let submitted_text = {
            let mut state = console.state.lock().unwrap();
            match state.key(key(KeyCode::Enter)) {
                Some(InputEvent::Prompt {
                    text,
                    file_references,
                    ..
                }) => {
                    assert!(file_references.is_empty());
                    text
                }
                _ => panic!("restored rewind draft should submit as a prompt"),
            }
        };

        assert_eq!(submitted_text, checkpoint_prompt);
        assert_eq!(
            console.take_restored_contexts(&submitted_text).unwrap(),
            [context]
        );
        assert!(
            console
                .take_restored_contexts(&submitted_text)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn the_input_has_one_editing_mode_without_a_mode_label() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        for character in "中文 hjkl x u i / ? v".chars() {
            assert!(state.key(key(KeyCode::Char(character))).is_none());
        }
        assert_eq!(state.editor.draft(), "中文 hjkl x u i / ? v");
        let status_line = state.frame().0.last().unwrap().clone();
        for label in ["-- INSERT --", "-- NORMAL --", "-- VISUAL"] {
            assert!(!status_line.contains(label), "{status_line}");
        }
    }

    #[test]
    fn cursor_and_draft_survive_transcript_and_menu_focus() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("keep this draft");
        state.key(key(KeyCode::Left));
        let cursor = state.editor.cursor();
        let draft = state.editor.draft().to_owned();

        state.start_transcript();
        state.key(key(KeyCode::Char('j')));
        state.key(key(KeyCode::Esc));
        assert!(state.transcript.is_none());
        assert_eq!(state.editor.cursor(), cursor);
        assert_eq!(state.editor.draft(), draft);

        state.menu = Some(menu());
        state.menu_focused = true;
        let selected = state.menu.as_ref().unwrap().selected;
        state.key(key(KeyCode::Char('j')));
        assert_eq!(state.menu.as_ref().unwrap().selected, selected + 1);
        state.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(state.menu.as_ref().unwrap().selected, selected);
        assert_eq!(state.editor.cursor(), cursor);
        assert_eq!(state.editor.draft(), draft);
    }

    #[test]
    fn escape_closes_candidates_first_then_confirms_before_clearing_the_draft() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("/mo");
        assert!(state.completion.is_open());

        assert!(state.key(key(KeyCode::Esc)).is_none());
        assert!(!state.completion.is_open());
        assert_eq!(state.editor.draft(), "/mo");
        assert_eq!(state.pending_escape_action(), None);

        assert!(matches!(
            state.key(key(KeyCode::Esc)),
            Some(InputEvent::Escape)
        ));
        assert_eq!(state.editor.draft(), "/mo");
        assert_eq!(
            state.pending_escape_action(),
            Some(EscapeAction::ClearDraft)
        );

        assert!(state.key(key(KeyCode::Esc)).is_none());
        assert!(state.editor.draft().is_empty());
    }

    #[test]
    fn chinese_editing_and_paste_keep_utf8_boundaries() {
        let mut state = State::new(true, 20, 10);
        state.prompt_enabled = true;
        state.insert("目录中文");
        state.key(key(KeyCode::Left));
        state.key(key(KeyCode::Backspace));
        state.key(key(KeyCode::Char('新')));
        assert_eq!(state.editor.draft(), "目录新文");
        state.key(key(KeyCode::Home));
        state.key(key(KeyCode::Delete));
        assert_eq!(state.editor.draft(), "录新文");
        assert_eq!(display_text("粘贴\n文字\u{1b}[2J"), "粘贴\n文字�[2J");
    }

    #[test]
    fn multiline_paste_preserves_code_and_the_view_uses_real_rows() {
        let mut state = State::new(true, 100, 24);
        state.prompt_enabled = true;
        state.insert("请检查:\n");
        state.menu = Some(menu());
        state.menu_focused = true;
        let code = "fn main() {\n\tprintln!(\"你好\");\n}";
        state.key(key(KeyCode::Tab));
        state.paste(code);
        assert!(state.menu_focused);
        assert!(state.menu.is_some());
        assert_eq!(state.editor.draft(), "请检查:\n");
        let menu_id = state.menu.as_ref().unwrap().id;
        state.clear_menu(menu_id);
        state.paste(code);
        assert_eq!(state.editor.draft(), format!("请检查:\n{code}"));
        let (lines, row, _) = state.frame();
        assert!(lines.iter().any(|line| line.contains("println!")));
        assert!(lines.iter().all(|line| !line.contains(['↵', '⇥', '\n'])));
        assert!(row > 0);
        assert!(
            matches!(state.key(key(KeyCode::Enter)), Some(InputEvent::Prompt { text, file_references, .. }) if text == format!("请检查:\n{code}") && file_references.is_empty())
        );
        assert!(state.menu.is_none());
        state.key(key(KeyCode::Up));
        assert_eq!(state.editor.draft(), format!("请检查:\n{code}"));
    }

    #[test]
    fn large_paste_is_folded_for_display_but_submits_the_original_text() {
        let mut state = State::new(true, 100, 24);
        state.prompt_enabled = true;
        let paste = "first\nsecond\nthird\nfourth";

        state.paste(paste);

        assert_eq!(state.editor.draft(), paste);
        let (lines, _, _) = state.frame();
        assert!(
            lines
                .iter()
                .any(|line| line.contains("[Pasted text #1 +3 lines]"))
        );
        assert!(lines.iter().all(|line| !line.contains("second")));
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Prompt { text, file_references, .. })
                if text == paste && file_references.is_empty()
        ));
    }

    #[test]
    fn arrow_navigation_and_delete_treat_a_folded_paste_as_one_block() {
        let mut state = State::new(true, 100, 24);
        state.prompt_enabled = true;
        let paste = "first\nsecond\nthird\nfourth";
        state.paste(paste);

        assert_eq!(state.editor.cursor(), paste.len());
        state.key(key(KeyCode::Left));
        assert_eq!(state.editor.cursor(), 0);
        state.key(key(KeyCode::Right));
        assert_eq!(state.editor.cursor(), paste.len());
        state.key(key(KeyCode::Left));
        state.key(key(KeyCode::Delete));
        assert!(state.editor.draft().is_empty());
        assert_eq!(state.editor.cursor(), 0);
    }

    fn png_bytes() -> Vec<u8> {
        let mut png = Vec::new();
        ::image::DynamicImage::ImageRgba8(::image::RgbaImage::new(4, 3))
            .write_to(&mut io::Cursor::new(&mut png), ::image::ImageFormat::Png)
            .unwrap();
        png
    }

    fn clipboard_image() -> Image {
        tickloop_agent::image::prepare(&png_bytes()).unwrap()
    }

    fn state_with_clipboard(image: Option<Image>, remote: bool) -> State {
        let mut state = State::new(false, 100, 24);
        state.prompt_enabled = true;
        state.clipboard = Clipboard::Captured {
            copies: Arc::default(),
            image,
            remote,
        };
        state
    }

    fn pasted_image(id: u32) -> ImageContext {
        ImageContext {
            origin: ImageOrigin::Pasted { id },
            image: clipboard_image(),
        }
    }

    fn submitted_images(state: &mut State) -> (String, Vec<ImageContext>) {
        match state.key(key(KeyCode::Enter)) {
            Some(InputEvent::Prompt { text, images, .. }) => (text, images),
            other => panic!("Enter should send the prompt, got {other:?}"),
        }
    }

    #[test]
    fn control_v_attaches_clipboard_images_numbered_with_folded_pastes_in_placeholder_order() {
        let mut state = state_with_clipboard(Some(clipboard_image()), false);
        let paste = "first\nsecond\nthird\nfourth";
        state.paste(paste);
        state.insert(" then ");
        assert!(state.key(control(KeyCode::Char('v'))).is_none());
        set_cursor(&mut state, 0);
        state.key(control(KeyCode::Char('v')));

        let expected = format!("[Image #3]{paste} then [Image #2]");
        assert_eq!(state.editor.draft(), expected);
        assert!(
            state
                .input_display()
                .0
                .starts_with("[Image #3][Pasted text #1 +3 lines] then [Image #2]")
        );
        assert_eq!(
            submitted_images(&mut state),
            (expected, vec![pasted_image(3), pasted_image(2)])
        );

        state.insert("next ");
        state.key(control(KeyCode::Char('v')));
        assert_eq!(state.editor.draft(), "next [Image #4]");
    }

    #[test]
    fn pasted_images_are_cached_privately_per_chat_and_a_failed_write_still_attaches() {
        use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let chat = Uuid::new_v4();
        let mut state = state_with_clipboard(Some(clipboard_image()), false);
        state.switch_editor_chat(Some(chat));
        let console = console_with_state(state);
        let cached = |id: u32| {
            root.path()
                .join("image-cache")
                .join(chat.to_string())
                .join(format!("{id}.png"))
        };

        console.configure_image_cache(Some(root.path())).unwrap();
        let mut state = console.state.lock().unwrap();
        state.key(control(KeyCode::Char('v')));
        assert_eq!(
            std::fs::read(cached(1)).unwrap(),
            BASE64.decode(clipboard_image().data).unwrap()
        );
        assert_eq!(
            std::fs::metadata(cached(1)).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            submitted_images(&mut state),
            ("[Image #1]".into(), vec![pasted_image(1)])
        );
        drop(state);

        let unwritable_root = root.path().join("not-a-directory");
        std::fs::write(&unwritable_root, "").unwrap();
        console
            .configure_image_cache(Some(&unwritable_root))
            .unwrap();
        let mut state = console.state.lock().unwrap();
        state.key(control(KeyCode::Char('v')));
        assert_eq!(
            submitted_images(&mut state),
            ("[Image #2]".into(), vec![pasted_image(2)]),
            "a failed cache write must not block attaching"
        );
        drop(state);

        console.configure_image_cache(None).unwrap();
        let mut state = console.state.lock().unwrap();
        state.key(control(KeyCode::Char('v')));
        assert_eq!(submitted_images(&mut state).1, [pasted_image(3)]);
        assert!(!cached(3).exists(), "unsaved sessions cache nothing");
    }

    #[test]
    fn sent_prompts_and_the_transcript_link_cached_images_only_where_links_are_shown() {
        let root = tempfile::tempdir().unwrap();
        let chat = Uuid::new_v4();
        let cache = ImageCache::in_root(root.path());
        let cached = cache.store(chat, 1, &clipboard_image()).unwrap();
        let link_open = hyperlink::Hyperlink::file(&cached).unwrap().open();
        let prompt = "compare [Image #1] with [Image #2]";
        let drawn = |terminal_shows_links: bool, transcript: bool| {
            let mut state = State::new(true, 80, 24);
            state.switch_editor_chat(Some(chat));
            state.image_cache = Some(cache.clone());
            state.terminal_shows_links = terminal_shows_links;
            state.record_input(prompt);
            if transcript {
                state.start_transcript();
                let document = crate::terminal::presentation::transcript_document(
                    &serde_json::from_value(serde_json::json!({
                        "id": chat,
                        "workdir": "/work",
                        "messages": [{"role": "user", "content": [{"type": "text", "text": prompt}]}],
                        "loops": [],
                    }))
                    .unwrap(),
                    state.display,
                    &state.image_links(),
                );
                state.transcript.as_mut().unwrap().1.update(document);
            }
            let mut output = Vec::new();
            state.draw(&mut output).unwrap();
            (
                String::from_utf8(output).unwrap(),
                state.rendered_screen.join("\n"),
            )
        };

        for transcript in [false, true] {
            let (output, screen) = drawn(true, transcript);
            let linked = output
                .split(&link_open)
                .nth(1)
                .and_then(|rest| rest.split("\x1b]8;;\x07").next())
                .expect("the cached placeholder should be linked");
            assert!(
                linked.contains("[Image #1]") && !linked.contains("compare"),
                "{linked:?}"
            );
            assert_eq!(
                output.matches("\x1b]8;;").count(),
                2,
                "only the cached image is linked, once"
            );
            assert!(screen.contains(prompt), "the visible text is unchanged");

            let (plain, _) = drawn(false, transcript);
            assert!(!plain.contains("\x1b]8;"));
        }
    }

    #[test]
    fn control_v_without_a_clipboard_image_explains_why_and_points_ssh_users_to_paths() {
        let mut state = state_with_clipboard(None, false);
        state.key(control(KeyCode::Char('v')));
        assert_eq!(state.status, "No image found in clipboard");
        assert!(state.editor.draft().is_empty());

        let mut remote = state_with_clipboard(None, true);
        remote.key(control(KeyCode::Char('v')));
        assert!(
            remote.status.starts_with("No image found in clipboard")
                && remote.status.contains("remote host")
                && remote.status.contains("@path"),
            "{}",
            remote.status
        );
        assert!(remote.editor.draft().is_empty());
    }

    #[test]
    fn control_v_inserts_nothing_when_the_chat_model_does_not_accept_images() {
        let chat = Uuid::new_v4();
        let mut state = state_with_clipboard(Some(clipboard_image()), false);
        state.switch_editor_chat(Some(chat));
        let console = console_with_state(state);

        console.set_chat_images(chat, false, None).unwrap();
        let mut state = console.state.lock().unwrap();
        state.key(control(KeyCode::Char('v')));

        assert!(state.editor.draft().is_empty());
        assert!(
            state.status.contains("does not support image input")
                && state.status.contains("/model"),
            "{}",
            state.status
        );
    }

    fn status_line(state: &mut State) -> String {
        state.frame().0.last().unwrap().clone()
    }

    fn shows_clipboard_hint(state: &mut State) -> bool {
        status_line(state).contains("Image in clipboard · ctrl+v to paste")
    }

    /// Leaves the terminal and comes back at `now`.
    fn refocus(state: &mut State, now: Instant) {
        state.focus_lost();
        state.focus_gained(now);
    }

    #[test]
    fn regaining_focus_hints_at_a_clipboard_image_after_one_second_for_eight_seconds() {
        let mut state = state_with_clipboard(Some(clipboard_image()), false);
        let start = Instant::now();
        refocus(&mut state, start);

        assert!(!state.poll_clipboard_hint(start + Duration::from_millis(999)));
        assert!(!shows_clipboard_hint(&mut state));
        assert!(
            state.poll_clipboard_hint(start + Duration::from_secs(1)),
            "the hint appearing must redraw"
        );
        assert!(shows_clipboard_hint(&mut state));

        assert!(!state.poll_clipboard_hint(start + Duration::from_millis(8_999)));
        assert!(shows_clipboard_hint(&mut state));
        assert!(
            state.poll_clipboard_hint(start + Duration::from_secs(9)),
            "the hint expiring must redraw"
        );
        assert!(!shows_clipboard_hint(&mut state));
    }

    #[test]
    fn only_a_return_to_the_terminal_checks_the_clipboard_and_rapid_returns_restart_the_debounce() {
        let mut state = state_with_clipboard(Some(clipboard_image()), false);
        let start = Instant::now();
        state.focus_gained(start);
        assert!(!state.poll_clipboard_hint(start + Duration::from_secs(2)));

        refocus(&mut state, start + Duration::from_secs(3));
        state.focus_lost();
        assert!(
            !state.poll_clipboard_hint(start + Duration::from_secs(5)),
            "leaving again cancels the pending check"
        );

        refocus(&mut state, start + Duration::from_secs(6));
        refocus(&mut state, start + Duration::from_millis(6_800));
        assert!(!state.poll_clipboard_hint(start + Duration::from_secs(7)));
        assert!(state.poll_clipboard_hint(start + Duration::from_millis(7_800)));
        assert!(shows_clipboard_hint(&mut state));
    }

    #[test]
    fn the_clipboard_hint_does_not_return_within_thirty_seconds() {
        let mut state = state_with_clipboard(Some(clipboard_image()), false);
        let start = Instant::now();
        refocus(&mut state, start);
        assert!(state.poll_clipboard_hint(start + Duration::from_secs(1)));
        assert!(state.poll_clipboard_hint(start + Duration::from_secs(9)));

        refocus(&mut state, start + Duration::from_secs(20));
        assert!(!state.poll_clipboard_hint(start + Duration::from_secs(21)));
        assert!(!shows_clipboard_hint(&mut state));

        refocus(&mut state, start + Duration::from_secs(30));
        assert!(state.poll_clipboard_hint(start + Duration::from_secs(31)));
        assert!(shows_clipboard_hint(&mut state));
    }

    #[test]
    fn no_clipboard_hint_without_an_image_or_for_a_text_only_model() {
        let mut empty = state_with_clipboard(None, false);
        let start = Instant::now();
        refocus(&mut empty, start);
        assert!(!empty.poll_clipboard_hint(start + Duration::from_secs(1)));
        assert!(!shows_clipboard_hint(&mut empty));

        let chat = Uuid::new_v4();
        let mut state = state_with_clipboard(Some(clipboard_image()), false);
        state.switch_editor_chat(Some(chat));
        let console = console_with_state(state);
        console.set_chat_images(chat, false, None).unwrap();
        let mut state = console.state.lock().unwrap();
        refocus(&mut state, start);
        assert!(!state.poll_clipboard_hint(start + Duration::from_secs(1)));
        assert!(!shows_clipboard_hint(&mut state));
    }

    #[test]
    fn the_clipboard_hint_stands_in_for_key_hints_and_never_hides_the_status() {
        let mut state = state_with_clipboard(Some(clipboard_image()), false);
        state.status = "Cannot open Diff: disk full".into();
        let start = Instant::now();
        refocus(&mut state, start);
        state.poll_clipboard_hint(start + Duration::from_secs(1));
        assert_eq!(
            status_line(&mut state),
            "Cannot open Diff: disk full · Image in clipboard · ctrl+v to paste"
        );

        state.request_exit('C');
        assert!(status_line(&mut state).contains("Press Ctrl+C again to exit"));
        assert!(!shows_clipboard_hint(&mut state));
        state.exit_confirmation = None;

        state.menu = Some(menu());
        assert!(
            !shows_clipboard_hint(&mut state),
            "a permission or question menu keeps its own key hints"
        );
        state.menu = None;
        assert!(shows_clipboard_hint(&mut state));
    }

    #[test]
    fn the_configured_status_line_keeps_the_key_confirmations_visible() {
        let mut state = State::new(true, 120, 24);
        state.prompt_enabled = true;
        state
            .status_area
            .update("Auto".into(), "claude-fable-5-1".into(), None, Vec::new());
        assert!(!state.request_escape(EscapeAction::OpenRewind));
        let (lines, input_row, _) = state.frame();
        let line = lines.last().unwrap();
        assert!(line.contains("Press Esc again to rewind"), "{line:?}");
        let core = &lines[input_row + 1];
        assert!(
            core.starts_with('─')
                && core.contains("⏵⏵ Auto")
                && core.contains("claude-fable-5-1")
                && core.contains("Context unknown"),
            "{core:?}"
        );
        assert_eq!(
            lines.iter().filter(|row| row.contains("⏵⏵ Auto")).count(),
            1
        );
        state.escape_confirmation = None;

        state.status = "Not saved: disk full".into();
        assert!(!state.request_exit('C'));
        assert!(
            status_line(&mut state).contains("Not saved: disk full · Press Ctrl+C again to exit")
        );
        state.exit_confirmation = None;
        let line = status_line(&mut state);
        assert!(
            line.contains("Not saved: disk full") && !line.contains("Press"),
            "{line:?}"
        );
    }

    #[test]
    fn an_expired_key_confirmation_clears_itself_once_so_its_hint_can_leave() {
        let mut state = State::new(true, 80, 24);
        assert!(!state.expire_key_confirmations());
        state.exit_confirmation = Some(ExitConfirmation {
            control: 'C',
            requested_at: Instant::now() - EXIT_CONFIRMATION_WINDOW - Duration::from_millis(1),
        });
        state.escape_confirmation = Some(EscapeConfirmation {
            action: EscapeAction::OpenRewind,
            requested_at: Instant::now(),
        });
        assert!(state.expire_key_confirmations());
        assert!(state.exit_confirmation.is_none());
        assert!(
            state.escape_confirmation.is_some(),
            "a live confirmation stays"
        );
        assert!(!state.expire_key_confirmations());
    }

    #[test]
    fn a_frame_repaints_only_the_rows_that_changed() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.scrollback.append("first line\n");
        state.draw(&mut Vec::new()).unwrap();

        let mut output = Vec::new();
        state.draw(&mut output).unwrap();
        let unchanged = String::from_utf8(output).unwrap();
        assert!(
            !unchanged.contains("\x1b[2K") && !unchanged.contains("\x1b[2J"),
            "{unchanged:?}"
        );

        state.status = "Not saved: disk full".into();
        let mut output = Vec::new();
        state.draw(&mut output).unwrap();
        let changed = String::from_utf8(output).unwrap();
        assert_eq!(changed.matches("\x1b[2K").count(), 1, "{changed:?}");
        assert!(changed.contains("Not saved: disk full") && !changed.contains("first line"));

        state.width = 60;
        let mut output = Vec::new();
        state.draw(&mut output).unwrap();
        let resized = String::from_utf8(output).unwrap();
        assert!(
            resized.contains("\x1b[2J") && resized.contains("first line"),
            "{resized:?}"
        );
    }

    #[test]
    fn a_line_appended_to_a_full_conversation_scrolls_the_region_instead_of_repainting_it() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        for index in 0..40 {
            state.scrollback.append(&format!("line {index}\n"));
        }
        state.draw(&mut Vec::new()).unwrap();
        let Some(RenderedRegion {
            view: RenderedView::Conversation {
                body_rows: region_rows,
            },
            ..
        }) = state.rendered_region
        else {
            panic!("the conversation was drawn");
        };
        assert!(state.rendered_screen[0].contains("line") && region_rows > 2);

        state.scrollback.append("line 40\n");
        let mut output = Vec::new();
        state.draw(&mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(
            output.contains(&format!("\x1b[1;{region_rows}r\x1b[1S\x1b[r")),
            "{output:?}"
        );
        assert_eq!(output.matches("\x1b[2K").count(), 1, "{output:?}");
        assert!(
            output.contains("line 40") && !output.contains("line 39"),
            "{output:?}"
        );
        assert!(state.rendered_screen[region_rows - 2].contains("line 40"));

        let mut output = Vec::new();
        state.draw(&mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(
            !output.contains("\x1b[2K") && !output.contains('S'),
            "{output:?}"
        );
    }

    #[test]
    fn body_scroll_prefers_the_shift_that_leaves_the_fewest_rows_to_repaint() {
        let rows = |texts: &[&str]| {
            texts
                .iter()
                .map(|text| (*text).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            body_scroll(&rows(&["a", "b", "c", ""]), &rows(&["b", "c", "d", ""])),
            Some(1)
        );
        assert_eq!(
            body_scroll(&rows(&["a", "b", "c", "d"]), &rows(&["c", "d", "e", "f"])),
            Some(2)
        );
        assert_eq!(
            body_scroll(&rows(&["", "", "a"]), &rows(&["", "a", "b"])),
            Some(1),
            "a scroll plus one row beats two rows in place"
        );
        assert_eq!(
            body_scroll(&rows(&["a", "b", "c"]), &rows(&["a", "b", "x"])),
            None,
            "one row in place beats any scroll"
        );
        assert_eq!(body_scroll(&rows(&["a", "b"]), &rows(&["a", "b"])), None);
        assert_eq!(body_scroll(&rows(&["a", "b"]), &rows(&["x", "y"])), None);
        assert_eq!(body_scroll(&[], &[]), None);
    }

    fn frame_output(state: &mut State) -> String {
        let mut output = Vec::new();
        state.draw(&mut output).unwrap();
        String::from_utf8(output).unwrap()
    }

    fn assert_nothing_rewritten(output: &str, absent_text: &str) {
        assert!(
            !output.contains("\x1b[2J") && !output.contains("\x1b[2K"),
            "{output:?}"
        );
        assert!(!output.contains(absent_text), "{output:?}");
    }

    #[test]
    fn a_task_panel_frame_repaints_only_the_rows_that_changed() {
        let chat_id = Uuid::new_v4();
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.chat = Some(chat_id);
        state.scrollback.append("conversation above the panel\n");
        let console = console_with_state(state);
        console
            .show_tasks_at(task_snapshot(chat_id, 3), None)
            .unwrap();
        let mut state = console.state.lock().unwrap();

        assert_nothing_rewritten(&frame_output(&mut state), "printf task-0");

        state.key(key(KeyCode::Down));
        let moved = frame_output(&mut state);
        assert!(!moved.contains("\x1b[2J"), "{moved:?}");
        assert_eq!(
            moved.matches("\x1b[2K").count(),
            2,
            "the row the selection left and the row it reached: {moved:?}"
        );
        assert!(
            !moved.contains("conversation above the panel") && !moved.contains("Shell tasks"),
            "{moved:?}"
        );
    }

    #[test]
    fn transcript_and_diff_frames_are_not_rewritten_when_nothing_changed() {
        let chat_id = Uuid::new_v4();
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.chat = Some(chat_id);
        state.transcript = Some((
            Uuid::new_v4(),
            Transcript::new(vec!["saved history row".into()].into()),
        ));
        assert!(frame_output(&mut state).contains("\x1b[2J"));
        assert_nothing_rewritten(&frame_output(&mut state), "saved history row");

        state.transcript = None;
        state.open_diff_screen(chat_id, diff_catalog(40));
        assert!(frame_output(&mut state).contains("\x1b[2J"));
        assert_nothing_rewritten(&frame_output(&mut state), "Diff review");
    }

    #[test]
    fn switching_views_at_the_same_size_draws_the_new_view_whole() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.scrollback.append("conversation row\n");
        frame_output(&mut state);

        state.transcript = Some((
            Uuid::new_v4(),
            Transcript::new(vec!["saved history row".into()].into()),
        ));
        let opened = frame_output(&mut state);
        assert!(
            opened.contains("\x1b[2J") && opened.contains("saved history row"),
            "{opened:?}"
        );

        // Closed without the explicit invalidation the close paths add: the
        // cached view, not that convention, keeps the conversation's terminal
        // scroll from shifting rows another view painted.
        state.transcript = None;
        let closed = frame_output(&mut state);
        assert!(
            closed.contains("\x1b[2J") && closed.contains("conversation row"),
            "{closed:?}"
        );
        assert!(!closed.contains("S\x1b[r"), "{closed:?}");
    }

    #[test]
    fn a_resize_draws_the_whole_screen_even_when_it_ends_at_the_drawn_size() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.scrollback.append("kept line\n");
        frame_output(&mut state);
        let settled = frame_output(&mut state);
        assert!(!settled.contains("\x1b[2J"), "{settled:?}");

        // A FrameHold, or signals the terminal coalesced, leaves no frame
        // between the two sizes.
        let drawn = (state.width, state.height);
        state.resize(61, 24);
        state.resize(80, 24);
        assert_eq!((state.width, state.height), drawn);
        let resized = frame_output(&mut state);
        assert!(
            resized.contains("\x1b[2J") && resized.contains("kept line"),
            "{resized:?}"
        );
    }

    #[test]
    fn a_whole_screen_repaint_rewrites_a_retained_view_that_did_not_change() {
        let chat_id = Uuid::new_v4();
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.chat = Some(chat_id);
        state.open_diff_screen(chat_id, diff_catalog(40));
        frame_output(&mut state);

        // Renderer switches and failed hand-overs repaint this way; the screen
        // they leave behind may be blank although nothing in the view changed.
        let mut output = Vec::new();
        state.repaint_whole_screen(&mut output);
        let repainted = String::from_utf8(output).unwrap();
        assert!(
            repainted.contains("\x1b[2J") && repainted.contains("Diff review"),
            "{repainted:?}"
        );
    }

    #[test]
    fn an_unchanged_connection_notice_does_not_repaint_the_conversation() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.scrollback.append("first line\n");
        frame_output(&mut state);
        // Console methods draw to the process's stdout. While the State is not
        // interactive they only update it, so the frame they leave pending is
        // the one captured below.
        state.interactive = false;
        let console = console_with_state(state);
        console.set_connection_notice(None).unwrap();

        let mut state = console.state.lock().unwrap();
        state.interactive = true;
        assert_nothing_rewritten(&frame_output(&mut state), "first line");
    }

    #[test]
    fn content_setters_rewrite_only_the_rows_they_change() {
        for change in ["creation notice", "status lines"] {
            let mut state = State::new(true, 80, 24);
            state.prompt_enabled = true;
            state.scrollback.append("first line\n");
            frame_output(&mut state);
            state.interactive = false;
            let console = console_with_state(state);
            if change == "creation notice" {
                console
                    .set_creation_notice(Some("Creation unknown".into()))
                    .unwrap();
            } else {
                console
                    .set_status_lines(Some(vec!["custom status".into()]))
                    .unwrap();
            }
            let mut state = console.state.lock().unwrap();
            state.interactive = true;
            let changed = frame_output(&mut state);
            assert!(!changed.contains("\x1b[2J"), "{change}: {changed:?}");
            assert!(!changed.contains("first line"), "{change}: {changed:?}");
            assert_eq!(
                changed.matches("\x1b[2K").count(),
                1,
                "{change}: {changed:?}"
            );
        }
    }

    #[test]
    fn an_unchanged_view_writes_no_rows_with_or_without_synchronized_output() {
        for synchronized_output in [false, true] {
            let chat_id = Uuid::new_v4();
            let mut state = State::new(true, 80, 24);
            state.synchronized_output = synchronized_output;
            state.prompt_enabled = true;
            state.chat = Some(chat_id);
            state.open_diff_screen(chat_id, diff_catalog(40));
            frame_output(&mut state);
            let unchanged = frame_output(&mut state);
            assert_eq!(
                unchanged.contains("\x1b[?2026h"),
                synchronized_output,
                "{unchanged:?}"
            );
            assert_nothing_rewritten(&unchanged, "Diff review");
        }
    }

    #[test]
    fn floated_connection_details_hint_follows_focus_and_ctrl_o_dispatch() {
        for (notice, notice_text) in [
            (ExecutionConnectionNotice::Lost, "Lost · outcome unknown"),
            (
                ExecutionConnectionNotice::LostHistory,
                "Lost · cached history",
            ),
        ] {
            for focus in ["foreground Shell", "footer", "conversation"] {
                let mut state = State::new(true, 180, 24);
                state.prompt_enabled = true;
                state.execution_connection_notice = Some(notice);
                state.status_area.update(
                    "Auto".into(),
                    "loss-model".into(),
                    None,
                    vec![TodoItem {
                        content: "review".into(),
                        status: tickloop_agent::runtime::TodoStatus::Pending,
                        active_form: "Working".into(),
                    }],
                );
                super::layout_tests::set_status_family(&mut state.status_area);
                state.insert("preserved draft");
                let cursor = state.editor.cursor();
                state.scrollback.append(
                    &(0..100)
                        .map(|index| format!("history-{index}\n"))
                        .collect::<String>(),
                );
                frame_output(&mut state);

                match focus {
                    "foreground Shell" => state.set_foreground_shell(true),

                    "footer" => {
                        assert!(state.key(key(KeyCode::Down)).is_none());
                        assert_eq!(state.footer.selected(), Some(FooterItemId::Agents));
                    }
                    "conversation" => {
                        assert!(state.key(key(KeyCode::PageUp)).is_none());
                        assert!(state.conversation_focused);
                        assert!(!state.scrollback.is_at_bottom());
                    }
                    _ => unreachable!(),
                }
                frame_output(&mut state);
                let blocked = state.rendered_screen.join("\n");
                assert!(blocked.contains(notice_text), "{focus}: {blocked}");
                assert!(!blocked.contains("Ctrl+O details"), "{focus}: {blocked}");
                assert!(state.key(control(KeyCode::Char('o'))).is_none(), "{focus}");
                assert!(state.transcript.is_none(), "{focus}");
                assert_eq!(state.editor.draft(), "preserved draft", "{focus}");
                assert_eq!(state.editor.cursor(), cursor, "{focus}");

                match focus {
                    "foreground Shell" => {
                        assert!(matches!(
                            state.key(control(KeyCode::Char('b'))),
                            Some(InputEvent::BackgroundForegroundShell)
                        ));
                        state.set_foreground_shell(false);
                    }

                    "footer" => {
                        assert!(state.key(key(KeyCode::Esc)).is_none());
                        assert_eq!(state.footer.selected(), None);
                        assert!(state.status_area.is_open());
                    }
                    "conversation" => {
                        assert!(state.key(control(KeyCode::End)).is_none());
                        assert!(!state.conversation_focused);
                        assert!(state.scrollback.is_at_bottom());
                    }
                    _ => unreachable!(),
                }
                frame_output(&mut state);
                let returned = state.rendered_screen.join("\n");
                assert!(returned.contains(notice_text), "{focus}: {returned}");
                assert_eq!(
                    returned.matches("Ctrl+O details").count(),
                    1,
                    "{focus}: {returned}"
                );
                assert!(
                    returned.contains("Auto")
                        && returned.contains("loss-model")
                        && returned.contains("Context unknown"),
                    "{focus}: {returned}"
                );
                assert!(
                    state
                        .rendered_screen
                        .iter()
                        .all(|line| display_width(line) <= 179)
                );
                let Some(InputEvent::OpenTranscript { id }) =
                    state.key(control(KeyCode::Char('o')))
                else {
                    panic!("{focus}: the visible details hint did not open the transcript");
                };
                assert_eq!(state.transcript.as_ref().unwrap().0, id);
                assert_eq!(state.editor.draft(), "preserved draft", "{focus}");
                assert_eq!(state.editor.cursor(), cursor, "{focus}");
            }
        }
    }

    #[test]
    fn a_changed_connection_notice_rewrites_only_the_status_row() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.scrollback.append(
            "first line
",
        );
        frame_output(&mut state);
        state.interactive = false;
        let console = console_with_state(state);
        console
            .set_connection_notice(Some("Disconnected · cached view · owner gone".into()))
            .unwrap();
        let mut state = console.state.lock().unwrap();
        state.interactive = true;
        let changed = frame_output(&mut state);
        assert!(!changed.contains("[2J"), "{changed:?}");
        assert!(!changed.contains("first line"), "{changed:?}");
        assert_eq!(changed.matches("[2K").count(), 1, "{changed:?}");
        assert!(changed.contains("Disconnected"), "{changed:?}");
    }

    fn conversation_rows(state: &State) -> usize {
        match state.rendered_region {
            Some(RenderedRegion {
                view: RenderedView::Conversation { body_rows },
                ..
            }) => body_rows,
            other => panic!("the conversation was not drawn: {other:?}"),
        }
    }

    /// Draws a settled frame, applies `change`, which must change the height of
    /// the input area at the same screen size, and returns the next frame's
    /// output. The conversation is short, so the blank top row stays blank.
    pub(super) fn output_after_input_area_change(
        state: &mut State,
        change: impl FnOnce(&mut State),
    ) -> String {
        frame_output(state);
        let history_rows = conversation_rows(state);
        change(state);
        let output = frame_output(state);
        assert_ne!(
            conversation_rows(state),
            history_rows,
            "the change kept the input area's height"
        );
        assert!(!output.contains("\x1b[2J"), "{output:?}");
        assert!(
            !output.contains("\x1b[1;1H"),
            "the blank top row was written again: {output:?}"
        );
        output
    }

    #[test]
    fn conversation_occupancy_slash_candidates_growing_and_shrinking_keep_other_rows() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.scrollback.append("first line\n");
        state.insert("/mo");

        let grown = output_after_input_area_change(&mut state, |state| {
            state.key(key(KeyCode::Backspace));
            state.key(key(KeyCode::Backspace));
        });
        assert!(!grown.contains("Enter send"), "{grown:?}");

        let shrunk = output_after_input_area_change(&mut state, |state| state.insert("mo"));
        assert!(!shrunk.contains("Enter send"), "{shrunk:?}");
    }

    #[test]
    fn conversation_occupancy_a_draft_growing_a_line_keeps_other_rows() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.scrollback.append("first line\n");
        state.insert("draft line");

        let grown = output_after_input_area_change(&mut state, |state| {
            state.insert("\nsecond draft line");
        });
        assert!(grown.contains("second draft line"), "{grown:?}");
        assert!(!grown.contains("Enter send"), "{grown:?}");
    }

    #[test]
    fn conversation_occupancy_an_activity_row_arriving_and_leaving_keeps_input() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.scrollback.append("first line\n");
        state.insert("draft line");
        frame_output(&mut state);
        let bottom_rows = state.styled_frame().0.len();
        state
            .scrollback
            .set_activity(Some(RoleLine::role("activity fixture", Role::MutedText)));
        let arrived = frame_output(&mut state);
        assert_eq!(state.styled_frame().0.len(), bottom_rows);
        assert!(arrived.contains("activity fixture"), "{arrived:?}");
        assert!(
            !arrived.contains("draft line") && !arrived.contains("Enter send"),
            "{arrived:?}"
        );

        state.scrollback.set_activity(None);
        let left = frame_output(&mut state);
        assert_eq!(state.styled_frame().0.len(), bottom_rows);
        for output in [&arrived, &left] {
            assert!(!output.contains("\x1b[2J"), "{output:?}");
            assert!(!output.contains("\x1b[1;1H"), "{output:?}");
        }
        assert!(
            !left.contains("draft line") && !left.contains("Enter send"),
            "{left:?}"
        );
    }

    #[test]
    fn conversation_occupancy_a_changed_input_area_rewrites_instead_of_shifting() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        for index in 0..40 {
            state.scrollback.append(&format!("line {index}\n"));
        }
        state.insert("draft line");
        frame_output(&mut state);

        state.insert("\nsecond draft line");
        let grown = frame_output(&mut state);
        assert!(!grown.contains("\x1b[2J"), "{grown:?}");
        assert!(
            !grown.contains("S\x1b[r"),
            "the editor and footer rows must not be shifted with the history: {grown:?}"
        );
        assert!(!grown.contains("Enter send"), "{grown:?}");

        state.scrollback.append("line 40\n");
        let appended = frame_output(&mut state);
        assert!(
            appended.contains("S\x1b[r"),
            "history that kept its rows still scrolls: {appended:?}"
        );
        assert_eq!(appended.matches("\x1b[2K").count(), 1, "{appended:?}");
    }

    #[test]
    fn held_frames_reach_the_terminal_once_when_the_hold_is_released() {
        let console = InteractiveConsoleFixture::new(State::new(true, 80, 24));
        let hold = console.hold_frames().unwrap();
        {
            let mut state = console.state.lock().unwrap();
            state.scrollback.append("held\n");
            let mut output = Vec::new();
            state.draw(&mut output).unwrap();
            assert!(output.is_empty() && state.frame_deferred);
        }
        drop(hold);
        let state = console.state.lock().unwrap();
        assert_eq!(state.held_frames, 0);
        assert!(!state.frame_deferred);
        assert!(
            state
                .rendered_screen
                .iter()
                .any(|line| line.contains("held"))
        );
    }

    #[test]
    fn focus_reporting_is_switched_with_bracketed_paste_and_off_when_the_terminal_is_restored() {
        let mut enabled = Vec::new();
        enable_input_reporting(&mut enabled).unwrap();
        let enabled = String::from_utf8(enabled).unwrap();
        assert!(enabled.contains("\x1b[?2004h") && enabled.contains("\x1b[?1004h"));

        let mut restored = Vec::new();
        let _ = restore_terminal(&mut restored, &TerminalSurface::new(false));
        let restored = String::from_utf8(restored).unwrap();
        assert!(restored.contains("\x1b[?2004l") && restored.contains("\x1b[?1004l"));
    }

    #[test]
    fn an_image_placeholder_moves_and_deletes_as_one_block_and_editing_it_drops_the_image() {
        let mut state = state_with_clipboard(Some(clipboard_image()), false);
        state.insert("see ");
        state.key(control(KeyCode::Char('v')));
        let end = state.editor.draft().len();

        for width in [24, 40, 80] {
            state.width = width;
            let (lines, _, _) = state.normal_styled_frame();
            let image_tag = lines
                .iter()
                .flat_map(|line| &line.spans)
                .filter(|span| span.role == Some(Role::Accent))
                .map(|span| span.text.as_str())
                .collect::<String>();
            assert_eq!(image_tag, "[Image #1]");
            assert_eq!(state.editor.cursor(), end);
        }

        state.key(key(KeyCode::Left));
        assert_eq!(state.editor.cursor(), "see ".len());
        state.key(key(KeyCode::Right));
        assert_eq!(state.editor.cursor(), end);
        state.key(key(KeyCode::Backspace));
        assert_eq!(state.editor.draft(), "see ");
        assert_eq!(submitted_images(&mut state).1, []);

        state.insert("again ");
        state.key(control(KeyCode::Char('v')));
        assert_eq!(state.editor.draft(), "again [Image #2]");
        state.key(control(KeyCode::Char('w')));
        assert_eq!(state.editor.draft(), "again ");
        assert_eq!(submitted_images(&mut state), ("again ".into(), Vec::new()));
    }

    #[test]
    fn word_and_line_kills_that_reach_into_a_pasted_block_remove_the_whole_block() {
        let alt = |code| KeyEvent::new(code, KeyModifiers::ALT);

        let mut state = state_with_clipboard(Some(clipboard_image()), false);
        state.insert("see ");
        state.key(control(KeyCode::Char('v')));
        state.key(alt(KeyCode::Backspace));
        assert_eq!(state.editor.draft(), "see ");
        assert_eq!(submitted_images(&mut state), ("see ".into(), Vec::new()));

        state.key(control(KeyCode::Char('v')));
        state.insert(" later");
        state.key(control(KeyCode::Char('a')));
        state.key(alt(KeyCode::Char('d')));
        assert_eq!(state.editor.draft(), " later");
        assert_eq!(submitted_images(&mut state), (" later".into(), Vec::new()));

        state.key(control(KeyCode::Char('v')));
        state.insert(" note");
        state.key(control(KeyCode::Char('w')));
        assert_eq!(state.editor.draft(), "[Image #3] ");
        assert_eq!(
            submitted_images(&mut state),
            ("[Image #3] ".into(), vec![pasted_image(3)])
        );

        let pasted = "first line\nsecond line\nthird line\nfourth line words";
        state.insert("intro ");
        state.paste(pasted);
        state.key(control(KeyCode::Char('w')));
        assert_eq!(state.editor.draft(), "intro ");
        state.paste(pasted);
        state.key(control(KeyCode::Char('u')));
        assert!(state.editor.draft().is_empty(), "{}", state.editor.draft());

        state.paste(pasted);
        state.insert(" tail");
        for _ in 0..=" tail".len() {
            state.key(key(KeyCode::Left));
        }
        assert_eq!(state.editor.cursor(), 0);
        state.key(control(KeyCode::Char('k')));
        assert!(state.editor.draft().is_empty(), "{}", state.editor.draft());
    }

    #[test]
    fn each_agent_keeps_its_own_draft_images_and_numbering() {
        let first_chat = Uuid::new_v4();
        let second_chat = Uuid::new_v4();
        let mut state = state_with_clipboard(Some(clipboard_image()), false);
        state.switch_editor_chat(Some(first_chat));
        state.key(control(KeyCode::Char('v')));

        state.switch_editor_chat(Some(second_chat));
        assert!(state.editor.draft().is_empty());
        state.key(control(KeyCode::Char('v')));
        assert_eq!(
            submitted_images(&mut state),
            ("[Image #1]".into(), vec![pasted_image(1)])
        );

        state.switch_editor_chat(Some(first_chat));
        assert_eq!(state.editor.draft(), "[Image #1]");
        assert_eq!(
            submitted_images(&mut state),
            ("[Image #1]".into(), vec![pasted_image(1)])
        );
        state.key(control(KeyCode::Char('v')));
        assert_eq!(state.editor.draft(), "[Image #2]");
    }

    #[test]
    fn input_history_keeps_image_placeholders_as_text_without_their_images() {
        let project = tempfile::tempdir().unwrap();
        let mut state = state_with_clipboard(Some(clipboard_image()), false);
        state.input_history = Some(InputHistory::in_memory());
        state.history_project = project.path().into();
        state.history_session = "image-chat".into();
        state.insert("Look at ");
        state.key(control(KeyCode::Char('v')));
        let (text, images) = submitted_images(&mut state);
        assert_eq!(images, [pasted_image(1)]);
        let console = console_with_state(state);
        console.record_prompt_history(&text).unwrap();

        let mut state = console.state.lock().unwrap();
        state.key(key(KeyCode::Up));
        assert_eq!(state.editor.draft(), "Look at [Image #1]");
        assert_eq!(
            submitted_images(&mut state),
            ("Look at [Image #1]".into(), Vec::new())
        );
    }

    #[test]
    fn a_resumed_chat_numbers_new_images_after_the_highest_one_it_already_sent() {
        let chat = Uuid::new_v4();
        let mut state = state_with_clipboard(Some(clipboard_image()), false);
        state.switch_editor_chat(Some(chat));
        let console = console_with_state(state);

        console
            .set_chat_images(Uuid::new_v4(), true, Some(9))
            .unwrap();
        console.set_chat_images(chat, true, Some(5)).unwrap();
        let mut state = console.state.lock().unwrap();
        state.key(control(KeyCode::Char('v')));

        assert_eq!(state.editor.draft(), "[Image #6]");
    }

    #[test]
    fn a_rewound_draft_gets_its_pasted_images_back_as_image_blocks() {
        let notes = saved_file_context("notes.md", "checkpoint notes");
        let console = console_with_state(state_with_clipboard(Some(clipboard_image()), false));
        console
            .restore_rewind_draft(
                "Compare [Image #3] with @notes.md",
                &[
                    InputContext::Image(pasted_image(3)),
                    notes.clone(),
                    InputContext::Image(pasted_image(7)),
                ],
            )
            .unwrap();

        let mut state = console.state.lock().unwrap();
        assert_eq!(state.restored_contexts, [notes]);
        set_cursor(&mut state, 0);
        state.insert("Now ");
        state.key(control(KeyCode::Char('v')));
        assert_eq!(
            state.editor.draft(),
            "Now [Image #4]Compare [Image #3] with @notes.md"
        );
        assert_eq!(
            submitted_images(&mut state).1,
            [pasted_image(4), pasted_image(3)]
        );
    }

    #[test]
    fn a_refused_prompt_returns_with_its_pasted_images() {
        let mut state = state_with_clipboard(Some(clipboard_image()), false);
        state.insert("Describe ");
        state.key(control(KeyCode::Char('v')));
        submitted_images(&mut state);

        state.restore_rejected_prompt(Vec::new());

        assert_eq!(
            submitted_images(&mut state),
            ("Describe [Image #1]".into(), vec![pasted_image(1)])
        );
    }

    #[test]
    fn an_unchanged_restored_prompt_retries_its_unknown_input_and_any_change_makes_it_new() {
        let mut state = state_with_clipboard(Some(clipboard_image()), false);
        state.insert("Describe ");
        state.key(control(KeyCode::Char('v')));
        state.paste(" with\nfirst\nsecond\nthird");
        let sent = submitted_images(&mut state);
        let retry = Uuid::new_v4();

        state.restore_unknown_prompt(Vec::new(), Some(retry));
        assert_eq!(
            submitted_images(&mut state),
            sent,
            "the restored draft sends the same text and images"
        );
        assert_eq!(state.submitted_retry_of.take(), Some(retry));

        state.restore_unknown_prompt(Vec::new(), Some(retry));
        state.insert("!");
        state.key(key(KeyCode::Backspace));
        submitted_images(&mut state);
        assert_eq!(
            state.submitted_retry_of.take(),
            None,
            "an edit, even one undone, makes the draft a new input"
        );

        state.restore_unknown_prompt(Vec::new(), Some(retry));
        state.switch_view_chat(Some(Uuid::new_v4()));
        state.switch_view_chat(None);
        submitted_images(&mut state);
        assert_eq!(
            state.submitted_retry_of.take(),
            None,
            "switching the input target ends the retry"
        );

        state.insert("typed meanwhile");
        let typed = Uuid::new_v4();
        state.restore_unknown_prompt(Vec::new(), Some(typed));
        assert_eq!(state.editor.draft(), "typed meanwhile");
        submitted_images(&mut state);
        assert_eq!(
            state.submitted_retry_of.take(),
            None,
            "a draft typed after sending is never taken for the retried input"
        );
    }

    fn image_file(directory: &Path, name: &str) -> String {
        let path = directory.join(name);
        std::fs::write(&path, png_bytes()).unwrap();
        path.to_str().unwrap().into()
    }

    #[test]
    fn a_dragged_image_path_becomes_an_image_block_in_place() {
        let directory = tempfile::tempdir().unwrap();
        let path = image_file(directory.path(), "Screen Shot (1).png");
        let escaped = path
            .replace(' ', "\\ ")
            .replace('(', "\\(")
            .replace(')', "\\)");
        let mut state = state_with_clipboard(None, false);
        state.insert("Explain ");

        state.paste(&format!("{escaped} "));

        assert_eq!(
            submitted_images(&mut state),
            ("Explain [Image #1] ".into(), vec![pasted_image(1)])
        );
    }

    #[test]
    fn quoted_file_url_and_relative_image_paths_are_attached() {
        let directory = tempfile::tempdir().unwrap();
        let path = image_file(directory.path(), "chart one.PNG");
        let mut state = state_with_clipboard(None, false);
        state.history_project = directory.path().into();

        state.paste(&format!("'{path}'"));
        state.paste(&format!(" \"{path}\""));
        state.paste(&format!(" file://{}", path.replace(' ', "%20")));
        state.paste(" chart\\ one.PNG");

        assert_eq!(
            submitted_images(&mut state),
            (
                "[Image #1] [Image #2] [Image #3] [Image #4]".into(),
                (1..=4).map(pasted_image).collect()
            )
        );
    }

    #[test]
    fn a_mixed_paste_keeps_its_text_in_order_and_still_folds_long_text() {
        let directory = tempfile::tempdir().unwrap();
        let first = image_file(directory.path(), "first.jpg");
        let second = image_file(directory.path(), "second.webp");
        let mut state = state_with_clipboard(None, false);

        state.paste(&format!(
            "Compare these\r\n{first} {second}\nnotes one\nnotes two\nnotes three\nnotes four"
        ));

        let expected =
            "Compare these\n[Image #1] [Image #2]\nnotes one\nnotes two\nnotes three\nnotes four";
        assert_eq!(state.editor.draft(), expected);
        assert_eq!(state.status, "Pasted text #3 folded");
        assert_eq!(
            submitted_images(&mut state),
            (expected.into(), vec![pasted_image(1), pasted_image(2)])
        );
    }

    #[test]
    fn missing_non_image_and_undecodable_paths_are_pasted_as_text() {
        let directory = tempfile::tempdir().unwrap();
        let notes = directory.path().join("notes.txt");
        std::fs::write(&notes, "plain notes").unwrap();
        let fake = directory.path().join("fake.png");
        std::fs::write(&fake, "not really a png").unwrap();
        let mut state = state_with_clipboard(None, false);
        let paste = format!(
            "{}\n{}\n{}",
            directory.path().join("missing.png").display(),
            notes.display(),
            fake.display()
        );

        state.paste(&paste);

        assert_eq!(state.editor.draft(), paste);
        assert!(
            state.status.starts_with("Cannot attach") && state.status.contains("fake.png"),
            "{}",
            state.status
        );
        assert_eq!(submitted_images(&mut state), (paste, Vec::new()));
    }

    #[test]
    fn an_empty_paste_on_macos_attaches_the_clipboard_image_or_does_nothing() {
        let mut state = state_with_clipboard(Some(clipboard_image()), false);
        state.empty_paste_reads_clipboard_image = true;
        state.paste("");
        assert_eq!(state.editor.draft(), "[Image #1]");

        let mut empty = state_with_clipboard(None, false);
        empty.empty_paste_reads_clipboard_image = true;
        empty.paste("");
        assert!(empty.editor.draft().is_empty());
        assert!(empty.status.is_empty(), "{}", empty.status);

        let mut elsewhere = state_with_clipboard(Some(clipboard_image()), false);
        elsewhere.empty_paste_reads_clipboard_image = false;
        elsewhere.paste("");
        assert!(elsewhere.editor.draft().is_empty());
    }

    #[test]
    fn a_text_only_model_gets_pasted_image_paths_as_text_with_an_explanation() {
        let directory = tempfile::tempdir().unwrap();
        let path = image_file(directory.path(), "chart.png");
        let mut state = state_with_clipboard(Some(clipboard_image()), false);
        state.accepts_images = false;
        state.empty_paste_reads_clipboard_image = true;

        state.paste(&path);
        assert_eq!(state.editor.draft(), path);
        assert_eq!(state.status, IMAGE_INPUT_UNSUPPORTED);

        state.status.clear();
        state.paste("");
        assert_eq!(state.editor.draft(), path);
        assert_eq!(state.status, IMAGE_INPUT_UNSUPPORTED);
        assert_eq!(submitted_images(&mut state), (path, Vec::new()));
    }

    #[test]
    fn shell_input_keeps_pasted_image_paths_as_text() {
        let directory = tempfile::tempdir().unwrap();
        let path = image_file(directory.path(), "chart.png");
        let mut state = state_with_clipboard(None, false);
        state.key(key(KeyCode::Char('!')));

        state.paste(&path);

        assert_eq!(state.editor.draft(), path);
    }

    fn scripted_external_editor(state: &mut State, content: &str) -> Arc<Mutex<Vec<String>>> {
        let opened_drafts = Arc::new(Mutex::new(Vec::new()));
        state.external_editor = ExternalEditorLauncher::Scripted {
            opened_drafts: Arc::clone(&opened_drafts),
            outcome: ExternalEditorOutcome::Updated {
                content: content.into(),
            },
        };
        opened_drafts
    }

    #[test]
    fn control_g_opens_the_external_editor_while_typing_and_applies_its_result() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        let opened_drafts = scripted_external_editor(&mut state, "edited in vim\nsecond line");
        state.insert("/mo");
        assert!(state.completion.is_open());

        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL))
                .is_none()
        );

        assert_eq!(opened_drafts.lock().unwrap().as_slice(), ["/mo"]);
        assert_eq!(state.editor.draft(), "edited in vim\nsecond line");
        assert_eq!(state.editor.cursor(), state.editor.draft().len());
        assert_eq!(state.status, "Draft updated by external editor");
        assert!(!state.completion.is_open());
    }

    #[test]
    fn control_g_is_ignored_where_the_prompt_is_not_editable() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        let opened_drafts = scripted_external_editor(&mut state, "replacement");
        let control_g = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);

        state.menu = Some(menu());
        state.menu_focused = true;
        state.key(control_g);
        state.menu = None;
        state.menu_focused = false;

        state.foreground_shell = true;
        state.key(control_g);
        state.foreground_shell = false;

        state.prompt_enabled = false;
        state.key(control_g);
        state.prompt_enabled = true;

        assert!(opened_drafts.lock().unwrap().is_empty());
        state.key(control_g);
        assert_eq!(opened_drafts.lock().unwrap().as_slice(), [""]);
        assert_eq!(state.editor.draft(), "replacement");
    }

    #[test]
    fn foreground_shell_background_controls_share_the_same_event() {
        let mut state = State::new(true, 80, 24);
        state.set_foreground_shell(true);

        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            Some(InputEvent::BackgroundForegroundShell)
        ));

        state.set_foreground_shell(true);
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert!(state.shell_background_chord_started_at.is_some());
        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            Some(InputEvent::BackgroundForegroundShell)
        ));
        assert!(state.shell_background_chord_started_at.is_none());
    }

    #[test]
    fn foreground_shell_background_chord_consumes_mismatches_and_expires() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.set_foreground_shell(true);

        state.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        #[cfg(unix)]
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL))
                .is_none()
        );
        #[cfg(not(unix))]
        assert!(state.key(key(KeyCode::Char('a'))).is_none());
        assert!(state.editor.draft().is_empty());
        assert!(state.shell_background_chord_started_at.is_none());

        state.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        state.shell_background_chord_started_at =
            Some(Instant::now() - SHELL_BACKGROUND_CHORD_WINDOW - Duration::from_millis(1));
        state.expire_shell_background_chord();
        assert!(state.shell_background_chord_started_at.is_none());
        assert!(state.key(key(KeyCode::Char('q'))).is_none());
        assert!(state.editor.draft().is_empty());
    }

    #[test]
    fn foreground_shell_background_chord_clears_on_terminal_and_surface_changes() {
        let mut state = State::new(true, 80, 24);
        state.set_foreground_shell(true);
        state.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        state.set_foreground_shell(false);
        assert!(state.shell_background_chord_started_at.is_none());

        state.set_foreground_shell(true);
        state.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        state.start_transcript();
        assert!(state.shell_background_chord_started_at.is_none());
        state.transcript = None;

        state.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        state.menu = Some(menu());
        state.menu_focused = true;
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert!(state.shell_background_chord_started_at.is_none());
        assert!(state.menu.is_some());
    }

    #[test]
    fn foreground_shell_ignores_paste_without_leaking_into_the_chat_draft() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.set_foreground_shell(true);
        state.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));

        state.paste("must stay out of the editor");

        assert!(state.editor.draft().is_empty());
        assert!(state.shell_background_chord_started_at.is_none());
    }

    #[test]
    fn modified_enter_and_control_j_insert_newlines_without_submitting_or_approving() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("第一行");
        for modified_enter in [
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        ] {
            assert!(state.key(modified_enter).is_none());
        }
        assert_eq!(state.editor.draft(), "第一行\n\n\n");
        state.menu = Some(menu());
        state.menu_focused = true;
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT))
                .is_none()
        );
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert!(state.menu.is_some());
        assert_eq!(state.editor.draft(), "第一行\n\n\n");
    }

    #[test]
    fn every_undo_spelling_of_control_underscore_reaches_the_editor() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        let undo_keys = [
            KeyEvent::new(KeyCode::Char('_'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('7'), KeyModifiers::CONTROL),
            KeyEvent::new(
                KeyCode::Char('-'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
        ];

        for undo in undo_keys {
            state.insert("撤销 draft");
            assert_eq!(state.editor.draft(), "撤销 draft");
            assert!(state.key(undo).is_none());
            assert!(
                state.editor.draft().is_empty(),
                "{undo:?} did not undo the draft"
            );
        }

        state.insert("keep this draft");
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert_eq!(state.editor.draft(), "keep this draft");
    }

    #[test]
    fn control_a_and_e_do_not_move_an_unfocused_editor() {
        let control = |character| KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL);
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("keep cursor here");
        let end = state.editor.cursor();

        state.prompt_enabled = false;
        state.key(control('a'));
        assert_eq!(state.editor.cursor(), end);

        state.prompt_enabled = true;
        state.key(control('a'));
        assert_eq!(state.editor.cursor(), 0);
        state.menu = Some(menu());
        state.menu_focused = true;
        state.key(control('e'));
        assert_eq!(state.editor.cursor(), 0);
    }

    #[test]
    fn input_uses_readline_emacs_keys() {
        let control = |character| KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL);
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("hello world");

        assert!(state.key(control('a')).is_none());
        assert_eq!(state.editor.cursor(), 0);
        assert!(state.key(control('f')).is_none());
        assert_eq!(state.editor.cursor(), 1);
        assert!(state.key(control('e')).is_none());
        assert_eq!(state.editor.cursor(), "hello world".len());
        assert!(state.key(control('b')).is_none());
        assert_eq!(state.editor.cursor(), "hello worl".len());

        assert!(state.key(control('h')).is_none());
        assert_eq!(state.editor.draft(), "hello word");
        assert!(state.key(control('7')).is_none());
        assert_eq!(state.editor.draft(), "hello world");

        state.key(control('e'));
        state.key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(state.editor.cursor(), "hello ".len());
        state.key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(state.editor.cursor(), 0);
        state.key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
        assert_eq!(state.editor.cursor(), "hello".len());
        state.key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(state.editor.cursor(), "hello world".len());

        state.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::CONTROL));
        assert_eq!(state.editor.draft(), "hello ");
        state.key(control('a'));
        state.key(KeyEvent::new(KeyCode::Delete, KeyModifiers::ALT));
        assert!(state.editor.draft().is_empty());
        state.key(control('y'));
        assert_eq!(state.editor.draft(), "hello ");
    }

    #[test]
    fn control_b_moves_left_in_a_shell_draft_and_backgrounds_only_a_running_shell() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.paste("!sleep 5");
        assert_eq!(state.shell_input.kind(), InputKind::Shell);
        let end = state.editor.cursor();

        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert_eq!(state.editor.cursor(), end - 1);
        assert_eq!(state.shell_input.kind(), InputKind::Shell);

        state.set_foreground_shell(true);
        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            Some(InputEvent::BackgroundForegroundShell)
        ));
    }

    #[test]
    fn control_c_clears_the_draft_before_interrupting_and_escape_is_distinct() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("草稿\n第二行");
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert!(state.editor.draft().is_empty());
        assert_eq!(state.editor.cursor(), 0);
        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(InputEvent::Interrupt)
        ));
        assert!(matches!(
            state.key(key(KeyCode::Esc)),
            Some(InputEvent::Escape)
        ));
        state.insert("保留");
        state.menu = Some(menu());
        assert!(matches!(
            state.key(key(KeyCode::Esc)),
            Some(InputEvent::Cancel { .. })
        ));
        assert_eq!(state.editor.draft(), "保留");
    }

    #[test]
    fn control_d_deletes_forward_with_a_draft_and_requires_a_timed_second_press_to_exit() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("abc");
        state.key(key(KeyCode::Left));

        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert_eq!(state.editor.draft(), "ab");
        assert_eq!(state.pending_exit_control(), None);

        state.edit(EditorCommand::Clear);
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert_eq!(state.pending_exit_control(), Some('D'));
        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            Some(InputEvent::Eof)
        ));
    }

    #[test]
    fn control_d_confirmation_expires_and_other_keys_cancel_it() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.exit_confirmation = Some(ExitConfirmation {
            control: 'D',
            requested_at: Instant::now() - EXIT_CONFIRMATION_WINDOW - Duration::from_millis(1),
        });

        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert_eq!(state.pending_exit_control(), Some('D'));
        state.key(key(KeyCode::Char('x')));
        assert_eq!(state.pending_exit_control(), None);

        assert!(!state.request_exit('C'));
        assert_eq!(state.pending_exit_control(), Some('C'));
        assert!(state.request_exit('C'));
        assert_eq!(state.pending_exit_control(), None);
    }

    #[test]
    fn double_escape_clears_a_draft_into_history_and_opens_rewind_when_empty() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("清空后可以召回的中文草稿");

        assert!(matches!(
            state.key(key(KeyCode::Esc)),
            Some(InputEvent::Escape)
        ));
        assert_eq!(
            state.pending_escape_action(),
            Some(EscapeAction::ClearDraft)
        );
        assert_eq!(state.editor.draft(), "清空后可以召回的中文草稿");
        assert!(state.key(key(KeyCode::Esc)).is_none());
        assert!(state.editor.draft().is_empty());
        state.key(key(KeyCode::Up));
        assert_eq!(state.editor.draft(), "清空后可以召回的中文草稿");

        state.edit(EditorCommand::Clear);
        assert!(matches!(
            state.key(key(KeyCode::Esc)),
            Some(InputEvent::Escape)
        ));
        assert_eq!(
            state.pending_escape_action(),
            Some(EscapeAction::OpenRewind)
        );
        assert!(matches!(
            state.key(key(KeyCode::Esc)),
            Some(InputEvent::OpenRewind)
        ));
    }

    #[test]
    fn double_escape_expires_and_local_focus_does_not_arm_it() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.escape_confirmation = Some(EscapeConfirmation {
            action: EscapeAction::OpenRewind,
            requested_at: Instant::now() - ESCAPE_CONFIRMATION_WINDOW - Duration::from_millis(1),
        });

        assert!(matches!(
            state.key(key(KeyCode::Esc)),
            Some(InputEvent::Escape)
        ));
        assert_eq!(
            state.pending_escape_action(),
            Some(EscapeAction::OpenRewind)
        );
        state.key(key(KeyCode::Char('x')));
        assert_eq!(state.pending_escape_action(), None);

        state.edit(EditorCommand::Clear);
        state.insert("/mo");
        assert!(state.completion.is_open());
        state.key(key(KeyCode::Esc));
        assert_eq!(state.pending_escape_action(), None);
        assert!(!state.completion.is_open());
    }

    #[cfg(unix)]
    #[test]
    fn control_z_is_global_across_transcript_and_menu_focus() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.start_transcript();
        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL)),
            Some(InputEvent::Suspend)
        ));
        assert!(state.transcript.is_some());

        state.transcript = None;
        state.menu = Some(menu());
        state.menu_focused = true;
        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL)),
            Some(InputEvent::Suspend)
        ));
        assert!(state.menu.is_some());
    }

    #[test]
    fn control_u_deletes_to_the_current_line_start_and_keeps_unicode_suffixes() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("之前\n删除保留");
        state.key(key(KeyCode::Left));
        state.key(key(KeyCode::Left));
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert_eq!(state.editor.draft(), "之前\n保留");
        assert_eq!(state.editor.cursor(), "之前\n".len());
        let input = InputView::frame(state.editor.draft(), state.editor.cursor(), 40, 8);
        assert_eq!(input.lines, ["> 之前", "  保留"]);
        assert_eq!((input.cursor_row, input.cursor_column), (1, 2));
    }

    #[test]
    fn context_paging_exposes_the_end_of_a_long_authorization_command() {
        let mut state = State::new(true, 30, 12);
        state.prompt_enabled = true;
        let mut long_menu = menu();
        long_menu.context = vec![format!("{}\nEND_OF_COMMAND", "long-path/".repeat(50))];
        state.menu = Some(long_menu);
        state.menu_focused = true;
        assert!(!state.frame().0.join("\n").contains("END_OF_COMMAND"));
        for _ in 0..100 {
            state.key(key(KeyCode::PageDown));
            state.frame();
        }
        let lines = state.frame().0;
        assert!(
            lines
                .iter()
                .map(|line| line.trim_start())
                .collect::<String>()
                .contains("END_OF_COMMAND")
        );
        assert!(lines.len() <= state.height);
        assert!(lines.iter().all(|line| display_width(line) <= state.width));
    }

    #[test]
    fn a_long_permission_rule_wraps_without_exceeding_the_menu_frame() {
        let mut state = State::new(true, 32, 12);
        state.prompt_enabled = true;
        let mut long_menu = menu();
        long_menu.options[1] = format!(
            "Yes, and don't ask again for: Bash({})",
            "very-long-argument ".repeat(30)
        );
        long_menu.selected = 1;
        state.menu = Some(long_menu);
        state.menu_focused = true;

        let lines = state.frame().0;
        assert!(lines.len() <= state.height);
        assert!(lines.iter().all(|line| display_width(line) <= state.width));
        assert!(lines.join("\n").contains("Yes, and don't ask"));
    }

    #[test]
    fn wide_text_wraps_and_keeps_the_cursor_on_its_character() {
        assert_eq!(wrap_text("ab中文cd", 5), vec!["ab中", "文cd"]);
        let input = InputView::frame("ab中文cd", "ab中".len(), 5, 8);
        assert_eq!(input.lines, ["> ab", "  中", "  文c", "  d"]);
        assert_eq!((input.cursor_row, input.cursor_column), (1, 4));
        let mut state = State::new(true, 8, 5);
        state.prompt_enabled = true;
        state.insert(&"中文".repeat(40));
        let (lines, row, column) = state.frame();
        assert_eq!(lines.len(), 3);
        assert_eq!(row, lines.len() - 2);
        assert!(column <= state.width);
        set_cursor(&mut state, 0);
        let (lines, row, column) = state.frame();
        assert!(lines[1].starts_with("❯\u{a0}"));
        assert_eq!((row, column), (1, 2));
    }

    #[test]
    fn history_restores_the_unsubmitted_draft() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("first");
        state.key(key(KeyCode::Enter));
        state.insert("second");
        state.key(key(KeyCode::Enter));
        state.insert("unfinished 中文");
        state.key(key(KeyCode::Up));
        assert_eq!(state.editor.draft(), "second");
        state.key(key(KeyCode::Up));
        assert_eq!(state.editor.draft(), "first");
        state.key(key(KeyCode::Down));
        state.key(key(KeyCode::Down));
        assert_eq!(state.editor.draft(), "unfinished 中文");
    }

    #[test]
    fn footer_row_reserves_geometry_and_truncates_on_narrow_screens() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state
            .status_area
            .update("Auto".into(), "fixture".into(), None, vec![]);
        super::layout_tests::set_status_family(&mut state.status_area);
        state.footer.update(state.status_area.available_panels());
        let (lines, input_row, _) = state.frame();
        assert!(lines[input_row].starts_with('❯'));
        assert!(lines[input_row + 1].contains("Subagents 19"));
        state.key(key(KeyCode::Down));
        assert_eq!(state.footer.selected(), Some(FooterItemId::Agents));
        state.height = MIN_FOOTER_ITEMS_TERMINAL_ROWS - 1;
        let lines = state.frame().0;
        assert!(lines.iter().all(|line| !line.contains("Subagents 19")));
        assert_eq!(state.footer.selected(), None);
        state.height = 24;
        state.width = 23;
        let lines = state.frame().0;
        assert!(lines.iter().any(|line| line.contains("Subagents 19")));
        assert!(lines.iter().all(|line| display_width(line) <= state.width));
        assert_eq!(state.footer.selected(), None);
    }

    #[test]
    fn down_enters_footer_only_after_visual_rows_and_history_are_exhausted() {
        let mut state = State::new(true, 40, 24);
        state.prompt_enabled = true;
        state.footer.update([FooterItemId::Agents]);
        state.insert("first line\nlast line");
        set_cursor(&mut state, 0);

        state.key(key(KeyCode::Down));
        assert_eq!(state.footer.selected(), None);
        assert!(state.editor.cursor() > 0);
        state.key(key(KeyCode::Down));
        assert_eq!(state.footer.selected(), Some(FooterItemId::Agents));

        state.apply_footer_action(FooterAction::Clear);
        state.replace_draft(String::new(), 0);
        state.insert("history item");
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Prompt { .. })
        ));
        state.insert("original draft");
        state.key(key(KeyCode::Up));
        assert!(state.editor.is_browsing_history());
        assert_eq!(state.editor.draft(), "history item");

        state.key(key(KeyCode::Down));
        assert_eq!(state.editor.draft(), "original draft");
        assert!(!state.editor.is_browsing_history());
        assert_eq!(state.footer.selected(), None);
        state.key(key(KeyCode::Down));
        assert_eq!(state.footer.selected(), Some(FooterItemId::Agents));
    }

    #[test]
    fn folded_paste_uses_its_display_projection_for_footer_entry() {
        let mut state = State::new(true, 100, 24);
        state.prompt_enabled = true;
        state.footer.update([FooterItemId::Agents]);
        state.paste("first\nsecond\nthird\nfourth");
        set_cursor(&mut state, 0);

        let (display_draft, display_cursor) = state.input_display();
        assert!(display_draft.contains("[Pasted text #1 +3 lines]"));
        assert_eq!(display_cursor, 0);
        assert!(state.can_enter_footer());

        state.key(key(KeyCode::Down));
        assert_eq!(state.footer.selected(), Some(FooterItemId::Agents));
        assert_eq!(state.editor.cursor(), 0);
    }

    #[test]
    fn footer_escape_and_printable_keys_return_to_the_editor() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.footer.update([FooterItemId::Agents]);
        state.insert("abc");

        state.key(key(KeyCode::Down));
        state.key(key(KeyCode::Esc));
        assert_eq!(state.footer.selected(), None);
        assert_eq!(state.editor.draft(), "abc");

        state.key(key(KeyCode::Down));
        state.key(key(KeyCode::Char('i')));
        assert_eq!(state.footer.selected(), None);
        assert_eq!(state.editor.draft(), "abci");

        state.key(key(KeyCode::Down));
        state.key(KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::SHIFT));
        assert_eq!(state.footer.selected(), None);
        assert_eq!(state.editor.draft(), "abciZ");
    }

    #[test]
    fn closing_inline_task_details_returns_to_the_same_task_in_the_footer_list() {
        let chat = Uuid::new_v4();
        let mut state = State::new(true, 140, 24);
        state.prompt_enabled = true;
        state.chat = Some(chat);
        state.insert("keep this draft");
        let console = InteractiveConsoleFixture::new(state);
        let snapshot = task_snapshot(chat, 6);
        let mut snapshot = snapshot;
        for task in &mut snapshot.tasks {
            task.was_background = true;
        }
        console.update_shell_ticks(&snapshot).unwrap();
        let task = snapshot.tasks[4].id;
        let panel = {
            let mut state = console.state.lock().unwrap();
            state
                .tick_list
                .select(&crate::tick_stream::InstanceRef::shell(task).to_string());
            state.key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
            state
                .tick_list
                .select(&crate::tick_stream::InstanceRef::shell(task).to_string());
            let Some(InputEvent::TickAction { action, .. }) = state.key(key(KeyCode::Enter)) else {
                panic!("Shell did not open the Tick panel");
            };
            assert_eq!(
                action.instance,
                crate::tick_stream::InstanceRef::shell(task).to_string()
            );
            state.tick_list.detail.as_ref().unwrap().id()
        };
        console
            .update_tick_detail::<crate::terminal::tick_shell::TickShell, _>(
                chat,
                panel,
                |detail| detail.load(snapshot, None),
            )
            .unwrap();
        let mut state = console.state.lock().unwrap();
        state.key(key(KeyCode::Esc));
        assert!(state.task_surface.is_none());
        assert!(state.tick_list.detail.is_none());
        assert_eq!(
            state.tick_list.selected().unwrap().id,
            crate::tick_stream::InstanceRef::shell(task).to_string()
        );
        assert_eq!(state.editor.draft(), "keep this draft");
    }

    #[test]
    fn agents_panel_keeps_its_visibility_when_focus_returns_to_the_editor() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("preserved draft");
        let cursor = state.editor.cursor();
        state
            .status_area
            .update("Auto".into(), "model".into(), None, vec![]);
        super::layout_tests::set_status_family(&mut state.status_area);
        state.footer.update(state.status_area.available_panels());
        state.key(key(KeyCode::Down));
        state.key(key(KeyCode::Char(' ')));
        assert!(!state.status_area.is_open());
        state.key(key(KeyCode::Char(' ')));
        state.key(key(KeyCode::Esc));
        assert!(state.status_area.is_open());
        assert!(state.footer.selected().is_none());
        state.frame();
        let target = state.footer_bar_target.as_ref().unwrap();
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: target.categories[0].0.start as u16,
            row: target.row as u16,
            modifiers: KeyModifiers::NONE,
        };
        assert!(state.pointer(event, &mut Vec::new()));
        assert!(!state.status_area.is_open());
        state.key(key(KeyCode::Esc));
        assert!(!state.status_area.is_open());
        assert!(state.footer.selected().is_none());
        assert_eq!(state.editor.draft(), "preserved draft");
        assert_eq!(state.editor.cursor(), cursor);
    }

    #[test]
    fn task_center_opens_the_task_selected_in_the_footer() {
        let chat_id = Uuid::new_v4();
        let mut surface = TaskSurface::open(task_snapshot(chat_id, 2), chat_id);
        surface.open_detail(
            Uuid::from_u128(2),
            TaskViewport {
                width: 80,
                height: 20,
            },
        );
        assert_eq!(
            surface.focus(),
            TaskSurfaceFocus::Detail {
                task_id: Uuid::from_u128(2)
            }
        );
    }

    #[test]
    fn shift_arrows_open_active_background_tasks_from_the_chat() {
        for direction in [KeyCode::Up, KeyCode::Down] {
            let chat = Uuid::new_v4();
            let mut state = State::new(true, 80, 24);
            state.chat = Some(chat);
            state.prompt_enabled = true;
            state.insert("draft");
            let console = console_with_state(state);
            let mut snapshot = task_snapshot(chat, 1);
            snapshot.tasks[0].was_background = true;
            console.update_shell_ticks(&snapshot).unwrap();
            let mut state = console.state.lock().unwrap();
            assert!(matches!(
                state.key(KeyEvent::new(direction, KeyModifiers::SHIFT)),
                Some(InputEvent::TickAction { action, .. }) if action.key == 'r'
            ));
            assert!(state.tick_list.focused);
            assert!(state.tick_list.overlay);
            assert_eq!(state.tick_list.selected().unwrap().group, TickGroup::Shells);
            assert_eq!(state.editor.draft(), "draft");
        }
    }

    #[test]
    fn shift_arrows_do_not_open_hidden_tasks_over_a_menu() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.menu = Some(menu());
        state.menu_focused = true;
        state
            .status_area
            .update("Auto".into(), "model".into(), None, vec![]);
        state.footer.update([FooterItemId::Agents]);

        assert!(!matches!(
            state.key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT)),
            Some(InputEvent::OpenAgent(_))
        ));
    }

    #[test]
    fn task_detail_stays_below_the_conversation_at_the_bottom() {
        let chat_id = Uuid::new_v4();
        let mut state = State::new(true, 80, 24);
        state.chat = Some(chat_id);
        state.scrollback.append("conversation above task\n");
        let mut surface = TaskSurface::open(task_snapshot(chat_id, 1), chat_id);
        surface.open_detail(
            Uuid::from_u128(1),
            TaskViewport {
                width: 80,
                height: 10,
            },
        );
        state.task_surface = Some(surface);

        state.draw(&mut Vec::new()).unwrap();

        assert_eq!(state.rendered_screen.len(), 24);
        let conversation_row = state
            .rendered_screen
            .iter()
            .position(|line| line.contains("conversation above task"))
            .unwrap();
        let detail_row = state
            .rendered_screen
            .iter()
            .position(|line| line.trim() == "Shell task")
            .unwrap();
        assert!(
            state
                .rendered_screen
                .iter()
                .any(|line| line.contains("printf task-0"))
        );
        assert!(matches!(
            state.task_surface.as_ref().unwrap().focus(),
            TaskSurfaceFocus::Detail { task_id } if task_id == Uuid::from_u128(1)
        ));
        assert!(detail_row > conversation_row);
        assert!(detail_row >= 12);
        assert!(
            state.rendered_screen[23].contains("Esc/Enter/Space close"),
            "{:?}",
            state.rendered_screen[23]
        );
    }

    #[test]
    fn task_detail_enter_and_space_close_to_the_chat() {
        let chat_id = Uuid::new_v4();
        for close_key in [KeyCode::Enter, KeyCode::Char(' ')] {
            let mut state = State::new(true, 80, 24);
            state.chat = Some(chat_id);
            let mut surface = TaskSurface::open(task_snapshot(chat_id, 1), chat_id);
            surface.open_detail(
                Uuid::from_u128(1),
                TaskViewport {
                    width: 80,
                    height: 10,
                },
            );
            state.task_surface = Some(surface);

            assert!(state.key(key(close_key)).is_none());
            assert!(state.task_surface.is_none());
        }
    }

    #[test]
    fn paste_leaves_footer_focus_and_preserves_the_original_text() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.footer.update([FooterItemId::Agents]);
        state.insert("before");
        state.key(key(KeyCode::Down));
        assert_eq!(state.footer.selected(), Some(FooterItemId::Agents));

        state.paste("\n\t粘贴");

        assert_eq!(state.footer.selected(), None);
        assert_eq!(state.editor.draft(), "before\n\t粘贴");
    }

    #[test]
    fn candidate_lists_clear_footer_focus_before_owning_navigation() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.footer.update([FooterItemId::Agents]);
        state.insert("/mo");
        assert!(state.completion.is_open());
        state.footer.apply(FooterAction::Down);

        state.key(key(KeyCode::Down));

        assert_eq!(state.footer.selected(), None);
        assert!(state.completion.is_open());
    }

    #[test]
    fn pending_footer_open_blocks_repeat_input_until_escape_or_finish() {
        let chat = Uuid::new_v4();
        let mut state = State::new(true, 80, 24);
        state.chat = Some(chat);
        state.prompt_enabled = true;
        state.insert("preserved draft");
        let console = console_with_state(state);
        let mut snapshot = task_snapshot(chat, 1);
        snapshot.tasks[0].was_background = true;
        console.update_shell_ticks(&snapshot).unwrap();
        let mut state = console.state.lock().unwrap();
        state.key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
        assert!(state.key(key(KeyCode::Char('j'))).is_none());
        assert_eq!(state.editor.draft(), "preserved draft");
        state.key(key(KeyCode::Esc));
        assert!(!state.tick_list.focused);
        state.key(key(KeyCode::Char('x')));
        assert_eq!(state.editor.draft(), "preserved draftx");
    }

    #[test]
    fn blocking_and_exclusive_surfaces_clear_footer_focus_without_key_leaks() {
        let chat_id = Uuid::new_v4();
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.chat = Some(chat_id);
        state.footer.update([FooterItemId::Agents]);
        state.insert("preserved draft");
        state.key(key(KeyCode::Down));
        assert!(state.footer.selected().is_some());

        let pending = menu();
        let pending_id = pending.id;
        state.menu = Some(pending);
        state.menu_focused = false;
        state.clear_footer_selection_if_hidden();
        assert_eq!(state.footer.selected(), None);
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Choice { menu_id, index: 0, .. }) if menu_id == pending_id
        ));
        assert_eq!(state.menu.as_ref().unwrap().id, pending_id);

        assert_eq!(state.editor.draft(), "preserved draft");
        state.mark_menu_submission_finished(pending_id);
        state.release_finished_submissions();
        state.task_surface = Some(TaskSurface::open(task_snapshot(chat_id, 2), chat_id));
        state.footer.apply(FooterAction::Down);
        let event = state.key(key(KeyCode::Enter));
        assert!(matches!(event, None | Some(InputEvent::TaskSurface(_))));
        assert_eq!(state.footer.selected(), None);
        assert!(matches!(
            state.task_surface.as_ref().unwrap().focus(),
            TaskSurfaceFocus::Detail { .. }
        ));
        state.hide_task_surface();

        state.footer.apply(FooterAction::Down);
        state.set_foreground_shell(true);
        assert_eq!(state.footer.selected(), None);
        assert!(state.key(key(KeyCode::Down)).is_none());
        assert_eq!(state.footer.selected(), None);
    }

    #[test]
    fn changing_agent_restores_its_own_footer_and_focus() {
        let first_chat = Uuid::new_v4();
        let second_chat = Uuid::new_v4();
        let mut state = State::new(false, 80, 24);
        state.chat = Some(first_chat);
        state.footer.update([FooterItemId::Agents]);
        state.footer.apply(FooterAction::Down);
        let console = console_with_state(state);

        console.select_chat(Some(second_chat)).unwrap();
        let state = console.state.lock().unwrap();
        assert_eq!(state.footer.selected(), None);
        assert_eq!(state.footer.frame().items.len(), 0);
        drop(state);
        console.select_chat(Some(first_chat)).unwrap();
        let state = console.state.lock().unwrap();
        assert_eq!(state.footer.frame().items.len(), 1);
        assert_eq!(state.footer.selected(), Some(FooterItemId::Agents));
    }

    #[test]
    fn footer_stays_at_the_bottom_after_menu_logs_and_resize() {
        let mut state = State::new(true, 140, 40);
        state.prompt_enabled = true;
        state.status = "DeepSeek · Messages".into();
        let mut output = Vec::new();
        state.draw(&mut output).unwrap();
        assert!(String::from_utf8(output).unwrap().contains("\x1b[38;3H"));
        for index in 0..50 {
            state.scrollback.append(&format!("tool log {index}\n"));
        }
        state.menu = Some(menu());
        state.menu_focused = false;
        state.width = 79;
        state.height = 18;
        let mut output = Vec::new();
        state.draw(&mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(state.frame().0.len(), 12);
        assert_eq!(
            state.rendered_screen[6 + input_dialog::INPUT_DIALOG_RULE_ROWS],
            "Run this command?"
        );
        assert!(output.contains("tool log 49"));
        assert!(
            !state
                .rendered_screen
                .iter()
                .any(|line| line.contains("DeepSeek · Messages"))
        );
        state.menu = None;
        state.width = 139;
        state.height = 40;
        let mut output = Vec::new();
        state.draw(&mut output).unwrap();
        assert!(String::from_utf8(output).unwrap().contains("\x1b[38;3H"));
    }

    #[test]
    fn scrolling_does_not_change_the_configured_status_line() {
        let mut state = State::new(true, 180, 24);
        state.prompt_enabled = true;
        state.status_area.update(
            "Auto".into(),
            "deepseek-flash".into(),
            Some((12_500, 1_000_000)),
            vec![],
        );
        state.scrollback.append(
            &(0..100)
                .map(|index| format!("ordinary-{index}\n"))
                .collect::<String>(),
        );
        state.draw(&mut Vec::new()).unwrap();
        state.key(key(KeyCode::PageUp));
        state.draw(&mut Vec::new()).unwrap();

        let status = &state.rendered_screen[state.height - 1];
        assert_eq!(status.matches("⏵⏵ Auto").count(), 1);
        assert_eq!(status.matches("deepseek-flash").count(), 1);
        assert_eq!(status.matches("Context 1% used").count(), 1);
        assert!(!status.contains("Scrolled up"));
        assert!(!status.contains("Ctrl+End: latest"));
    }

    #[test]
    fn tiny_terminals_block_invisible_menu_choices_and_restore_the_reviewed_selection() {
        for height in 1..=6 {
            let mut state = State::new(true, 80, height);
            state.prompt_enabled = true;
            state.insert("未提交\n\t保留缩进");
            let draft = state.editor.draft().to_owned();
            let mut approval = menu();
            approval.selected = 1;
            let id = approval.id;
            state.menu = Some(approval);
            state.menu_focused = true;
            assert_eq!(state.height, usize::from(height));
            let (frame, row, _) = state.frame();
            assert!(frame.len() <= usize::from(height));
            assert!(row < frame.len());
            assert!(frame.iter().any(|line| line.contains("Resize")));
            assert!(!frame.iter().any(|line| line.contains("Allow once")));

            let mut output = Vec::new();
            state.draw(&mut output).unwrap();
            state.scrollback.append("retained output while small\n");
            let output = String::from_utf8(output).unwrap();
            assert!(!output.contains("\x1b[1;1r"));
            for escape in output.split("\x1b[").skip(1) {
                if let Some((coordinates, _)) = escape.split_once('H')
                    && coordinates
                        .chars()
                        .all(|character| character.is_ascii_digit() || character == ';')
                {
                    let rendered_row: u16 = coordinates.split(';').next().unwrap().parse().unwrap();
                    assert!((1..=height).contains(&rendered_row));
                }
            }
            for code in [
                KeyCode::Enter,
                KeyCode::Char('y'),
                KeyCode::Char('Y'),
                KeyCode::Char('n'),
                KeyCode::Char('N'),
                KeyCode::Char('1'),
                KeyCode::Char('2'),
                KeyCode::Up,
                KeyCode::Down,
            ] {
                assert!(state.key(key(code)).is_none());
            }
            assert_eq!(state.menu.as_ref().unwrap().id, id);
            assert_eq!(state.menu.as_ref().unwrap().selected, 1);
            assert_eq!(state.editor.draft(), draft);

            state.height = 24;
            let mut output = Vec::new();
            state.draw(&mut output).unwrap();
            let output = String::from_utf8(output).unwrap();
            assert!(output.contains("Allow for this session"));
            assert!(output.contains("retained output while small"));
            assert_eq!(state.editor.draft(), draft);
            assert!(
                matches!(state.key(key(KeyCode::Enter)), Some(InputEvent::Choice { menu_id, index: 1, .. }) if menu_id == id)
            );
            state.mark_menu_submission_finished(id);
            assert!(state.clear_finished_menu_submission());

            state.menu = Some(menu());
            state.menu_focused = true;
            state.height = usize::from(height);
            assert!(matches!(
                state.key(key(KeyCode::Esc)),
                Some(InputEvent::Cancel { .. })
            ));
            assert_eq!(state.editor.draft(), draft);
            let (frame, row, _) = state.frame();
            assert!(frame.len() <= usize::from(height));
            assert!(row < frame.len());
        }
    }

    #[test]
    fn transcript_captures_keys_immediately_and_preserves_permission_focus_and_draft() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("中文\nkeep this draft");
        set_cursor(&mut state, "中文".len());
        let cursor = state.editor.cursor();
        let menu = menu();
        let menu_id = menu.id;
        state.menu = Some(menu);
        state.menu_focused = true;
        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Some(InputEvent::OpenTranscript { .. })
        ));
        for code in [KeyCode::Enter, KeyCode::Char('y'), KeyCode::Char('1')] {
            assert!(state.key(key(code)).is_none());
        }
        state.key(key(KeyCode::Char('/')));
        state.key(key(KeyCode::Char('n')));
        assert!(state.key(key(KeyCode::Esc)).is_none());
        assert!(state.transcript.is_some());
        assert!(state.key(key(KeyCode::Esc)).is_none());
        assert!(state.transcript.is_none());
        assert_eq!(state.menu.as_ref().unwrap().id, menu_id);
        assert!(state.menu_focused);
        assert_eq!(state.editor.draft(), "中文\nkeep this draft");
        assert_eq!(state.editor.cursor(), cursor);
        assert!(
            matches!(state.key(key(KeyCode::Enter)), Some(InputEvent::Choice { menu_id: id, index: 0, .. }) if id == menu_id)
        );
    }

    #[test]
    fn paused_scrolling_does_not_replace_the_input_or_resize_notice_in_tiny_windows() {
        for height in [1, 2] {
            let mut state = State::new(true, 80, height);
            state.prompt_enabled = true;
            state.insert("preserved input");
            state.scrollback.set_auto_scroll(false);
            if height == 2 {
                state.menu = Some(menu());
            }
            let mut output = Vec::new();
            state.draw(&mut output).unwrap();
            let output = String::from_utf8(output).unwrap();
            if height == 1 {
                assert!(output.contains("❯\u{a0}preserved input"), "{output}");
            }
            assert_eq!(state.editor.draft(), "preserved input");
            if height == 2 {
                assert!(output.contains("Resize terminal"), "{output}");
            }
            assert!(!output.contains("Auto-scroll off"), "{output}");
        }
    }

    #[test]
    fn only_task_submissions_resume_following_and_auto_scroll_off_stays_off() {
        let mut state = State::new(true, 80, 24);
        state.scrollback.append(
            &(0..100)
                .map(|index| format!("ordinary-{index}\n"))
                .collect::<String>(),
        );
        state.draw(&mut Vec::new()).unwrap();
        state.key(key(KeyCode::PageUp));
        let before = state.scrollback.frame(state.width, 21).lines;
        for input in ["", " \t ", "/settings", "/model"] {
            state.record_input(input);
            let view = state.scrollback.frame(state.width, 21);
            assert_eq!(view.lines, before);
            assert!(!view.following);
            assert_eq!(view.unread, 0);
        }
        state.record_input("another task");
        assert!(state.scrollback.frame(state.width, 21).following);
        state.scrollback.set_auto_scroll(false);
        state.scrollback.scroll(ScrollAction::First);
        let before = state.scrollback.frame(state.width, 21).lines;
        state.record_input("task without automatic following");
        let view = state.scrollback.frame(state.width, 21);
        assert_eq!(view.lines, before);
        assert!(!view.following);
        assert_eq!(view.unread, 1);
    }

    #[test]
    fn merged_echoes_join_only_their_own_adjacent_blocks_and_keep_every_text() {
        let mut state = State::new(true, 80, 40);
        let prompt = format!("{} ", state.display.glyph(Glyph::Prompt));
        let alpha = state.record_input("alpha deadbeef").unwrap();
        let bravo = state.record_input("bravo\nsecond line").unwrap();
        state.record_input("/status").unwrap();
        let charlie = state.record_input("charlie").unwrap();
        let delta = state.record_input("delta").unwrap();
        let mut elsewhere = State::new(true, 80, 40);
        let foreign = elsewhere.record_input("foreign").unwrap();
        assert!(
            !state.scrollback.join_inputs(bravo, charlie, &prompt),
            "a local command echo between them keeps two blocks"
        );
        assert!(!state.scrollback.join_inputs(charlie, alpha, &prompt));
        assert!(!state.scrollback.join_inputs(delta, foreign, &prompt));
        assert!(state.scrollback.join_inputs(alpha, bravo, &prompt));
        assert!(state.scrollback.join_inputs(charlie, delta, &prompt));
        let rows = state.scrollback.frame(80, 40).lines;
        let rows = rows
            .iter()
            .map(|row| row.trim_end())
            .skip_while(|row| row.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(
            rows[..8],
            [
                "❯ alpha deadbeef",
                "  bravo",
                "second line",
                "",
                "❯ /status",
                "",
                "❯ charlie",
                "  delta",
            ],
            "{rows:#?}"
        );

        state.clear_history();
        let after_reset = state.record_input("echo after reset").unwrap();
        assert!(
            !state.scrollback.join_inputs(alpha, after_reset, &prompt),
            "an echo of a replaced presentation never names a new one"
        );
    }

    #[test]
    fn joining_merged_echoes_keeps_a_reading_view_where_it_was() {
        let mut state = State::new(true, 80, 24);
        let prompt = format!("{} ", state.display.glyph(Glyph::Prompt));
        state.scrollback.set_auto_scroll(false);
        state.scrollback.append(
            &(0..100)
                .map(|index| format!("ordinary-{index}\n"))
                .collect::<String>(),
        );
        state.scrollback.scroll(ScrollAction::First);
        let alpha = state.record_input("alpha").unwrap();
        let bravo = state.record_input("bravo").unwrap();
        let before = state.scrollback.frame(state.width, 21);
        assert!(state.scrollback.join_inputs(alpha, bravo, &prompt));
        let after = state.scrollback.frame(state.width, 21);
        assert_eq!(after.lines, before.lines);
        assert!(!after.following);
    }

    #[test]
    fn rebuilt_summary_history_moves_an_auto_scroll_off_view_to_its_end() {
        let mut state = State::new(true, 80, 24);
        state.scrollback.set_auto_scroll(false);
        state.scrollback.append(
            &(0..100)
                .map(|index| format!("old-{index}\n"))
                .collect::<String>(),
        );
        state.scrollback.scroll(ScrollAction::First);

        state.clear_history();
        state.scrollback.append(
            &(0..100)
                .map(|index| format!("rebuilt-{index}\n"))
                .collect::<String>(),
        );
        state.scrollback.scroll(ScrollAction::Last);

        let frame = state.scrollback.frame(state.width, 21);
        assert_eq!(frame.lines.last().map(String::as_str), Some("rebuilt-99"));
        assert!(!frame.following);
        assert!(!state.scrollback.auto_scroll());
    }

    #[test]
    fn transcript_navigation_never_moves_the_normal_conversation() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.scrollback.append(
            &(0..100)
                .map(|index| format!("ordinary-{index}\n"))
                .collect::<String>(),
        );
        state.draw(&mut Vec::new()).unwrap();
        state.key(key(KeyCode::PageUp));
        let before = state.scrollback.frame(state.width, 21);
        assert!(!before.following);
        state.start_transcript();
        state.transcript.as_mut().unwrap().1.update(
            (0..200)
                .map(|index| format!("transcript-{index}"))
                .collect::<Vec<_>>()
                .into(),
        );
        state.draw(&mut Vec::new()).unwrap();
        for event in [
            KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL),
            key(KeyCode::PageDown),
            KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL),
            key(KeyCode::PageUp),
        ] {
            let prior = state
                .transcript
                .as_mut()
                .unwrap()
                .1
                .frame(state.width, state.height)
                .lines;
            assert!(state.key(event).is_none());
            let changed = state
                .transcript
                .as_mut()
                .unwrap()
                .1
                .frame(state.width, state.height)
                .lines;
            assert_ne!(prior, changed);
            assert_eq!(state.scrollback.frame(state.width, 21).lines, before.lines);
        }
        state.key(key(KeyCode::Esc));
        state.key(key(KeyCode::Char('中')));
        state.draw(&mut Vec::new()).unwrap();
        assert_eq!(state.scrollback.frame(state.width, 21).lines, before.lines);
        assert_eq!(state.editor.draft(), "中");
    }

    #[test]
    fn menu_paging_and_chat_paging_follow_the_keyboard_focus() {
        let mut state = State::new(true, 80, 24);
        state.scrollback.append(
            &(0..100)
                .map(|index| format!("ordinary-{index}\n"))
                .collect::<String>(),
        );
        let mut review = menu();
        review.context = (0..40).map(|index| format!("review-{index}")).collect();
        state.menu = Some(review);
        state.menu_focused = true;
        state.draw(&mut Vec::new()).unwrap();
        let body_height = state.height - state.frame().0.len();
        let before = state.scrollback.frame(state.width, body_height).lines;
        state.key(key(KeyCode::PageDown));
        state.draw(&mut Vec::new()).unwrap();
        assert_eq!(state.context_offset, 1);
        assert_eq!(
            state.scrollback.frame(state.width, body_height).lines,
            before
        );
        let menu_id = state.menu.as_ref().unwrap().id;
        state.clear_menu(menu_id);
        state.key(key(KeyCode::PageUp));
        state.draw(&mut Vec::new()).unwrap();
        assert_eq!(state.context_offset, 1);
        assert_ne!(
            state.scrollback.frame(state.width, body_height).lines,
            before
        );
        assert!(state.menu.is_none());
    }

    #[test]
    fn details_captures_escape_before_its_snapshot_has_loaded() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("/details");
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::OpenTranscript { .. })
        ));
        assert!(state.key(key(KeyCode::Esc)).is_none());
        assert!(state.transcript.is_none());
        assert!(state.editor.draft().is_empty());
        state.insert("next instruction");
        assert!(
            matches!(state.key(key(KeyCode::Enter)), Some(InputEvent::Prompt { text, file_references, .. }) if text == "next instruction" && file_references.is_empty())
        );
    }

    #[test]
    fn a_late_transcript_snapshot_cannot_reopen_or_replace_a_newer_view() {
        let (console, _) = Console::start(false).unwrap();
        let old_id = Uuid::new_v4();
        let current_id = Uuid::new_v4();
        console.state.lock().unwrap().transcript = Some((
            current_id,
            Transcript::new(vec!["current history".into()].into()),
        ));
        console
            .update_transcript(old_id, vec!["stale history".into()].into())
            .unwrap();
        let mut state = console.state.lock().unwrap();
        let (_, transcript) = state.transcript.as_mut().unwrap();
        let frame = transcript.frame(80, 24);
        assert!(
            frame
                .lines
                .iter()
                .any(|line| line.contains("current history"))
        );
        assert!(
            !frame
                .lines
                .iter()
                .any(|line| line.contains("stale history"))
        );
        state.transcript = None;
        drop(state);
        console
            .update_transcript(current_id, vec!["late result".into()].into())
            .unwrap();
        assert!(console.transcript_id().unwrap().is_none());
    }

    #[test]
    fn cancel_restores_the_draft_and_shortcuts_emit_explicit_choices() {
        let mut state = State::new(true, 80, 24);
        state.insert("keep this draft");
        let menu = menu();
        let id = menu.id;
        state.menu = Some(menu);
        state.menu_focused = true;
        assert!(
            matches!(state.key(key(KeyCode::Esc)), Some(InputEvent::Cancel { menu_id }) if menu_id == id)
        );
        assert_eq!(state.editor.draft(), "keep this draft");
        state.mark_menu_submission_finished(id);
        assert!(state.clear_finished_menu_submission());
        let menu = super::tests::menu();
        let id = menu.id;
        state.menu = Some(menu);
        state.menu_focused = true;
        assert!(matches!(
            state.key(key(KeyCode::Char('y'))),
            Some(InputEvent::Choice { index: 0, .. })
        ));
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL))
                .is_none()
        );
        state.mark_menu_submission_finished(id);
        assert!(state.clear_finished_menu_submission());
        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Some(InputEvent::OpenTranscript { .. })
        ));
    }

    fn queued_input(kind: QueuedInputKind, text: &str, sequence: u64) -> QueuedInput {
        QueuedInput {
            id: Uuid::new_v4(),
            chat_id: Uuid::new_v4(),
            loop_id: Some(Uuid::new_v4()),
            kind,
            text: text.into(),
            contexts: Vec::new(),
            queued_sequence: sequence,
            armed: true,
        }
    }

    #[test]
    fn queue_summary_and_previews_keep_the_captured_types_without_owner_hashes() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        let message = queued_input(QueuedInputKind::Message, "check deadbeef exactly", 1);
        let shell = queued_input(QueuedInputKind::Shell, "printf exact-shell", 2);
        let hashes = [
            message.chat_id.to_string()[..8].to_owned(),
            message.loop_id.unwrap().to_string()[..8].to_owned(),
        ];
        state.queued_inputs = vec![message, shell];
        let lines = state.frame().0;
        let divider = lines.iter().position(|line| line.starts_with('─')).unwrap();
        let message_row = lines
            .iter()
            .position(|line| line.contains("message") && line.contains("check deadbeef exactly"))
            .unwrap();
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Messages to be submitted (2)"))
        );
        assert!(lines.iter().any(|line| line.contains("printf exact-shell")));
        assert!(message_row < divider);
        assert_eq!(state.queued_inputs.len(), 2);
        assert!(
            hashes.iter().all(|hash| !lines[message_row].contains(hash)),
            "the queue belongs to the shown Agent; its row names no generated owner: {}",
            lines[message_row]
        );

        let mut new_goal = queued_input(QueuedInputKind::Message, "start fresh", 3);
        new_goal.loop_id = None;
        let chat = new_goal.chat_id.to_string()[..8].to_owned();
        state.queued_inputs = vec![new_goal];
        state.help_open = true;
        let help = state.frame().0;
        state.help_open = false;
        let lines = state.frame().0;
        let row = lines
            .iter()
            .chain(&help)
            .find(|line| line.contains("start fresh"))
            .unwrap();
        assert!(row.contains("new Loop") && !row.contains(&chat), "{row}");
    }

    #[test]
    fn a_task_of_another_agent_is_marked_without_its_chat_hash() {
        let view_chat = Uuid::new_v4();
        let other_chat = Uuid::new_v4();
        let mut snapshot = task_snapshot(view_chat, 2);
        snapshot.tasks[1].chat_id = other_chat;
        snapshot.tasks[1].command = "git show deadbeef".into();
        let mut state = State::new(true, 100, 24);
        state.prompt_enabled = true;
        state.task_surface = Some(TaskSurface::open(snapshot, view_chat));
        state.draw(&mut std::io::sink()).unwrap();
        let lines = state.rendered_screen.clone();
        let row = lines
            .iter()
            .find(|line| line.contains("git show deadbeef"))
            .unwrap_or_else(|| panic!("the other Agent's task is listed: {lines:#?}"));
        assert!(
            row.contains("other Agent") && !row.contains(&other_chat.simple().to_string()[..8]),
            "{row}"
        );
    }

    #[test]
    fn queued_previews_truncate_to_one_physical_row_and_merge_on_short_screens() {
        let mut state = State::new(true, 40, 24);
        state.prompt_enabled = true;
        state.queued_inputs = vec![queued_input(
            QueuedInputKind::Message,
            "first complete line\nsecond complete line",
            1,
        )];
        let lines = state.frame().0;
        let rendered = lines.join(" ");
        let compact = rendered.replace(' ', "");
        assert!(compact.contains("message"));
        assert!(!compact.contains("secondcompleteline"));
        assert!(lines.iter().all(|line| display_width(line) <= state.width));
        assert!(lines.len() <= agent_layout::bottom_capacity(state.height));
        assert_eq!(
            state.queued_inputs[0].text,
            "first complete line\nsecond complete line"
        );

        state.height = 8;
        state.queued_inputs = (0..20)
            .map(|sequence| queued_input(QueuedInputKind::Message, "queued", sequence))
            .collect();
        let lines = state.frame().0;
        assert!(lines.len() <= agent_layout::bottom_capacity(state.height));
        assert!(lines.iter().any(|line| line.contains("Pending 20")));
    }

    #[test]
    fn pending_previews_recalculate_visible_messages_and_omissions_with_the_row_budget() {
        for width in [20, 40, 80] {
            for count in [1, 2, 3, 7] {
                let entries = (0..count)
                    .map(|sequence| {
                        queued_input(
                            QueuedInputKind::Message,
                            &format!("message {sequence}"),
                            sequence as u64,
                        )
                    })
                    .collect::<Vec<_>>();
                for rows in 1..=MAX_PENDING_INPUT_ROWS {
                    let lines = pending_input_lines(&entries, width, rows);
                    assert!(lines.len() <= rows, "{width}/{count}/{rows}: {lines:?}");
                    assert!(lines[0].text().contains(&count.to_string()), "{lines:?}");
                    if count > rows.saturating_sub(1) && rows > 1 {
                        assert!(
                            lines.iter().any(|line| line.text().contains("more")),
                            "{lines:?}"
                        );
                    }
                    if rows == 4 && count <= 3 {
                        for sequence in 0..count {
                            assert!(
                                lines.iter().any(|line| line
                                    .text()
                                    .contains(&format!("message {sequence}"))),
                                "{lines:?}"
                            );
                        }
                    }
                    assert!(
                        lines
                            .iter()
                            .flat_map(|line| &line.spans)
                            .all(|span| span.role == Some(Role::MutedText)),
                        "{lines:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn compact_pending_count_survives_zero_and_custom_status_rows_and_help() {
        for height in [4, 8, 12, 15] {
            for configured in [
                Some(Vec::new()),
                None,
                Some(vec!["ordinary custom status".into()]),
                Some(vec![
                    "ordinary first status".into(),
                    "second".into(),
                    "third".into(),
                ]),
            ] {
                for help_open in [false, true] {
                    let mut state = State::new(true, 24, height);
                    state.prompt_enabled = true;
                    state
                        .status_area
                        .update("Ask".into(), "fixture".into(), None, Vec::new());
                    state.configured_status_lines = configured.clone();
                    state.help_open = help_open;
                    state.queued_inputs = vec![
                        queued_input(QueuedInputKind::Message, "Hello", 1),
                        queued_input(QueuedInputKind::Message, "Hello", 2),
                    ];
                    let (lines, _, _) = state.normal_styled_frame();
                    let pending=lines.iter().find(|line|line.text().contains("Pending 2")).unwrap_or_else(||panic!("height {height}, status {configured:?}, help {help_open}: {lines:?}"));
                    assert!(
                        pending
                            .spans
                            .iter()
                            .filter(|span| span.text.contains("Pending 2"))
                            .all(|span| span.role == Some(Role::MutedText)),
                        "{pending:?}"
                    );
                    assert!(
                        lines.len() <= agent_layout::bottom_capacity(usize::from(height)),
                        "{lines:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn compact_pending_count_preserves_urgent_facts_and_discards_optional_labels() {
        for width in [24, 40] {
            for fact in ["offline", "approval", "creation"] {
                let mut state = State::new(true, width, 4);
                state.prompt_enabled = true;
                state.configured_status_lines = Some(vec![
                    "ordinary custom status".into(),
                    "second".into(),
                    "third".into(),
                ]);
                state.queued_inputs = vec![
                    queued_input(QueuedInputKind::Message, "Hello", 1),
                    queued_input(QueuedInputKind::Message, "Hello", 2),
                ];
                match fact {
                    "offline" => {
                        state.connection_notice =
                            Some("Disconnected · cached execution · owned fixture".into())
                    }
                    "creation" => {
                        state.creation_notice =
                            Some("Creation attempt is unknown; reconcile before retrying".into())
                    }
                    _ => {
                        state.background_status_notice = Some(BackgroundStatusNotice {
                            title: Some("very long optional task label".into()),
                            ordinal: None,
                            message: "Waiting for approval".into(),
                            message_role: Role::Permission,
                            compact_message: Some("Approval".into()),
                            necessary: true,
                        })
                    }
                }
                let (lines, _, _) = state.normal_styled_frame();
                let text = lines
                    .iter()
                    .map(RoleLine::text)
                    .collect::<Vec<_>>()
                    .join("\n");
                let expected = match fact {
                    "offline" => {
                        if width == 24 {
                            "Offline"
                        } else {
                            "Disconnected"
                        }
                    }
                    "creation" => "Creation",
                    _ => "Approval",
                };
                assert!(
                    text.to_lowercase().contains(&expected.to_lowercase()),
                    "{fact}/{width}: {lines:?}"
                );
                if fact != "creation" || width == 40 {
                    assert!(text.contains("Pending 2"), "{fact}/{width}: {lines:?}");
                }
                if fact == "approval" {
                    assert!(
                        lines.iter().flat_map(|line| &line.spans).any(|span| span
                            .text
                            .to_lowercase()
                            .contains("approval")
                            && span.role == Some(Role::Permission)),
                        "{lines:?}"
                    );
                }
            }
        }
        for height in [1, 2] {
            let mut state = State::new(true, 24, height);
            state.prompt_enabled = true;
            state.insert("preserved input");
            state.queued_inputs = vec![queued_input(QueuedInputKind::Message, "Hello", 1)];
            let (lines, _, _) = state.normal_styled_frame();
            assert!(
                lines
                    .iter()
                    .any(|line| line.text().contains("preserved input")),
                "{lines:?}"
            );
        }
    }

    #[test]
    fn up_requests_takeback_only_from_the_focused_editors_first_visual_row() {
        let mut state = State::new(true, 20, 24);
        state.prompt_enabled = true;
        state.queued_inputs = vec![queued_input(QueuedInputKind::Message, "queued", 1)];
        state.insert("first line\nsecond line");
        assert!(state.key(key(KeyCode::Up)).is_none());
        assert!(matches!(
            state.key(key(KeyCode::Up)),
            Some(InputEvent::TakeBackQueued { draft_empty: false })
        ));

        state.menu = Some(menu());
        state.menu_focused = true;
        assert!(state.key(key(KeyCode::Up)).is_none());
        assert!(state.menu.is_some());
    }

    #[test]
    fn takeback_restores_messages_before_the_draft_and_an_isolated_shell_as_shell_mode() {
        let (console, _) = Console::start(false).unwrap();
        let message = queued_input(QueuedInputKind::Message, "first queued", 1);
        {
            let mut state = console.state.lock().unwrap();
            state.prompt_enabled = true;
            state.queued_inputs = vec![message.clone()];
            state.insert("existing draft");
        }
        console.restore_queued_inputs(&[message]).unwrap();
        {
            let state = console.state.lock().unwrap();
            assert_eq!(state.editor.draft(), "first queued\nexisting draft");
            assert_eq!(state.shell_input.kind(), InputKind::Prompt);
        }

        let shell = queued_input(QueuedInputKind::Shell, "printf restored", 2);
        {
            let mut state = console.state.lock().unwrap();
            state.replace_draft(String::new(), 0);
            state.queued_inputs = vec![shell.clone()];
        }
        console.restore_queued_inputs(&[shell]).unwrap();
        let state = console.state.lock().unwrap();
        assert_eq!(state.editor.draft(), "printf restored");
        assert_eq!(state.shell_input.kind(), InputKind::Shell);
    }

    #[test]
    fn task_list_owns_j_k_navigation_and_control_keys() {
        let chat_id = Uuid::new_v4();
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("draft stays here");
        state.task_surface = Some(TaskSurface::open(task_snapshot(chat_id, 2), chat_id));

        assert!(state.key(key(KeyCode::Char('j'))).is_none());
        let frame = state.task_surface.as_mut().unwrap().frame(TaskViewport {
            width: 80,
            height: 22,
        });
        assert_eq!(frame.selected_task_id, Some(Uuid::from_u128(2)));
        assert_eq!(state.editor.draft(), "draft stays here");

        assert!(matches!(
            state.key(key(KeyCode::Char('x'))),
            Some(InputEvent::TaskSurface(TaskSurfaceEffect::Control {
                task_id,
                action: tickloop_agent::runtime::ShellTaskAction::Stop,
            })) if task_id == Uuid::from_u128(2)
        ));
        assert_eq!(state.editor.draft(), "draft stays here");
    }

    #[test]
    fn task_surface_footer_only_advertises_selected_task_capabilities() {
        let chat_id = Uuid::new_v4();
        let mut snapshot = task_snapshot(chat_id, 2);
        snapshot.tasks[0].state = ShellTaskState::Completed;
        snapshot.tasks[0].capabilities = ShellTaskCapabilities {
            can_background: false,
            can_stop: false,
            can_read_output: true,
        };
        snapshot.tasks[1].capabilities = ShellTaskCapabilities {
            can_background: false,
            can_stop: true,
            can_read_output: true,
        };
        let mut surface = TaskSurface::open(snapshot, chat_id);

        let completed_lines =
            task_surface_lines(&mut surface, 100, 10, None, DisplayProfile::default());
        let completed_footer = completed_lines.last().unwrap();
        assert!(!completed_footer.contains("b background"));
        assert!(!completed_footer.contains("x stop"));

        surface.apply(
            TaskSurfaceAction::Next,
            TaskViewport {
                width: 98,
                height: 8,
            },
        );
        let stoppable_lines =
            task_surface_lines(&mut surface, 100, 10, None, DisplayProfile::default());
        let stoppable_footer = stoppable_lines.last().unwrap();
        assert!(!stoppable_footer.contains("b background"));
        assert!(stoppable_footer.contains("x stop"));

        surface.apply(
            TaskSurfaceAction::Open,
            TaskViewport {
                width: 98,
                height: 8,
            },
        );
        let detail_lines =
            task_surface_lines(&mut surface, 100, 10, None, DisplayProfile::default());
        let detail_footer = detail_lines.last().unwrap();
        assert!(!detail_footer.contains("b background"));
        assert!(detail_footer.contains("x stop"));
    }

    #[test]
    fn single_task_opens_detail_and_left_then_escape_restores_chat() {
        let chat_id = Uuid::new_v4();
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("kept draft");
        let cursor = state.editor.cursor();
        state.task_surface = Some(TaskSurface::open(task_snapshot(chat_id, 1), chat_id));
        assert!(matches!(
            state.task_surface.as_ref().unwrap().focus(),
            TaskSurfaceFocus::Detail { .. }
        ));

        assert!(state.key(key(KeyCode::Left)).is_none());
        assert_eq!(
            state.task_surface.as_ref().unwrap().focus(),
            TaskSurfaceFocus::List
        );
        assert!(state.key(key(KeyCode::Esc)).is_none());
        assert!(state.task_surface.is_none());
        assert_eq!(state.editor.draft(), "kept draft");
        assert_eq!(state.editor.cursor(), cursor);
    }

    #[test]
    fn visible_permission_menu_owns_keys_over_a_stale_task_surface() {
        for code in [KeyCode::Enter, KeyCode::Esc] {
            let chat_id = Uuid::new_v4();
            let mut state = State::new(true, 80, 24);
            state.prompt_enabled = true;
            state.menu = Some(menu());
            state.menu_focused = false;
            state.task_surface = Some(TaskSurface::open(task_snapshot(chat_id, 2), chat_id));
            let menu_id = state.menu.as_ref().unwrap().id;

            state.paste("allow this");
            assert!(state.editor.draft().is_empty());
            let event = state.key(key(code));
            if code == KeyCode::Enter {
                assert!(
                    matches!(event, Some(InputEvent::Choice { menu_id: id, index: 0, .. }) if id == menu_id)
                );
            } else {
                assert!(matches!(event, Some(InputEvent::Cancel { menu_id: id }) if id == menu_id));
            }
            assert!(matches!(
                state.task_surface.as_ref().unwrap().focus(),
                TaskSurfaceFocus::List
            ));
            assert_eq!(state.menu.as_ref().unwrap().id, menu_id);

            assert!(state.menu_focused);
            assert_eq!(state.menu.as_ref().unwrap().id, menu_id);
        }
    }

    #[test]
    fn task_surface_cache_restores_detail_selection_and_unread_position() {
        let chat_id = Uuid::new_v4();
        let mut state = State::new(true, 80, 24);
        state.task_surface = Some(TaskSurface::open(task_snapshot(chat_id, 2), chat_id));
        state.key(key(KeyCode::Char('j')));
        state.key(key(KeyCode::Enter));
        state.key(key(KeyCode::Char('k')));
        let mut updated = task_snapshot(chat_id, 2);
        updated.live_revision = 2;
        updated.tasks[1].output_bytes = 20;
        state
            .task_surface
            .as_mut()
            .unwrap()
            .update_snapshot(updated.clone());
        let before = state.task_surface.as_mut().unwrap().frame(TaskViewport {
            width: 80,
            height: 22,
        });
        assert_eq!(before.selected_task_id, Some(Uuid::from_u128(2)));
        assert!(matches!(
            before.focus,
            TaskSurfaceFocus::Detail { task_id } if task_id == Uuid::from_u128(2)
        ));
        assert_eq!(before.unread_bytes, 20);

        state.hide_task_surface();
        let mut restored = state.task_surface_cache.remove(&chat_id).unwrap();
        restored.update_snapshot(updated);
        state.task_surface = Some(restored);
        let after = state.task_surface.as_mut().unwrap().frame(TaskViewport {
            width: 80,
            height: 22,
        });
        assert_eq!(after.selected_task_id, before.selected_task_id);
        assert_eq!(after.focus, before.focus);
        assert_eq!(after.unread_bytes, before.unread_bytes);
    }

    #[test]
    fn task_surface_control_o_opens_detailed_transcript_and_caches_position() {
        let chat_id = Uuid::new_v4();
        let mut state = State::new(true, 80, 24);
        state.task_surface = Some(TaskSurface::open(task_snapshot(chat_id, 2), chat_id));
        state.key(key(KeyCode::Char('j')));
        assert!(matches!(
            state.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Some(InputEvent::OpenTranscript { .. })
        ));
        assert!(state.task_surface.is_none());
        assert!(state.task_surface_cache.contains_key(&chat_id));
        assert!(state.transcript.is_some());
    }

    #[test]
    fn task_action_error_replaces_the_surface_footer_until_cleared() {
        let chat_id = Uuid::new_v4();
        let mut surface = TaskSurface::open(task_snapshot(chat_id, 1), chat_id);
        surface.report_action_error(Uuid::from_u128(1), "owner mismatch".into());
        let lines = task_surface_lines(&mut surface, 80, 24, None, DisplayProfile::default());
        assert!(
            lines
                .last()
                .unwrap()
                .contains("Error: printf task-0: owner mismatch")
        );
        surface.clear_action_error();
        let lines = task_surface_lines(&mut surface, 80, 24, None, DisplayProfile::default());
        assert!(!lines.last().unwrap().contains("j/k/↑↓ scroll"));
        assert!(lines.last().unwrap().contains("Esc/Enter/Space close"));
        assert!(!lines.last().unwrap().contains("owner mismatch"));
    }

    #[tokio::test]
    async fn rewind_menu_transitions_keep_keyboard_focus_and_the_insert_draft() {
        use crate::terminal::menus::Menus;
        use tickloop_agent::{
            model::{HttpModelClient, InputModality, ModelConfig, Protocol},
            runtime::ChatSnapshot,
        };
        let config = ModelConfig {
            provider: crate::model::ModelProvider::Api,
            auth_source: crate::model::ModelAuthSource::ApiKeyEnv,
            name: "focus fixture".into(),
            protocol: Protocol::Messages,
            base_url: "http://127.0.0.1".into(),
            model: "fixture".into(),
            api_key_env: String::new(),
            max_output_tokens: 128,
            provider_id: String::new(),
            provider_name: String::new(),
            model_id: String::new(),
            connection: None,
            card: None,
            input_modalities: vec![InputModality::Text],
        };
        let agent = Agent::new_embedded(
            config.clone(),
            Arc::new(HttpModelClient::new(config.clone()).unwrap()),
        );
        let chat_id = Uuid::new_v4();
        let snapshot: ChatSnapshot = serde_json::from_value(serde_json::json!({
            "id":chat_id,"workdir":"/workspace","model":config,
            "messages":[{"role":"user","content":[{"type":"text","text":"Recorded prompt"}]}],
            "operations":[],"loops":[],"last_sequence":0,
            "message_ids":{"0":Uuid::new_v4()},
            "prompt_execution_modes":{"0":tickloop_agent::permissions::ExecutionMode::AcceptEdits}
        }))
        .unwrap();
        let mut state = State::new(true, 100, 30);
        state.prompt_enabled = true;
        state.insert("keep jk draft");
        let console = InteractiveConsoleFixture::new(state);
        let mut menus = Menus::default();
        assert!(console.is_interactive());
        menus.rewind(&console, &snapshot).unwrap();
        assert_eq!(
            console.state.lock().unwrap().menu.as_ref().unwrap().title,
            "Rewind"
        );
        let event = {
            let mut state = console.state.lock().unwrap();
            state.key(key(KeyCode::Char('k')));

            assert_eq!(state.editor.draft(), "keep jk draft");
            state.key(key(KeyCode::Enter)).unwrap()
        };
        choose_rewind_key(&mut menus, &agent, &console, event).await;
        for redraw in 0..3 {
            let event = {
                let mut state = console.state.lock().unwrap();
                assert_eq!(state.menu.as_ref().unwrap().title, "Rewind actions");
                state.key(key(KeyCode::Char('j')));
                assert_eq!(state.menu.as_ref().unwrap().selected, 1);
                state.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
                assert_eq!(state.menu.as_ref().unwrap().selected, 0);
                assert_eq!(state.editor.draft(), "keep jk draft");
                if redraw == 0 {
                    state.key(key(KeyCode::Enter)).unwrap()
                } else if redraw == 1 {
                    state.key(key(KeyCode::Esc)).unwrap()
                } else {
                    for _ in 0..3 {
                        state.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
                    }
                    state.key(key(KeyCode::Enter)).unwrap()
                }
            };
            choose_rewind_key(&mut menus, &agent, &console, event).await;
            if redraw == 1 {
                let event = {
                    let mut state = console.state.lock().unwrap();
                    assert_eq!(state.menu.as_ref().unwrap().title, "Rewind");
                    state.key(key(KeyCode::Char('k')));
                    assert_eq!(state.menu.as_ref().unwrap().selected, 0);
                    assert_eq!(state.editor.draft(), "keep jk draft");
                    state.key(key(KeyCode::Enter)).unwrap()
                };
                choose_rewind_key(&mut menus, &agent, &console, event).await;
            }
        }
        let mut state = console.state.lock().unwrap();
        assert_eq!(state.menu.as_ref().unwrap().title, "Rewind");
        state.key(key(KeyCode::Char('k')));
        assert_eq!(state.menu.as_ref().unwrap().selected, 0);
        assert_eq!(state.editor.draft(), "keep jk draft");
    }

    async fn choose_rewind_key(
        menus: &mut crate::terminal::menus::Menus,
        agent: &Agent,
        console: &Console,
        event: InputEvent,
    ) {
        let (menu_id, index, comment) = match event {
            InputEvent::Choice {
                menu_id,
                index,
                comment,
            } => (menu_id, Some(index), comment),
            InputEvent::Cancel { menu_id } => (menu_id, None, None),
            event => panic!("expected a menu action, got {event:?}"),
        };
        menus
            .choose(agent, console, menu_id, index, comment)
            .await
            .unwrap();
        console
            .state
            .lock()
            .unwrap()
            .clear_finished_menu_submission();
    }

    #[tokio::test]
    async fn settings_menu_has_no_editor_mode_and_later_options_keep_their_actions() {
        use crate::terminal::menus::Menus;
        let directory = tempfile::tempdir().unwrap();
        let agent = Agent::new_embedded(
            support::config(),
            Arc::new(support::ScriptedModel::new(Vec::new())),
        );
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut state = State::new(true, 100, 30);
        state.prompt_enabled = true;
        let console = InteractiveConsoleFixture::new(state);
        let mut menus = Menus::default();
        async fn open_settings_and_choose(
            menus: &mut Menus,
            agent: &Agent,
            console: &Console,
            chat: Uuid,
            index: usize,
        ) -> String {
            menus
                .settings(agent, console, crate::terminal::ConfigTarget::Agent(chat))
                .await
                .unwrap();
            let settings_id = {
                let state = console.state.lock().unwrap();
                let settings = state.menu.as_ref().unwrap();
                assert_eq!(settings.title, "Settings");
                assert!(
                    settings
                        .options
                        .iter()
                        .chain(&settings.context)
                        .all(|line| !line.contains("Editor mode")),
                    "{:?}",
                    settings.options
                );
                let labels = settings
                    .options
                    .iter()
                    .map(|option| option.split(':').next().unwrap())
                    .collect::<Vec<_>>();
                assert_eq!(
                    labels,
                    [
                        "Model",
                        "Thinking",
                        "Thinking",
                        "Execution mode",
                        "Session status",
                        "Usage",
                        "Auto-scroll",
                        "Theme",
                        "Glyph Set",
                        "Reduce motion",
                        "Output style",
                        "Permissions",
                        "Artifact Service",
                        "Prompt suggestions",
                    ]
                );
                settings.id
            };
            menus
                .choose(agent, console, settings_id, Some(index), None)
                .await
                .unwrap();
            let mut state = console.state.lock().unwrap();
            state.clear_finished_menu_submission();
            state.menu.as_ref().unwrap().title.clone()
        }

        assert_eq!(
            open_settings_and_choose(&mut menus, &agent, &console, chat, 7).await,
            "Choose theme"
        );
        assert_eq!(
            open_settings_and_choose(&mut menus, &agent, &console, chat, 8).await,
            "Choose Glyph Set"
        );
        let auto_scroll = console.auto_scroll().unwrap();
        assert_eq!(
            open_settings_and_choose(&mut menus, &agent, &console, chat, 6).await,
            "Settings"
        );
        assert_eq!(console.auto_scroll().unwrap(), !auto_scroll);
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn settings_menu_reduce_motion_saves_before_applying_and_keeps_later_actions() {
        use crate::settings::UserSettings;
        use crate::terminal::menus::{
            Menus, OUTPUT_STYLE_SETTING_INDEX, PERMISSIONS_SETTING_INDEX,
            REDUCE_MOTION_SETTING_INDEX,
        };
        let directory = tempfile::tempdir().unwrap();
        let settings = UserSettings::in_root(directory.path());
        let agent = Agent::new_embedded(
            support::config(),
            Arc::new(support::ScriptedModel::new(Vec::new())),
        );
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let target = crate::terminal::ConfigTarget::Agent(chat);
        let mut state = State::new(true, 100, 30);
        state.prompt_enabled = true;
        state.insert("keep jk draft");
        let console = InteractiveConsoleFixture::new(state);
        let mut menus = Menus::new(Some(settings.clone()), Some(directory.path().into()));
        menus.settings(&agent, &console, target).await.unwrap();
        let id = console.state.lock().unwrap().menu.as_ref().unwrap().id;
        menus
            .choose(
                &agent,
                &console,
                id,
                Some(REDUCE_MOTION_SETTING_INDEX),
                None,
            )
            .await
            .unwrap();
        assert!(
            console
                .display_preferences()
                .unwrap()
                .prefers_reduced_motion
        );
        assert!(settings.display_preferences().prefers_reduced_motion);
        assert_eq!(
            console.state.lock().unwrap().editor.draft(),
            "keep jk draft"
        );

        let id = console.state.lock().unwrap().menu.as_ref().unwrap().id;
        std::fs::write(directory.path().join("settings.json"), "invalid JSON").unwrap();
        assert!(
            menus
                .choose(
                    &agent,
                    &console,
                    id,
                    Some(REDUCE_MOTION_SETTING_INDEX),
                    None
                )
                .await
                .is_err()
        );
        assert!(
            console
                .display_preferences()
                .unwrap()
                .prefers_reduced_motion
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("settings.json")).unwrap(),
            "invalid JSON"
        );

        menus.settings(&agent, &console, target).await.unwrap();
        let id = console.state.lock().unwrap().menu.as_ref().unwrap().id;
        menus
            .choose(&agent, &console, id, Some(OUTPUT_STYLE_SETTING_INDEX), None)
            .await
            .unwrap();
        assert_eq!(
            console.state.lock().unwrap().menu.as_ref().unwrap().title,
            "Preferred output style"
        );
        menus.settings(&agent, &console, target).await.unwrap();
        let id = console.state.lock().unwrap().menu.as_ref().unwrap().id;
        menus
            .choose(&agent, &console, id, Some(PERMISSIONS_SETTING_INDEX), None)
            .await
            .unwrap();
        assert_eq!(
            console.state.lock().unwrap().menu.as_ref().unwrap().title,
            "Permissions · Choose mode"
        );
        agent.shutdown().await;
    }

    #[test]
    fn explicit_menu_focus_preserves_the_draft() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("keep this draft");
        let console = console_with_state(state);
        console.show_focused_menu(menu()).unwrap();
        let mut state = console.state.lock().unwrap();
        state.key(key(KeyCode::Char('j')));
        assert_eq!(state.menu.as_ref().unwrap().selected, 1);
        assert_eq!(state.editor.draft(), "keep this draft");
        assert!(matches!(
            state.key(key(KeyCode::Enter)),
            Some(InputEvent::Choice { index: 1, .. })
        ));
        assert_eq!(state.editor.draft(), "keep this draft");
    }

    #[test]
    fn provider_picker_routes_shortcuts_with_the_current_selection() {
        let mut state = State::new(true, 160, 24);
        state.prompt_enabled = true;
        let console = console_with_state(state);
        let selection = menu();
        let menu_id = selection.id;
        console
            .show_picker_menu(selection, PickerKind::Provider)
            .unwrap();
        let mut state = console.state.lock().unwrap();
        assert!(state.menu_focused);
        assert_eq!(state.picker_kind, Some(PickerKind::Provider));
        assert!(state.key(key(KeyCode::Down)).is_none());
        assert!(state.key(key(KeyCode::Char('j'))).is_none());
        assert!(
            state
                .key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
                .is_none()
        );
        assert_eq!(state.menu.as_ref().unwrap().selected, 1);
        for (code, action) in [
            (KeyCode::Char('p'), PickerAction::ConfigureProvider),
            (KeyCode::Char('f'), PickerAction::ToggleConnections),
        ] {
            assert!(matches!(
                state.key(key(code)),
                Some(InputEvent::PickerAction {
                    menu_id: id,
                    index: 1,
                    action: actual,
                }) if id == menu_id && actual == action
            ));
        }
        let rendered = state.frame().0.join("\n");
        assert!(
            rendered.contains("p Connection · f All/Configured · Enter models/setup · Esc close")
        );
        assert!(!rendered.contains("a Adapter"));
        assert!(state.key(key(KeyCode::Char('a'))).is_none());
        assert_eq!(state.editor.draft(), "");
    }

    #[test]
    fn model_picker_routes_thinking_keys_only_while_visible_and_focused() {
        let mut state = State::new(true, 160, 24);
        state.prompt_enabled = true;
        let console = console_with_state(state);
        let mut selection = menu();
        selection.options.push(REFRESH_ACCOUNT_MODELS_LABEL.into());
        let menu_id = selection.id;
        console
            .show_presented_picker_menu(
                selection,
                PickerKind::Model,
                PickerPresentation {
                    refresh: true,
                    hint: "p Connection · r Refresh · Enter apply · Esc back".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        let mut state = console.state.lock().unwrap();
        for (code, action) in [
            (KeyCode::Left, PickerAction::ThinkingPrevious),
            (KeyCode::Char('h'), PickerAction::ThinkingPrevious),
            (KeyCode::Right, PickerAction::ThinkingNext),
            (KeyCode::Char('l'), PickerAction::ThinkingNext),
            (KeyCode::Char('p'), PickerAction::ConfigureProvider),
            (KeyCode::Char('r'), PickerAction::RefreshAccountModels),
        ] {
            assert!(matches!(
                state.key(key(code)),
                Some(InputEvent::PickerAction {
                    menu_id: id,
                    index: 0,
                    action: actual,
                }) if id == menu_id && actual == action
            ));
        }
        let rendered = state.frame().0.join("\n");
        assert!(rendered.contains("p Connection · r Refresh · Enter apply · Esc back"));
        assert!(!rendered.contains("←→ Thinking"));
        assert!(!rendered.contains("↑↓/j/k"));
        drop(state);
        let request = Uuid::new_v4();
        console.set_request_layout(Some(request)).unwrap();
        let mut question = menu();
        question.id = request;
        console.show_focused_menu(question).unwrap();
        assert!(!matches!(
            console.state.lock().unwrap().key(key(KeyCode::Right)),
            Some(InputEvent::PickerAction { .. })
        ));
        console.clear_menu(request).unwrap();
        {
            let mut state = console.state.lock().unwrap();
            state.height = 10;
            assert!(
                matches!(state.key(key(KeyCode::Left)), Some(InputEvent::PickerAction { menu_id: id, action: PickerAction::ThinkingPrevious, .. }) if id == menu_id)
            );
        }
        console.clear_menu(menu_id).unwrap();
        assert!(!matches!(
            console.state.lock().unwrap().key(key(KeyCode::Right)),
            Some(InputEvent::PickerAction { .. })
        ));
    }

    #[test]
    fn same_picker_refresh_keeps_newer_navigation_and_visible_focus() {
        let mut state = State::new(true, 160, 24);
        state.prompt_enabled = true;
        let console = console_with_state(state);
        let selection = menu();
        let menu_id = selection.id;
        console
            .show_picker_menu(selection, PickerKind::Model)
            .unwrap();
        {
            let mut state = console.state.lock().unwrap();
            assert!(matches!(
                state.key(key(KeyCode::Left)),
                Some(InputEvent::PickerAction {
                    action: PickerAction::ThinkingPrevious,
                    ..
                })
            ));
            assert!(state.key(key(KeyCode::Char('j'))).is_none());
            assert_eq!(state.menu.as_ref().unwrap().selected, 1);
        }
        let mut refreshed = menu();
        refreshed.id = menu_id;
        refreshed.selected = 0;
        console
            .show_picker_menu(refreshed, PickerKind::Model)
            .unwrap();
        {
            let mut state = console.state.lock().unwrap();
            assert_eq!(state.menu.as_ref().unwrap().selected, 1);
            assert!(state.menu_focused);
            assert!(state.key(key(KeyCode::Tab)).is_none());
            assert!(state.menu_focused);
        }
        let mut refreshed = menu();
        refreshed.id = menu_id;
        console
            .show_picker_menu(refreshed, PickerKind::Model)
            .unwrap();
        {
            let mut state = console.state.lock().unwrap();
            assert_eq!(state.menu.as_ref().unwrap().selected, 1);
            assert!(state.menu_focused);
            state.menu_focused = true;
            state.height = 10;
        }
        let mut refreshed = menu();
        refreshed.id = menu_id;
        console
            .show_picker_menu(refreshed, PickerKind::Model)
            .unwrap();
        let state = console.state.lock().unwrap();
        assert_eq!(state.menu.as_ref().unwrap().selected, 1);
        assert!(state.menu_focused);
    }

    #[test]
    fn long_picker_keeps_title_selected_row_and_scroll_hint_visible() {
        let mut state = State::new(true, 80, 20);
        state.prompt_enabled = true;
        state.menu_focused = true;
        state.picker_kind = Some(PickerKind::Model);
        let mut choices = menu();
        choices.title = "Choose model · Test Provider".into();
        choices.context = vec!["Account directory checked".into()];
        choices.options = (0..30)
            .map(|index| format!("Model {index:02} Thinking auto"))
            .collect();
        choices.selected = 29;
        state.menu = Some(choices);
        for expected in (0..30).rev() {
            let (lines, _, _) = state.styled_frame();
            let visible = lines
                .iter()
                .map(RoleLine::text)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(visible.contains("Choose model · Test Provider"));
            assert!(visible.contains(&format!("Model {expected:02}")));
            assert!(visible.contains("More above") || visible.contains("More below"));
            assert!(lines.iter().any(|line| line.fill == Some(Role::Selection)
                && line.text().contains(&format!("Model {expected:02}"))));
            state.key(key(KeyCode::Char('k')));
        }
    }

    #[test]
    fn model_context_stays_visible_on_model_and_action_rows() {
        let mut state = State::new(true, 160, 24);
        state.prompt_enabled = true;
        let console = console_with_state(state);
        let mut selection = menu();
        selection.context = vec![
            "Provider account check failed".into(),
            "Provider notice".into(),
        ];
        selection.options.push("Refresh account models".into());
        console
            .show_picker_menu(selection, PickerKind::Model)
            .unwrap();
        let mut state = console.state.lock().unwrap();
        for _ in 0..3 {
            let frame = state.frame().0.join("\n");
            assert!(frame.contains("Provider account check failed"));
            assert!(frame.contains("Provider notice"));
            state.key(key(KeyCode::Char('j')));
        }
        assert_eq!(state.editor.draft(), "");
    }

    #[test]
    fn provider_and_model_pickers_omit_numbers_and_ignore_numeric_submission() {
        for kind in [PickerKind::Provider, PickerKind::Actions, PickerKind::Model] {
            let mut state = State::new(true, 160, 24);
            state.prompt_enabled = true;
            let console = console_with_state(state);
            console.show_picker_menu(menu(), kind).unwrap();
            let mut state = console.state.lock().unwrap();
            let rendered = state.frame().0.join("\n");
            assert!(!rendered.contains("1. "), "{rendered}");
            assert!(!rendered.contains("2. "), "{rendered}");
            for digit in ['1', '2', '3'] {
                assert!(state.key(key(KeyCode::Char(digit))).is_none());
                assert_eq!(state.menu.as_ref().unwrap().selected, 0);
                assert_eq!(state.editor.draft(), "");
            }
            assert!(matches!(
                state.key(key(KeyCode::Enter)),
                Some(InputEvent::Choice { index: 0, .. })
            ));
        }
    }

    #[test]
    fn ordinary_and_secret_menus_clear_picker_shortcuts() {
        let mut state = State::new(true, 120, 24);
        state.prompt_enabled = true;
        let console = console_with_state(state);
        let mut selection = menu();
        selection.selected = 2;
        console
            .show_picker_menu(selection, PickerKind::Provider)
            .unwrap();
        {
            let state = console.state.lock().unwrap();
            assert_eq!(state.menu.as_ref().unwrap().selected, 2);
        }
        console.show_focused_menu(menu()).unwrap();
        {
            let mut state = console.state.lock().unwrap();
            assert_eq!(state.picker_kind, None);
            assert!(!matches!(
                state.key(key(KeyCode::Char('a'))),
                Some(InputEvent::PickerAction { .. })
            ));
        }
        let secret_id = Uuid::new_v4();
        console
            .show_secret_menu(Menu {
                id: secret_id,
                title: "API key".into(),
                context: vec![],
                options: vec!["Save".into()],
                selected: 0,
                confirmation: false,
            })
            .unwrap();
        let mut state = console.state.lock().unwrap();
        assert_eq!(state.picker_kind, Some(PickerKind::Actions));
        assert!(state.key(key(KeyCode::Char('a'))).is_none());
        assert_eq!(state.selected_menu_comment().as_deref(), Some("a"));
        assert!(state.clear_menu(secret_id));
        assert_eq!(state.picker_kind, None);
    }
    #[test]
    fn connection_headers_are_not_choices_and_actions_follow_presentation_metadata() {
        for (width, height) in [(120, 38), (80, 24), (48, 16)] {
            let mut state = State::new(true, width, height);
            state.prompt_enabled = true;
            let console = console_with_state(state);
            let mut selection = menu();
            selection.title = "Providers · All".into();
            selection.options = vec![
                "China PAYG · API key saved".into(),
                "International PAYG · API key required".into(),
                "Connection details".into(),
            ];
            console
                .show_presented_picker_menu(
                    selection,
                    PickerKind::Provider,
                    PickerPresentation {
                        headers: vec![(0, "Zhipu GLM · 1/4 configured".into())],
                        hint: "p Connection · f Configured · Enter models/setup · Esc close".into(),
                        refresh: false,
                        ..Default::default()
                    },
                )
                .unwrap();
            let mut state = console.state.lock().unwrap();
            state.key(key(KeyCode::Char('j')));
            assert_eq!(state.menu.as_ref().unwrap().selected, 1);
            let lines = state.styled_frame().0;
            assert!(
                lines
                    .iter()
                    .any(|line| line.text().contains("International PAYG")
                        && line.fill == Some(Role::Selection))
            );
            assert!(
                lines
                    .iter()
                    .any(|line| line.text().contains("Providers · All"))
            );
            for code in ['r', 's', 'd', '1'] {
                assert!(state.key(key(KeyCode::Char(code))).is_none());
            }
            assert!(matches!(
                state.key(key(KeyCode::Enter)),
                Some(InputEvent::Choice { index: 1, .. })
            ));
        }
    }

    #[test]
    fn secret_external_editor_preserves_chat_draft_and_masks_returned_candidate() {
        let mut state = State::new(true, 80, 24);
        state.prompt_enabled = true;
        state.insert("untouched Chat draft");
        let console = console_with_state(state);
        let mut selection = menu();
        selection.title = "API key · Fixture".into();
        selection.options = vec!["Save API key".into(), "Cancel".into()];
        console
            .show_secret_menu_with_value(selection, "candidate-jk123")
            .unwrap();
        let mut state = console.state.lock().unwrap();
        let opened = scripted_external_editor(&mut state, "replacement-jk123\n");
        state.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert_eq!(opened.lock().unwrap().as_slice(), ["candidate-jk123"]);
        assert_eq!(
            state.selected_menu_comment().as_deref(),
            Some("replacement-jk123")
        );
        assert_eq!(state.editor.draft(), "untouched Chat draft");
        let frame = state.frame().0.join("\n");
        assert!(!frame.contains("replacement-jk123"));
        assert!(!frame.contains("1. Save"));
        state.key(key(KeyCode::Esc));
        assert!(state.key(key(KeyCode::Char('2'))).is_none());
        assert_eq!(state.menu.as_ref().unwrap().selected, 0);
        assert!(
            matches!(state.key(key(KeyCode::Enter)), Some(InputEvent::Choice { comment: Some(value), .. }) if value == "replacement-jk123")
        );
    }
}
