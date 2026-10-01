use super::*;
use tickloop_agent::model::credentials::ApiKeyStatus;

impl Menus {
    pub(in crate::terminal) async fn judgment_settings_from_permissions(
        &mut self,
        agent: &Agent,
        console: &Console,
        subject: PermissionsSubject,
        select_reviewer: bool,
    ) -> Result<(), String> {
        self.judgment_settings(
            agent,
            console,
            JudgmentReturn::AutoSettings {
                subject,
                select_reviewer,
            },
        )
        .await
    }

    pub(super) async fn judgment_settings(
        &mut self,
        agent: &Agent,
        console: &Console,
        return_to: JudgmentReturn,
    ) -> Result<(), String> {
        let status = agent
            .jev_key_status()
            .await
            .map_err(|error| error.to_string())?;
        let has_key = status != ApiKeyStatus::Missing;
        let select_reviewer = matches!(
            &return_to,
            JudgmentReturn::AutoSettings {
                select_reviewer: true,
                ..
            }
        );
        let mut options = Vec::new();
        if select_reviewer && has_key {
            options.push("Use existing key · select Judgment".into());
        }
        options.push(if has_key {
            "Update Jev (TypeSafe) key".into()
        } else {
            "Set Jev (TypeSafe) key".into()
        });
        options.push("Back".into());
        let key_status = match status {
            ApiKeyStatus::Saved => "Saved in Agent settings",
            ApiKeyStatus::Environment => "Available from environment",
            ApiKeyStatus::Missing => "Missing",
        };
        let mut context = vec![
            format!("Jev (TypeSafe) key: {key_status}"),
            "Authentication is checked when Judgment is used.".into(),
        ];
        if select_reviewer {
            context.push(
                "Selecting Judgment changes the Auto reviewer only after confirmation.".into(),
            );
        }
        self.show(
            console,
            Menu {
                id: Uuid::new_v4(),
                title: "Judgment settings · Jev (TypeSafe)".into(),
                context,
                options,
                selected: 0,
                confirmation: false,
            },
            Selection::JudgmentSettings { return_to, has_key },
        )
    }

    pub(super) fn judgment_key(
        &mut self,
        console: &Console,
        return_to: JudgmentReturn,
    ) -> Result<(), String> {
        if !console.is_interactive() {
            return Err("Jev (TypeSafe) key entry requires an interactive terminal".into());
        }
        let menu = Menu {
            id: Uuid::new_v4(),
            title: "Jev (TypeSafe) key".into(),
            context: vec![
                "Enter a key for Judgment. Its contents stay hidden.".into(),
                "Saving the key does not change the Chat model or permission mode.".into(),
            ],
            options: vec!["Save key".into(), "Cancel".into()],
            selected: 0,
            confirmation: false,
        };
        self.dismiss(console)?;
        let id = menu.id;
        console.show_secret_menu(menu)?;
        self.active = Some((id, Selection::JudgmentKey { return_to }));
        Ok(())
    }

    pub(super) async fn return_from_judgment(
        &mut self,
        agent: &Agent,
        console: &Console,
        return_to: JudgmentReturn,
    ) -> Result<(), String> {
        match return_to {
            JudgmentReturn::Providers { target } => self.back_to_providers(console, target),
            JudgmentReturn::AutoSettings {
                subject: PermissionsSubject::Agent { chat, .. },
                ..
            } => {
                let view = agent
                    .permissions_view(chat)
                    .await
                    .map_err(|error| error.to_string())?;
                console.permissions_refreshed(view)
            }
            JudgmentReturn::AutoSettings {
                subject: PermissionsSubject::NewDraft { project, .. },
                ..
            } => {
                let page = agent
                    .draft_permissions(&project)
                    .await
                    .map_err(|error| error.to_string())?;
                console.permissions_refreshed(page)
            }
        }
    }

    pub(super) async fn finish_judgment_selection(
        &mut self,
        agent: &Agent,
        console: &Console,
        return_to: JudgmentReturn,
        key_saved: bool,
    ) -> Result<(), String> {
        let JudgmentReturn::AutoSettings { subject, .. } = return_to else {
            return self.return_from_judgment(agent, console, return_to).await;
        };
        let (chat, revision) = match subject {
            PermissionsSubject::Agent { chat, revision } => (chat, revision),
            PermissionsSubject::NewDraft { project, revision } => {
                return self
                    .finish_draft_judgment_selection(agent, console, &project, &revision, key_saved)
                    .await;
            }
        };
        match agent
            .apply_permission_change(
                chat,
                PermissionChange::SelectReviewer {
                    reviewer: AutoReviewerPreference::Jev,
                },
                revision,
            )
            .await
        {
            Ok(view) if key_saved => console.permissions_key_and_reviewer_saved(view),
            Ok(view) => console.permissions_saved(view),
            Err(PermissionChangeError::Conflict { latest }) if key_saved => console
                .permissions_key_saved_choice_failed(
                    *latest,
                    "settings changed elsewhere; review and retry".into(),
                ),
            Err(PermissionChangeError::Conflict { latest }) => {
                console.permissions_conflict(*latest)
            }
            Err(error) => {
                let view = agent
                    .permissions_view(chat)
                    .await
                    .map_err(|refresh_error| refresh_error.to_string())?;
                if key_saved {
                    console.permissions_key_saved_choice_failed(view, safe_text(&error.to_string()))
                } else {
                    console.permissions_refreshed(view)?;
                    console.permissions_error(safe_text(&error.to_string()))
                }
            }
        }
    }
    /// Selects Jev for the New Agent screen: the reviewer is a saved Auto setting, written now
    /// against the revision the view was read at; no Agent is created.
    async fn finish_draft_judgment_selection(
        &mut self,
        agent: &Agent,
        console: &Console,
        project: &std::path::Path,
        revision: &super::super::permissions_surface::DraftPolicyRevision,
        key_saved: bool,
    ) -> Result<(), String> {
        use super::super::execution::DraftPermissionsError;
        match agent
            .change_draft_permissions(
                project,
                revision,
                PermissionChange::SelectReviewer {
                    reviewer: AutoReviewerPreference::Jev,
                },
            )
            .await
        {
            Ok(change) if key_saved => console.permissions_key_and_reviewer_saved(change.page),
            Ok(change) => console.permissions_saved(change.page),
            Err(DraftPermissionsError::Conflict(latest)) if key_saved => console
                .permissions_key_saved_choice_failed(
                    *latest,
                    "settings changed elsewhere; review and retry".into(),
                ),
            Err(DraftPermissionsError::Conflict(latest)) => console.permissions_conflict(*latest),
            Err(DraftPermissionsError::Rejected(error)) => {
                let page = agent
                    .draft_permissions(project)
                    .await
                    .map_err(|refresh_error| refresh_error.to_string())?;
                if key_saved {
                    console.permissions_key_saved_choice_failed(page, safe_text(&error.to_string()))
                } else {
                    console.permissions_refreshed(page)?;
                    console.permissions_error(safe_text(&error.to_string()))
                }
            }
        }
    }
}
