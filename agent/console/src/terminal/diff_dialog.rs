use std::ops::Range;

use super::{
    diff_catalog::{
        CatalogChangeDetail, CatalogFileChange, CatalogFileChangeId, CatalogUnavailableKind,
        ChangeCatalog, ChangeSource, ChangeSourceId,
    },
    diff_view::{
        DiffFrame, DiffNavigation, DiffNavigationAction, DiffViewport, navigate_diff,
        navigate_error, render_diff, render_error,
    },
};
use tickloop_agent_protocol::{FileChangeAction, FileChangeReviewSummary};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DiffDialogMode {
    #[default]
    List,
    Detail,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffDialogAction {
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Enter,
    Escape,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffDialogOutcome {
    Stay,
    Close,
}

pub struct DiffDialogFrame<'a> {
    pub mode: DiffDialogMode,
    pub source: Option<&'a ChangeSource>,
    pub source_index: Option<usize>,
    pub source_count: usize,
    pub selected_file: Option<usize>,
    pub file_count: usize,
    pub visible_files: Range<usize>,
    pub detail: Option<DiffFrame>,
}

#[derive(Default)]
pub struct DiffDialog {
    mode: DiffDialogMode,
    active_source: Option<ChangeSourceId>,
    fallback_source_index: usize,
    source_cursors: Vec<SourceCursor>,
    detail_anchors: Vec<StoredAnchor>,
}

struct SourceCursor {
    source: ChangeSourceId,
    selected_file: Option<CatalogFileChangeId>,
    fallback_file_index: usize,
    list_top: usize,
}

struct StoredAnchor {
    source: ChangeSourceId,
    file: CatalogFileChangeId,
    revision: DetailRevision,
    navigation: DiffNavigation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DetailRevision {
    Available {
        version: u32,
        summary: FileChangeReviewSummary,
    },
    Unavailable {
        action: FileChangeAction,
        path: std::path::PathBuf,
        summary: Option<FileChangeReviewSummary>,
        kind: CatalogUnavailableKind,
        message: String,
    },
}

impl DetailRevision {
    fn from_file(file: &CatalogFileChange) -> Self {
        match &file.detail {
            CatalogChangeDetail::Available(review) => Self::Available {
                version: review.version,
                summary: review.summary(),
            },
            CatalogChangeDetail::Unavailable { kind, message } => Self::Unavailable {
                action: file.action,
                path: file.path.clone(),
                summary: file.summary.clone(),
                kind: *kind,
                message: message.clone(),
            },
        }
    }
}

impl DiffDialog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mode(&self) -> DiffDialogMode {
        self.mode
    }

    pub fn apply(
        &mut self,
        catalog: &ChangeCatalog,
        viewport: DiffViewport,
        action: DiffDialogAction,
    ) -> DiffDialogOutcome {
        let Some(source_index) = self.normalize(catalog, viewport.height) else {
            return if action == DiffDialogAction::Escape {
                DiffDialogOutcome::Close
            } else {
                DiffDialogOutcome::Stay
            };
        };
        match self.mode {
            DiffDialogMode::List => self.apply_list(catalog, source_index, viewport, action),
            DiffDialogMode::Detail => self.apply_detail(catalog, source_index, viewport, action),
        }
    }

    pub fn frame<'a>(
        &mut self,
        catalog: &'a ChangeCatalog,
        viewport: DiffViewport,
    ) -> DiffDialogFrame<'a> {
        let Some(source_index) = self.normalize(catalog, viewport.height) else {
            return DiffDialogFrame {
                mode: DiffDialogMode::List,
                source: None,
                source_index: None,
                source_count: 0,
                selected_file: None,
                file_count: 0,
                visible_files: 0..0,
                detail: None,
            };
        };
        let source = &catalog.sources[source_index];
        let selected_file = self.selected_file(source, viewport.height);
        if selected_file.is_none() {
            self.mode = DiffDialogMode::List;
        }
        let detail = if self.mode == DiffDialogMode::Detail {
            selected_file.map(|file_index| self.render_detail(source, file_index, viewport))
        } else {
            None
        };
        let visible_files = self.visible_files(source, viewport.height);
        DiffDialogFrame {
            mode: self.mode,
            source: Some(source),
            source_index: Some(source_index),
            source_count: catalog.sources.len(),
            selected_file,
            file_count: source.files.len(),
            visible_files,
            detail,
        }
    }

    fn apply_list(
        &mut self,
        catalog: &ChangeCatalog,
        source_index: usize,
        viewport: DiffViewport,
        action: DiffDialogAction,
    ) -> DiffDialogOutcome {
        match action {
            DiffDialogAction::Left => {
                self.select_source(catalog, source_index.saturating_sub(1), viewport.height)
            }
            DiffDialogAction::Right => self.select_source(
                catalog,
                source_index
                    .saturating_add(1)
                    .min(catalog.sources.len().saturating_sub(1)),
                viewport.height,
            ),
            DiffDialogAction::Up => {
                self.move_file(&catalog.sources[source_index], false, viewport.height)
            }
            DiffDialogAction::Down => {
                self.move_file(&catalog.sources[source_index], true, viewport.height)
            }
            DiffDialogAction::Enter if !catalog.sources[source_index].files.is_empty() => {
                self.mode = DiffDialogMode::Detail;
            }
            DiffDialogAction::Escape => return DiffDialogOutcome::Close,
            DiffDialogAction::PageUp
            | DiffDialogAction::PageDown
            | DiffDialogAction::Home
            | DiffDialogAction::End
            | DiffDialogAction::Enter => {}
        }
        DiffDialogOutcome::Stay
    }

    fn apply_detail(
        &mut self,
        catalog: &ChangeCatalog,
        source_index: usize,
        viewport: DiffViewport,
        action: DiffDialogAction,
    ) -> DiffDialogOutcome {
        if matches!(action, DiffDialogAction::Left | DiffDialogAction::Escape) {
            self.mode = DiffDialogMode::List;
            return DiffDialogOutcome::Stay;
        }
        let navigation_action = match action {
            DiffDialogAction::Up => Some(DiffNavigationAction::Up),
            DiffDialogAction::Down => Some(DiffNavigationAction::Down),
            DiffDialogAction::PageUp => Some(DiffNavigationAction::PageUp),
            DiffDialogAction::PageDown => Some(DiffNavigationAction::PageDown),
            DiffDialogAction::Home => Some(DiffNavigationAction::Home),
            DiffDialogAction::End => Some(DiffNavigationAction::End),
            DiffDialogAction::Left
            | DiffDialogAction::Right
            | DiffDialogAction::Enter
            | DiffDialogAction::Escape => None,
        };
        let source = &catalog.sources[source_index];
        let Some(file_index) = self.selected_file(source, viewport.height) else {
            self.mode = DiffDialogMode::List;
            return DiffDialogOutcome::Stay;
        };
        if let Some(navigation_action) = navigation_action {
            self.navigate_detail(source, file_index, viewport, navigation_action);
        }
        DiffDialogOutcome::Stay
    }

    fn normalize(&mut self, catalog: &ChangeCatalog, list_height: usize) -> Option<usize> {
        if catalog.sources.is_empty() {
            self.mode = DiffDialogMode::List;
            self.active_source = None;
            self.fallback_source_index = 0;
            return None;
        }
        let source_index = self
            .active_source
            .as_ref()
            .and_then(|active| {
                catalog
                    .sources
                    .iter()
                    .position(|source| &source.id == active)
            })
            .unwrap_or_else(|| {
                self.fallback_source_index
                    .min(catalog.sources.len().saturating_sub(1))
            });
        self.fallback_source_index = source_index;
        self.active_source = Some(catalog.sources[source_index].id.clone());
        self.normalize_cursor(&catalog.sources[source_index], list_height);
        Some(source_index)
    }

    fn select_source(&mut self, catalog: &ChangeCatalog, index: usize, list_height: usize) {
        self.fallback_source_index = index;
        self.active_source = Some(catalog.sources[index].id.clone());
        self.normalize_cursor(&catalog.sources[index], list_height);
    }

    fn normalize_cursor(&mut self, source: &ChangeSource, list_height: usize) {
        let cursor = self.cursor_mut(source);
        if source.files.is_empty() {
            cursor.selected_file = None;
            cursor.fallback_file_index = 0;
            cursor.list_top = 0;
            return;
        }
        let selected = cursor
            .selected_file
            .as_ref()
            .and_then(|file_id| source.files.iter().position(|file| &file.id == file_id))
            .unwrap_or_else(|| {
                cursor
                    .fallback_file_index
                    .min(source.files.len().saturating_sub(1))
            });
        cursor.fallback_file_index = selected;
        cursor.selected_file = Some(source.files[selected].id.clone());
        keep_visible(cursor, source.files.len(), list_height);
    }

    fn selected_file(&mut self, source: &ChangeSource, list_height: usize) -> Option<usize> {
        self.normalize_cursor(source, list_height);
        (!source.files.is_empty()).then_some(self.cursor_mut(source).fallback_file_index)
    }

    fn visible_files(&mut self, source: &ChangeSource, list_height: usize) -> Range<usize> {
        self.normalize_cursor(source, list_height);
        let cursor = self.cursor_mut(source);
        let end = cursor
            .list_top
            .saturating_add(list_height)
            .min(source.files.len());
        cursor.list_top..end
    }

    fn move_file(&mut self, source: &ChangeSource, forward: bool, list_height: usize) {
        self.normalize_cursor(source, list_height);
        if source.files.is_empty() {
            return;
        }
        let cursor = self.cursor_mut(source);
        cursor.fallback_file_index = if forward {
            cursor
                .fallback_file_index
                .saturating_add(1)
                .min(source.files.len().saturating_sub(1))
        } else {
            cursor.fallback_file_index.saturating_sub(1)
        };
        cursor.selected_file = Some(source.files[cursor.fallback_file_index].id.clone());
        keep_visible(cursor, source.files.len(), list_height);
    }

    fn cursor_mut(&mut self, source: &ChangeSource) -> &mut SourceCursor {
        if let Some(index) = self
            .source_cursors
            .iter()
            .position(|cursor| cursor.source == source.id)
        {
            return &mut self.source_cursors[index];
        }
        self.source_cursors.push(SourceCursor {
            source: source.id.clone(),
            selected_file: None,
            fallback_file_index: 0,
            list_top: 0,
        });
        self.source_cursors.last_mut().unwrap()
    }

    fn render_detail(
        &mut self,
        source: &ChangeSource,
        file_index: usize,
        viewport: DiffViewport,
    ) -> DiffFrame {
        let file = &source.files[file_index];
        let navigation = self.navigation(source, file);
        let frame = match &file.detail {
            CatalogChangeDetail::Available(review) => render_diff(review, &navigation, viewport),
            CatalogChangeDetail::Unavailable { message, .. } => render_error(
                &format!("Review unavailable: {}", file.path.display()),
                message,
                &navigation,
                viewport,
            ),
        };
        self.store_navigation(source, file, frame.navigation.clone());
        frame
    }

    fn navigate_detail(
        &mut self,
        source: &ChangeSource,
        file_index: usize,
        viewport: DiffViewport,
        action: DiffNavigationAction,
    ) {
        let file = &source.files[file_index];
        let navigation = self.navigation(source, file);
        let navigation = match &file.detail {
            CatalogChangeDetail::Available(review) => {
                navigate_diff(review, &navigation, viewport, action)
            }
            CatalogChangeDetail::Unavailable { message, .. } => navigate_error(
                &format!("Review unavailable: {}", file.path.display()),
                message,
                &navigation,
                viewport,
                action,
            ),
        };
        self.store_navigation(source, file, navigation);
    }

    fn navigation(&self, source: &ChangeSource, file: &CatalogFileChange) -> DiffNavigation {
        let revision = DetailRevision::from_file(file);
        self.detail_anchors
            .iter()
            .find(|stored| {
                stored.source == source.id && stored.file == file.id && stored.revision == revision
            })
            .map_or_else(DiffNavigation::default, |stored| stored.navigation.clone())
    }

    fn store_navigation(
        &mut self,
        source: &ChangeSource,
        file: &CatalogFileChange,
        navigation: DiffNavigation,
    ) {
        let revision = DetailRevision::from_file(file);
        if let Some(stored) = self
            .detail_anchors
            .iter_mut()
            .find(|stored| stored.source == source.id && stored.file == file.id)
        {
            stored.revision = revision;
            stored.navigation = navigation;
            return;
        }
        self.detail_anchors.push(StoredAnchor {
            source: source.id.clone(),
            file: file.id.clone(),
            revision,
            navigation,
        });
    }
}

fn keep_visible(cursor: &mut SourceCursor, file_count: usize, height: usize) {
    if height == 0 || file_count == 0 {
        cursor.list_top = 0;
        return;
    }
    let maximum_top = file_count.saturating_sub(height);
    cursor.list_top = cursor.list_top.min(maximum_top);
    if cursor.fallback_file_index < cursor.list_top {
        cursor.list_top = cursor.fallback_file_index;
    } else if cursor.fallback_file_index >= cursor.list_top.saturating_add(height) {
        cursor.list_top = cursor.fallback_file_index + 1 - height;
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use uuid::Uuid;

    use super::*;
    use crate::terminal::diff_catalog::{
        CatalogFileChange, CatalogFileChangeId, CatalogUnavailableKind, ChangeSourceState,
    };
    use tickloop_agent_protocol::{FileChangeAction, FileVersion};
    use tickloop_agent_tools::file_change::{
        FileChangeHunk, FileChangeLine, FileChangeLineKind, FileChangeReview,
    };

    fn version(bytes: u64, lines: u64, exists: bool) -> FileVersion {
        FileVersion {
            exists,
            identity: None,
            byte_len: bytes,
            line_count: lines,
            ends_with_newline: exists,
            sha256: format!("hash-{bytes}-{lines}-{exists}"),
        }
    }

    fn review(path: &str, line_count: u64) -> FileChangeReview {
        FileChangeReview {
            version: 1,
            action: FileChangeAction::Edit,
            path: PathBuf::from(path),
            before: version(line_count, line_count, true),
            after: version(line_count + 1, line_count, true),
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
        }
    }

    fn available_file(id: u128, path: &str, line_count: u64) -> CatalogFileChange {
        let review = review(path, line_count);
        CatalogFileChange {
            id: CatalogFileChangeId::AgentOperation(Uuid::from_u128(id)),
            path: PathBuf::from(path),
            action: review.action,
            current_status: None,
            summary: Some(review.summary()),
            detail: CatalogChangeDetail::Available(Box::new(review)),
        }
    }

    fn current_file(path: &str, line_count: u64) -> CatalogFileChange {
        let mut file = available_file(1, path, line_count);
        file.id = CatalogFileChangeId::CurrentWorktreePath(PathBuf::from(path));
        file
    }

    fn source(id: ChangeSourceId, files: Vec<CatalogFileChange>) -> ChangeSource {
        ChangeSource {
            id,
            state: ChangeSourceState::Ready,
            user_prompt: None,
            files,
        }
    }

    fn catalog() -> ChangeCatalog {
        ChangeCatalog {
            sources: vec![
                source(
                    ChangeSourceId::Current,
                    vec![
                        available_file(1, "a.txt", 1),
                        available_file(2, "b.txt", 1),
                        available_file(3, "c.txt", 1),
                    ],
                ),
                source(
                    ChangeSourceId::AgentRun(Uuid::from_u128(20).into()),
                    vec![
                        available_file(4, "run-a.txt", 1),
                        available_file(5, "run-b.txt", 30),
                    ],
                ),
            ],
        }
    }

    fn viewport(width: usize, height: usize) -> DiffViewport {
        DiffViewport { width, height }
    }

    #[test]
    fn list_navigation_keeps_an_independent_file_cursor_for_each_source() {
        let catalog = catalog();
        let mut dialog = DiffDialog::new();

        dialog.apply(&catalog, viewport(80, 2), DiffDialogAction::Down);
        dialog.apply(&catalog, viewport(80, 2), DiffDialogAction::Down);
        let current = dialog.frame(&catalog, viewport(80, 2));
        assert_eq!(current.source_index, Some(0));
        assert_eq!(current.selected_file, Some(2));
        assert_eq!(current.visible_files, 1..3);

        dialog.apply(&catalog, viewport(80, 2), DiffDialogAction::Right);
        let run = dialog.frame(&catalog, viewport(80, 2));
        assert_eq!(run.source_index, Some(1));
        assert_eq!(run.selected_file, Some(0));
        dialog.apply(&catalog, viewport(80, 2), DiffDialogAction::Down);

        dialog.apply(&catalog, viewport(80, 2), DiffDialogAction::Left);
        let restored = dialog.frame(&catalog, viewport(80, 2));
        assert_eq!(restored.source_index, Some(0));
        assert_eq!(restored.selected_file, Some(2));
        assert_eq!(restored.visible_files, 1..3);
    }

    #[test]
    fn detail_navigation_and_back_preserve_the_file_and_scroll_anchor() {
        let catalog = catalog();
        let mut dialog = DiffDialog::new();
        let viewport = viewport(18, 4);
        dialog.apply(&catalog, viewport, DiffDialogAction::Right);
        dialog.apply(&catalog, viewport, DiffDialogAction::Down);
        dialog.apply(&catalog, viewport, DiffDialogAction::Enter);
        assert_eq!(dialog.mode(), DiffDialogMode::Detail);

        dialog.apply(&catalog, viewport, DiffDialogAction::PageDown);
        dialog.apply(&catalog, viewport, DiffDialogAction::End);
        let at_end = dialog.frame(&catalog, viewport);
        let end_top = at_end.detail.as_ref().unwrap().top_row;
        assert!(!at_end.detail.as_ref().unwrap().can_move_down);

        dialog.apply(&catalog, viewport, DiffDialogAction::Escape);
        let list = dialog.frame(&catalog, viewport);
        assert_eq!(list.mode, DiffDialogMode::List);
        assert_eq!(list.source_index, Some(1));
        assert_eq!(list.selected_file, Some(1));
        dialog.apply(&catalog, viewport, DiffDialogAction::Enter);
        assert_eq!(
            dialog.frame(&catalog, viewport).detail.unwrap().top_row,
            end_top
        );

        dialog.apply(&catalog, viewport, DiffDialogAction::Left);
        assert_eq!(dialog.mode(), DiffDialogMode::List);
        assert_eq!(
            dialog.apply(&catalog, viewport, DiffDialogAction::Escape),
            DiffDialogOutcome::Close
        );
    }

    #[test]
    fn detail_supports_every_vertical_navigation_action() {
        let catalog = ChangeCatalog {
            sources: vec![source(
                ChangeSourceId::Current,
                vec![available_file(1, "long.txt", 40)],
            )],
        };
        let mut dialog = DiffDialog::new();
        let viewport = viewport(20, 5);
        dialog.apply(&catalog, viewport, DiffDialogAction::Enter);
        dialog.apply(&catalog, viewport, DiffDialogAction::Down);
        assert_eq!(dialog.frame(&catalog, viewport).detail.unwrap().top_row, 1);
        dialog.apply(&catalog, viewport, DiffDialogAction::PageDown);
        assert_eq!(dialog.frame(&catalog, viewport).detail.unwrap().top_row, 6);
        dialog.apply(&catalog, viewport, DiffDialogAction::Up);
        assert_eq!(dialog.frame(&catalog, viewport).detail.unwrap().top_row, 5);
        dialog.apply(&catalog, viewport, DiffDialogAction::Home);
        assert_eq!(dialog.frame(&catalog, viewport).detail.unwrap().top_row, 0);
        dialog.apply(&catalog, viewport, DiffDialogAction::End);
        assert!(
            !dialog
                .frame(&catalog, viewport)
                .detail
                .unwrap()
                .can_move_down
        );
        dialog.apply(&catalog, viewport, DiffDialogAction::PageUp);
        assert!(
            dialog
                .frame(&catalog, viewport)
                .detail
                .unwrap()
                .can_move_down
        );
    }

    #[test]
    fn unavailable_detail_is_an_explicit_navigable_error() {
        let file = CatalogFileChange {
            id: CatalogFileChangeId::CurrentPath(PathBuf::from("binary.dat")),
            path: PathBuf::from("binary.dat"),
            action: FileChangeAction::Edit,
            current_status: None,
            summary: None,
            detail: CatalogChangeDetail::Unavailable {
                kind: CatalogUnavailableKind::UnsupportedText,
                message: "binary content cannot be reviewed as text".into(),
            },
        };
        let catalog = ChangeCatalog {
            sources: vec![source(ChangeSourceId::Current, vec![file])],
        };
        let mut dialog = DiffDialog::new();
        let viewport = viewport(12, 2);
        dialog.apply(&catalog, viewport, DiffDialogAction::Enter);
        dialog.apply(&catalog, viewport, DiffDialogAction::End);
        let frame = dialog.frame(&catalog, viewport);
        let detail = frame.detail.unwrap();

        assert!(!detail.can_move_down);
        assert!(
            detail
                .lines
                .iter()
                .all(|line| line.kind == super::super::diff_view::DiffLineKind::Error)
        );
        assert!(detail.total_rows > detail.lines.len());
    }

    #[test]
    fn source_level_empty_states_remain_typed_and_cannot_open_detail() {
        let states = [
            ChangeSourceState::Ready,
            ChangeSourceState::Clean,
            ChangeSourceState::NonGit,
            ChangeSourceState::GitError {
                message: "git status failed".into(),
            },
        ];
        let catalog = ChangeCatalog {
            sources: states
                .into_iter()
                .enumerate()
                .map(|(index, state)| ChangeSource {
                    id: if index == 0 {
                        ChangeSourceId::Current
                    } else {
                        ChangeSourceId::AgentRun(Uuid::from_u128(index as u128).into())
                    },
                    state,
                    user_prompt: None,
                    files: Vec::new(),
                })
                .collect(),
        };
        let mut dialog = DiffDialog::new();

        for expected in &[
            ChangeSourceState::Ready,
            ChangeSourceState::Clean,
            ChangeSourceState::NonGit,
            ChangeSourceState::GitError {
                message: "git status failed".into(),
            },
        ] {
            let frame = dialog.frame(&catalog, viewport(80, 10));
            assert_eq!(&frame.source.unwrap().state, expected);
            assert_eq!(frame.selected_file, None);
            dialog.apply(&catalog, viewport(80, 10), DiffDialogAction::Enter);
            assert_eq!(dialog.mode(), DiffDialogMode::List);
            dialog.apply(&catalog, viewport(80, 10), DiffDialogAction::Right);
        }
    }

    #[test]
    fn stable_source_and_path_selection_survive_catalog_reordering() {
        let catalog = catalog();
        let mut dialog = DiffDialog::new();
        dialog.apply(&catalog, viewport(80, 10), DiffDialogAction::Right);
        dialog.apply(&catalog, viewport(80, 10), DiffDialogAction::Down);

        let reordered = ChangeCatalog {
            sources: vec![catalog.sources[1].clone(), catalog.sources[0].clone()],
        };
        let frame = dialog.frame(&reordered, viewport(80, 10));

        assert_eq!(frame.source_index, Some(0));
        assert_eq!(frame.selected_file, Some(1));
        assert_eq!(
            frame.source.unwrap().files[1].path,
            PathBuf::from("run-b.txt")
        );
    }

    #[test]
    fn reloaded_unchanged_file_preserves_detail_anchor() {
        let catalog = ChangeCatalog {
            sources: vec![source(
                ChangeSourceId::Current,
                vec![current_file("same.txt", 50)],
            )],
        };
        let mut dialog = DiffDialog::new();
        let viewport = viewport(18, 4);
        dialog.apply(&catalog, viewport, DiffDialogAction::Enter);
        dialog.apply(&catalog, viewport, DiffDialogAction::End);
        let before_reload = dialog.frame(&catalog, viewport).detail.unwrap();
        assert!(before_reload.top_row > 0);
        assert!(before_reload.navigation.stick_to_end);

        let reloaded = catalog.clone();
        let after_reload = dialog.frame(&reloaded, viewport);

        assert_eq!(after_reload.mode, DiffDialogMode::Detail);
        assert_eq!(after_reload.selected_file, Some(0));
        let detail = after_reload.detail.unwrap();
        assert_eq!(detail.top_row, before_reload.top_row);
        assert!(detail.navigation.stick_to_end);
    }

    #[test]
    fn reloaded_changed_file_preserves_selection_but_resets_detail_anchor() {
        let catalog = ChangeCatalog {
            sources: vec![source(
                ChangeSourceId::Current,
                vec![current_file("same.txt", 50)],
            )],
        };
        let mut dialog = DiffDialog::new();
        let viewport = viewport(18, 4);
        dialog.apply(&catalog, viewport, DiffDialogAction::Enter);
        dialog.apply(&catalog, viewport, DiffDialogAction::End);
        assert!(dialog.frame(&catalog, viewport).detail.unwrap().top_row > 0);

        let changed = ChangeCatalog {
            sources: vec![source(
                ChangeSourceId::Current,
                vec![current_file("same.txt", 60)],
            )],
        };
        let after_reload = dialog.frame(&changed, viewport);

        assert_eq!(after_reload.mode, DiffDialogMode::Detail);
        assert_eq!(after_reload.selected_file, Some(0));
        let detail = after_reload.detail.unwrap();
        assert_eq!(detail.top_row, 0);
        assert!(!detail.navigation.stick_to_end);
    }

    #[test]
    fn changed_unavailable_detail_resets_its_anchor() {
        let unavailable = |message: &str| CatalogFileChange {
            id: CatalogFileChangeId::CurrentPath(PathBuf::from("binary.dat")),
            path: PathBuf::from("binary.dat"),
            action: FileChangeAction::Edit,
            current_status: None,
            summary: None,
            detail: CatalogChangeDetail::Unavailable {
                kind: CatalogUnavailableKind::UnsupportedText,
                message: message.repeat(20),
            },
        };
        let catalog = ChangeCatalog {
            sources: vec![source(
                ChangeSourceId::Current,
                vec![unavailable("old unavailable detail ")],
            )],
        };
        let mut dialog = DiffDialog::new();
        let viewport = viewport(12, 2);
        dialog.apply(&catalog, viewport, DiffDialogAction::Enter);
        dialog.apply(&catalog, viewport, DiffDialogAction::End);
        assert!(dialog.frame(&catalog, viewport).detail.unwrap().top_row > 0);

        let changed = ChangeCatalog {
            sources: vec![source(
                ChangeSourceId::Current,
                vec![unavailable("new unavailable detail ")],
            )],
        };
        let detail = dialog.frame(&changed, viewport).detail.unwrap();

        assert_eq!(detail.top_row, 0);
        assert!(detail.can_move_down);
        assert!(!detail.navigation.stick_to_end);
    }

    #[test]
    fn repeated_changes_to_the_same_path_keep_distinct_selection_and_anchors() {
        let catalog = ChangeCatalog {
            sources: vec![source(
                ChangeSourceId::AgentRun(Uuid::from_u128(20).into()),
                vec![
                    available_file(1, "same.txt", 2),
                    available_file(2, "same.txt", 40),
                ],
            )],
        };
        let mut dialog = DiffDialog::new();
        let viewport = viewport(18, 4);
        dialog.apply(&catalog, viewport, DiffDialogAction::Down);
        assert_eq!(dialog.frame(&catalog, viewport).selected_file, Some(1));
        dialog.apply(&catalog, viewport, DiffDialogAction::Enter);
        dialog.apply(&catalog, viewport, DiffDialogAction::End);
        assert!(
            !dialog
                .frame(&catalog, viewport)
                .detail
                .unwrap()
                .can_move_down
        );

        dialog.apply(&catalog, viewport, DiffDialogAction::Escape);
        dialog.apply(&catalog, viewport, DiffDialogAction::Up);
        dialog.apply(&catalog, viewport, DiffDialogAction::Enter);
        assert_eq!(dialog.frame(&catalog, viewport).detail.unwrap().top_row, 0);
        dialog.apply(&catalog, viewport, DiffDialogAction::Escape);
        dialog.apply(&catalog, viewport, DiffDialogAction::Down);
        dialog.apply(&catalog, viewport, DiffDialogAction::Enter);
        assert!(
            dialog
                .frame(&catalog, viewport)
                .detail
                .unwrap()
                .navigation
                .stick_to_end
        );
    }

    #[test]
    fn end_affinity_survives_a_detail_resize() {
        let catalog = ChangeCatalog {
            sources: vec![source(
                ChangeSourceId::Current,
                vec![available_file(1, "long.txt", 50)],
            )],
        };
        let mut dialog = DiffDialog::new();
        dialog.apply(&catalog, viewport(100, 20), DiffDialogAction::Enter);
        dialog.apply(&catalog, viewport(100, 20), DiffDialogAction::End);
        let narrowed = dialog.frame(&catalog, viewport(12, 5));

        assert!(!narrowed.detail.as_ref().unwrap().can_move_down);
        assert!(narrowed.detail.unwrap().navigation.stick_to_end);
    }

    #[test]
    fn an_empty_catalog_only_closes_on_escape() {
        let catalog = ChangeCatalog {
            sources: Vec::new(),
        };
        let mut dialog = DiffDialog::new();
        let frame = dialog.frame(&catalog, viewport(80, 10));

        assert!(frame.source.is_none());
        assert_eq!(frame.source_count, 0);
        assert_eq!(
            dialog.apply(&catalog, viewport(80, 10), DiffDialogAction::Enter),
            DiffDialogOutcome::Stay
        );
        assert_eq!(
            dialog.apply(&catalog, viewport(80, 10), DiffDialogAction::Escape),
            DiffDialogOutcome::Close
        );
    }
}
