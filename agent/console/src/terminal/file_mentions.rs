use std::cmp::Ordering;
use std::collections::VecDeque;
use std::fs;
use std::io;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use tickloop_agent_protocol::ExecutionRef;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) enum MentionKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct FamilyMentionTarget {
    pub execution: ExecutionRef,
    pub handle: String,
    pub description: String,
    pub available: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct AcceptedFamilyMention {
    pub target: FamilyMentionTarget,
    pub replacement_range: Range<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MentionCandidate {
    pub display_path: String,
    pub replacement: String,
    pub target: MentionTarget,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum MentionTarget {
    File {
        canonical_path: PathBuf,
        kind: MentionKind,
    },
    Agent(FamilyMentionTarget),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FileMentionCompletion {
    pub replacement_range: Range<usize>,
    pub candidates: Vec<MentionCandidate>,
    original_token: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AppliedMention {
    pub draft: String,
    pub cursor: usize,
    pub path: PathBuf,
    pub kind: MentionKind,
    pub token: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct AcceptedMention {
    pub token: String,
    pub canonical_path: PathBuf,
    pub kind: MentionKind,
    pub replacement_range: Range<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ValidatedMention {
    pub token: String,
    pub canonical_path: PathBuf,
    pub kind: MentionKind,
    pub replacement_range: Range<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct IndexGeneration(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum IndexReplacement {
    Replaced,
    Stale,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FileMentionKey {
    Previous,
    Next,
    Complete,
    Choose,
    Cancel,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum FileMentionAction {
    Ignored,
    Consumed,
    ReplaceDraft { draft: String, cursor: usize },
}

const CACHED_DIRECTORY_LIMIT: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum MentionDirectory {
    Roots,
    Path(PathBuf),
    // Explicit navigation is not an attachment or file-access grant.
    Browse(PathBuf),
    Home(PathBuf),
}

#[derive(Debug, Default)]
pub(super) struct FileMentionState {
    index: Option<FileMentionIndex>,
    cached_indexes: VecDeque<FileMentionIndex>,
    requested_directory: Option<MentionDirectory>,
    family_targets: Vec<FamilyMentionTarget>,
    accepted_family: Vec<AcceptedFamilyMention>,
    next_generation: u64,
    requested_generation: Option<IndexGeneration>,
    completion: Option<FileMentionCompletion>,
    query_canceled: bool,
    selected: Option<usize>,
    draft: String,
    cursor: usize,
    limit: usize,
    accepted: Vec<AcceptedMention>,
}

#[derive(Clone, Debug)]
pub(super) struct FileMentionIndex {
    directory: MentionDirectory,
    allowed_roots: Vec<PathBuf>,
    entries: Vec<IndexedEntry>,
}

#[derive(Clone, Debug)]
struct IndexedEntry {
    relative_path: Option<String>,
    absolute_path: String,
    canonical_path: PathBuf,
    kind: MentionKind,
}

#[derive(Clone, Debug)]
struct IgnoreRule {
    base_prefix: String,
    pattern: Vec<char>,
    directory_only: bool,
    negated: bool,
    anchored: bool,
}

impl FileMentionState {
    #[cfg(test)]
    pub(super) fn with_index(index: FileMentionIndex) -> Self {
        Self {
            index: Some(index),
            ..Self::default()
        }
    }

    pub(super) fn set_family_targets(&mut self, targets: Vec<FamilyMentionTarget>) {
        if self.family_targets != targets {
            self.family_targets = targets;
            self.refresh_completion();
        }
    }

    pub(super) fn clear_indexes(&mut self) {
        self.cached_indexes.clear();
        self.index = None;
        self.cancel_index_request();
        self.refresh_completion();
    }

    pub(super) fn cancel_index_request(&mut self) {
        self.requested_generation = None;
        self.requested_directory = None;
    }

    pub(super) fn requested_directory(&self) -> Option<MentionDirectory> {
        if self.query_canceled {
            return None;
        }
        MentionDirectory::for_query(&self.draft, self.cursor)
    }

    pub(super) fn request_directory(
        &mut self,
        directory: MentionDirectory,
    ) -> Option<IndexGeneration> {
        if self
            .index
            .as_ref()
            .is_some_and(|index| index.directory == directory)
        {
            return None;
        }
        if self.requested_directory.as_ref() == Some(&directory) {
            return self.requested_generation;
        }
        if let Some(index) = self.index.take() {
            self.cached_indexes.push_back(index);
            while self.cached_indexes.len() > CACHED_DIRECTORY_LIMIT {
                self.cached_indexes.pop_front();
            }
        }
        if let Some(position) = self
            .cached_indexes
            .iter()
            .position(|index| index.directory == directory)
        {
            self.index = self.cached_indexes.remove(position);
            self.cancel_index_request();
            self.refresh_completion();
            return None;
        }
        let generation = self.request_index_generation();
        self.requested_directory = Some(directory);
        self.refresh_completion();
        Some(generation)
    }

    pub(super) fn request_index_generation(&mut self) -> IndexGeneration {
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("file mention index generation exhausted");
        let generation = IndexGeneration(self.next_generation);
        self.requested_generation = Some(generation);
        self.index = None;
        self.dismiss();
        generation
    }

    pub(super) fn replace_index(
        &mut self,
        generation: IndexGeneration,
        index: FileMentionIndex,
    ) -> IndexReplacement {
        if self.requested_generation != Some(generation) {
            return IndexReplacement::Stale;
        }

        self.index = Some(index);
        self.requested_generation = None;
        self.refresh_completion();
        IndexReplacement::Replaced
    }

    pub(super) fn update_at(&mut self, draft: &str, cursor: usize, limit: usize) {
        let cursor = floor_char_boundary(draft, cursor.min(draft.len()));
        if self.draft != draft || self.cursor != cursor {
            self.query_canceled = false;
        }
        self.sync_accepted_mentions(draft);
        self.draft.clear();
        self.draft.push_str(draft);
        self.cursor = cursor;
        self.limit = limit;
        self.refresh_completion();
    }

    pub(super) fn dismiss(&mut self) {
        self.completion = None;
        self.selected = None;
    }

    pub(super) fn is_open(&self) -> bool {
        self.completion
            .as_ref()
            .is_some_and(|completion| !completion.candidates.is_empty())
    }

    pub(super) fn candidates(&self) -> &[MentionCandidate] {
        self.completion
            .as_ref()
            .map_or(&[], |completion| completion.candidates.as_slice())
    }

    pub(super) fn completing_recipient(&self) -> bool {
        self.completion.as_ref().is_some_and(|completion| {
            self.draft[..completion.replacement_range.start]
                .trim()
                .is_empty()
        })
    }

    pub(super) fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    pub(super) fn accepted_mentions(&self) -> impl ExactSizeIterator<Item = &AcceptedMention> {
        self.accepted.iter()
    }

    pub(super) fn accepted_family(&self) -> Option<&FamilyMentionTarget> {
        let start = self.draft.len() - self.draft.trim_start().len();
        self.accepted_family
            .iter()
            .find(|mention| mention.replacement_range.start == start)
            .map(|mention| &mention.target)
    }

    pub(super) fn accepted_family_mentions(&self) -> &[AcceptedFamilyMention] {
        &self.accepted_family
    }

    pub(super) fn take_family(&mut self) -> Vec<AcceptedFamilyMention> {
        std::mem::take(&mut self.accepted_family)
    }

    pub(super) fn restore_family(&mut self, mentions: Vec<AcceptedFamilyMention>) {
        self.accepted_family = mentions;
    }

    pub(super) fn take_accepted(&mut self) -> Vec<AcceptedMention> {
        std::mem::take(&mut self.accepted)
    }

    /// Reinstates mentions taken from `draft`, whose ranges still point into it.
    pub(super) fn restore_accepted(&mut self, draft: &str, accepted: Vec<AcceptedMention>) {
        if self.draft != draft {
            self.query_canceled = false;
        }
        self.draft.clear();
        self.draft.push_str(draft);
        self.accepted = accepted;
    }

    pub(super) fn handle_key(&mut self, key: FileMentionKey) -> FileMentionAction {
        let candidate_count = self.candidates().len();
        if candidate_count == 0 {
            return FileMentionAction::Ignored;
        }

        match key {
            FileMentionKey::Previous => {
                self.selected = Some(self.selected.map_or(0, |index| index.saturating_sub(1)));
                FileMentionAction::Consumed
            }
            FileMentionKey::Next => {
                self.selected = Some(
                    self.selected
                        .map_or(0, |index| (index + 1).min(candidate_count - 1)),
                );
                FileMentionAction::Consumed
            }
            FileMentionKey::Complete => {
                let candidate_index = self.selected.unwrap_or(0);
                self.accept_candidate(candidate_index)
            }
            FileMentionKey::Choose => {
                let Some(candidate_index) = self.selected else {
                    return FileMentionAction::Ignored;
                };
                self.accept_candidate(candidate_index)
            }
            FileMentionKey::Cancel => {
                self.query_canceled = true;
                self.dismiss();
                FileMentionAction::Consumed
            }
        }
    }

    pub(super) fn revalidate_all_for_submission(
        &mut self,
        draft: &str,
    ) -> io::Result<Vec<ValidatedMention>> {
        self.sync_accepted_mentions(draft);
        if self.accepted.is_empty() {
            return Ok(Vec::new());
        }
        let index = self
            .index
            .as_ref()
            .or_else(|| self.cached_indexes.back())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    "file mention index is not available",
                )
            })?;

        self.accepted
            .iter()
            .map(|accepted| {
                let applied = AppliedMention {
                    draft: draft.to_owned(),
                    cursor: accepted.replacement_range.end,
                    path: accepted.canonical_path.clone(),
                    kind: accepted.kind,
                    token: accepted.token.clone(),
                };
                let canonical_path = index.revalidate(&applied)?;
                Ok(ValidatedMention {
                    token: accepted.token.clone(),
                    canonical_path,
                    kind: accepted.kind,
                    replacement_range: accepted.replacement_range.clone(),
                })
            })
            .collect()
    }

    fn refresh_completion(&mut self) {
        if self.query_canceled {
            self.dismiss();
            return;
        }
        self.selected = None;
        let files = self
            .index
            .as_ref()
            .and_then(|index| index.complete(&self.draft, self.cursor, self.limit));
        self.completion = files;
        // As Claude Code, every `@` offers the family; only a leading one directs the message.
        if let Some(token) = mention_token(&self.draft, self.cursor) {
            let raw = &self.draft[token.start..token.cursor];
            if !raw.contains('/') && !raw.contains('\\') {
                let query = raw.trim_start_matches('@');
                let mut agents = self
                    .family_targets
                    .iter()
                    .filter(|target| target.available && target.handle.starts_with(query))
                    .take(self.limit)
                    .map(|target| MentionCandidate {
                        display_path: target.handle.clone(),
                        replacement: format!("@{}", target.handle),
                        target: MentionTarget::Agent(target.clone()),
                    })
                    .collect::<Vec<_>>();
                if !agents.is_empty() {
                    let mut completion =
                        self.completion
                            .take()
                            .unwrap_or_else(|| FileMentionCompletion {
                                replacement_range: token.start..token.end,
                                original_token: self.draft[token.start..token.end].into(),
                                candidates: Vec::new(),
                            });
                    agents.append(&mut completion.candidates);
                    agents.truncate(self.limit);
                    completion.candidates = agents;
                    self.completion = Some(completion);
                }
            }
        }
        if self
            .completion
            .as_ref()
            .is_some_and(|completion| completion.candidates.is_empty())
        {
            self.completion = None;
        }
    }

    pub(super) fn family_target_for_submission(
        &self,
        draft: &str,
    ) -> Result<Option<FamilyMentionTarget>, String> {
        let leading = draft.trim_start();
        let Some(token) = leading
            .split_whitespace()
            .next()
            .and_then(|token| token.strip_prefix('@'))
        else {
            return Ok(None);
        };
        if token.contains('/') || token.contains('\\') {
            return Ok(None);
        }
        let start = draft.len() - leading.len();
        if self
            .accepted
            .iter()
            .any(|file| file.replacement_range.start == start)
        {
            return Ok(None);
        }
        let selected = self
            .accepted_family()
            .filter(|target| target.handle == token);
        let current = self
            .family_targets
            .iter()
            .find(|target| target.handle == token);
        if let Some(selected) = selected
            && current.is_none_or(|current| current.execution != selected.execution)
        {
            return Err("selected Agent is no longer the same execution".into());
        }
        let Some(target) = current else {
            return Ok(None);
        };
        if !target.available {
            return Err("Agent target is unavailable or is the current Agent".into());
        }
        if leading
            .split_once(char::is_whitespace)
            .is_none_or(|(_, body)| body.trim().is_empty())
        {
            return Err("enter a message after the Agent target".into());
        }
        Ok(Some(target.clone()))
    }

    fn accept_candidate(&mut self, candidate_index: usize) -> FileMentionAction {
        if let Some(MentionCandidate {
            target: MentionTarget::Agent(target),
            ..
        }) = self.candidates().get(candidate_index)
        {
            let target = target.clone();
            let Some((mut draft, mut cursor)) = self
                .completion
                .as_ref()
                .and_then(|completion| completion.apply_text(&self.draft, candidate_index))
            else {
                return FileMentionAction::Ignored;
            };
            let range = cursor - target.handle.len() - 1..cursor;
            if draft[cursor..].starts_with(char::is_whitespace) {
                cursor += draft[cursor..].chars().next().unwrap().len_utf8();
            } else {
                draft.insert(cursor, ' ');
                cursor += 1;
            }
            self.sync_accepted_mentions(&draft);
            self.accepted
                .retain(|mention| mention.replacement_range != range);
            self.accepted_family
                .retain(|mention| mention.replacement_range != range);
            self.accepted_family.push(AcceptedFamilyMention {
                target,
                replacement_range: range,
            });
            self.draft.clone_from(&draft);
            self.cursor = cursor;
            self.dismiss();
            return FileMentionAction::ReplaceDraft { draft, cursor };
        }
        let Some(applied) = self
            .completion
            .as_ref()
            .and_then(|completion| completion.apply(&self.draft, candidate_index))
        else {
            self.dismiss();
            return FileMentionAction::Ignored;
        };
        let token_start = applied.cursor - applied.token.len();

        self.sync_accepted_mentions(&applied.draft);
        self.accepted_family
            .retain(|mention| mention.replacement_range != (token_start..applied.cursor));
        self.accepted.push(AcceptedMention {
            token: applied.token.clone(),
            canonical_path: applied.path,
            kind: applied.kind,
            replacement_range: token_start..applied.cursor,
        });
        self.draft.clone_from(&applied.draft);
        self.cursor = applied.cursor;
        self.dismiss();

        FileMentionAction::ReplaceDraft {
            draft: applied.draft,
            cursor: applied.cursor,
        }
    }

    fn sync_accepted_mentions(&mut self, draft: &str) {
        let edit = changed_range(&self.draft, draft);
        self.accepted.retain_mut(|mention| {
            sync_accepted_range(draft, &mention.token, &mut mention.replacement_range, &edit)
        });
        self.accepted_family.retain_mut(|mention| {
            sync_accepted_range(
                draft,
                &format!("@{}", mention.target.handle),
                &mut mention.replacement_range,
                &edit,
            )
        });
        self.draft.clear();
        self.draft.push_str(draft);
    }
}

#[derive(Clone, Copy, Debug)]
struct ChangedRange {
    old_start: usize,
    old_end: usize,
    delta: isize,
}

fn changed_range(old: &str, new: &str) -> ChangedRange {
    let prefix = old
        .chars()
        .zip(new.chars())
        .take_while(|(left, right)| left == right)
        .map(|(character, _)| character.len_utf8())
        .sum::<usize>();
    let shared_suffix = old[prefix..]
        .chars()
        .rev()
        .zip(new[prefix..].chars().rev())
        .take_while(|(left, right)| left == right)
        .map(|(character, _)| character.len_utf8())
        .sum::<usize>();
    let old_end = old.len() - shared_suffix;
    let new_end = new.len() - shared_suffix;

    ChangedRange {
        old_start: prefix,
        old_end,
        delta: new_end as isize - old_end as isize,
    }
}

fn shift_index(index: usize, delta: isize) -> Option<usize> {
    if delta >= 0 {
        index.checked_add(delta as usize)
    } else {
        index.checked_sub(delta.unsigned_abs())
    }
}

fn sync_accepted_range(
    draft: &str,
    text: &str,
    range: &mut Range<usize>,
    edit: &ChangedRange,
) -> bool {
    if range.end > edit.old_start {
        if range.start < edit.old_end {
            return false;
        }
        let Some(start) = shift_index(range.start, edit.delta) else {
            return false;
        };
        let Some(end) = shift_index(range.end, edit.delta) else {
            return false;
        };
        *range = start..end;
    }
    draft.get(range.clone()) == Some(text)
        && mention_token(draft, range.end)
            .is_some_and(|token| token.start == range.start && token.end == range.end)
}

impl MentionDirectory {
    pub(super) fn for_query(draft: &str, cursor: usize) -> Option<Self> {
        let token = mention_token(draft, cursor)?;
        let query = unescape_path_token(&draft[token.start + 1..token.cursor]);
        if query == "~" {
            return Some(Self::Home(PathBuf::new()));
        }
        let Some((parent, _)) = query.rsplit_once('/') else {
            return Some(Self::Roots);
        };
        if let Some(relative) = parent
            .strip_prefix("~/")
            .or_else(|| (parent == "~").then_some(""))
        {
            return Some(Self::Home(PathBuf::from(relative.trim_start_matches('/'))));
        }
        if is_parent_relative(&query) {
            return Some(Self::Browse(PathBuf::from(parent)));
        }
        if query.starts_with('/') {
            Some(Self::Browse(PathBuf::from(if parent.is_empty() {
                "/"
            } else {
                parent
            })))
        } else if parent == "." {
            Some(Self::Roots)
        } else {
            Some(Self::Path(PathBuf::from(parent)))
        }
    }
}

impl FileMentionIndex {
    #[cfg(test)]
    pub(super) fn build(
        working_directory: &Path,
        additional_directories: &[PathBuf],
    ) -> io::Result<Self> {
        Self::build_directory(
            working_directory,
            additional_directories,
            MentionDirectory::Roots,
            &|| false,
        )
    }

    pub(super) fn build_directory(
        working_directory: &Path,
        additional_directories: &[PathBuf],
        directory: MentionDirectory,
        canceled: &dyn Fn() -> bool,
    ) -> io::Result<Self> {
        Self::build_directory_with_home(
            working_directory,
            additional_directories,
            directory,
            std::env::home_dir().as_deref(),
            canceled,
        )
    }

    fn build_directory_with_home(
        working_directory: &Path,
        additional_directories: &[PathBuf],
        directory: MentionDirectory,
        home: Option<&Path>,
        canceled: &dyn Fn() -> bool,
    ) -> io::Result<Self> {
        check_scan_canceled(canceled)?;
        let working_directory = working_directory.canonicalize()?;
        let mut allowed_roots = vec![working_directory.clone()];
        for directory in additional_directories {
            check_scan_canceled(canceled)?;
            let canonical = directory.canonicalize()?;
            if !allowed_roots.contains(&canonical) {
                allowed_roots.push(canonical);
            }
        }
        let mut entries = Vec::new();
        let browsing = matches!(
            directory,
            MentionDirectory::Browse(_) | MentionDirectory::Home(_)
        );
        let directories = match &directory {
            MentionDirectory::Browse(path) => vec![working_directory.join(path)],
            MentionDirectory::Home(relative) => {
                let home = home.filter(|path| path.is_absolute()).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "home directory is unavailable")
                })?;
                vec![home.join(relative)]
            }
            MentionDirectory::Roots => allowed_roots.clone(),
            MentionDirectory::Path(path) => vec![if path.is_absolute() {
                path.clone()
            } else {
                working_directory.join(path)
            }],
        };
        for selected in directories {
            check_scan_canceled(canceled)?;
            // User-spelled aliases are later restored in the displayed path.
            if !is_safe_display_path(&selected) {
                continue;
            }
            let canonical = selected.canonicalize()?;
            let (root_index, root) = match allowed_roots
                .iter()
                .enumerate()
                .find(|(_, root)| canonical.starts_with(root))
            {
                Some(root) => root,
                None if browsing => (usize::MAX, &canonical),
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "file mention directory is outside the allowed roots",
                    ));
                }
            };
            if (if browsing {
                canonical.as_path()
            } else {
                canonical.strip_prefix(root).unwrap()
            })
            .components()
            .any(|component| component.as_os_str() == ".git")
            {
                return Ok(Self {
                    directory,
                    allowed_roots,
                    entries,
                });
            }
            // Match ignore rules against canonical paths even when navigation used a symlink.
            let scan_selected = if browsing { &canonical } else { &selected };
            let mut rules = Vec::new();
            let mut ancestor = root.clone();
            for component in scan_selected
                .strip_prefix(root)
                .unwrap_or_else(|_| canonical.strip_prefix(root).unwrap())
                .components()
            {
                check_scan_canceled(canceled)?;
                rules.extend(read_ignore_rules(
                    &ancestor,
                    ancestor.strip_prefix(root).unwrap(),
                )?);
                ancestor.push(component);
                if is_ignored(ancestor.strip_prefix(root).unwrap(), true, &rules) {
                    return Ok(Self {
                        directory,
                        allowed_roots,
                        entries,
                    });
                }
            }
            let first_entry = entries.len();
            scan_directory(
                root,
                scan_selected,
                root_index == 0,
                if browsing { None } else { Some(&allowed_roots) },
                &rules,
                &mut entries,
                canceled,
            )?;
            if browsing {
                for entry in &mut entries[first_entry..] {
                    let path = Path::new(entry.absolute_path.trim_end_matches('/'));
                    let name = path.file_name().expect("a scanned child has a filename");
                    let suffix = if entry.kind == MentionKind::Directory {
                        "/"
                    } else {
                        ""
                    };
                    entry.absolute_path =
                        format!("{}{suffix}", normalize_path(&selected.join(name)));
                }
            }
        }
        check_scan_canceled(canceled)?;
        entries.sort_by(|left, right| left.absolute_path.cmp(&right.absolute_path));
        entries.dedup_by(|left, right| left.canonical_path == right.canonical_path);
        Ok(Self {
            directory,
            allowed_roots,
            entries,
        })
    }

    pub(super) fn complete(
        &self,
        draft: &str,
        cursor: usize,
        limit: usize,
    ) -> Option<FileMentionCompletion> {
        if MentionDirectory::for_query(draft, cursor).as_ref() != Some(&self.directory) {
            return None;
        }
        let token = mention_token(draft, cursor)?;
        let query = unescape_path_token(&draft[token.start + 1..token.cursor]);
        let uses_home_path = query == "~" || query.starts_with("~/");
        let keeps_spelled_parent = uses_home_path || is_parent_relative(&query);
        let uses_absolute_path = query.starts_with('/') || keeps_spelled_parent;
        let uses_explicit_relative_path = query.starts_with("./");
        let query_for_matching = normalize_separators(
            query
                .strip_prefix("./")
                .filter(|_| uses_explicit_relative_path)
                .unwrap_or(&query),
        );

        let mut matches = self
            .entries
            .iter()
            .filter_map(|entry| {
                let indexed_path = if uses_absolute_path {
                    entry.absolute_path.as_str()
                } else {
                    entry.relative_path.as_deref()?
                };
                let (candidate, query) = if self.directory == MentionDirectory::Roots {
                    (indexed_path, query_for_matching.as_str())
                } else {
                    (
                        indexed_path
                            .trim_end_matches('/')
                            .rsplit('/')
                            .next()
                            .unwrap_or(indexed_path),
                        if query == "~" {
                            ""
                        } else {
                            query_for_matching
                                .rsplit('/')
                                .next()
                                .unwrap_or(&query_for_matching)
                        },
                    )
                };
                fuzzy_score(candidate, query).map(|score| (score, entry, indexed_path))
            })
            .collect::<Vec<_>>();

        // Explicit dot-prefix queries still rank hidden entries by match quality.
        let prefer_non_hidden = !query_for_matching
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .starts_with('.');
        matches.sort_by(
            |(left_score, left_entry, left_path), (right_score, right_entry, right_path)| {
                (prefer_non_hidden && is_hidden_entry(left_path))
                    .cmp(&(prefer_non_hidden && is_hidden_entry(right_path)))
                    .then_with(|| left_score.cmp(right_score))
                    .then_with(|| kind_rank(left_entry.kind).cmp(&kind_rank(right_entry.kind)))
                    .then_with(|| path_depth(left_path).cmp(&path_depth(right_path)))
                    .then_with(|| left_path.cmp(right_path))
            },
        );

        let candidates = matches
            .into_iter()
            .take(limit)
            .map(|(_, entry, path)| {
                let display_path = if keeps_spelled_parent {
                    let parent = query.rsplit_once('/').map_or("~", |(parent, _)| parent);
                    let name = path
                        .trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .unwrap_or(path);
                    let suffix = if entry.kind == MentionKind::Directory {
                        "/"
                    } else {
                        ""
                    };
                    format!("{parent}/{name}{suffix}")
                } else if uses_explicit_relative_path {
                    format!("./{path}")
                } else {
                    path.to_owned()
                };
                MentionCandidate {
                    replacement: format!("@{}", escape_path_token(&display_path)),
                    display_path,
                    target: MentionTarget::File {
                        canonical_path: entry.canonical_path.clone(),
                        kind: entry.kind,
                    },
                }
            })
            .collect();

        Some(FileMentionCompletion {
            replacement_range: token.start..token.end,
            original_token: draft[token.start..token.end].to_owned(),
            candidates,
        })
    }

    pub(super) fn revalidate(&self, mention: &AppliedMention) -> io::Result<PathBuf> {
        let canonical_path = mention.path.canonicalize()?;
        if !self
            .allowed_roots
            .iter()
            .any(|root| canonical_path.starts_with(root))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "file mention resolves outside the allowed directories",
            ));
        }

        let metadata = fs::metadata(&canonical_path)?;
        let current_kind = if metadata.is_dir() {
            MentionKind::Directory
        } else if metadata.is_file() {
            MentionKind::File
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file mention no longer resolves to a file or directory",
            ));
        };

        if current_kind != mention.kind {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file mention changed kind after selection",
            ));
        }

        Ok(canonical_path)
    }
}

impl FileMentionCompletion {
    fn apply_text(&self, draft: &str, candidate_index: usize) -> Option<(String, usize)> {
        if draft.get(self.replacement_range.clone())? != self.original_token {
            return None;
        }
        let current = mention_token(draft, self.replacement_range.end)?;
        if current.start != self.replacement_range.start
            || current.end != self.replacement_range.end
        {
            return None;
        }
        let candidate = self.candidates.get(candidate_index)?;
        let mut updated = draft.to_owned();
        updated.replace_range(self.replacement_range.clone(), &candidate.replacement);
        Some((
            updated,
            self.replacement_range.start + candidate.replacement.len(),
        ))
    }

    pub(super) fn apply(&self, draft: &str, candidate_index: usize) -> Option<AppliedMention> {
        let candidate = self.candidates.get(candidate_index)?;
        let MentionTarget::File {
            canonical_path,
            kind,
        } = &candidate.target
        else {
            return None;
        };
        let (draft, cursor) = self.apply_text(draft, candidate_index)?;
        Some(AppliedMention {
            draft,
            cursor,
            path: canonical_path.clone(),
            kind: *kind,
            token: candidate.replacement.clone(),
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct MentionToken {
    start: usize,
    cursor: usize,
    end: usize,
}

fn mention_token(draft: &str, cursor: usize) -> Option<MentionToken> {
    let cursor = floor_char_boundary(draft, cursor.min(draft.len()));
    let mut start = None;

    for (index, character) in draft[..cursor].char_indices().rev() {
        if character == '@' && (index == 0 || preceding_character_is_whitespace(draft, index)) {
            start = Some(index);
            break;
        }
        if character.is_whitespace() && !is_escaped(draft, index) {
            break;
        }
    }

    let start = start?;
    let end = draft[cursor..]
        .char_indices()
        .find_map(|(offset, character)| {
            let index = cursor + offset;
            (character.is_whitespace() && !is_escaped(draft, index)).then_some(index)
        })
        .unwrap_or(draft.len());

    Some(MentionToken { start, cursor, end })
}

fn preceding_character_is_whitespace(text: &str, index: usize) -> bool {
    text[..index]
        .chars()
        .next_back()
        .is_some_and(char::is_whitespace)
}

fn is_escaped(text: &str, index: usize) -> bool {
    text[..index]
        .chars()
        .rev()
        .take_while(|character| *character == '\\')
        .count()
        % 2
        == 1
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn escape_path_token(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for character in path.chars() {
        if character.is_whitespace() || character == '\\' {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn unescape_path_token(token: &str) -> String {
    let mut unescaped = String::with_capacity(token.len());
    let mut characters = token.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            if let Some(next) = characters.next() {
                unescaped.push(next);
            } else {
                unescaped.push(character);
            }
        } else {
            unescaped.push(character);
        }
    }
    unescaped
}

pub(super) fn draft_mentions_path(draft: &str, path: &Path) -> bool {
    let expected = path.to_string_lossy();
    draft.char_indices().any(|(index, character)| {
        if character != '@' {
            return false;
        }
        mention_token(draft, index + character.len_utf8()).is_some_and(|token| {
            token.start == index
                && unescape_path_token(&draft[index + character.len_utf8()..token.end]) == expected
        })
    })
}

fn scan_directory(
    root: &Path,
    directory: &Path,
    exposes_relative_paths: bool,
    allowed_roots: Option<&[PathBuf]>,
    inherited_rules: &[IgnoreRule],
    entries: &mut Vec<IndexedEntry>,
    canceled: &dyn Fn() -> bool,
) -> io::Result<()> {
    check_scan_canceled(canceled)?;
    let relative_directory = directory.strip_prefix(root).unwrap_or(Path::new(""));
    let mut rules = inherited_rules.to_vec();
    rules.extend(read_ignore_rules(directory, relative_directory)?);

    let mut children = Vec::new();
    for child in fs::read_dir(directory)? {
        check_scan_canceled(canceled)?;
        children.push(child?);
    }
    children.sort_by_key(fs::DirEntry::file_name);

    for child in children {
        check_scan_canceled(canceled)?;
        let file_name = child.file_name();
        if file_name == ".git" {
            continue;
        }

        let path = child.path();
        let relative_path = path.strip_prefix(root).unwrap_or(&path);
        if !is_safe_display_path(&path) || !is_safe_display_path(relative_path) {
            continue;
        }
        let symlink_metadata = fs::symlink_metadata(&path)?;
        let metadata = if symlink_metadata.file_type().is_symlink() {
            match fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            }
        } else {
            symlink_metadata
        };
        let kind = if metadata.is_dir() {
            MentionKind::Directory
        } else if metadata.is_file() {
            MentionKind::File
        } else {
            continue;
        };

        if is_ignored(relative_path, kind == MentionKind::Directory, &rules) {
            continue;
        }

        let canonical_path = match path.canonicalize() {
            Ok(path) => path,
            Err(_) => continue,
        };
        if allowed_roots
            .is_some_and(|roots| !roots.iter().any(|root| canonical_path.starts_with(root)))
        {
            continue;
        }

        let suffix = if kind == MentionKind::Directory {
            "/"
        } else {
            ""
        };
        let absolute_path = format!("{}{}", normalize_path(&path), suffix);
        let relative_path_text =
            exposes_relative_paths.then(|| format!("{}{}", normalize_path(relative_path), suffix));
        entries.push(IndexedEntry {
            relative_path: relative_path_text,
            absolute_path,
            canonical_path,
            kind,
        });
    }

    Ok(())
}

fn check_scan_canceled(canceled: &dyn Fn() -> bool) -> io::Result<()> {
    if canceled() {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "file mention scan canceled",
        ))
    } else {
        Ok(())
    }
}

fn read_ignore_rules(directory: &Path, base: &Path) -> io::Result<Vec<IgnoreRule>> {
    let ignore_file = directory.join(".gitignore");
    let contents = match fs::read_to_string(ignore_file) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    Ok(contents
        .lines()
        .filter_map(|line| parse_ignore_rule(base, line))
        .collect())
}

fn parse_ignore_rule(base: &Path, line: &str) -> Option<IgnoreRule> {
    let mut pattern = line.trim_end().to_owned();
    if pattern.is_empty() || pattern.starts_with('#') {
        return None;
    }

    let negated = pattern.starts_with('!');
    if negated || pattern.starts_with("\\!") || pattern.starts_with("\\#") {
        pattern.remove(0);
    }
    if pattern.is_empty() {
        return None;
    }

    let directory_only = pattern.ends_with('/');
    if directory_only {
        pattern.pop();
    }
    let anchored = pattern.starts_with('/');
    if anchored {
        pattern.remove(0);
    }
    if pattern.is_empty() {
        return None;
    }

    let base = normalize_path(base);
    Some(IgnoreRule {
        base_prefix: if base.is_empty() {
            base
        } else {
            format!("{base}/")
        },
        pattern: pattern.chars().collect(),
        directory_only,
        negated,
        anchored,
    })
}

fn is_ignored(relative_path: &Path, is_directory: bool, rules: &[IgnoreRule]) -> bool {
    let path = normalize_path(relative_path);
    rules.iter().fold(false, |ignored, rule| {
        if rule.matches(&path, is_directory) {
            !rule.negated
        } else {
            ignored
        }
    })
}

impl IgnoreRule {
    fn matches(&self, relative_path: &str, is_directory: bool) -> bool {
        let Some(path) = relative_path.strip_prefix(&self.base_prefix) else {
            return false;
        };
        if path.is_empty() {
            return false;
        }

        if self.pattern.contains(&'/') || self.anchored {
            return glob_matches(&self.pattern, path)
                || (self.directory_only
                    && directory_prefixes(path).any(|prefix| glob_matches(&self.pattern, prefix)));
        }

        path.split('/').enumerate().any(|(index, component)| {
            if !glob_matches(&self.pattern, component) {
                return false;
            }
            !self.directory_only || index + 1 < path.split('/').count() || is_directory
        })
    }
}

fn directory_prefixes(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(|(index, _)| &path[..index])
}

fn glob_matches(pattern: &[char], candidate: &str) -> bool {
    if !pattern
        .iter()
        .any(|character| matches!(character, '*' | '?'))
    {
        return candidate.chars().eq(pattern.iter().copied());
    }
    let candidate = candidate.chars().collect::<Vec<_>>();
    let mut reachable = vec![false; candidate.len() + 1];
    let mut next = vec![false; candidate.len() + 1];
    reachable[0] = true;
    for (pattern_index, character) in pattern.iter().enumerate() {
        next.fill(false);
        for candidate_index in 0..=candidate.len() {
            if !reachable[candidate_index] {
                continue;
            }
            match character {
                '*' => {
                    next[candidate_index] = true;
                    let crosses_directories = pattern.get(pattern_index + 1) == Some(&'*')
                        || pattern.get(pattern_index.wrapping_sub(1)) == Some(&'*');
                    if candidate_index < candidate.len()
                        && (crosses_directories || candidate[candidate_index] != '/')
                    {
                        reachable[candidate_index + 1] = true;
                    }
                }
                '?' if candidate_index < candidate.len() && candidate[candidate_index] != '/' => {
                    next[candidate_index + 1] = true
                }
                literal if candidate.get(candidate_index) == Some(literal) => {
                    next[candidate_index + 1] = true
                }
                _ => {}
            }
        }
        std::mem::swap(&mut reachable, &mut next);
    }
    reachable[candidate.len()]
}

fn fuzzy_score(candidate: &str, query: &str) -> Option<FuzzyScore> {
    if query.is_empty() {
        return Some(FuzzyScore {
            class: 3,
            gaps: 0,
            start: 0,
        });
    }

    let candidate_lower = candidate.to_lowercase();
    let query_lower = query.to_lowercase();
    if candidate_lower == query_lower {
        return Some(FuzzyScore {
            class: 0,
            gaps: 0,
            start: 0,
        });
    }
    if candidate_lower.starts_with(&query_lower) {
        return Some(FuzzyScore {
            class: 1,
            gaps: candidate_lower.len() - query_lower.len(),
            start: 0,
        });
    }

    let basename = candidate_lower
        .rsplit('/')
        .next()
        .unwrap_or(&candidate_lower);
    if basename.starts_with(&query_lower) {
        return Some(FuzzyScore {
            class: 2,
            gaps: basename.len() - query_lower.len(),
            start: candidate_lower.len() - basename.len(),
        });
    }

    let mut candidate_characters = candidate_lower.char_indices();
    let mut first = None;
    let mut last = 0;
    for query_character in query_lower.chars() {
        let (index, _) = candidate_characters
            .find(|(_, candidate_character)| *candidate_character == query_character)?;
        first.get_or_insert(index);
        last = index;
    }

    Some(FuzzyScore {
        class: 3,
        gaps: last.saturating_sub(first.unwrap_or(0)) + 1 - query_lower.chars().count(),
        start: first.unwrap_or(0),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FuzzyScore {
    class: u8,
    gaps: usize,
    start: usize,
}

impl Ord for FuzzyScore {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.class, self.gaps, self.start).cmp(&(other.class, other.gaps, other.start))
    }
}

impl PartialOrd for FuzzyScore {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn is_parent_relative(query: &str) -> bool {
    query.starts_with("../")
}

fn is_hidden_entry(path: &str) -> bool {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .is_some_and(|name| name.starts_with('.'))
}

fn kind_rank(kind: MentionKind) -> u8 {
    match kind {
        MentionKind::Directory => 0,
        MentionKind::File => 1,
    }
}

fn path_depth(path: &str) -> usize {
    path.bytes().filter(|byte| *byte == b'/').count()
}

fn normalize_path(path: &Path) -> String {
    let is_absolute = path.is_absolute();
    let normalized = path
        .components()
        .filter_map(|component| match component {
            Component::RootDir => None,
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            Component::ParentDir => Some("..".to_owned()),
            Component::CurDir | Component::Prefix(_) => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    if is_absolute {
        format!("/{normalized}")
    } else {
        normalized
    }
}

fn is_safe_display_path(path: &Path) -> bool {
    path.to_str()
        .is_some_and(|path| !path.chars().any(char::is_control))
}

fn normalize_separators(path: &str) -> String {
    path.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "tickloop-file-mentions-{}-{unique}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, AtomicOrdering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn file(&self, relative: &str) {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, "fixture contents must never be read by the index").unwrap();
        }

        fn directory(&self, relative: &str) {
            fs::create_dir_all(self.root.join(relative)).unwrap();
        }

        fn index(&self) -> FileMentionIndex {
            FileMentionIndex::build(&self.root, &[]).unwrap()
        }

        fn index_for(&self, query: &str) -> FileMentionIndex {
            FileMentionIndex::build_directory(
                &self.root,
                &[],
                MentionDirectory::for_query(query, query.len()).unwrap(),
                &|| false,
            )
            .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn explicit_absolute_navigation_does_not_grant_attachment_access() {
        let project = Fixture::new();
        let outside = Fixture::new();
        outside.file("notes.txt");
        outside.file("nested/deep.txt");
        let root = project.index_for("@/");
        assert!(!root.complete("@/", 2, 100).unwrap().candidates.is_empty());
        assert_eq!(
            root.allowed_roots,
            vec![project.root.canonicalize().unwrap()]
        );

        let query = format!("@{}/", outside.root.display());
        let index = project.index_for(&query);
        let completion = index.complete(&query, query.len(), 100).unwrap();
        assert_eq!(completion.candidates.len(), 2);
        assert!(
            !completion
                .candidates
                .iter()
                .any(|entry| entry.display_path.ends_with("deep.txt"))
        );
        let candidate = completion
            .candidates
            .iter()
            .position(|entry| entry.display_path.ends_with("notes.txt"))
            .unwrap();
        let applied = completion.apply(&query, candidate).unwrap();
        assert_eq!(
            index.revalidate(&applied).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let mut state = FileMentionState::with_index(index);
        state.update_at(&query, query.len(), 100);
        state.selected = Some(candidate);
        let FileMentionAction::ReplaceDraft { draft, .. } =
            state.handle_key(FileMentionKey::Choose)
        else {
            panic!("path navigation must allow choosing an outside candidate");
        };
        assert_eq!(
            state
                .revalidate_all_for_submission(&draft)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(project.index().allowed_roots, root.allowed_roots);
    }

    #[test]
    fn parent_navigation_preserves_prefix_and_requires_separate_attachment_authorization() {
        let workspace = Fixture::new();
        workspace.file("project/main.rs");
        workspace.file("sibling/notes.txt");
        workspace.file("sibling/nested/deep.txt");
        let project = workspace.root.join("project");
        let build = |query: &str, additional: &[PathBuf]| {
            FileMentionIndex::build_directory(
                &project,
                additional,
                MentionDirectory::for_query(query, query.len()).unwrap(),
                &|| false,
            )
            .unwrap()
        };

        let query = "@../";
        let completion = build(query, &[]).complete(query, query.len(), 100).unwrap();
        let paths = completion
            .candidates
            .iter()
            .map(|candidate| candidate.display_path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["../project/", "../sibling/"]);

        let query = "@../sibling/no";
        let index = build(query, &[]);
        let completion = index.complete(query, query.len(), 100).unwrap();
        assert_eq!(completion.candidates.len(), 1);
        let applied = completion.apply(query, 0).unwrap();
        assert_eq!(applied.token, "@../sibling/notes.txt");
        assert_eq!(
            applied.path,
            workspace
                .root
                .join("sibling/notes.txt")
                .canonicalize()
                .unwrap()
        );
        assert_eq!(
            index.revalidate(&applied).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );

        let authorized = build(query, &[workspace.root.join("sibling")]);
        let applied = authorized
            .complete(query, query.len(), 100)
            .unwrap()
            .apply(query, 0)
            .unwrap();
        assert_eq!(authorized.revalidate(&applied).unwrap(), applied.path);

        assert_eq!(
            MentionDirectory::for_query("@../../", 7),
            Some(MentionDirectory::Browse(PathBuf::from("../..")))
        );
        assert_eq!(
            MentionDirectory::for_query("@src/../", 8),
            Some(MentionDirectory::Path(PathBuf::from("src/..")))
        );
    }

    #[test]
    fn home_navigation_preserves_prefix_and_requires_separate_attachment_authorization() {
        let project = Fixture::new();
        let home = Fixture::new();
        home.file("目录 space/notes.txt");
        home.file("目录 space/nested/deep.txt");
        for query in ["@~", "@~/", "@~/目录\\ space/"] {
            let index = FileMentionIndex::build_directory_with_home(
                &project.root,
                &[],
                MentionDirectory::for_query(query, query.len()).unwrap(),
                Some(&home.root),
                &|| false,
            )
            .unwrap();
            let completion = index.complete(query, query.len(), 100).unwrap();
            assert!(!completion.candidates.is_empty());
            assert!(
                completion
                    .candidates
                    .iter()
                    .all(|entry| entry.display_path.starts_with("~/"))
            );
            assert!(
                !completion
                    .candidates
                    .iter()
                    .any(|entry| entry.display_path.ends_with("deep.txt"))
            );
            let applied = completion.apply(query, 0).unwrap();
            assert!(applied.token.starts_with("@~/"));
            assert_eq!(
                index.revalidate(&applied).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
            assert_eq!(
                index.allowed_roots,
                vec![project.root.canonicalize().unwrap()]
            );
        }
        let query = "@~/目录\\ space/notes";
        let index = FileMentionIndex::build_directory_with_home(
            &project.root,
            std::slice::from_ref(&home.root),
            MentionDirectory::for_query(query, query.len()).unwrap(),
            Some(&home.root),
            &|| false,
        )
        .unwrap();
        let completion = index.complete(query, query.len(), 100).unwrap();
        let applied = completion.apply(query, 0).unwrap();
        assert_eq!(
            index.revalidate(&applied).unwrap(),
            home.root
                .join("目录 space/notes.txt")
                .canonicalize()
                .unwrap()
        );
        assert!(
            index
                .complete(&applied.draft, applied.cursor, 100)
                .is_some()
        );
    }

    #[test]
    fn home_navigation_rejects_missing_home_and_does_not_expand_other_users() {
        let project = Fixture::new();
        for home in [None, Some(Path::new("")), Some(Path::new("relative"))] {
            assert_eq!(
                FileMentionIndex::build_directory_with_home(
                    &project.root,
                    &[],
                    MentionDirectory::Home(PathBuf::new()),
                    home,
                    &|| false,
                )
                .unwrap_err()
                .kind(),
                io::ErrorKind::NotFound
            );
        }
        assert_eq!(
            MentionDirectory::for_query("@~someone/", 10),
            Some(MentionDirectory::Path(PathBuf::from("~someone")))
        );
        assert_eq!(
            MentionDirectory::for_query("@~//nested/", 11),
            Some(MentionDirectory::Home(PathBuf::from("nested")))
        );
    }

    #[cfg(unix)]
    #[test]
    fn explicit_navigation_follows_directory_aliases_without_authorizing_the_target() {
        use std::os::unix::fs::symlink;
        let project = Fixture::new();
        let outside = Fixture::new();
        outside.file("notes.txt");
        outside.file("secret");
        outside.file("bad\nname.txt");
        outside.file(".git/config");
        fs::write(outside.root.join(".gitignore"), "/secret\n").unwrap();
        symlink(&outside.root, project.root.join("alias")).unwrap();
        let query = format!("@{}/alias/", project.root.display());
        let index = project.index_for(&query);
        let completion = index.complete(&query, query.len(), 100).unwrap();
        assert!(!completion.candidates.iter().any(|entry| {
            entry.display_path.ends_with("/secret")
                || entry.display_path.contains('\n')
                || entry.display_path.ends_with("/.git/")
        }));
        let candidate = completion
            .candidates
            .iter()
            .position(|entry| entry.display_path.ends_with("/notes.txt"))
            .unwrap();
        let applied = completion.apply(&query, candidate).unwrap();
        assert!(applied.token.contains("/alias/notes.txt"));
        assert_eq!(
            applied.path,
            outside.root.join("notes.txt").canonicalize().unwrap()
        );
        assert_eq!(
            index.revalidate(&applied).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        // Implicit project completion still excludes the escaping link.
        assert!(
            !project
                .index()
                .complete("@", 1, 100)
                .unwrap()
                .candidates
                .iter()
                .any(|entry| entry.display_path == "alias/")
        );
        let unsafe_alias = project.root.join("alias\u{1b}[31m");
        symlink(&outside.root, &unsafe_alias).unwrap();
        for directory in [
            MentionDirectory::Browse(unsafe_alias.clone()),
            MentionDirectory::Home(PathBuf::new()),
        ] {
            let index = FileMentionIndex::build_directory_with_home(
                &project.root,
                &[],
                directory,
                Some(&unsafe_alias),
                &|| false,
            )
            .unwrap();
            assert!(
                index.entries.is_empty(),
                "unsafe aliases must not reappear in displayed paths"
            );
        }
    }

    #[test]
    fn navigation_results_are_isolated_from_stale_generations_and_implicit_completion() {
        let project = Fixture::new();
        let outside = Fixture::new();
        outside.file("outside.txt");
        project.file("project.txt");
        let query = format!("@{}/", outside.root.display());
        let directory = MentionDirectory::for_query(&query, query.len()).unwrap();
        let mut state = FileMentionState::default();
        state.update_at(&query, query.len(), 100);
        let stale = state.request_directory(directory.clone()).unwrap();
        state.update_at("@", 1, 100);
        let current = state.request_directory(MentionDirectory::Roots).unwrap();
        assert_eq!(
            state.replace_index(stale, project.index_for(&query)),
            IndexReplacement::Stale
        );
        assert!(state.candidates().is_empty());
        assert_eq!(
            state.replace_index(current, project.index()),
            IndexReplacement::Replaced
        );
        assert!(
            state
                .candidates()
                .iter()
                .all(|entry| entry.display_path != "outside.txt")
        );
        state.update_at(&query, query.len(), 100);
        let current = state.request_directory(directory).unwrap();
        assert_eq!(
            state.replace_index(current, project.index_for(&query)),
            IndexReplacement::Replaced
        );
        assert!(
            state
                .candidates()
                .iter()
                .any(|entry| entry.display_path.ends_with("outside.txt"))
        );
        state.update_at("@", 1, 100);
        assert!(state.request_directory(MentionDirectory::Roots).is_none());
        assert_eq!(state.candidates().len(), 1);
        assert_eq!(state.candidates()[0].display_path, "project.txt");
    }

    #[test]
    fn every_at_offers_family_targets_before_files_and_explicit_paths_stay_files() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("audit.txt"), "file").unwrap();
        let index = FileMentionIndex::build(directory.path(), &[]).unwrap();
        let mut state = FileMentionState::with_index(index);
        let target = FamilyMentionTarget {
            execution: tickloop_agent_protocol::ExecutionRef {
                daemon_epoch: uuid::Uuid::new_v4(),
                agent_id: uuid::Uuid::new_v4(),
                generation: 1,
            },
            handle: "audit".into(),
            description: "Audit evidence".into(),
            available: true,
        };
        state.set_family_targets(vec![target.clone()]);
        state.update_at("@a", 2, 15);
        assert_eq!(state.candidates()[0].display_path, "audit");
        state.update_at("words @a", 8, 15);
        assert_eq!(state.candidates()[0].display_path, "audit");
        state.update_at("@audit and @a", 13, 15);
        assert_eq!(
            state.candidates()[0].display_path,
            "audit",
            "a later `@` offers the family again"
        );
        state.update_at("@./a", 5, 15);
        assert!(
            state
                .candidates()
                .iter()
                .all(|row| row.display_path != "audit")
        );
    }

    #[test]
    fn only_a_leading_family_mention_directs_the_message() {
        let directory = tempfile::tempdir().unwrap();
        let mut state =
            FileMentionState::with_index(FileMentionIndex::build(directory.path(), &[]).unwrap());
        state.set_family_targets(vec![FamilyMentionTarget {
            execution: tickloop_agent_protocol::ExecutionRef {
                daemon_epoch: uuid::Uuid::new_v4(),
                agent_id: uuid::Uuid::new_v4(),
                generation: 1,
            },
            handle: "audit".into(),
            description: "Audit evidence".into(),
            available: true,
        }]);
        state.update_at("ask @a", 6, 15);
        let FileMentionAction::ReplaceDraft { draft, cursor } =
            state.handle_key(FileMentionKey::Complete)
        else {
            panic!("the later mention completes");
        };
        assert_eq!((draft.as_str(), cursor), ("ask @audit ", 11));
        assert!(state.accepted_family().is_none());
        assert!(
            state
                .family_target_for_submission("ask @audit to check")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn selected_agent_and_file_collisions_keep_identity_and_reject_empty_stale_or_self_targets() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("audit"), "evidence").unwrap();
        let mut state =
            FileMentionState::with_index(FileMentionIndex::build(directory.path(), &[]).unwrap());
        let target = FamilyMentionTarget {
            execution: tickloop_agent_protocol::ExecutionRef {
                daemon_epoch: uuid::Uuid::new_v4(),
                agent_id: uuid::Uuid::new_v4(),
                generation: 1,
            },
            handle: "audit".into(),
            description: "Check evidence".into(),
            available: true,
        };
        state.set_family_targets(vec![target.clone()]);
        state.update_at("@a", 2, 15);
        assert!(matches!(
            state.handle_key(FileMentionKey::Complete),
            FileMentionAction::ReplaceDraft { .. }
        ));
        assert!(state.family_target_for_submission("@audit").is_err());
        assert_eq!(
            state
                .family_target_for_submission("@audit hello")
                .unwrap()
                .unwrap()
                .execution,
            target.execution
        );
        let chip = state.take_family();
        state.update_at("", 0, 15);
        state.restore_accepted("@audit hello", vec![]);
        state.restore_family(chip);
        assert_eq!(
            state
                .family_target_for_submission("@audit hello")
                .unwrap()
                .unwrap()
                .execution,
            target.execution
        );
        let mut stale = target.clone();
        stale.execution.generation += 1;
        state.set_family_targets(vec![stale]);
        assert!(state.family_target_for_submission("@audit hello").is_err());
        state.restore_family(Vec::new());
        let mut unavailable = target.clone();
        unavailable.available = false;
        state.set_family_targets(vec![unavailable]);
        assert!(state.family_target_for_submission("@audit hello").is_err());
        state.set_family_targets(vec![target]);
        state.update_at("@a", 2, 15);
        assert!(matches!(
            state.accept_candidate(1),
            FileMentionAction::ReplaceDraft { .. }
        ));
        assert!(
            state
                .family_target_for_submission("@audit hello")
                .unwrap()
                .is_none(),
            "accepted file metadata beats a bare Agent handle collision"
        );
    }

    #[test]
    fn agent_completion_moves_file_ranges_and_file_selection_replaces_agent_identity() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("audit"), "evidence").unwrap();
        let mut state =
            FileMentionState::with_index(FileMentionIndex::build(directory.path(), &[]).unwrap());
        let target = FamilyMentionTarget {
            execution: ExecutionRef {
                daemon_epoch: uuid::Uuid::new_v4(),
                agent_id: uuid::Uuid::new_v4(),
                generation: 1,
            },
            handle: "audit".into(),
            description: "Review".into(),
            available: true,
        };
        state.set_family_targets(vec![target.clone()]);
        state.update_at("@a @audit", 9, 15);
        assert!(matches!(
            state.accept_candidate(1),
            FileMentionAction::ReplaceDraft { .. }
        ));
        state.update_at("@a @audit", 2, 15);
        let FileMentionAction::ReplaceDraft { draft, cursor } = state.accept_candidate(0) else {
            panic!("Agent completion should apply");
        };
        assert_eq!(draft, "@audit @audit");
        assert_eq!(state.accepted[0].replacement_range, 7..13);
        assert_eq!(
            state.revalidate_all_for_submission(&draft).unwrap().len(),
            1
        );
        assert_eq!(state.accepted_family_mentions().len(), 1);
        let mut replacement = target;
        replacement.execution.generation += 1;
        state.set_family_targets(vec![replacement]);
        assert!(state.family_target_for_submission(&draft).is_err());
        state.update_at(&draft, cursor - 1, 15);
        assert!(matches!(
            state.accept_candidate(1),
            FileMentionAction::ReplaceDraft { .. }
        ));
        assert!(state.accepted_family_mentions().is_empty());
        assert!(
            state
                .family_target_for_submission(&draft)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn completes_files_and_directories_with_fuzzy_matching() {
        let fixture = Fixture::new();
        fixture.file("src/terminal/file_mentions.rs");
        fixture.file("src/model.rs");
        fixture.directory("docs/design");
        let index = fixture.index();

        assert!(
            index
                .complete("inspect @fm", 11, 8)
                .unwrap()
                .candidates
                .is_empty()
        );
        let nested = fixture.index_for("@src/terminal/fm");
        let completion = nested
            .complete("@src/terminal/fm", "@src/terminal/fm".len(), 8)
            .unwrap();

        assert_eq!(completion.candidates.len(), 1);
        assert_eq!(
            completion.candidates[0].display_path,
            "src/terminal/file_mentions.rs"
        );
        assert!(matches!(
            completion.candidates[0].target,
            MentionTarget::File {
                kind: MentionKind::File,
                ..
            }
        ));

        let directories = index.complete("@doc", 4, 8).unwrap();
        assert_eq!(directories.candidates[0].display_path, "docs/");
        assert!(matches!(
            directories.candidates[0].target,
            MentionTarget::File {
                kind: MentionKind::Directory,
                ..
            }
        ));
    }

    #[test]
    fn non_hidden_entries_precede_hidden_entries_before_limiting_results() {
        let fixture = Fixture::new();
        fixture.directory(".cache");
        fixture.directory("Documents");
        fixture.file(".env");
        fixture.file("notes.txt");
        let absolute = format!("{}/", normalize_path(&fixture.root));
        for (query, prefix) in [
            ("@".to_owned(), ""),
            ("@./".to_owned(), "./"),
            (format!("@{absolute}"), absolute.as_str()),
            ("@~".to_owned(), "~/"),
            ("@~/".to_owned(), "~/"),
        ] {
            let index = FileMentionIndex::build_directory_with_home(
                &fixture.root,
                &[],
                MentionDirectory::for_query(&query, query.len()).unwrap(),
                Some(&fixture.root),
                &|| false,
            )
            .unwrap();
            for limit in [2, 100] {
                let completion = index.complete(&query, query.len(), limit).unwrap();
                let paths = completion
                    .candidates
                    .iter()
                    .map(|candidate| candidate.display_path.clone())
                    .collect::<Vec<_>>();
                let expected = ["Documents/", "notes.txt", ".cache/", ".env"]
                    .into_iter()
                    .take(limit)
                    .map(|name| format!("{prefix}{name}"))
                    .collect::<Vec<_>>();
                assert_eq!(paths, expected, "query {query}, limit {limit}");
            }
        }
    }

    #[test]
    fn hidden_entries_remain_matchable_and_explicit_dot_queries_keep_match_ranking() {
        let fixture = Fixture::new();
        fixture.directory(".cache");
        fixture.file(".cache/.env");
        fixture.file(".cache/notes.txt");
        fixture.file(".env");
        fixture.file("a.env.backup");
        for (query, expected) in [
            ("@env", vec!["a.env.backup", ".env"]),
            ("@.env", vec![".env", "a.env.backup"]),
            ("@./.ca", vec!["./.cache/"]),
            ("@.cache/", vec![".cache/notes.txt", ".cache/.env"]),
            ("@.cache/.e", vec![".cache/.env"]),
        ] {
            let completion = fixture
                .index_for(query)
                .complete(query, query.len(), 100)
                .unwrap();
            let paths = completion
                .candidates
                .iter()
                .map(|candidate| candidate.display_path.as_str())
                .collect::<Vec<_>>();
            assert_eq!(paths, expected, "query {query}");
            let hidden = completion
                .candidates
                .iter()
                .position(|candidate| is_hidden_entry(&candidate.display_path))
                .unwrap();
            let applied = completion.apply(query, hidden).unwrap();
            assert_eq!(
                fixture.index_for(query).revalidate(&applied).unwrap(),
                applied.path
            );
        }
    }

    #[test]
    fn applies_only_the_token_around_the_cursor() {
        let fixture = Fixture::new();
        fixture.file("src/model.rs");
        let index = fixture.index_for("@src/mod");
        let draft = "先看 @szz 后看 @src/modxx 再决定";
        let cursor = draft.find("mod").unwrap() + "mod".len();
        let completion = index.complete(draft, cursor, 8).unwrap();

        let applied = completion.apply(draft, 0).unwrap();

        assert_eq!(applied.draft, "先看 @szz 后看 @src/model.rs 再决定");
        assert_eq!(
            &applied.draft[..applied.cursor],
            "先看 @szz 后看 @src/model.rs"
        );
        assert_eq!(applied.kind, MentionKind::File);
    }

    #[test]
    fn ignores_email_addresses_and_handles_utf8_cursor_positions() {
        let fixture = Fixture::new();
        fixture.file("资料/模型说明.md");
        let index = fixture.index_for("@资料/模");

        assert!(index.complete("name@example.com", 10, 8).is_none());
        let draft = "检查 @资料/模 后续";
        let cursor = draft.find('模').unwrap() + '模'.len_utf8();
        let completion = index.complete(draft, cursor, 8).unwrap();
        assert_eq!(completion.candidates[0].display_path, "资料/模型说明.md");
    }

    #[test]
    fn escapes_spaces_and_can_complete_the_escaped_token_again() {
        let fixture = Fixture::new();
        fixture.file("notes/design review.md");
        let index = fixture.index_for("@notes/review");
        let draft = "read @notes/review";
        let completion = index.complete(draft, draft.len(), 8).unwrap();
        let applied = completion.apply(draft, 0).unwrap();

        assert_eq!(applied.draft, "read @notes/design\\ review.md");
        let refreshed = index.complete(&applied.draft, applied.cursor, 8).unwrap();
        assert_eq!(
            refreshed.candidates[0].display_path,
            "notes/design review.md"
        );
    }

    #[test]
    fn honors_root_and_nested_gitignore_rules() {
        let fixture = Fixture::new();
        fixture.file("visible.txt");
        fixture.file("ignored.log");
        fixture.file("build/output.txt");
        fixture.file("nested/secret.txt");
        fixture.file("nested/public.txt");
        fs::write(fixture.root.join(".gitignore"), "*.log\nbuild/\n").unwrap();
        fs::write(
            fixture.root.join("nested/.gitignore"),
            "*.txt\n!public.txt\n",
        )
        .unwrap();
        let index = fixture.index();

        let all = index.complete("@", 1, 100).unwrap();
        let paths = all
            .candidates
            .iter()
            .map(|candidate| candidate.display_path.as_str())
            .collect::<Vec<_>>();
        assert!(paths.contains(&"visible.txt"));
        assert!(paths.contains(&"nested/"));
        assert!(!paths.contains(&"nested/public.txt"));
        let nested = fixture.index_for("@nested/");
        let nested_paths = nested
            .complete("@nested/", 8, 100)
            .unwrap()
            .candidates
            .into_iter()
            .map(|candidate| candidate.display_path)
            .collect::<Vec<_>>();
        assert!(nested_paths.iter().any(|path| path == "nested/public.txt"));
        assert!(!nested_paths.iter().any(|path| path == "nested/secret.txt"));
        assert!(!paths.contains(&"ignored.log"));
        assert!(!paths.contains(&"build/"));
        assert!(!paths.contains(&"nested/secret.txt"));
    }

    #[test]
    fn offers_absolute_paths_and_marks_directory_suffixes() {
        let fixture = Fixture::new();
        fixture.directory("src");
        fixture.file("src/lib.rs");
        let prefix = normalize_path(&fixture.root.join("sr"));
        let draft = format!("open @{prefix}");
        let index = fixture.index_for(&draft);
        let completion = index.complete(&draft, draft.len(), 8).unwrap();

        assert!(completion.candidates.iter().any(|candidate| {
            candidate.display_path.ends_with("/src/")
                && matches!(
                    candidate.target,
                    MentionTarget::File {
                        kind: MentionKind::Directory,
                        ..
                    }
                )
        }));
    }

    #[test]
    fn preserves_an_explicit_relative_path_prefix() {
        let fixture = Fixture::new();
        fixture.file("src/lib.rs");
        let index = fixture.index();

        let completion = index.complete("inspect @./sr", 13, 8).unwrap();
        assert_eq!(completion.candidates[0].display_path, "./src/");
        assert_eq!(completion.candidates[0].replacement, "@./src/");
    }

    #[cfg(unix)]
    #[test]
    fn excludes_symlinks_that_resolve_outside_allowed_roots() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let outside = Fixture::new();
        outside.file("secret.txt");
        symlink(
            outside.root.join("secret.txt"),
            fixture.root.join("secret-link"),
        )
        .unwrap();
        fixture.file("safe.txt");

        let all = fixture.index().complete("@", 1, 100).unwrap();
        assert!(
            all.candidates
                .iter()
                .all(|candidate| candidate.display_path != "secret-link")
        );
        assert!(
            all.candidates
                .iter()
                .any(|candidate| candidate.display_path == "safe.txt")
        );
    }

    #[test]
    fn rejects_stale_completion_and_revalidates_selection() {
        let fixture = Fixture::new();
        fixture.file("src/lib.rs");
        let index = fixture.index_for("@src/lib");
        let completion = index.complete("@src/lib", 8, 8).unwrap();
        assert!(completion.apply("@src/libs", 0).is_none());

        let applied = completion.apply("@src/lib", 0).unwrap();
        assert_eq!(index.revalidate(&applied).unwrap(), applied.path);
        fs::remove_file(&applied.path).unwrap();
        assert_eq!(
            index.revalidate(&applied).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn supports_additional_authorized_directories_without_relative_aliases() {
        let fixture = Fixture::new();
        let additional = Fixture::new();
        additional.file("shared.md");
        let index =
            FileMentionIndex::build(&fixture.root, std::slice::from_ref(&additional.root)).unwrap();

        assert!(
            index
                .complete("@shared", 7, 8)
                .unwrap()
                .candidates
                .is_empty()
        );
        let absolute_query = format!("@{}", normalize_path(&additional.root.join("sha")));
        let index = FileMentionIndex::build_directory(
            &fixture.root,
            std::slice::from_ref(&additional.root),
            MentionDirectory::for_query(&absolute_query, absolute_query.len()).unwrap(),
            &|| false,
        )
        .unwrap();
        assert!(
            index
                .complete(&absolute_query, absolute_query.len(), 8)
                .unwrap()
                .candidates
                .iter()
                .any(|candidate| candidate.display_path.ends_with("/shared.md"))
        );
    }

    #[test]
    fn never_returns_more_than_the_requested_limit() {
        let fixture = Fixture::new();
        for index in 0..10 {
            fixture.file(&format!("file-{index}.txt"));
        }

        assert_eq!(
            fixture
                .index()
                .complete("@", 1, 3)
                .unwrap()
                .candidates
                .len(),
            3
        );
    }

    #[cfg(unix)]
    #[test]
    fn excludes_names_that_could_inject_terminal_controls() {
        use std::os::unix::ffi::OsStringExt;

        let fixture = Fixture::new();
        fixture.file("safe.txt");
        fs::write(fixture.root.join("line\nbreak.txt"), "unsafe display").unwrap();
        let _ = fs::write(
            fixture
                .root
                .join(std::ffi::OsString::from_vec(b"invalid-\xff.txt".to_vec())),
            "non UTF-8 display",
        );

        let candidates = fixture.index().complete("@", 1, 100).unwrap().candidates;
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].display_path, "safe.txt");
    }

    #[test]
    fn candidate_state_starts_unselected_and_distinguishes_key_outcomes() {
        let fixture = Fixture::new();
        fixture.file("alpha.txt");
        fixture.file("alpine.txt");
        let mut state = FileMentionState::with_index(fixture.index());
        state.update_at("read @al", 8, 8);

        assert!(state.is_open());
        assert_eq!(state.selected_index(), None);
        assert_eq!(
            state.handle_key(FileMentionKey::Choose),
            FileMentionAction::Ignored
        );
        assert_eq!(state.selected_index(), None);
        assert_eq!(
            state.handle_key(FileMentionKey::Next),
            FileMentionAction::Consumed
        );
        assert_eq!(state.selected_index(), Some(0));
        assert_eq!(
            state.handle_key(FileMentionKey::Next),
            FileMentionAction::Consumed
        );
        assert_eq!(state.selected_index(), Some(1));
        assert_eq!(
            state.handle_key(FileMentionKey::Next),
            FileMentionAction::Consumed
        );
        assert_eq!(state.selected_index(), Some(1));
        assert_eq!(
            state.handle_key(FileMentionKey::Previous),
            FileMentionAction::Consumed
        );
        assert_eq!(state.selected_index(), Some(0));
        assert!(matches!(
            state.handle_key(FileMentionKey::Choose),
            FileMentionAction::ReplaceDraft { .. }
        ));
        assert!(!state.is_open());
        assert_eq!(state.accepted_mentions().len(), 1);
        assert_eq!(
            state.handle_key(FileMentionKey::Next),
            FileMentionAction::Ignored
        );
    }

    #[test]
    fn tab_accepts_the_first_candidate_and_escape_only_dismisses() {
        let fixture = Fixture::new();
        fixture.file("alpha.txt");
        fixture.file("alpine.txt");
        let mut state = FileMentionState::with_index(fixture.index());
        state.update_at("@al", 3, 8);
        let first_replacement = state.candidates().first().unwrap().replacement.clone();

        assert_eq!(
            state.handle_key(FileMentionKey::Complete),
            FileMentionAction::ReplaceDraft {
                cursor: first_replacement.len(),
                draft: first_replacement,
            }
        );

        state.update_at("@al", 3, 8);
        assert_eq!(
            state.handle_key(FileMentionKey::Cancel),
            FileMentionAction::Consumed
        );
        assert!(!state.is_open());
        assert_eq!(state.accepted_mentions().len(), 0);
    }

    #[test]
    fn canceled_mentions_stay_closed_across_unchanged_input_and_async_index_refresh() {
        let fixture = Fixture::new();
        fixture.file("alpha.txt");
        let mut state = FileMentionState::with_index(fixture.index());
        state.update_at("@al", 3, 8);
        assert!(state.is_open());
        assert_eq!(
            state.handle_key(FileMentionKey::Cancel),
            FileMentionAction::Consumed
        );
        state.update_at("@al", 3, 8);
        assert!(
            !state.is_open(),
            "a background refresh cannot undo the user's cancellation"
        );
        let generation = state.request_index_generation();
        state.replace_index(generation, fixture.index());
        assert!(
            !state.is_open(),
            "an index response cannot reopen the canceled query"
        );
        state.update_at("@alp", 4, 8);
        assert!(state.is_open(), "new input opens its own completion query");
    }

    #[test]
    fn asynchronous_index_replacement_rejects_stale_generations() {
        let fixture = Fixture::new();
        fixture.file("alpha.txt");
        let mut state = FileMentionState::with_index(fixture.index());
        state.update_at("@alp", 4, 8);
        assert!(state.is_open());
        let stale_generation = state.request_index_generation();
        assert!(!state.is_open());
        let current_generation = state.request_index_generation();

        assert_eq!(
            state.replace_index(stale_generation, fixture.index()),
            IndexReplacement::Stale
        );
        assert!(!state.is_open());
        assert_eq!(
            state.replace_index(current_generation, fixture.index()),
            IndexReplacement::Replaced
        );
        assert!(state.is_open());
        assert_eq!(state.selected_index(), None);
        assert_eq!(
            state.candidates().first().unwrap().display_path,
            "alpha.txt"
        );
    }

    #[test]
    fn accepted_registry_tracks_unrelated_edits_and_drops_edited_tokens() {
        let fixture = Fixture::new();
        fixture.file("one.md");
        fixture.file("two.md");
        let mut state = FileMentionState::with_index(fixture.index());
        state.update_at("@one", 4, 8);
        let FileMentionAction::ReplaceDraft { draft, .. } =
            state.handle_key(FileMentionKey::Complete)
        else {
            panic!("first mention was not accepted");
        };

        let draft = format!("{draft} then @two");
        state.update_at(&draft, draft.len(), 8);
        let FileMentionAction::ReplaceDraft { draft, .. } =
            state.handle_key(FileMentionKey::Complete)
        else {
            panic!("second mention was not accepted");
        };
        assert_eq!(state.accepted_mentions().len(), 2);

        let prefixed = format!("please {draft}");
        state.update_at(&prefixed, prefixed.len(), 8);
        let accepted = state.accepted_mentions().collect::<Vec<_>>();
        assert_eq!(accepted.len(), 2);
        assert!(
            accepted
                .iter()
                .all(|mention| prefixed.get(mention.replacement_range.clone())
                    == Some(&mention.token))
        );

        let edited = prefixed.replacen("@one.md", "@once.md", 1);
        state.update_at(&edited, edited.len(), 8);
        let remaining = state.accepted_mentions().collect::<Vec<_>>();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].token, "@two.md");
        assert_eq!(
            edited.get(remaining[0].replacement_range.clone()),
            Some("@two.md")
        );
    }

    #[test]
    fn submission_revalidates_every_still_accepted_mention() {
        let fixture = Fixture::new();
        fixture.file("one.md");
        fixture.file("two.md");
        let mut state = FileMentionState::with_index(fixture.index());
        state.update_at("@one", 4, 8);
        let FileMentionAction::ReplaceDraft { draft, .. } =
            state.handle_key(FileMentionKey::Complete)
        else {
            panic!("first mention was not accepted");
        };
        let draft = format!("{draft} @two");
        state.update_at(&draft, draft.len(), 8);
        let FileMentionAction::ReplaceDraft { draft, .. } =
            state.handle_key(FileMentionKey::Complete)
        else {
            panic!("second mention was not accepted");
        };

        let validated = state.revalidate_all_for_submission(&draft).unwrap();
        assert_eq!(validated.len(), 2);
        assert!(
            validated
                .iter()
                .all(|mention| mention.canonical_path.is_absolute())
        );

        fs::remove_file(fixture.root.join("two.md")).unwrap();
        assert_eq!(
            state
                .revalidate_all_for_submission(&draft)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn submission_without_accepted_mentions_does_not_require_an_index() {
        let mut state = FileMentionState::default();

        assert_eq!(
            state
                .revalidate_all_for_submission("plain @unaccepted text")
                .unwrap(),
            Vec::new()
        );
    }
    #[test]
    fn root_and_selected_directories_are_indexed_one_level_at_a_time() {
        let fixture = Fixture::new();
        fixture.file("src/lib.rs");
        fixture.file("src/deep/hidden.rs");
        fs::write(fixture.root.join("src/deep/.gitignore"), [0xff]).unwrap();
        let root = fixture.index();
        assert!(
            root.complete("@hidden", 7, 10)
                .unwrap()
                .candidates
                .is_empty()
        );
        let nested = fixture.index_for("@src/");
        let paths = nested
            .complete("@src/", 5, 100)
            .unwrap()
            .candidates
            .into_iter()
            .map(|candidate| candidate.display_path)
            .collect::<Vec<_>>();
        assert!(paths.contains(&"src/lib.rs".into()));
        assert!(paths.contains(&"src/deep/".into()));
        assert!(!paths.contains(&"src/deep/hidden.rs".into()));
    }

    #[test]
    fn selected_directories_keep_ignore_and_authorized_root_boundaries() {
        let fixture = Fixture::new();
        let outside = Fixture::new();
        fixture.file("blocked/hidden.txt");
        fixture.file(".git/config");
        fs::write(fixture.root.join(".gitignore"), "blocked/\n").unwrap();
        assert!(
            fixture
                .index_for("@blocked/")
                .complete("@blocked/", 9, 10)
                .unwrap()
                .candidates
                .is_empty()
        );
        assert!(
            fixture
                .index_for("@.git/")
                .complete("@.git/", 6, 10)
                .unwrap()
                .candidates
                .is_empty()
        );
        assert_eq!(
            FileMentionIndex::build_directory(
                &fixture.root,
                &[],
                MentionDirectory::Path(outside.root.clone()),
                &|| false
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn directory_scans_stop_before_io_and_during_directory_enumeration_when_canceled() {
        assert_eq!(
            FileMentionIndex::build_directory(
                Path::new("/does-not-exist"),
                &[],
                MentionDirectory::Roots,
                &|| true
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::Interrupted
        );
        let fixture = Fixture::new();
        for index in 0..40 {
            fixture.file(&format!("file-{index}.txt"));
        }
        let checks = std::cell::Cell::new(0);
        let canceled = || {
            checks.set(checks.get() + 1);
            checks.get() > 12
        };
        assert_eq!(
            FileMentionIndex::build_directory(
                &fixture.root,
                &[],
                MentionDirectory::Roots,
                &canceled
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(checks.get(), 13);
    }

    #[test]
    fn returning_to_a_visited_directory_uses_its_cache_and_canceling_allows_a_new_request() {
        let fixture = Fixture::new();
        fixture.file("root.txt");
        fixture.file("src/lib.rs");
        let mut state = FileMentionState::with_index(fixture.index());
        state.update_at("@src/", 5, 10);
        let scope = state.requested_directory().unwrap();
        let generation = state.request_directory(scope.clone()).unwrap();
        state.cancel_index_request();
        let replacement = state.request_directory(scope).unwrap();
        assert_ne!(generation, replacement);
        assert_eq!(
            state.replace_index(generation, fixture.index_for("@src/")),
            IndexReplacement::Stale
        );
        assert_eq!(
            state.replace_index(replacement, fixture.index_for("@src/")),
            IndexReplacement::Replaced
        );
        assert!(
            state
                .candidates()
                .iter()
                .any(|candidate| candidate.display_path == "src/lib.rs")
        );
        state.update_at("@root", 5, 10);
        assert!(state.request_directory(MentionDirectory::Roots).is_none());
        assert!(
            state
                .candidates()
                .iter()
                .any(|candidate| candidate.display_path == "root.txt")
        );
    }
}
