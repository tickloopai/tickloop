//! The one management panel a Chat's dialog shows at a time: `/plugins` or `/ticks`, or `/mcp`.

use crossterm::event::KeyEvent;
use uuid::Uuid;

use super::{
    conversation_picker::ConversationPickerFrame,
    display::DisplayProfile,
    mcp_panel::{McpPanel, McpPanelEffect, McpRequest},
    ticks_panel::{TickPackageRequest, TicksPanel, TicksPanelEffect},
};

// A dialog holds at most one panel, so its size does not add up.
#[allow(clippy::large_enum_variant)]
pub(super) enum UserPanel {
    Packages(TicksPanel),
    Mcp(McpPanel),
}

pub(super) enum UserPanelEffect {
    None,
    Close,
    /// Edits the panel's text field in the external editor.
    EditText,
    Packages(TickPackageRequest),
    Mcp(McpRequest),
}

impl UserPanel {
    pub(super) fn id(&self) -> Uuid {
        match self {
            Self::Packages(panel) => panel.id(),
            Self::Mcp(panel) => panel.id(),
        }
    }

    pub(super) fn chat(&self) -> Option<Uuid> {
        match self {
            Self::Packages(panel) => panel.chat,
            Self::Mcp(panel) => panel.chat,
        }
    }

    pub(super) fn title(&self) -> &'static str {
        match self {
            Self::Packages(panel) => panel.title(),
            Self::Mcp(panel) => panel.title(),
        }
    }

    pub(super) fn minimum_frame_rows(&self, width: usize) -> usize {
        match self {
            Self::Packages(panel) => panel.minimum_frame_rows(width),
            Self::Mcp(panel) => panel.minimum_frame_rows(width),
        }
    }

    pub(super) fn content_rows(&self, width: usize) -> usize {
        match self {
            Self::Packages(panel) => panel.content_rows(width),
            Self::Mcp(panel) => panel.content_rows(width),
        }
    }

    pub(super) fn frame(
        &mut self,
        width: usize,
        height: usize,
        display: DisplayProfile,
    ) -> ConversationPickerFrame {
        match self {
            Self::Packages(panel) => panel.frame(width, height, display),
            Self::Mcp(panel) => panel.frame(width, height, display),
        }
    }

    pub(super) fn select_at_row(&mut self, row: usize) -> bool {
        match self {
            Self::Packages(panel) => panel.select_at_row(row),
            Self::Mcp(panel) => panel.select_at_row(row),
        }
    }

    pub(super) fn paste(&mut self, text: &str) {
        match self {
            Self::Packages(panel) => panel.paste(text),
            Self::Mcp(panel) => panel.paste(text),
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> UserPanelEffect {
        match self {
            Self::Packages(panel) => match panel.key(key) {
                TicksPanelEffect::None => UserPanelEffect::None,
                TicksPanelEffect::Close => UserPanelEffect::Close,
                TicksPanelEffect::EditFilter => UserPanelEffect::EditText,
                TicksPanelEffect::Work(request) => UserPanelEffect::Packages(request),
            },
            Self::Mcp(panel) => match panel.key(key) {
                McpPanelEffect::None => UserPanelEffect::None,
                McpPanelEffect::Close => UserPanelEffect::Close,
                McpPanelEffect::EditCallback => UserPanelEffect::EditText,
                McpPanelEffect::Work(request) => UserPanelEffect::Mcp(request),
            },
        }
    }

    /// The text the external editor starts from.
    pub(super) fn editable_text(&self) -> &str {
        match self {
            Self::Packages(panel) => panel.filter_text(),
            Self::Mcp(panel) => panel.callback_text(),
        }
    }

    pub(super) fn replace_text(&mut self, text: &str) {
        match self {
            Self::Packages(panel) => panel.replace_filter(text),
            Self::Mcp(panel) => panel.replace_callback(text),
        }
    }

    pub(super) fn report(&mut self, error: String) {
        match self {
            Self::Packages(panel) => panel.report(error),
            Self::Mcp(panel) => panel.report(error),
        }
    }

    pub(super) fn cancel_uninstall(&mut self) {
        if let Self::Packages(panel) = self {
            panel.cancel_uninstall();
        }
    }

    pub(super) fn expire_confirmation(&mut self) -> bool {
        match self {
            Self::Packages(panel) => panel.expire_confirmation(),
            Self::Mcp(_) => false,
        }
    }
}
