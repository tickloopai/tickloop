use super::*;
use crate::terminal::prompt_suggestion::SuggestionRequest;
use tokio_util::sync::CancellationToken;

impl Console {
    pub(in crate::terminal) fn prompt_suggestion_enabled(&self) -> Result<bool, String> {
        Ok(self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .prompt_suggestion_enabled)
    }

    pub(in crate::terminal) fn prompt_suggestion_suppression(
        &self,
    ) -> Result<Option<&'static str>, String> {
        Ok(self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .prompt_suggestion_suppression())
    }

    pub(in crate::terminal) fn set_prompt_suggestion_enabled(
        &self,
        enabled: bool,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.prompt_suggestion_enabled = enabled;
        if !enabled {
            state.clear_prompt_suggestion();
        }
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(in crate::terminal) fn clear_prompt_suggestion(&self) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if state.prompt_suggestion.is_none() && state.prompt_suggestion_request.is_none() {
            return Ok(());
        }
        state.clear_prompt_suggestion();
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    pub(in crate::terminal) fn begin_prompt_suggestion(
        &self,
        chat: Uuid,
    ) -> Result<Option<SuggestionRequest>, String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.clear_prompt_suggestion();
        if state.chat != Some(chat) || !state.can_show_prompt_suggestion() {
            return Ok(None);
        }
        let request = SuggestionRequest {
            chat,
            id: tickloop_ids::generate(),
            cancel: CancellationToken::new(),
        };
        state.prompt_suggestion_request = Some(request.clone());
        Ok(Some(request))
    }

    pub(in crate::terminal) fn finish_prompt_suggestion(
        &self,
        request: &SuggestionRequest,
        text: Option<String>,
    ) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if state.chat != Some(request.chat)
            || state
                .prompt_suggestion_request
                .as_ref()
                .is_none_or(|current| current.id != request.id)
        {
            return Ok(());
        }
        if !request.cancel.is_cancelled() && state.can_show_prompt_suggestion() {
            state.prompt_suggestion = text;
        }
        state.prompt_suggestion_request = None;
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }
}

impl State {
    pub(super) fn clear_prompt_suggestion(&mut self) {
        if let Some(request) = self.prompt_suggestion_request.take() {
            request.cancel.cancel();
        }
        self.prompt_suggestion = None;
    }

    fn can_show_prompt_suggestion(&self) -> bool {
        self.prompt_suggestion_suppression().is_none()
    }

    fn prompt_suggestion_suppression(&self) -> Option<&'static str> {
        if !self.interactive {
            Some("suppressed: noninteractive View")
        } else if !self.prompt_suggestion_enabled {
            Some("suppressed: setting Off")
        } else if self.chat.is_none() {
            Some("suppressed: no selected Agent")
        } else if self.visible_delegated {
            Some("suppressed: delegated Agent (return to Main)")
        } else if !self.ordinary_input_is_visible()
            || self.input_dialog_visible()
            || self.request_layout.is_some()
        {
            Some("suppressed: input hidden or Input Dialog active")
        } else if self.conversation_focused
            || self.footer.selected().is_some()
            || self.monitor.focused
        {
            Some("suppressed: input unfocused")
        } else if !self.editor.draft().is_empty()
            || !self.restored_contexts.is_empty()
            || !self.paste_blocks.images("").is_empty()
        {
            Some("suppressed: draft or attachments present")
        } else {
            None
        }
    }

    pub(super) fn visible_prompt_suggestion(&self) -> Option<&str> {
        self.can_show_prompt_suggestion()
            .then_some(self.prompt_suggestion.as_deref())
            .flatten()
    }
}
