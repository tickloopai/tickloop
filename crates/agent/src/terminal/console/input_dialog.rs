use super::*;

pub(super) const INPUT_DIALOG_ROWS: usize = 12;
pub(super) const INPUT_DIALOG_RULE_ROWS: usize = 1;
const INPUT_DIALOG_RULE_GLYPH: &str = "▔";
const MIN_CONTENT_ROWS: usize = 4;

pub(super) fn body_height(height: usize) -> usize {
    height.saturating_sub(INPUT_DIALOG_RULE_ROWS).max(1)
}

pub(super) fn height_for_terminal(height: usize) -> usize {
    height
        .saturating_sub(MIN_CONTENT_ROWS)
        .clamp(1, INPUT_DIALOG_ROWS)
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
    pub(super) task_surface: Option<TaskSurface<Uuid>>,
    pub(super) subscription_surface: Option<SubscriptionSurface>,
    pub(super) permissions_surface: Option<PermissionsSurface>,
    help_open: bool,
    help_offset: usize,
}

impl DialogState {
    fn update_login(&mut self, menu: &Menu, address: &str, callback: bool) -> bool {
        if self
            .menu
            .as_ref()
            .is_none_or(|current| current.id != menu.id)
        {
            return false;
        }
        let selected = self.menu.as_ref().map_or(0, |current| current.selected);
        self.menu = Some(Menu {
            selected: selected.min(menu.options.len().saturating_sub(1)),
            ..menu.clone()
        });
        self.menu_link = valid_login_link(address)
            .filter(|_| menu.context.iter().any(|line| line == address))
            .map(|address| (menu.id, address));
        if callback && self.menu_comment.is_none() {
            self.menu_comment = Some(MenuCommentState {
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

impl State {
    pub(super) fn suspended_task_chats(&self) -> Vec<Uuid> {
        self.suspended_dialog
            .iter()
            .chain(
                self.dialog_cache.values().flat_map(|(active, suspended)| {
                    std::iter::once(active).chain(suspended.iter())
                }),
            )
            .filter_map(|dialog| {
                dialog
                    .task_surface
                    .as_ref()
                    .map(|surface| *surface.restore())
            })
            .chain(self.task_surface_cache.keys().copied())
            .collect()
    }

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

    pub(super) fn subscription_surfaces_mut(
        &mut self,
    ) -> impl Iterator<Item = &mut SubscriptionSurface> {
        self.subscription_surface
            .iter_mut()
            .chain(
                self.suspended_dialog
                    .iter_mut()
                    .filter_map(|dialog| dialog.subscription_surface.as_mut()),
            )
            .chain(
                self.dialog_cache
                    .values_mut()
                    .flat_map(|(active, suspended)| {
                        active.subscription_surface.iter_mut().chain(
                            suspended
                                .iter_mut()
                                .filter_map(|dialog| dialog.subscription_surface.as_mut()),
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
            task_surface: std::mem::replace(&mut self.task_surface, next.task_surface),
            subscription_surface: std::mem::replace(
                &mut self.subscription_surface,
                next.subscription_surface,
            ),
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

    pub(super) fn hidden_task_surface_mut(&mut self, chat: Uuid) -> Option<&mut TaskSurface<Uuid>> {
        if let Some(surface) = self
            .suspended_dialog
            .as_mut()
            .and_then(|dialog| dialog.task_surface.as_mut())
            .filter(|surface| *surface.restore() == chat)
        {
            return Some(surface);
        }
        for (active, suspended) in self.dialog_cache.values_mut() {
            if let Some(surface) = active
                .task_surface
                .as_mut()
                .filter(|surface| *surface.restore() == chat)
            {
                return Some(surface);
            }
            if let Some(surface) = suspended
                .as_mut()
                .and_then(|dialog| dialog.task_surface.as_mut())
                .filter(|surface| *surface.restore() == chat)
            {
                return Some(surface);
            }
        }
        self.task_surface_cache.get_mut(&chat)
    }

    pub(super) fn task_surface_mut(&mut self, chat: Uuid) -> Option<&mut TaskSurface<Uuid>> {
        if self
            .task_surface
            .as_ref()
            .is_some_and(|surface| *surface.restore() == chat)
        {
            return self.task_surface.as_mut();
        }
        self.hidden_task_surface_mut(chat)
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

    pub(super) fn update_hidden_login(
        &mut self,
        menu: &Menu,
        address: &str,
        callback: bool,
    ) -> bool {
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
            self.resumed_task_effect = None;
        }
    }

    pub(super) fn reconcile_resumed_task(&mut self) {
        self.resumed_task_effect = self.task_surface.as_mut().and_then(|surface| {
            let effect = surface.resume();
            (effect != TaskSurfaceEffect::None).then(|| (*surface.restore(), effect))
        });
    }

    pub(super) fn resume_dialog(&mut self) {
        if let Some(dialog) = self.suspended_dialog.take() {
            self.replace_dialog(dialog);
            self.reconcile_resumed_task();
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
                "v / Esc return to plan approval",
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
        lines.push(RoleLine::role("v / Esc return to request", Role::MutedText));
        lines.truncate(self.height);
        for line in &mut lines {
            line.truncate(self.width);
        }
        lines
    }

    pub(super) fn input_dialog_height(&self) -> usize {
        if self.workspace_trust.is_some() && self.chat.is_none() {
            return self.height.min(INPUT_DIALOG_ROWS);
        }
        height_for_terminal(self.height)
    }

    pub(super) fn input_dialog_visible(&self) -> bool {
        self.menu.is_some()
            || self.workspace_trust.is_some()
            || self.task_surface.is_some()
            || self.permissions_surface.is_some()
            || self.subscription_surface.is_some()
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
        let mut surface_lines = if let Some(surface) = &mut self.permissions_surface {
            Some(surface.styled_lines(self.width, capacity))
        } else if let Some(surface) = &mut self.subscription_surface {
            Some(
                subscription_surface_lines(surface, capacity, self.display)
                    .into_iter()
                    .map(RoleLine::plain)
                    .collect(),
            )
        } else {
            self.task_surface.as_mut().map(|surface| {
                task_surface_lines(surface, self.width, capacity, None, self.display)
                    .into_iter()
                    .map(RoleLine::plain)
                    .collect()
            })
        };
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
            lines.push(RoleLine::role("Esc / ? close help", Role::MutedText));
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
            lines.resize(capacity, RoleLine::default());
        }
        lines.truncate(capacity);
        for line in &mut lines {
            line.truncate(self.width);
        }
        (lines, 0, 0)
    }
}
