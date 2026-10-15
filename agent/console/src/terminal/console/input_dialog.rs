use super::*;

pub(super) const FIXED_INPUT_DIALOG_ROWS: usize = 12;
pub(super) const INPUT_DIALOG_RULE_ROWS: usize = 1;
const INPUT_DIALOG_RULE_GLYPH: &str = "▔";
const FIXED_CONTENT_RESERVE_ROWS: usize = 4;
const PICKER_CONTENT_RESERVE_ROWS: usize = 2;

pub(super) fn body_height(height: usize) -> usize {
    height.saturating_sub(INPUT_DIALOG_RULE_ROWS).max(1)
}

pub(super) fn fixed_height_for_terminal(height: usize) -> usize {
    height
        .saturating_sub(FIXED_CONTENT_RESERVE_ROWS)
        .clamp(1, FIXED_INPUT_DIALOG_ROWS)
}

fn pad_dialog(lines: &mut Vec<RoleLine>, height: usize) {
    if lines.len() < height
        && let Some(hint) = lines.pop()
    {
        lines.resize(height.saturating_sub(1), RoleLine::default());
        lines.push(hint);
    }
    lines.truncate(height);
}

#[derive(Default)]
pub(super) struct DialogState {
    menu: Option<Menu>,
    menu_rule_role: Option<Role>,
    picker_kind: Option<PickerKind>,
    picker_presentation: PickerPresentation,
    picker_search: Option<PickerSearch>,
    artifacts_picker: Option<super::super::artifacts_picker::ArtifactsPicker>,
    conversation_picker: Option<ConversationPicker>,
    menu_link: Option<(Uuid, String)>,
    menu_review: Option<MenuReviewState>,
    full_review: bool,
    menu_plan: bool,
    menu_comment: Option<MenuCommentState>,
    menu_expand_options: Vec<usize>,
    question_menu: Option<QuestionMenu>,
    request_queue_count: usize,
    request_layout: Option<Uuid>,
    request_menu: Option<Uuid>,
    menu_focused: bool,
    review_focused: bool,
    context_offset: usize,
    history_search: Option<HistorySearch>,
    pub(super) permissions_surface: Option<PermissionsSurface>,
    help_open: bool,
    help_offset: usize,
}

impl DialogState {
    fn complete_login(&mut self, id: Uuid, status: &str, notice: &str) -> bool {
        self.menu.as_mut().is_some_and(|menu| {
            complete_login_menu(menu, &mut self.picker_presentation, id, status, notice)
        })
    }

    fn update_login(&mut self, menu: &Menu, address: &str, callback: bool) -> bool {
        update_login_menu(
            &mut self.menu,
            &mut self.menu_link,
            &mut self.menu_comment,
            menu,
            address,
            callback,
        )
    }

    fn clear_artifact_picker(&mut self, id: Uuid) -> bool {
        if self
            .artifacts_picker
            .as_ref()
            .is_some_and(|picker| picker.id() == id)
        {
            self.artifacts_picker = None;
            return true;
        }
        false
    }

    fn reset_menu(&mut self) {
        self.menu = None;
        self.menu_rule_role = None;
        self.picker_kind = None;
        self.picker_presentation = Default::default();
        self.picker_search = None;
        self.menu_link = None;
        self.menu_review = None;
        self.full_review = false;
        self.menu_plan = false;
        self.menu_comment = None;
        self.menu_expand_options.clear();
        self.question_menu = None;
        self.request_layout = None;
        self.request_menu = None;
        self.menu_focused = false;
        self.review_focused = false;
    }

    fn clear_menu(&mut self, id: Uuid) -> bool {
        if self
            .conversation_picker
            .as_ref()
            .is_some_and(|picker| picker.id() == id)
        {
            self.conversation_picker = None;
            return true;
        }
        if self.menu.as_ref().is_none_or(|menu| menu.id != id) {
            return false;
        }
        self.reset_menu();
        true
    }
}

fn update_login_menu(
    current: &mut Option<Menu>,
    link: &mut Option<(Uuid, String)>,
    comment: &mut Option<MenuCommentState>,
    menu: &Menu,
    address: &str,
    callback: bool,
) -> bool {
    let Some(current) = current.as_mut().filter(|current| current.id == menu.id) else {
        return false;
    };
    *current = Menu {
        selected: current.selected.min(menu.options.len().saturating_sub(1)),
        ..menu.clone()
    };
    *link = valid_login_link(address)
        .filter(|_| menu.context.iter().any(|line| line == address))
        .map(|address| (menu.id, address));
    if callback && comment.is_none() {
        *comment = Some(MenuCommentState {
            options: vec![0],
            values: HashMap::new(),
            label: "Callback URL",
            editing: false,
            cursor: 0,
            secret: false,
            standalone_input: false,
        });
    }
    true
}

fn complete_login_menu(
    menu: &mut Menu,
    presentation: &mut PickerPresentation,
    id: Uuid,
    status: &str,
    notice: &str,
) -> bool {
    if menu.id != id {
        return false;
    }
    presentation.hint = "enter return to Provider · esc back".into();
    presentation.row_hints.clear();
    presentation.editing_hint = Some("Callback URL · enter return to Provider · esc close field");
    menu.title = format!("{} · {status}", menu.title);
    menu.context = vec![
        notice.into(),
        "enter returns to Provider; esc leaves editing first, otherwise returns to Provider. Saved credentials remain applied.".into(),
    ];
    for option in &mut menu.options {
        *option = "Back to Provider".into();
    }
    true
}

impl State {
    pub(super) fn permissions_surfaces_mut(
        &mut self,
    ) -> impl Iterator<Item = &mut PermissionsSurface> {
        self.permissions_surface
            .iter_mut()
            .chain(
                self.suspended_dialog
                    .iter_mut()
                    .filter_map(|dialog| dialog.permissions_surface.as_mut()),
            )
            .chain(
                self.dialog_cache
                    .values_mut()
                    .flat_map(|(active, suspended)| {
                        active.permissions_surface.iter_mut().chain(
                            suspended
                                .iter_mut()
                                .filter_map(|dialog| dialog.permissions_surface.as_mut()),
                        )
                    }),
            )
    }

    pub(super) fn reset_active_menu(&mut self) {
        let mut dialog = self.replace_dialog(DialogState::default());
        dialog.reset_menu();
        self.replace_dialog(dialog);
    }

    pub(super) fn replace_dialog(&mut self, next: DialogState) -> DialogState {
        DialogState {
            menu: std::mem::replace(&mut self.menu, next.menu),
            menu_rule_role: std::mem::replace(&mut self.menu_rule_role, next.menu_rule_role),
            picker_kind: std::mem::replace(&mut self.picker_kind, next.picker_kind),
            picker_presentation: std::mem::replace(
                &mut self.picker_presentation,
                next.picker_presentation,
            ),
            picker_search: std::mem::replace(&mut self.picker_search, next.picker_search),
            artifacts_picker: std::mem::replace(&mut self.artifacts_picker, next.artifacts_picker),
            conversation_picker: std::mem::replace(
                &mut self.conversation_picker,
                next.conversation_picker,
            ),
            menu_link: std::mem::replace(&mut self.menu_link, next.menu_link),
            full_review: std::mem::replace(&mut self.full_review, next.full_review),
            menu_plan: std::mem::replace(&mut self.menu_plan, next.menu_plan),
            menu_review: std::mem::replace(&mut self.menu_review, next.menu_review),
            menu_comment: std::mem::replace(&mut self.menu_comment, next.menu_comment),
            menu_expand_options: std::mem::replace(
                &mut self.menu_expand_options,
                next.menu_expand_options,
            ),
            question_menu: std::mem::replace(&mut self.question_menu, next.question_menu),
            request_queue_count: std::mem::replace(
                &mut self.request_queue_count,
                next.request_queue_count,
            ),
            request_layout: std::mem::replace(&mut self.request_layout, next.request_layout),
            request_menu: std::mem::replace(&mut self.request_menu, next.request_menu),
            menu_focused: std::mem::replace(&mut self.menu_focused, next.menu_focused),
            review_focused: std::mem::replace(&mut self.review_focused, next.review_focused),
            context_offset: std::mem::replace(&mut self.context_offset, next.context_offset),
            history_search: std::mem::replace(&mut self.history_search, next.history_search),
            permissions_surface: std::mem::replace(
                &mut self.permissions_surface,
                next.permissions_surface,
            ),
            help_open: std::mem::replace(&mut self.help_open, next.help_open),
            help_offset: std::mem::replace(&mut self.help_offset, next.help_offset),
        }
    }

    pub(super) fn install_artifact_picker(
        &mut self,
        picker: super::super::artifacts_picker::ArtifactsPicker,
    ) {
        let chat = picker.chat;
        let next = DialogState {
            artifacts_picker: Some(picker),
            ..DialogState::default()
        };
        self.install_user_dialog(chat, next);
    }

    pub(super) fn install_user_dialog(&mut self, chat: Option<Uuid>, next: DialogState) {
        if self.chat == chat {
            if self.request_layout.is_some() {
                self.suspended_dialog = Some(next);
            } else {
                self.replace_dialog(next);
            }
        } else {
            let (active, suspended) = self.dialog_cache.entry(chat).or_default();
            if active.request_layout.is_some() {
                *suspended = Some(next);
            } else {
                *active = next;
            }
        }
    }

    pub(super) fn clear_artifact_picker(&mut self, id: Uuid) {
        if self
            .artifacts_picker
            .as_ref()
            .is_some_and(|picker| picker.id() == id)
        {
            self.artifacts_picker = None;
        }
        if let Some(dialog) = &mut self.suspended_dialog {
            dialog.clear_artifact_picker(id);
        }
        for (active, suspended) in self.dialog_cache.values_mut() {
            active.clear_artifact_picker(id);
            if let Some(dialog) = suspended {
                dialog.clear_artifact_picker(id);
            }
        }
    }

    pub(super) fn artifact_picker_for_load(
        &mut self,
        id: Uuid,
    ) -> Option<&mut super::super::artifacts_picker::ArtifactsPicker> {
        if let Some(picker) = self
            .artifacts_picker
            .as_mut()
            .filter(|picker| picker.id() == id)
        {
            return Some(picker);
        }
        if let Some(picker) = self
            .suspended_dialog
            .as_mut()
            .and_then(|dialog| dialog.artifacts_picker.as_mut())
            .filter(|picker| picker.id() == id)
        {
            return Some(picker);
        }
        for (active, suspended) in self.dialog_cache.values_mut() {
            if let Some(picker) = active
                .artifacts_picker
                .as_mut()
                .filter(|picker| picker.id() == id)
            {
                return Some(picker);
            }
            if let Some(picker) = suspended
                .as_mut()
                .and_then(|dialog| dialog.artifacts_picker.as_mut())
                .filter(|picker| picker.id() == id)
            {
                return Some(picker);
            }
        }
        None
    }

    pub(super) fn update_login(&mut self, menu: &Menu, address: &str, callback: bool) -> bool {
        if update_login_menu(
            &mut self.menu,
            &mut self.menu_link,
            &mut self.menu_comment,
            menu,
            address,
            callback,
        ) {
            return true;
        }
        if self
            .suspended_dialog
            .as_mut()
            .is_some_and(|dialog| dialog.update_login(menu, address, callback))
        {
            return true;
        }
        self.dialog_cache.values_mut().any(|(active, suspended)| {
            active.update_login(menu, address, callback)
                || suspended
                    .as_mut()
                    .is_some_and(|dialog| dialog.update_login(menu, address, callback))
        })
    }

    pub(super) fn complete_login(&mut self, id: Uuid, status: &str, notice: &str) -> bool {
        if self.menu.as_mut().is_some_and(|menu| {
            complete_login_menu(menu, &mut self.picker_presentation, id, status, notice)
        }) || self
            .suspended_dialog
            .as_mut()
            .is_some_and(|dialog| dialog.complete_login(id, status, notice))
        {
            return true;
        }
        self.dialog_cache.values_mut().any(|(active, suspended)| {
            active.complete_login(id, status, notice)
                || suspended
                    .as_mut()
                    .is_some_and(|dialog| dialog.complete_login(id, status, notice))
        })
    }

    pub(super) fn clear_hidden_menu(&mut self, id: Uuid) -> bool {
        if self
            .suspended_dialog
            .as_mut()
            .is_some_and(|dialog| dialog.clear_menu(id))
        {
            return true;
        }
        for (active, suspended) in self.dialog_cache.values_mut() {
            if active.clear_menu(id) {
                if let Some(user) = suspended.take() {
                    *active = user;
                }
                return true;
            }
            if suspended
                .as_mut()
                .is_some_and(|dialog| dialog.clear_menu(id))
            {
                return true;
            }
        }
        false
    }

    pub(super) fn suspend_dialog(&mut self) {
        if self.suspended_dialog.is_none() {
            self.suspended_dialog = Some(self.replace_dialog(DialogState::default()));
        }
    }

    pub(super) fn resume_dialog(&mut self) {
        if let Some(dialog) = self.suspended_dialog.take() {
            self.replace_dialog(dialog);
        }
    }

    pub(super) fn review_navigation_key(key: KeyEvent) -> Option<DiffNavigationAction> {
        match (key.code, key.modifiers) {
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
            | (KeyCode::Char('p'), KeyModifiers::CONTROL) => Some(DiffNavigationAction::Up),
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
            | (KeyCode::Char('n'), KeyModifiers::CONTROL) => Some(DiffNavigationAction::Down),
            (KeyCode::PageUp, _) => Some(DiffNavigationAction::PageUp),
            (KeyCode::PageDown, _) => Some(DiffNavigationAction::PageDown),
            (KeyCode::Home, _) => Some(DiffNavigationAction::Home),
            (KeyCode::End, _) => Some(DiffNavigationAction::End),
            _ => None,
        }
    }

    pub(super) fn full_review_frame(&mut self) -> Vec<RoleLine> {
        if self.menu_plan {
            let capacity = self.height.saturating_sub(2).max(1);
            let content = self
                .menu
                .as_ref()
                .map(|menu| {
                    menu.context
                        .iter()
                        .flat_map(|line| wrap_text(&display_text(line), self.width))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            self.context_offset = self
                .context_offset
                .min(content.len().saturating_sub(capacity));
            let mut lines = vec![RoleLine::role("Plan", Role::DialogTitle)];
            lines.extend(
                content
                    .into_iter()
                    .skip(self.context_offset)
                    .take(capacity)
                    .map(RoleLine::plain),
            );
            lines.resize(self.height.saturating_sub(1), RoleLine::default());
            lines.push(RoleLine::role(
                "v / esc return to plan approval",
                Role::MutedText,
            ));
            lines.truncate(self.height);
            for line in &mut lines {
                line.truncate(self.width);
            }
            return lines;
        }
        let review = self
            .menu_review
            .as_mut()
            .expect("full Diff belongs to a request");
        let viewport = DiffViewport {
            width: self.width,
            height: self.height.saturating_sub(2).max(1),
        };
        let frame = match &review.review {
            Ok(source) => render_diff(source, &review.navigation, viewport),
            Err(error) => render_error("Review unavailable", error, &review.navigation, viewport),
        };
        review.navigation = frame.navigation;
        review.viewport = viewport;
        let mut lines = vec![RoleLine::role("Diff", Role::DialogTitle)];
        lines.extend(
            frame
                .lines
                .into_iter()
                .take(self.height.saturating_sub(2))
                .map(|line| match line.kind {
                    DiffLineKind::Added => RoleLine::plain(line.text).filled(Role::DiffAdded),
                    DiffLineKind::Removed => RoleLine::plain(line.text).filled(Role::DiffRemoved),
                    DiffLineKind::Error => RoleLine::role(line.text, Role::Error),
                    _ => RoleLine::plain(line.text),
                }),
        );
        lines.resize(self.height.saturating_sub(1), RoleLine::default());
        lines.push(RoleLine::role("v / esc return to request", Role::MutedText));
        lines.truncate(self.height);
        for line in &mut lines {
            line.truncate(self.width);
        }
        lines
    }

    pub(super) fn input_dialog_has_fixed_height(&self) -> bool {
        self.workspace_trust.is_some()
            || self.request_layout.is_some()
            || self.question_menu.is_some()
            || self.menu_plan
    }

    pub(super) fn menu_context_rows(&self, menu: &Menu, limit: usize) -> usize {
        if let Some(review) = &self.menu_review {
            let viewport = DiffViewport {
                width: self.width.saturating_sub(2).max(1),
                height: limit,
            };
            match &review.review {
                Ok(source) => render_diff(source, &review.navigation, viewport).total_rows,
                Err(error) => {
                    render_error("Review unavailable", error, &review.navigation, viewport)
                        .total_rows
                }
            }
        } else {
            self.picker_context(menu)
                .iter()
                .map(|line| {
                    wrap_text(&display_text(line), self.width.saturating_sub(2).max(1)).len()
                })
                .sum()
        }
    }

    fn menu_content_rows(&self, menu: &Menu, limit: usize) -> usize {
        let selected = menu.selected.min(menu.options.len().saturating_sub(1));
        let context = self.menu_context_rows(menu, limit);
        let options: usize = if self.picker_kind.is_some() {
            menu.options.len()
                + if self.width < COMPACT_PICKER_COLUMNS {
                    self.picker_presentation
                        .compact_rows
                        .len()
                        .min(menu.options.len())
                } else {
                    0
                }
        } else {
            menu.options
                .iter()
                .enumerate()
                .map(|(index, option)| {
                    let text = format!(
                        "{} {}. {}",
                        self.display.glyph(Glyph::Prompt),
                        index + 1,
                        display_text(option)
                    );
                    wrap_text(&text, self.width).len()
                })
                .sum()
        };
        let headers = if self.picker_kind.is_some() {
            self.picker_presentation
                .headers
                .iter()
                .map(|(index, _)| *index)
                .filter(|index| *index < menu.options.len())
                .collect::<HashSet<_>>()
                .len()
        } else {
            0
        };
        let search = usize::from(self.picker_search.is_some());
        (2 + search + context + options + headers + self.menu_comment_frame(selected).0.len())
            .min(limit)
    }

    pub(super) fn input_dialog_requires_resize(&self) -> bool {
        if !self.input_dialog_visible()
            || self.input_dialog_has_fixed_height()
            || self.full_review
            || self.transcript.is_some()
        {
            return false;
        }
        let minimum_body = if let Some(menu) = &self.menu {
            let selected = menu.selected.min(menu.options.len().saturating_sub(1));
            if self.menu_comment_is_editing() {
                3
            } else {
                3 + usize::from(self.picker_search.is_some())
                    + usize::from(!self.picker_presentation.headers.is_empty())
                    + usize::from(
                        self.width < COMPACT_PICKER_COLUMNS
                            && self
                                .picker_presentation
                                .compact_rows
                                .get(selected)
                                .is_some(),
                    )
            }
        } else if let Some(picker) = &self.artifacts_picker {
            picker.minimum_frame_rows()
        } else if self.conversation_picker.is_some() {
            4
        } else if let Some(surface) = &self.permissions_surface {
            surface.minimum_frame_rows(self.width)
        } else if self.monitor.confirmation.is_some()
            || self
                .monitor
                .detail
                .as_ref()
                .is_some_and(|detail| detail.confirmation().is_some())
        {
            4
        } else {
            3
        };
        self.input_dialog_height() < INPUT_DIALOG_RULE_ROWS + minimum_body
    }

    fn resize_frame(&mut self, capacity: usize) -> (Vec<RoleLine>, usize, usize) {
        self.dialog_cursor_visible = false;
        let title = self.menu.as_ref().map_or_else(
            || {
                if self.artifacts_picker.is_some() {
                    "Artifacts"
                } else if let Some(picker) = &self.conversation_picker {
                    picker.title()
                } else if self.permissions_surface.is_some() {
                    "Permissions"
                } else if self.help_open {
                    "Help"
                } else {
                    "Tick action"
                }
            },
            |menu| menu.title.as_str(),
        );
        let mut lines = Vec::new();
        if capacity >= 2 {
            lines.push(RoleLine::role(title, Role::DialogTitle));
        }
        lines.resize(capacity.saturating_sub(1), RoleLine::default());
        lines.push(RoleLine::role(
            if self.user_menu_submission_is_unfinished() {
                MENU_SUBMISSION_PENDING_HINT
            } else if self.menu_comment_is_editing() {
                "Resize · esc close field"
            } else {
                "Resize · esc back"
            },
            Role::MutedText,
        ));
        for line in &mut lines {
            line.truncate(self.width);
        }
        (lines, 0, 0)
    }

    pub(super) fn input_dialog_height(&self) -> usize {
        if self.workspace_trust.is_some() && self.chat.is_none() {
            return self.height.min(FIXED_INPUT_DIALOG_ROWS);
        }
        if self.input_dialog_has_fixed_height() {
            return fixed_height_for_terminal(self.height);
        }
        let maximum = self
            .height
            .saturating_sub(PICKER_CONTENT_RESERVE_ROWS)
            .max(1);
        let body = if let Some(menu) = &self.menu {
            self.menu_content_rows(menu, maximum)
        } else if let Some(picker) = &self.artifacts_picker {
            picker.content_rows()
        } else if let Some(picker) = &self.conversation_picker {
            picker.content_rows()
        } else if let Some(surface) = &self.permissions_surface {
            surface.content_rows(self.width, maximum)
        } else if self.help_open {
            1 + agent_layout::help_lines(self.width, self.width + 1, None).len()
        } else if self.monitor.confirmation.is_some()
            || self
                .monitor
                .detail
                .as_ref()
                .is_some_and(|detail| detail.confirmation().is_some())
        {
            4
        } else {
            return maximum;
        };
        (INPUT_DIALOG_RULE_ROWS + body).min(maximum)
    }

    pub(super) fn input_dialog_visible(&self) -> bool {
        self.menu.is_some()
            || self.workspace_trust.is_some()
            || self.permissions_surface.is_some()
            || self.artifacts_picker.is_some()
            || self.conversation_picker.is_some()
            || self.help_open && self.overview.projection.is_none()
            || self.monitor.confirmation.is_some()
            || self
                .monitor
                .detail
                .as_ref()
                .is_some_and(|detail| detail.confirmation().is_some())
    }

    pub(super) fn input_dialog_body_height(&self) -> usize {
        body_height(self.input_dialog_height())
    }

    pub(super) fn input_dialog_frame(&mut self) -> (Vec<RoleLine>, usize, usize) {
        self.dialog_option_rows.clear();
        self.dialog_scrolls_own_content = false;
        let height = self.input_dialog_height();
        let capacity = self.input_dialog_body_height();
        let rule_rows = height - capacity;
        let role = if self.workspace_trust.is_some() {
            Role::Warning
        } else {
            self.menu_rule_role.unwrap_or(Role::Permission)
        };
        let (mut lines, row, column) = self.input_dialog_body_frame(capacity);
        if rule_rows > 0 {
            lines.insert(
                0,
                RoleLine::role(INPUT_DIALOG_RULE_GLYPH.repeat(self.width), role),
            );
            for (rows, _) in &mut self.dialog_option_rows {
                rows.start += rule_rows;
                rows.end += rule_rows;
            }
        }
        (lines, row + rule_rows, column)
    }

    fn input_dialog_body_frame(&mut self, capacity: usize) -> (Vec<RoleLine>, usize, usize) {
        if self.input_dialog_requires_resize() {
            return self.resize_frame(capacity);
        }
        if let Some(dialog) = &self.workspace_trust {
            let frame = dialog.frame(self.width, capacity, self.display);
            self.dialog_cursor_visible = frame.cursor.is_some();
            let (row, column) = frame.cursor.unwrap_or_default();
            let mut lines = frame.lines;
            pad_dialog(&mut lines, capacity);
            return (lines, row.min(capacity - 1), column);
        }
        if self.menu.is_some() {
            return self.request_styled_frame(capacity);
        }
        let frame = if let Some(picker) = &mut self.artifacts_picker {
            Some(picker.frame(self.width, capacity, self.display))
        } else {
            self.conversation_picker
                .as_mut()
                .map(|picker| picker.frame_in_dialog(self.width, capacity, self.display))
        };
        if let Some(frame) = frame {
            self.dialog_cursor_visible =
                frame.cursor.is_some_and(|(row, _)| row < frame.lines.len());
            let (row, column) = frame.cursor.unwrap_or_default();
            return (
                frame.lines,
                row.min(capacity - 1),
                column.min(self.width.saturating_sub(1)),
            );
        }
        let mut surface_lines = self
            .permissions_surface
            .as_mut()
            .map(|surface| surface.styled_lines(self.width, capacity));
        if let Some(ref mut lines) = surface_lines {
            pad_dialog(lines, capacity);
            for line in lines.iter_mut() {
                line.truncate(self.width);
            }
            return (std::mem::take(lines), 0, 0);
        }
        let mut lines = Vec::new();
        if self.help_open {
            lines.push(RoleLine::role("Help", Role::DialogTitle));
            let mut body = agent_layout::help_lines(self.width, self.width + 1, None);
            body.pop();
            let available = capacity.saturating_sub(2);
            self.help_offset = self.help_offset.min(body.len().saturating_sub(available));
            lines.extend(body.into_iter().skip(self.help_offset).take(available));
            lines.resize(capacity.saturating_sub(1), RoleLine::default());
            lines.push(RoleLine::role("esc / ? close help", Role::MutedText));
        } else {
            let detail = self
                .monitor
                .detail
                .as_ref()
                .and_then(|detail| detail.confirmation());
            let (title, message, label) = detail
                .map(|view| (view.title, view.message, view.label))
                .unwrap_or_else(|| {
                    (
                        "Tick action".into(),
                        self.monitor
                            .confirmation
                            .as_ref()
                            .map(|c| c.prompt.clone())
                            .unwrap_or_default(),
                        "Confirm".into(),
                    )
                });
            lines.push(RoleLine::role(title, Role::DialogTitle));
            lines.push(RoleLine::plain(message));
            lines.push(RoleLine::role(format!("› {label}"), Role::Text).filled(Role::Selection));
            self.dialog_option_rows.push((2..3, 0));
            lines.resize(capacity.saturating_sub(1), RoleLine::default());
            lines.push(RoleLine::role("enter confirm · esc back", Role::MutedText));
        }
        lines.truncate(capacity);
        for line in &mut lines {
            line.truncate(self.width);
        }
        (lines, 0, 0)
    }
}
