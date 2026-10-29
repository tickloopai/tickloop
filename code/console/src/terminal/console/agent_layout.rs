use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use uuid::Uuid;

use super::{
    Role, RoleLine, display_single_line, display_width, truncate_path_width, truncate_width,
};
use crate::agent_console::{AgentState, AttentionGroup};
use crate::terminal::display::{DisplayProfile, Glyph};
use crate::terminal::list_row::ROW_INDENT;
use tickloop_code_access::EndReason;
use tickloop_harness::protocol::{LoopState, SubagentRegistration};

pub(super) const MIN_CONVERSATION_ROWS: usize = 6;
pub(super) const NORMAL_LAYOUT_MIN_ROWS: usize = 16;
pub(super) const TALL_LAYOUT_MIN_ROWS: usize = 24;
pub(super) const MAX_CANDIDATE_ROWS: usize = 6;
pub(in crate::terminal) const WIDE_TERMINAL_COLUMNS: usize = 80;
const MIN_AGENT_TITLE_COLUMNS: usize = 8;
/// The family panel's title floor before its request labels shrink further.
pub(in crate::terminal) const MIN_REQUEST_AGENT_TITLE_COLUMNS: usize = 4;
const MIN_METADATA_COLUMNS: usize = 12;
const MIN_PREVIEW_COLUMNS: usize = 10;
const PREVIEW_GAP: &str = "  ";
/// Claude Code's agent view keeps at least this many Completed rows however short the terminal.
const MIN_COMPLETED_ROWS: usize = 3;
const AGE_NOW: &str = "now";
pub(super) const REQUEST_IDLE_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Clone, Debug)]
pub(crate) struct AgentRow {
    pub(crate) agent_id: Uuid,
    pub(crate) parent_id: Option<Uuid>,
    pub(crate) title: String,
    pub(crate) status: String,
    pub(crate) state: LoopState,
    pub(crate) project: String,
    pub(crate) workspace: Option<String>,
    pub(crate) approvals: usize,
    pub(crate) questions: usize,
    /// This row's own state, as its status word names it.
    pub(crate) agent_state: AgentState,
    /// The row's attention group; an overview family groups by its most urgent member.
    pub(crate) group: AttentionGroup,
    /// The state shape drawn before the title; it pairs with the status word and never
    /// replaces it. The active DisplayProfile resolves it.
    pub(crate) glyph: Glyph,
    /// The latest reply or typed prompt, when it says more than the title.
    pub(crate) preview: Option<String>,
    /// Unix milliseconds of the latest reported state change; `None` when nobody recorded it.
    pub(crate) updated_at: Option<u64>,
    pub(crate) run_started_at: Option<u64>,
    pub(crate) subagent: Option<SubagentRegistration>,
    /// Set on a root Agent whose execution ended; such a row is keyed by its Chat.
    pub(crate) ended: Option<EndedRow>,
}

/// What the overview needs to continue or forget an ended root Agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EndedRow {
    pub(crate) chat_id: Uuid,
    pub(crate) reason: EndReason,
    /// Whether its saved history is still on disk to load.
    pub(crate) resumable: bool,
}

/// The request a peeked Agent waits on, with the question options a typed number picks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PeekRequest {
    pub(crate) request_id: Uuid,
    /// The full question, or which tool asks for permission.
    pub(crate) prompt: String,
    /// Option labels, numbered from 1. Empty when only the Agent's own view can answer: every
    /// permission request, whose context and choices only that view shows, and several questions
    /// at once.
    pub(crate) options: Vec<String>,
}

/// Claude Code's peek panel: the selected Agent's state below the overview list, with the input
/// replying to that Agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Peek {
    pub(super) agent_id: Uuid,
    pub(super) request: Option<PeekRequest>,
    /// Whether the last drawn panel showed the request's prompt and every option whole; a number
    /// answers only what the user could read.
    pub(super) options_visible: bool,
}

/// The most rows the peek panel takes from the list.
const MAX_PEEK_ROWS: usize = 10;

/// What a row shows after its title when there is room: the newest message, path metadata and
/// the age of its latest state change. Narrow rows drop path first, then the message, then age.
pub(in crate::terminal) struct RowExtras<'a> {
    /// The row's state glyph as the active profile draws it; empty draws none.
    pub(in crate::terminal) glyph: &'a str,
    pub(in crate::terminal) glyph_role: Role,
    pub(in crate::terminal) project: &'a str,
    pub(in crate::terminal) workspace: Option<&'a str>,
    pub(in crate::terminal) preview: Option<&'a str>,
    pub(in crate::terminal) age: Option<String>,
}

/// The pieces of one summary line: everything before the title, the room for the title and its
/// preview, everything after them, and the age that ends the row at its right edge.
struct RowFrame {
    /// The state shape and its space, or nothing when a narrow row gave it up.
    glyph: String,
    title_columns: usize,
    preview_columns: usize,
    suffix: Vec<(String, Role)>,
    age: Option<String>,
}

impl AgentRow {
    pub(in crate::terminal) fn glyph_role(&self) -> Role {
        match self.group {
            AttentionGroup::NeedsInput => group_role(self.group),
            AttentionGroup::Working => Role::RunningMark,
            _ => self.status_role(),
        }
    }

    /// The row's own status word retains its state even when a child's request sets the group:
    /// a failure is an error, while an interruption is only a warning.
    pub(in crate::terminal) fn status_role(&self) -> Role {
        match self.agent_state {
            AgentState::Failed => Role::Error,
            AgentState::Completed if self.state == LoopState::Interrupted => Role::Warning,
            _ if self.group != AttentionGroup::Completed => group_role(self.group),
            AgentState::Completed => Role::Success,
            _ => Role::MutedText,
        }
    }

    /// How long a running Agent has waited for the user, as Claude Code's peek panel says it; an
    /// ended row no longer waits.
    fn waiting_label(&self, now: u64) -> Option<String> {
        if self.agent_state != AgentState::NeedsInput || self.ended.is_some() {
            return None;
        }
        let age = age_label(self.updated_at?, now);
        Some(if age == AGE_NOW {
            "waiting <1m".into()
        } else {
            format!("waiting {age}")
        })
    }

    pub(in crate::terminal) fn state_glyph(
        &self,
        display: DisplayProfile,
        now: u64,
    ) -> &'static str {
        if self.group == AttentionGroup::Working {
            display.working_dot(now)
        } else {
            display.glyph(self.glyph)
        }
    }

    /// A row's own status, unless a narrow row needs that width for its request labels.
    pub(in crate::terminal) fn own_status_variants(
        &self,
        status: Vec<(String, Role)>,
        terminal_columns: usize,
    ) -> Vec<Vec<(String, Role)>> {
        if terminal_columns >= WIDE_TERMINAL_COLUMNS || self.approvals == 0 && self.questions == 0 {
            vec![status]
        } else {
            vec![Vec::new()]
        }
    }

    /// The columns the title gets before any reading number is added.
    fn title_room(
        &self,
        width: usize,
        extras: &RowExtras<'_>,
        status_variants: &[Vec<(String, Role)>],
        min_title: usize,
    ) -> usize {
        self.summary_frame(width, extras, status_variants, min_title, 0)
            .title_columns
    }

    pub(in crate::terminal) fn summary_line(
        &self,
        width: usize,
        extras: &RowExtras<'_>,
        ordinal: Option<usize>,
        status_variants: &[Vec<(String, Role)>],
        min_title: usize,
    ) -> RoleLine {
        let ordinal = ordinal.map(|ordinal| format!(" ({ordinal})"));
        let ordinal_columns = ordinal.as_deref().map_or(0, display_width);
        let frame = self.summary_frame(width, extras, status_variants, min_title, ordinal_columns);
        let title = display_single_line(&self.title);
        let title = match &ordinal {
            Some(ordinal) => format!(
                "{}{ordinal}",
                ellipsize(&title, frame.title_columns.saturating_sub(ordinal_columns))
            ),
            None => ellipsize(&title, frame.title_columns),
        };
        let mut line = RoleLine::role(ROW_INDENT, Role::Text);
        line.push(frame.glyph, Some(extras.glyph_role));
        line.push(title.as_str(), Some(Role::Text));
        if frame.preview_columns > 0
            && let Some(preview) = extras.preview
        {
            let shown = display_width(&title);
            let padding = frame.title_columns.saturating_sub(shown);
            line.push(
                format!(
                    "{}{PREVIEW_GAP}{}",
                    " ".repeat(padding),
                    ellipsize(&display_single_line(preview), frame.preview_columns)
                ),
                Some(Role::MutedText),
            );
        }
        for (text, role) in frame.suffix {
            line.push(text, Some(role));
        }
        if let Some(age) = frame.age {
            let padding = width.saturating_sub(display_width(&line.text()) + display_width(&age));
            line.push(" ".repeat(padding), None);
            line.push(age, Some(Role::MutedText));
        }
        line.truncate(width);
        line
    }

    /// Lays a row out. The title keeps `min_title` columns beside its ordinal. To make room, the
    /// status falls back through `status_variants` (richest first; the last is what the row
    /// always keeps, such as a folded family's `+N`), then request labels shrink from words with
    /// counts to words, then to the `A`/`Q` letters both helps explain, with counts while they
    /// fit, and a row with requests then tries single-space delimiters. Only then does the title
    /// shrink. Age, the newest message and path metadata only take what is left, in that order;
    /// the age keeps its own columns at the row's right edge.
    fn summary_frame(
        &self,
        width: usize,
        extras: &RowExtras<'_>,
        status_variants: &[Vec<(String, Role)>],
        min_title: usize,
        ordinal_columns: usize,
    ) -> RowFrame {
        let shape = if extras.glyph.is_empty() {
            String::new()
        } else {
            format!("{} ", extras.glyph)
        };
        let with_glyph = format!("{ROW_INDENT}{shape}");
        let delimiter = |text: &str| (text.to_owned(), Role::MutedText);
        let separator = || delimiter(" · ");
        let status_segments = |separator: &str, status: &[(String, Role)]| {
            let mut segments = Vec::new();
            if !status.is_empty() {
                segments.push(delimiter(separator));
                segments.extend(
                    status
                        .iter()
                        .map(|(text, role)| (display_single_line(text), *role)),
                );
            }
            segments
        };
        let requests = [
            ("approvals", 'A', self.approvals),
            ("questions", 'Q', self.questions),
        ];
        let requests = requests
            .iter()
            .filter(|(.., count)| *count > 0)
            .collect::<Vec<_>>();
        let words = |separator: &str, counts: bool| {
            requests
                .iter()
                .flat_map(|(word, _, count)| {
                    let label = if counts {
                        format!("{word} {count}")
                    } else {
                        (*word).to_owned()
                    };
                    [delimiter(separator), (label, Role::Permission)]
                })
                .collect::<Vec<_>>()
        };
        let letters = |separator: &str, counts: bool| {
            if requests.is_empty() {
                return Vec::new();
            }
            let label = requests
                .iter()
                .map(|(_, letter, count)| {
                    if counts {
                        format!("{letter}{count}")
                    } else {
                        letter.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            vec![delimiter(separator), (label, Role::Permission)]
        };
        let request_forms = |separator: &str| {
            [
                words(separator, true),
                words(separator, false),
                letters(separator, true),
                letters(separator, false),
            ]
        };
        let suffix_with =
            |separator: &str, status: &[(String, Role)], requests: &[(String, Role)]| {
                let mut suffix = status_segments(separator, status);
                suffix.extend(requests.iter().cloned());
                suffix
            };
        let delimiters: &[&'static str] = if requests.is_empty() {
            &[" · "]
        } else {
            &[" · ", " "]
        };
        // The richest status and request form that fits beside a prefix, with its rank (lower is
        // richer).
        let choose = |prefix: &str| {
            let reserved = display_width(prefix) + min_title + ordinal_columns;
            delimiters
                .iter()
                .enumerate()
                .find_map(|(delimiter_rank, &separator)| {
                    request_forms(separator).iter().enumerate().find_map(
                        |(request_rank, requests)| {
                            status_variants
                                .iter()
                                .enumerate()
                                .find_map(|(status_rank, status)| {
                                    let suffix = suffix_with(separator, status, requests);
                                    (reserved + segments_width(&suffix) <= width).then_some((
                                        (delimiter_rank, request_rank, status_rank),
                                        suffix,
                                    ))
                                })
                        },
                    )
                })
        };
        // The glyph is the first thing a narrow row gives up: the status word still names the
        // state, and status or request labels never shrink to keep the shape.
        let (glyph, chosen) = match (choose(&with_glyph), choose(ROW_INDENT)) {
            (Some((glyph_rank, _)), Some(plain)) if plain.0 < glyph_rank => {
                (String::new(), Some(plain.1))
            }
            (None, plain) => (String::new(), plain.map(|(_, suffix)| suffix)),
            (glyph, _) => (shape, glyph.map(|(_, suffix)| suffix)),
        };
        let prefix_columns = display_width(ROW_INDENT) + display_width(&glyph);
        let mut suffix = chosen.unwrap_or_else(|| {
            let separator = delimiters[delimiters.len() - 1];
            let status = status_variants.last().map_or(&[][..], Vec::as_slice);
            suffix_with(separator, status, &request_forms(separator)[3])
        });
        let title_columns = display_width(&display_single_line(&self.title)) + ordinal_columns;
        // The title keeps a readable share before optional extras take the rest.
        let title_reserve =
            title_columns.min((MIN_AGENT_TITLE_COLUMNS + ordinal_columns).max(width * 3 / 10));
        let mut spare =
            width.saturating_sub(prefix_columns + segments_width(&suffix) + title_reserve);
        let age = extras
            .age
            .as_ref()
            .filter(|age| display_width(age) + display_width(" · ") <= spare)
            .map(|age| {
                spare -= display_width(age) + display_width(" · ");
                age.clone()
            });
        let preview_reserve = display_width(PREVIEW_GAP) + MIN_PREVIEW_COLUMNS;
        let preview = extras.preview.is_some() && spare >= preview_reserve;
        if preview {
            spare -= preview_reserve;
        }
        if !extras.project.is_empty() && spare >= MIN_METADATA_COLUMNS + display_width(" · ") {
            let metadata_columns =
                (spare - display_width(" · ")).min(width / if preview { 4 } else { 3 });
            let mut metadata = display_single_line(extras.project);
            if let Some(workspace) = extras.workspace {
                let workspace = format!(" / {}", display_single_line(workspace));
                if display_width(&metadata) + display_width(&workspace) <= metadata_columns {
                    metadata.push_str(&workspace);
                }
            }
            suffix.push(separator());
            suffix.push((
                truncate_path_width(&metadata, metadata_columns),
                Role::MutedText,
            ));
        }
        let age_columns = age
            .as_ref()
            .map_or(0, |age| display_width(age) + display_width(" · "));
        let free = width.saturating_sub(prefix_columns + segments_width(&suffix) + age_columns);
        let (title_columns, preview_columns) = if preview {
            let title = title_columns.min(free.saturating_sub(preview_reserve).max(title_reserve));
            let preview = free.saturating_sub(title + display_width(PREVIEW_GAP));
            if preview >= MIN_PREVIEW_COLUMNS {
                (title, preview)
            } else {
                (free, 0)
            }
        } else {
            (free, 0)
        };
        RowFrame {
            glyph,
            title_columns,
            preview_columns,
            suffix,
            age,
        }
    }
}

fn segments_width(segments: &[(String, Role)]) -> usize {
    segments.iter().map(|(text, _)| display_width(text)).sum()
}

/// `text` cut to `width` columns, ending in `…` when anything was cut.
fn ellipsize(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    format!("{}…", truncate_width(text, width - 1))
}

/// Status words keep their text; colour only separates attention from finished work.
fn group_role(group: AttentionGroup) -> Role {
    match group {
        AttentionGroup::NeedsInput => Role::Warning,
        AttentionGroup::Working => Role::Accent,
        AttentionGroup::Completed => Role::Success,
    }
}

/// The semantic shape drawn before a row's title. Shapes differ per state so the list reads
/// without colour; a Completed row whose last turn was interrupted is not drawn as a success.
pub(crate) fn status_glyph(state: AgentState, interrupted: bool) -> Glyph {
    match state {
        AgentState::NeedsInput => Glyph::AgentNeedsInput,
        AgentState::Working => Glyph::AgentRunning,
        AgentState::Idle => Glyph::AgentIdle,
        AgentState::Completed if interrupted => Glyph::AgentStopped,
        AgentState::Completed => Glyph::AgentCompleted,
        AgentState::Failed => Glyph::AgentFailed,
        AgentState::Stopped => Glyph::AgentStopped,
    }
}

/// How long ago `updated_at` was, in the largest whole unit: `now` within the first minute, then
/// `5m`, `3h`, `2d`. Labels change at most once a minute, so unchanged rows are not repainted
/// every second.
fn age_label(updated_at: u64, now: u64) -> String {
    let seconds = now.saturating_sub(updated_at) / 1000;
    match seconds {
        0..60 => AGE_NOW.into(),
        60..3600 => format!("{}m", seconds / 60),
        3600..172_800 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// Reading numbers that tell apart rows whose shown titles would read alike, bound to each Agent
/// for this View's lifetime. Agents whose titles start with the same `NUMBER_KEY_COLUMNS` share
/// one sequence, so two rows that can ever render alike never carry the same number. An Agent
/// gets the next number of its sequence when this View first lists it (a batch in `agent_id`
/// order) and keeps it through sorting, filtering, refresh, arrivals and removals. A number is a
/// local reading aid, not a time, an order of creation or an identity; actions always use the
/// row's execution.
#[derive(Default)]
pub(super) struct ReadingNumbers {
    assigned: HashMap<Uuid, (String, usize)>,
    issued: HashMap<String, usize>,
}

impl ReadingNumbers {
    pub(super) fn register(&mut self, rows: &[AgentRow]) {
        let mut unnumbered = rows
            .iter()
            .map(|row| (row.agent_id, number_key(&row.title)))
            .filter(|(id, key)| {
                self.assigned
                    .get(id)
                    .is_none_or(|(assigned, _)| assigned != key)
            })
            .collect::<Vec<_>>();
        unnumbered.sort();
        for (id, key) in unnumbered {
            let issued = self.issued.entry(key.clone()).or_default();
            *issued += 1;
            self.assigned.insert(id, (key, *issued));
        }
    }

    pub(super) fn get(&self, id: Uuid) -> Option<usize> {
        self.assigned.get(&id).map(|(_, number)| *number)
    }

    /// `id`'s number while another Agent this View has numbered reads alike.
    pub(super) fn shared(&self, id: Uuid) -> Option<usize> {
        let (key, number) = self.assigned.get(&id)?;
        self.assigned
            .iter()
            .any(|(other, (other_key, _))| *other != id && other_key == key)
            .then_some(*number)
    }
}

/// The fewest columns a list or cue gives a title it names; titles that read alike at this width
/// share one sequence of reading numbers.
const NUMBER_KEY_COLUMNS: usize = MIN_REQUEST_AGENT_TITLE_COLUMNS;

/// A title as it reads in `columns`: whole, or cut with the ellipsis taking the last column.
fn title_in(title: &str, columns: usize) -> String {
    ellipsize(&display_single_line(title), columns)
}

fn number_key(title: &str) -> String {
    title_in(title, NUMBER_KEY_COLUMNS)
}

/// Whether another of `rows` reads like `row` when both titles get `columns` columns.
fn shares_title_prefix<'a>(
    rows: impl IntoIterator<Item = &'a AgentRow>,
    row: &AgentRow,
    columns: usize,
) -> bool {
    let shown = title_in(&row.title, columns);
    rows.into_iter()
        .any(|other| other.agent_id != row.agent_id && title_in(&other.title, columns) == shown)
}

/// Rows whose shown titles another row's would read alike: identical titles, equal shown text,
/// or a cut title that another title continues. Each row comes with its title columns.
fn ambiguous_titles(rows: &[(&AgentRow, usize)]) -> HashSet<Uuid> {
    let titles = rows
        .iter()
        .map(|(row, columns)| {
            let title = display_single_line(&row.title);
            let shown = if display_width(&title) <= *columns {
                title.clone()
            } else {
                truncate_width(&title, columns.saturating_sub(1))
            };
            (row.agent_id, title, shown)
        })
        .collect::<Vec<_>>();
    titles
        .iter()
        .filter(|(id, title, shown)| {
            titles.iter().any(|(other, other_title, other_shown)| {
                other != id
                    && (other_title == title
                        || shown == other_shown && (shown != title || other_shown != other_title)
                        || shown != title && other_title.starts_with(shown.as_str()))
            })
        })
        .map(|(id, ..)| *id)
        .collect()
}

#[derive(Clone, Debug)]
pub(crate) struct AgentOverview {
    pub(crate) rows: Vec<AgentRow>,
    pub(crate) dispatch_directory: PathBuf,
    /// The Agent named by this overview's resolved creation receipt.
    pub(crate) created: Option<Uuid>,
    /// The Agent this View showed before opening the overview: Esc returns to it, its row is
    /// marked `current` and is never folded away.
    pub(crate) visible: Option<Uuid>,
}

impl AgentOverview {
    fn request_counts(&self) -> (usize, usize) {
        self.rows
            .iter()
            .filter(|row| row.parent_id.is_none())
            .fold((0, 0), |(approvals, questions), row| {
                (approvals + row.approvals, questions + row.questions)
            })
    }

    pub(super) fn row(&self, id: Uuid) -> Option<&AgentRow> {
        self.rows.iter().find(|row| row.agent_id == id)
    }

    fn group_of(&self, id: Uuid) -> Option<AttentionGroup> {
        self.row(id).map(|row| row.group)
    }
}

/// A selectable overview line: a group heading, an Agent row, or the counter that folds the
/// rest of the Done group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) enum OverviewTarget {
    Heading(AttentionGroup),
    Agent(Uuid),
    CurrentDraft,
    Disclosure(AttentionGroup),
}

enum OverviewEntry<'a> {
    Heading {
        group: AttentionGroup,
        families: usize,
        folded: bool,
        approvals: usize,
        questions: usize,
        /// The folded group holds the Agent Esc returns to.
        current: bool,
    },
    Agent {
        row: &'a AgentRow,
    },
    CurrentDraft,
    Disclosure {
        group: AttentionGroup,
        hidden: usize,
        open: bool,
    },
}

impl OverviewEntry<'_> {
    fn target(&self) -> OverviewTarget {
        match self {
            Self::Heading { group, .. } => OverviewTarget::Heading(*group),
            Self::Agent { row, .. } => OverviewTarget::Agent(row.agent_id),
            Self::CurrentDraft => OverviewTarget::CurrentDraft,
            Self::Disclosure { group, .. } => OverviewTarget::Disclosure(*group),
        }
    }
}

#[derive(Default)]
pub(super) struct OverviewState {
    pub(super) projection: Option<AgentOverview>,
    pub(super) selected: Option<OverviewTarget>,
    pub(super) peek: Option<Peek>,
    pub(super) current_draft: bool,
    /// Group heading choices (`true` folded); groups start unfolded. They only change what this
    /// View lists.
    pub(super) folded_groups: HashMap<AttentionGroup, bool>,
    /// Groups whose budget-folded remainder the user opened.
    pub(super) opened_groups: HashSet<AttentionGroup>,
    pub(super) offset: usize,
    pub(super) input_focused: bool,
    pub(super) project: Option<String>,
    pub(super) workspace: Option<String>,
    /// Tree lines below the overview heading in the latest frame.
    pub(super) capacity: usize,
    /// Bottom rows that come and go while typing (extra editor lines, candidates). The Done budget
    /// counts them as list rows, so typing scrolls the list instead of refolding it.
    pub(super) transient_rows: usize,
    selected_position: usize,
    /// Agents selected during this overview visit; they stay listed so leaving a row the Done
    /// budget would fold does not make it vanish under the cursor.
    visited: HashSet<Uuid>,
    /// Reading numbers shared with the family panel, so both name an Agent alike.
    pub(super) numbers: ReadingNumbers,
    /// The numbers the latest frame showed after titles.
    ordinals: HashMap<Uuid, usize>,
}

impl OverviewState {
    pub(super) fn update(&mut self, projection: Option<AgentOverview>) {
        let entering = self.projection.is_none();
        let group = self.selection_context();
        let screen_row = self
            .selected_line(&self.entries())
            .map(|line| line.saturating_sub(self.offset));
        if projection.is_none() {
            self.visited.clear();
        }
        self.projection = projection;
        let Some(projection) = &self.projection else {
            return;
        };
        self.numbers.register(&projection.rows);
        if entering && self.current_draft {
            self.folded_groups.insert(AttentionGroup::NeedsInput, false);
            self.selected = Some(OverviewTarget::CurrentDraft);
            self.settle_selection(Some(AttentionGroup::NeedsInput));
            self.offset = self.scroll_offset(&self.entries());
            return;
        }
        if entering && let Some(source) = projection.visible {
            self.locate(source);
            return;
        }
        self.settle_selection(group);
        // Rows arriving above keep the selection on its screen line.
        if let (Some(screen_row), Some(line)) = (screen_row, self.selected_line(&self.entries())) {
            self.offset = line.saturating_sub(screen_row);
        }
    }

    /// Entering the overview from a conversation selects that conversation's Agent, or where it
    /// is folded or filtered away: its nearest listed ancestor, else its group heading.
    fn locate(&mut self, source: Uuid) {
        let targets = self.targets();
        let group = self
            .projection
            .as_ref()
            .and_then(|projection| projection.group_of(source));
        let target = std::iter::once(source)
            .map(OverviewTarget::Agent)
            .chain(group.map(OverviewTarget::Heading))
            .find(|target| targets.contains(target));
        match target {
            Some(target) => {
                self.selected = Some(target);
                self.selected_position = targets
                    .iter()
                    .position(|candidate| *candidate == target)
                    .unwrap_or(0);
                self.offset = self.scroll_offset(&self.entries());
            }
            None => self.settle_selection(None),
        }
    }

    fn selected_line(&self, entries: &[OverviewEntry<'_>]) -> Option<usize> {
        let selected = self.selected?;
        entries.iter().position(|entry| entry.target() == selected)
    }

    /// The ended Agent row `id`, when that row is one.
    pub(super) fn ended_row(&self, id: Uuid) -> Option<&EndedRow> {
        self.projection.as_ref()?.row(id)?.ended.as_ref()
    }

    /// Selects the listed row of `id` and scrolls to it; `false` when no such row is listed.
    pub(super) fn select_agent(&mut self, id: Uuid) -> bool {
        if !self.targets().contains(&OverviewTarget::Agent(id)) {
            return false;
        }
        self.locate(id);
        true
    }

    /// Moves the selection to the next (`step` 1) or previous (-1) Agent row, past headings and
    /// counters; the peek panel follows it.
    pub(super) fn select_adjacent_agent(&mut self, step: isize) -> Option<Uuid> {
        let agents = self
            .targets()
            .into_iter()
            .filter_map(|target| match target {
                OverviewTarget::Agent(id) => Some(id),
                _ => None,
            })
            .collect::<Vec<_>>();
        let current = agents
            .iter()
            .position(|id| Some(*id) == self.selected_agent())?;
        let next = *agents.get(current.checked_add_signed(step)?)?;
        self.select_agent(next);
        Some(next)
    }

    /// The peek panel's rows: the Agent's state and how long it has waited, its latest sentence,
    /// and the request it waits on with the numbers that answer it. Also says whether the prompt
    /// and every option fit whole within the panel.
    fn peek_lines(&self, width: usize, now: u64) -> (Vec<RoleLine>, bool) {
        let Some((peek, row)) = self.peek.as_ref().and_then(|peek| {
            let row = self.projection.as_ref()?.row(peek.agent_id)?;
            Some((peek, row))
        }) else {
            return (Vec::new(), false);
        };
        let text_columns = width.saturating_sub(4).max(1);
        let fits = |text: &str, limit: usize| {
            super::wrap_text(&display_single_line(text), text_columns).len() <= limit
        };
        let wrapped = |text: &str, role: Role, limit: usize| {
            super::wrap_text(&display_single_line(text), text_columns)
                .into_iter()
                .take(limit)
                .map(move |line| RoleLine::role(format!("{ROW_INDENT}  {line}"), role))
                .collect::<Vec<_>>()
        };
        let status = match row.waiting_label(now) {
            Some(waiting) => format!("{} · {waiting}", display_single_line(&row.status)),
            None => display_single_line(&row.status),
        };
        let mut lines = vec![RoleLine::role("─".repeat(width), Role::MutedText)];
        let mut heading = RoleLine::role(
            format!("{ROW_INDENT}{}", display_single_line(&row.title)),
            Role::Title,
        );
        heading.push(" · ", Some(Role::MutedText));
        heading.push(status, Some(row.status_role()));
        heading.truncate(width);
        lines.push(heading);
        match row.preview.as_deref() {
            Some(preview) => lines.extend(wrapped(preview, Role::Text, 3)),
            None => lines.extend(wrapped("Nothing said yet", Role::MutedText, 1)),
        }
        let mut options_visible = false;
        if let Some(request) = &peek.request {
            options_visible = !request.options.is_empty() && fits(&request.prompt, 3);
            lines.extend(wrapped(&request.prompt, Role::Warning, 3));
            for (number, label) in request.options.iter().enumerate() {
                let option = format!("{}. {label}", number + 1);
                options_visible &= fits(&option, 1);
                lines.extend(wrapped(&option, Role::Text, 1));
            }
            options_visible &= lines.len() <= MAX_PEEK_ROWS;
            if request.options.is_empty() {
                lines.extend(wrapped("→ opens the Agent to answer", Role::MutedText, 1));
            }
        }
        if let Some(ended) = &row.ended {
            let note = if ended.resumable {
                "A reply loads its saved history, then sends"
            } else {
                "Its saved history is gone; nothing can continue it"
            };
            lines.extend(wrapped(note, Role::MutedText, 1));
        }
        lines.truncate(MAX_PEEK_ROWS);
        (lines, options_visible)
    }

    pub(super) fn selected_agent(&self) -> Option<Uuid> {
        match self.selected {
            Some(OverviewTarget::Agent(id)) => Some(id),
            _ => None,
        }
    }

    fn selection_context(&self) -> Option<AttentionGroup> {
        if self.selected == Some(OverviewTarget::CurrentDraft) {
            return Some(AttentionGroup::NeedsInput);
        }
        self.projection.as_ref()?.group_of(self.selected_agent()?)
    }

    fn settle_selection(&mut self, group: Option<AttentionGroup>) {
        let entries = self.entries();
        // A vanished group counter falls back to the last row it was folding.
        let counter_group = match self.selected {
            Some(OverviewTarget::Disclosure(group)) => Some(group),
            _ => None,
        };
        let last_in_counter_group = counter_group.and_then(|counter| {
            let mut current = None;
            let mut last = None;
            for entry in &entries {
                match entry {
                    OverviewEntry::Heading { group, .. } => current = Some(*group),
                    OverviewEntry::Agent { row, .. } if current == Some(counter) => {
                        last = Some(OverviewTarget::Agent(row.agent_id));
                    }
                    _ => {}
                }
            }
            last
        });
        let first_agent = entries.iter().find_map(|entry| match entry {
            OverviewEntry::Agent { row, .. } => Some(OverviewTarget::Agent(row.agent_id)),
            _ => None,
        });
        drop(entries);
        let targets = self.targets();
        if let Some(position) = self
            .selected
            .and_then(|selected| targets.iter().position(|target| *target == selected))
        {
            self.selected_position = position;
            return;
        }
        let fallback = group
            .map(OverviewTarget::Disclosure)
            .into_iter()
            .chain(
                group
                    .filter(|group| self.is_group_folded(*group))
                    .map(OverviewTarget::Heading),
            )
            .chain(last_in_counter_group)
            .find(|target| targets.contains(target))
            .or_else(|| {
                if self.selected.is_none() {
                    first_agent.or_else(|| targets.first().copied())
                } else {
                    targets
                        .get(self.selected_position.min(targets.len().saturating_sub(1)))
                        .copied()
                }
            });
        self.selected = fallback;
        self.selected_position = fallback
            .and_then(|selected| targets.iter().position(|target| *target == selected))
            .unwrap_or(0);
    }

    fn passes_filters(&self, row: &AgentRow) -> bool {
        self.project
            .as_ref()
            .is_none_or(|project| row.project == *project)
            && self
                .workspace
                .as_ref()
                .is_none_or(|workspace| row.workspace.as_ref() == Some(workspace))
    }

    /// Whether the user folded `group`; a folded group hides every row.
    fn is_group_folded(&self, group: AttentionGroup) -> bool {
        self.folded_groups.get(&group) == Some(&true)
    }

    /// The rows status cues point at: the Agent Esc returns to and the one this overview's receipt
    /// names.
    fn anchors(&self) -> HashSet<Uuid> {
        self.projection
            .as_ref()
            .map(|projection| {
                [projection.visible, projection.created]
                    .into_iter()
                    .flatten()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Rows the Done budget and a default group fold never hide; a fold the user chose does.
    fn pinned(&self) -> HashSet<Uuid> {
        let mut pinned = self.anchors();
        pinned.extend(self.selected_agent());
        pinned.extend(self.visited.iter().copied());
        pinned
    }

    fn entries(&self) -> Vec<OverviewEntry<'_>> {
        struct Member<'a> {
            pinned: bool,
            current: bool,
            approvals: usize,
            questions: usize,
            lines: Vec<OverviewEntry<'a>>,
        }
        let Some(projection) = &self.projection else {
            return Vec::new();
        };
        let pinned = self.pinned();
        let mut grouped = AttentionGroup::ALL
            .iter()
            .map(|group| (*group, Vec::<Member<'_>>::new()))
            .collect::<Vec<_>>();
        if self.current_draft
            && let Some((_, members)) = grouped
                .iter_mut()
                .find(|(group, _)| *group == AttentionGroup::NeedsInput)
        {
            members.push(Member {
                pinned: true,
                current: true,
                approvals: 0,
                questions: 0,
                lines: vec![OverviewEntry::CurrentDraft],
            });
        }
        for row in &projection.rows {
            let group = row.group;
            let member = Member {
                pinned: pinned.contains(&row.agent_id),
                current: projection.visible == Some(row.agent_id),
                approvals: row.approvals,
                questions: row.questions,
                lines: if self.passes_filters(row) {
                    vec![OverviewEntry::Agent { row }]
                } else {
                    Vec::new()
                },
            };
            if member.lines.is_empty() {
                continue;
            }
            if let Some((_, members)) = grouped
                .iter_mut()
                .find(|(candidate, _)| *candidate == group)
            {
                members.push(member);
            }
        }
        let pinned_lines = |members: &[Member<'_>]| {
            members
                .iter()
                .filter(|member| member.pinned)
                .map(|member| member.lines.len())
                .sum::<usize>()
        };
        let mut entries = Vec::new();
        for (group, members) in grouped {
            if members.is_empty() {
                continue;
            }
            let folded = self.is_group_folded(group);
            let open = self.opened_groups.contains(&group);
            let remaining = (self.capacity + self.transient_rows).saturating_sub(entries.len());
            let total = 1 + members
                .iter()
                .map(|member| member.lines.len())
                .sum::<usize>();
            // As Claude Code's Completed group: it lists what the screen holds, at least its
            // minimum, and folds the rest behind a counter. The rows the View points at (current,
            // created, selected or visited during this visit) stay, so none vanishes under the
            // cursor.
            let over_budget = group == AttentionGroup::Completed && !folded && total > remaining;
            let shown = if folded {
                vec![false; members.len()]
            } else if over_budget {
                let mut available = remaining
                    .saturating_sub(2)
                    .max(MIN_COMPLETED_ROWS)
                    .saturating_sub(pinned_lines(&members));
                let mut fits = true;
                members
                    .iter()
                    .map(|member| {
                        if open || member.pinned {
                            return true;
                        }
                        fits = fits && member.lines.len() <= available;
                        if fits {
                            available -= member.lines.len();
                        }
                        fits
                    })
                    .collect::<Vec<_>>()
            } else {
                vec![true; members.len()]
            };
            let hidden = shown.iter().filter(|shown| !**shown).count();
            entries.push(OverviewEntry::Heading {
                group,
                families: members.len(),
                folded,
                approvals: members.iter().map(|member| member.approvals).sum(),
                questions: members.iter().map(|member| member.questions).sum(),
                current: folded
                    && members
                        .iter()
                        .zip(&shown)
                        .any(|(member, shown)| member.current && !shown),
            });
            for (member, shown) in members.into_iter().zip(shown) {
                if shown {
                    entries.extend(member.lines);
                }
            }
            if over_budget && (open || hidden > 0) {
                entries.push(OverviewEntry::Disclosure {
                    group,
                    hidden,
                    open,
                });
            }
        }
        entries
    }

    fn targets(&self) -> Vec<OverviewTarget> {
        self.entries().iter().map(OverviewEntry::target).collect()
    }

    fn row_extras(
        row: &AgentRow,
        terminal_columns: usize,
        now: u64,
        display: DisplayProfile,
    ) -> RowExtras<'_> {
        RowExtras {
            glyph: row.state_glyph(display, now),
            glyph_role: row.glyph_role(),
            project: if terminal_columns >= WIDE_TERMINAL_COLUMNS {
                row.project.as_str()
            } else {
                ""
            },
            workspace: (terminal_columns >= 120)
                .then_some(row.workspace.as_deref())
                .flatten(),
            preview: row.preview.as_deref(),
            age: row.updated_at.map(|updated_at| age_label(updated_at, now)),
        }
    }

    /// Status candidates for a row, richest first; the row frame keeps the richest that fits. The
    /// Agent this View returns to says so in every candidate.
    fn row_status(
        &self,
        row: &AgentRow,
        terminal_columns: usize,
        now: u64,
    ) -> Vec<Vec<(String, Role)>> {
        let status = match row.waiting_label(now) {
            Some(waiting) => format!("{} · {waiting}", display_single_line(&row.status)),
            None => display_single_line(&row.status),
        };
        let mut variants =
            row.own_status_variants(vec![(status, row.status_role())], terminal_columns);
        let current = self
            .projection
            .as_ref()
            .is_some_and(|projection| projection.visible == Some(row.agent_id));
        if current {
            for variant in &mut variants {
                let mut marked = vec![(CURRENT_LABEL.to_owned(), Role::Accent)];
                if !variant.is_empty() {
                    marked.push((" · ".into(), Role::MutedText));
                }
                marked.append(variant);
                *variant = marked;
            }
            variants.push(vec![(CURRENT_LABEL.to_owned(), Role::Accent)]);
        }
        variants
    }

    /// Whether a status cue may name `row`'s title in fewer columns than its list row does: then
    /// the row and the cue both carry its number while another row reads alike at the cue's
    /// narrowest title.
    fn needs_cue_number(&self, row: &AgentRow) -> bool {
        self.projection.as_ref().is_some_and(|projection| {
            [projection.created, projection.visible].contains(&Some(row.agent_id))
                && shares_title_prefix(&projection.rows, row, NUMBER_KEY_COLUMNS)
        })
    }

    /// A row's title and, when it carries one, its reading number as the status cues name it.
    fn label(&self, row: &AgentRow) -> (String, Option<String>) {
        let number = self
            .numbers
            .get(row.agent_id)
            .filter(|_| self.needs_cue_number(row) || self.ordinals.contains_key(&row.agent_id))
            .map(|number| format!(" ({number})"));
        (display_single_line(&row.title), number)
    }

    fn hiding_filters(&self, row: &AgentRow) -> Option<(&'static str, &'static str)> {
        let project = self
            .project
            .as_ref()
            .is_some_and(|project| row.project != *project);
        let workspace = self
            .workspace
            .as_ref()
            .is_some_and(|workspace| row.workspace.as_ref() != Some(workspace));
        match (project, workspace) {
            (true, true) => Some(("Project and Workspace filters", "p/w")),
            (true, false) => Some(("Project filter", "p")),
            (false, true) => Some(("Workspace filter", "w")),
            (false, false) => None,
        }
    }

    /// Why a row is not listed: hidden by this View's filters, in a collapsed group, or folded
    /// under its parent.
    fn unlisted_details(&self, row: &AgentRow) -> Option<Vec<String>> {
        let collapsed = self
            .projection
            .as_ref()
            .and_then(|projection| projection.group_of(row.agent_id))
            .filter(|group| self.is_group_folded(*group));
        if let Some((filters, keys)) = self.hiding_filters(row) {
            let mut details = Vec::new();
            if !self.input_focused {
                details.push(format!("hidden by {filters} · {keys} cycles"));
            }
            details.extend([
                format!("hidden by {filters}"),
                "hidden by filter".into(),
                "hidden".into(),
            ]);
            Some(details)
        } else if let Some(group) = collapsed {
            Some(vec![
                format!("in collapsed {}", group.label()),
                "collapsed".into(),
            ])
        } else if !self
            .targets()
            .contains(&OverviewTarget::Agent(row.agent_id))
        {
            Some(vec!["folded under its parent".into(), "folded".into()])
        } else {
            None
        }
    }

    /// The status-line cue for this overview: receipt feedback for the created Agent, and where the
    /// Agent Esc returns to is when its row is not listed. Both name rows by readable labels; the
    /// receipt fact keeps its room first.
    pub(super) fn orientation_cue(&self, width: usize) -> Option<String> {
        let current = self.current_cue(usize::MAX);
        let Some(creation) = self.creation_cue(width) else {
            return current.map(|_| self.current_cue(width).unwrap_or_default());
        };
        let Some(current) = current else {
            return Some(creation);
        };
        let full = format!("{creation} · {current}");
        if display_width(&full) <= width {
            return Some(full);
        }
        // Both facts stay: the receipt shrinks toward its minimum before the current cue does,
        // then the delimiter narrows to one space.
        let minimum = self.minimum_creation_cue().unwrap_or_default();
        for separator in [" · ", " "] {
            let creation_room =
                width.saturating_sub(display_width(separator) + display_width(MINIMUM_CURRENT_CUE));
            if creation_room < display_width(&minimum) {
                continue;
            }
            let creation = self.creation_cue(creation_room).unwrap_or_default();
            let room = width.saturating_sub(display_width(&creation) + display_width(separator));
            let current = self.current_cue(room).unwrap_or_default();
            return Some(format!("{creation}{separator}{current}"));
        }
        Some(truncate_width(
            &format!("{minimum} {MINIMUM_CURRENT_CUE}"),
            width,
        ))
    }

    fn creation_cue(&self, width: usize) -> Option<String> {
        let projection = self.projection.as_ref()?;
        let created = projection.created?;
        let Some(row) = projection.rows.iter().find(|row| row.agent_id == created) else {
            return Some(truncate_width("New Agent accepted · not listed yet", width));
        };
        Some(match self.unlisted_details(row) {
            Some(details) => self.cue("Created", row, &details, width),
            None => self.cue_with_optional_detail("Created", row, &row.status, width),
        })
    }

    /// Where the Agent Esc returns to is, when its row is not listed.
    fn current_cue(&self, width: usize) -> Option<String> {
        let projection = self.projection.as_ref()?;
        let row = projection
            .rows
            .iter()
            .find(|row| Some(row.agent_id) == projection.visible)?;
        let details = self.unlisted_details(row)?;
        let cue = self.cue("Current", row, &details, width);
        // Below its shortest reason the cue keeps the fact, not a bare verb.
        Some(
            if details.iter().any(|reason| cue.ends_with(reason.as_str())) {
                cue
            } else {
                truncate_width(MINIMUM_CURRENT_CUE, width)
            },
        )
    }

    /// `verb` and a listed row's label, with its status while that fits: the readable title and
    /// number come before the optional status.
    fn cue_with_optional_detail(
        &self,
        verb: &str,
        row: &AgentRow,
        status: &str,
        width: usize,
    ) -> String {
        let (title, number) = self.label(row);
        let number = number.unwrap_or_default();
        let status = display_single_line(status);
        [
            format!("{verb} {title}{number} · {status}"),
            format!("{verb} {title}{number}"),
        ]
        .into_iter()
        .find(|cue| display_width(cue) <= width)
        .unwrap_or_else(|| {
            let title_columns = width.saturating_sub(display_width(&format!("{verb} {number}")));
            if title_columns == 0 {
                truncate_width(&format!("{verb}{number}"), width)
            } else {
                format!("{verb} {}{number}", ellipsize(&title, title_columns))
            }
        })
    }

    /// `verb` and the row's label with the first detail that fits. When none does, the title
    /// shrinks before its reading number, which stays whole beside the shortest detail.
    fn cue(&self, verb: &str, row: &AgentRow, details: &[String], width: usize) -> String {
        let (title, number) = self.label(row);
        let number = number.unwrap_or_default();
        if let Some(cue) = details
            .iter()
            .map(|detail| format!("{verb} {title}{number} · {detail}"))
            .find(|cue| display_width(cue) <= width)
        {
            return cue;
        }
        let detail = details.last().cloned().unwrap_or_default();
        let title_columns =
            width.saturating_sub(display_width(&format!("{verb} {number} · {detail}")));
        if title_columns == 0 {
            let bare = format!("{verb}{number}");
            let detailed = format!("{bare} · {detail}");
            return truncate_width(
                if display_width(&detailed) <= width {
                    &detailed
                } else {
                    &bare
                },
                width,
            );
        }
        truncate_width(
            &format!(
                "{verb} {}{number} · {detail}",
                ellipsize(&title, title_columns)
            ),
            width,
        )
    }

    /// The shortest receipt cue that still states the fact: `Created` with the row's number, or
    /// `New Agent accepted` while the row is not listed yet.
    fn minimum_creation_cue(&self) -> Option<String> {
        let projection = self.projection.as_ref()?;
        let created = projection.created?;
        Some(
            match projection.rows.iter().find(|row| row.agent_id == created) {
                Some(row) => format!("Created{}", self.label(row).1.unwrap_or_default()),
                None => "New Agent accepted".to_owned(),
            },
        )
    }

    /// The shortest form of `orientation_cue` that still states each of its facts.
    pub(super) fn minimum_orientation_cue(&self) -> Option<String> {
        let creation = self.minimum_creation_cue();
        let current = self
            .current_cue(usize::MAX)
            .map(|_| MINIMUM_CURRENT_CUE.to_owned());
        match (creation, current) {
            (Some(creation), Some(current)) => Some(format!("{creation} · {current}")),
            (creation, current) => creation.or(current),
        }
    }

    pub(super) fn move_selection(&mut self, amount: isize) {
        let targets = self.targets();
        if targets.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .and_then(|selected| targets.iter().position(|target| *target == selected))
            .unwrap_or(0);
        let next = current.saturating_add_signed(amount).min(targets.len() - 1);
        if let Some(id) = self.selected_agent() {
            self.visited.insert(id);
        }
        self.selected = Some(targets[next]);
        self.selected_position = next;
        self.offset = self.scroll_offset(&self.entries());
    }

    /// Space, or Enter on a heading or counter: folds or unfolds the selected group heading, the
    /// selected row's family, or the Done counter.
    pub(super) fn toggle_selected(&mut self) {
        match self.selected {
            Some(OverviewTarget::Heading(group)) => {
                let folded = self.is_group_folded(group);
                self.folded_groups.insert(group, !folded);
            }
            Some(OverviewTarget::Disclosure(group)) => {
                if !self.opened_groups.remove(&group) {
                    self.opened_groups.insert(group);
                }
                self.visited.clear();
                self.settle_selection(Some(group));
            }
            Some(OverviewTarget::CurrentDraft | OverviewTarget::Agent(_)) | None => {}
        }
        self.offset = self.scroll_offset(&self.entries());
    }

    /// Cycles the Project or Workspace filter. A selection the new filter still lists stays, a
    /// heading or counter included; any other falls back to the first listed Agent.
    pub(super) fn cycle_filter(&mut self, project: bool) {
        let Some(projection) = &self.projection else {
            return;
        };
        let mut values = projection
            .rows
            .iter()
            .filter_map(|row| {
                if project {
                    Some(row.project.clone())
                } else {
                    row.workspace.clone()
                }
            })
            .collect::<Vec<_>>();
        values.sort();
        values.dedup();
        let value = if project {
            &mut self.project
        } else {
            &mut self.workspace
        };
        *value = value
            .as_ref()
            .and_then(|current| values.iter().position(|entry| entry == current))
            .map_or_else(
                || values.first().cloned(),
                |index| values.get(index + 1).cloned(),
            );
        let targets = self.targets();
        if !self
            .selected
            .is_some_and(|selected| targets.contains(&selected))
        {
            self.selected = None;
            self.selected_position = 0;
            self.settle_selection(None);
        }
        self.offset = 0;
        self.offset = self.scroll_offset(&self.entries());
    }

    /// The first listed line that keeps the selection, and the heading right above it, in view.
    fn scroll_offset(&self, entries: &[OverviewEntry<'_>]) -> usize {
        let capacity = self.capacity.max(1);
        let Some(line) = self.selected_line(entries) else {
            return self.offset.min(entries.len().saturating_sub(capacity));
        };
        let top = if line > 0 && matches!(entries[line - 1], OverviewEntry::Heading { .. }) {
            line - 1
        } else {
            line
        };
        let offset = if top < self.offset {
            top
        } else if line >= self.offset + capacity {
            line + 1 - capacity
        } else {
            self.offset
        };
        offset.min(entries.len().saturating_sub(capacity))
    }

    /// What Enter and Space do on the selected line.
    pub(super) fn row_hints(&self) -> Vec<String> {
        match self.selected {
            Some(OverviewTarget::CurrentDraft) => vec!["enter/→ return".into()],
            Some(OverviewTarget::Heading(group)) => vec![if self.is_group_folded(group) {
                "enter/→ expand".to_owned()
            } else {
                "enter/→ collapse".to_owned()
            }],
            Some(OverviewTarget::Agent(id)) => match self.ended_row(id) {
                Some(ended) if ended.resumable => vec![
                    "enter resume".to_owned(),
                    "space peek".to_owned(),
                    "x remove".to_owned(),
                ],
                Some(_) => vec!["x remove".to_owned()],
                None => vec![
                    "enter open".to_owned(),
                    "space peek".to_owned(),
                    "x stop".to_owned(),
                ],
            },
            Some(OverviewTarget::Disclosure(group)) => self
                .entries()
                .iter()
                .find_map(|entry| match entry {
                    OverviewEntry::Disclosure {
                        group: candidate,
                        hidden,
                        open,
                    } if *candidate == group => Some(if *open {
                        "enter/→ show fewer".to_owned()
                    } else {
                        format!("enter/→ show {hidden}")
                    }),
                    _ => None,
                })
                .into_iter()
                .collect(),
            None => Vec::new(),
        }
    }

    /// A group heading. A folded heading keeps its request counts, then says that it holds the
    /// Agent Esc returns to; request counts and `current` shrink to `A1 Q1` and `cur` before they
    /// are dropped.
    fn heading_line(
        &self,
        width: usize,
        (group, families, folded, current): (AttentionGroup, usize, bool, bool),
        (approvals, questions): (usize, usize),
        display: DisplayProfile,
    ) -> RoleLine {
        let fold = display.glyph(if folded {
            Glyph::Collapsed
        } else {
            Glyph::Expanded
        });
        let mut line = RoleLine::role(
            format!("{ROW_INDENT}{fold} {} · {families}", group.label()),
            Role::Title,
        );
        let requests = |words: bool| {
            [("approvals", 'A', approvals), ("questions", 'Q', questions)]
                .into_iter()
                .filter(|(.., count)| folded && *count > 0)
                .map(|(word, letter, count)| {
                    if words {
                        format!("{word} {count}")
                    } else {
                        format!("{letter}{count}")
                    }
                })
                .collect::<Vec<_>>()
        };
        let current_forms: &[&str] = if current {
            &[CURRENT_LABEL, "cur"]
        } else {
            &[""]
        };
        let candidates = [true, false]
            .into_iter()
            .flat_map(|words| current_forms.iter().map(move |current| (words, *current)));
        let heading_width = display_width(&line.text());
        let mut details = Vec::new();
        for (words, current) in candidates {
            let mut candidate = requests(words)
                .into_iter()
                .map(|text| (text, Role::Permission))
                .collect::<Vec<_>>();
            if !current.is_empty() {
                candidate.push((current.to_owned(), Role::Accent));
            }
            let width_of = |segments: &[(String, Role)]| {
                segments
                    .iter()
                    .map(|(text, _)| display_width(text) + display_width(" · "))
                    .sum::<usize>()
            };
            if heading_width + width_of(&candidate) <= width {
                details = candidate;
                break;
            }
        }
        for (text, role) in details {
            line.push(" · ", Some(Role::MutedText));
            line.push(text, Some(role));
        }
        line.truncate(width);
        line
    }

    pub(super) fn lines(
        &mut self,
        width: usize,
        height: usize,
        terminal_height: usize,
        terminal_columns: usize,
        help_open: bool,
        display: DisplayProfile,
    ) -> Vec<RoleLine> {
        let Some(projection) = &self.projection else {
            return Vec::new();
        };
        let (approvals, questions) = projection.request_counts();
        let header_rows = if terminal_height >= TALL_LAYOUT_MIN_ROWS && height > 1 {
            2
        } else {
            1
        };
        let now = tickloop_harness::base::private_fs::unix_time_milliseconds();
        let (peek, options_visible) = self.peek_lines(width, now);
        let peek = if height.saturating_sub(header_rows) > peek.len() + MIN_COMPLETED_ROWS {
            peek
        } else {
            Vec::new()
        };
        if let Some(state) = self.peek.as_mut() {
            state.options_visible = options_visible && !peek.is_empty();
        }
        self.capacity = height.saturating_sub(header_rows + peek.len());
        let entries = self.entries();
        let offset = self.scroll_offset(&entries);
        // Folded groups and counters scrolled out of the list stay discoverable in the heading.
        let folded = entries
            .iter()
            .enumerate()
            .filter(|(line, _)| *line < offset || *line >= offset + self.capacity)
            .filter_map(|(_, entry)| match entry {
                OverviewEntry::Disclosure {
                    group,
                    hidden,
                    open: false,
                } => Some(format!("+{hidden} {}", group.label().to_lowercase())),
                OverviewEntry::Heading {
                    group,
                    families,
                    folded: true,
                    ..
                } => Some(format!("+{families} {}", group.label().to_lowercase())),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        let counts = format!("approvals {approvals} · questions {questions}");
        let heading = [
            format!("Agents · local · {counts}"),
            format!("Agents · local approvals {approvals} questions {questions}"),
            format!("Agents · {counts}"),
            counts,
        ]
        .into_iter()
        .find(|heading| display_width(heading) <= width)
        .unwrap_or_else(|| "Agents · approvals / questions · ? help".into());
        let heading = [
            format!("{heading} · folded {folded}"),
            format!("{heading} · {folded}"),
        ]
        .into_iter()
        .filter(|_| !folded.is_empty())
        .find(|candidate| display_width(candidate) <= width)
        .unwrap_or(heading);
        let mut lines = vec![RoleLine::role(
            truncate_width(&heading, width),
            Role::DialogTitle,
        )];
        if header_rows == 2 {
            let project = display_single_line(self.project.as_deref().unwrap_or("all"));
            let workspace = display_single_line(self.workspace.as_deref().unwrap_or("all"));
            let row_hints = self.row_hints();
            let filters = if terminal_columns < WIDE_TERMINAL_COLUMNS {
                let hint = if help_open { "" } else { " · ? help" };
                let value_columns =
                    width.saturating_sub(display_width(&format!("Project:  · Workspace: {hint}")));
                format!(
                    "Project: {} · Workspace: {}{hint}",
                    truncate_path_width(&project, value_columns / 2),
                    truncate_width(&workspace, value_columns - value_columns / 2),
                )
            } else {
                let value_columns = width.saturating_sub(display_width("Project:  · Workspace: "));
                let project_columns = (width / 3).min(value_columns / 2);
                let project = truncate_path_width(&project, project_columns);
                let workspace = truncate_width(&workspace, value_columns - project_columns);
                let hints = if help_open {
                    Vec::new()
                } else if self.peek.is_some() {
                    vec!["↑/↓ Agent".to_owned(), "→ open".into(), "esc close".into()]
                } else if self.input_focused {
                    vec!["tab list".to_owned(), "esc back".into()]
                } else {
                    row_hints
                        .into_iter()
                        .chain(
                            ["p/w filter", "r history", "tab new Agent", "esc back"]
                                .map(String::from),
                        )
                        .collect()
                };
                let mut text = format!("Project: {project} · Workspace: {workspace}");
                for hint in hints {
                    let next = format!("{text} · {hint}");
                    if display_width(&next) > width {
                        break;
                    }
                    text = next;
                }
                text
            };
            lines.push(RoleLine::role(
                truncate_width(&filters, width),
                Role::MutedText,
            ));
        }
        let rooms = entries
            .iter()
            .filter_map(|entry| match entry {
                OverviewEntry::Agent { row } => Some((
                    *row,
                    row.title_room(
                        width,
                        &Self::row_extras(row, terminal_columns, now, display),
                        &self.row_status(row, terminal_columns, now),
                        MIN_AGENT_TITLE_COLUMNS,
                    ),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        let ambiguous = ambiguous_titles(&rooms);
        let ordinals = rooms
            .iter()
            .filter(|(row, _)| ambiguous.contains(&row.agent_id) || self.needs_cue_number(row))
            .filter_map(|(row, _)| Some((row.agent_id, self.numbers.get(row.agent_id)?)))
            .collect::<HashMap<_, _>>();
        // The peeked row stays marked while the reply box holds the input.
        let selected_target = self
            .selected
            .filter(|_| !self.input_focused || self.peek.is_some());
        let tree_lines = entries
            .iter()
            .map(|entry| {
                let selected = Some(entry.target()) == selected_target;
                let line = match entry {
                    OverviewEntry::Heading {
                        group,
                        families,
                        folded,
                        approvals,
                        questions,
                        current,
                    } => self.heading_line(
                        width,
                        (*group, *families, *folded, *current),
                        (*approvals, *questions),
                        display,
                    ),
                    OverviewEntry::Agent { row } => row.summary_line(
                        width,
                        &Self::row_extras(row, terminal_columns, now, display),
                        ordinals.get(&row.agent_id).copied(),
                        &self.row_status(row, terminal_columns, now),
                        MIN_AGENT_TITLE_COLUMNS,
                    ),
                    OverviewEntry::CurrentDraft => current_draft_line(width, display),
                    OverviewEntry::Disclosure {
                        group,
                        hidden,
                        open,
                    } => {
                        let label = if *open {
                            format!("… show fewer {}", group.label().to_lowercase())
                        } else {
                            format!("… {hidden} more {}", group.label().to_lowercase())
                        };
                        RoleLine::role(
                            truncate_width(&format!("{ROW_INDENT}{label}"), width),
                            Role::MutedText,
                        )
                    }
                };
                if selected {
                    line.filled(Role::Selection)
                } else {
                    line
                }
            })
            .collect::<Vec<_>>();
        self.offset = offset;
        self.ordinals = ordinals;
        let capacity = self.capacity;
        lines.extend(tree_lines.into_iter().skip(offset).take(capacity));
        lines.resize(height.saturating_sub(peek.len()), RoleLine::default());
        lines.extend(peek);
        lines
    }
}

/// The word that marks the Agent this View returns to with Esc.
fn current_draft_line(width: usize, display: DisplayProfile) -> RoleLine {
    let glyph = display.glyph(Glyph::AgentNeedsInput);
    let prefix = format!("{ROW_INDENT}{glyph} New Agent");
    let mut line = RoleLine::role(ROW_INDENT, Role::Text);
    line.push(glyph, Some(Role::Permission));
    line.push(" New Agent", Some(Role::Text));
    let details = [
        "current session · send a prompt to start",
        "current session",
        "current",
    ]
    .into_iter()
    .find(|details| display_width(&format!("{prefix} · {details}")) <= width);
    if let Some(details) = details {
        line.push(" · ", Some(Role::MutedText));
        if let Some((current, hint)) = details.split_once(" · ") {
            line.push(current, Some(Role::Accent));
            line.push(format!(" · {hint}"), Some(Role::MutedText));
        } else {
            line.push(details, Some(Role::Accent));
        }
    }
    line.truncate(width);
    line
}

const CURRENT_LABEL: &str = "current";
/// The shortest status cue for an Agent Esc returns to that is not listed.
const MINIMUM_CURRENT_CUE: &str = "Current hidden";

pub(super) fn editor_capacity(height: usize) -> usize {
    (height / 4).clamp(3, 10)
}

pub(super) fn panel_capacity(height: usize) -> usize {
    (height / 6).clamp(3, 8)
}

pub(super) fn bottom_capacity(height: usize) -> usize {
    if height < NORMAL_LAYOUT_MIN_ROWS {
        height.saturating_sub(1).clamp(1, 4)
    } else {
        (height / 2).min(height.saturating_sub(MIN_CONVERSATION_ROWS))
    }
}

pub(super) fn help_lines(
    width: usize,
    terminal_columns: usize,
    overview: Option<&AgentOverview>,
) -> Vec<RoleLine> {
    let entries = if let Some(overview) = overview {
        let (approvals, questions) = overview.request_counts();
        vec![
            format!("A approvals {approvals}"),
            format!("Q questions {questions}"),
            "p filter project".into(),
            "w filter Workspace".into(),
            "tab list / new Agent".into(),
            "enter / → open Agent (row)".into(),
            "enter resume ended Agent (row)".into(),
            "space peek and reply (row)".into(),
            "↑/↓ next Agent, → open (peek)".into(),
            "1-9 enter answers a question (peek)".into(),
            "x twice stops Agent / removes ended row".into(),
            "enter / → fold group (heading)".into(),
            "enter dispatch (input)".into(),
            "r open history".into(),
            "/cwd PATH set dispatch directory".into(),
            "↑ list (empty input)".into(),
            "ctrl+g editor".into(),
        ]
    } else {
        [
            "← Agents (empty input)",
            "enter send",
            "ctrl+enter send now (working)",
            "ctrl+j newline",
            "ctrl+g editor",
            "A/Q approval/question",
            "↓ Subagents list (input end)",
            "Click Subagents N fold list",
            "ctrl+n/p command/Skill completion",
            "enter open Agent (list)",
            "space fold descendants (list)",
            "x dismiss finished (list)",
            "esc list → input",
            "→ Monitor (empty, no suggestion)",
            "shift+↑/↓ Monitor Shells",
            "enter open Shell (Monitor)",
            "x twice confirms (3 seconds)",
            "enter expand +N more (Scheduled)",
            "x twice removes scheduled task",
            "x twice stops Shell (Monitor)",
            "x twice closes/removes/unlinks",
            "enter reopen restores subscriptions",
            "f/h Artifact new/all feedback",
            "ctrl+n/p list navigation",
            "pgup/pgdn conversation",
            "ctrl+end latest",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>()
    };
    let columns = if terminal_columns < 80 {
        1
    } else if terminal_columns < 120 {
        2
    } else {
        3
    };
    let rows = entries.len().div_ceil(columns);
    let column_width = width / columns;
    let mut lines = (0..rows)
        .map(|row| {
            let text = (0..columns)
                .map(|column| {
                    let entry = entries
                        .get(row + column * rows)
                        .map(String::as_str)
                        .unwrap_or_default();
                    let mut text = truncate_width(entry, column_width.saturating_sub(2));
                    let padding = column_width
                        .saturating_sub(unicode_width::UnicodeWidthStr::width(text.as_str()));
                    text.push_str(&" ".repeat(padding));
                    text
                })
                .collect::<String>();
            RoleLine::role(truncate_width(&text, width), Role::MutedText)
        })
        .collect::<Vec<_>>();
    lines.push(RoleLine::role("esc / ? close help", Role::MutedText));
    lines
}
