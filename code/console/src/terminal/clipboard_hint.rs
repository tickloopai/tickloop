use std::sync::mpsc::{self, TryRecvError};
use std::time::{Duration, Instant};

use super::clipboard::Clipboard;

pub(super) const CLIPBOARD_IMAGE_HINT: &str = "Image in clipboard · ctrl+v to paste";
/// Batches rapid focus changes into one clipboard check.
const FOCUS_CHECK_DEBOUNCE: Duration = Duration::from_secs(1);
const HINT_COOLDOWN: Duration = Duration::from_secs(30);
const HINT_DURATION: Duration = Duration::from_secs(8);

/// Claude Code `useClipboardImageHint`: when the terminal regains focus and the
/// clipboard holds an image the model can take, remind the user that `Ctrl+V`
/// pastes it. Time is passed in by the caller so the windows can be tested.
pub(super) struct ClipboardImageHint {
    focused: bool,
    check_due_at: Option<Instant>,
    pending_check: Option<mpsc::Receiver<bool>>,
    last_shown_at: Option<Instant>,
    visible_until: Option<Instant>,
}

impl ClipboardImageHint {
    pub(super) fn new() -> Self {
        Self {
            focused: true,
            check_due_at: None,
            pending_check: None,
            last_shown_at: None,
            visible_until: None,
        }
    }

    pub(super) fn focus_lost(&mut self) {
        self.focused = false;
        self.check_due_at = None;
    }

    /// Only a return after the focus was lost counts, so a terminal that reports
    /// focus as soon as reporting is enabled does not trigger a check.
    pub(super) fn focus_gained(&mut self, now: Instant) {
        if !self.focused {
            self.check_due_at = Some(now + FOCUS_CHECK_DEBOUNCE);
        }
        self.focused = true;
    }

    /// Advances the debounce, the clipboard check and the display window.
    /// Returns whether the hint appeared or disappeared, so the caller redraws.
    pub(super) fn poll(
        &mut self,
        now: Instant,
        accepts_images: bool,
        clipboard: &Clipboard,
    ) -> bool {
        let expired = self.visible_until.is_some_and(|until| now >= until);
        if expired {
            self.visible_until = None;
        }
        if self.check_due_at.is_some_and(|due| now >= due) {
            self.check_due_at = None;
            if accepts_images && self.pending_check.is_none() && self.cooled_down(now) {
                self.pending_check = Some(clipboard.has_image());
            }
        }
        let has_image = match self.pending_check.as_ref().map(mpsc::Receiver::try_recv) {
            Some(Ok(has_image)) => has_image,
            Some(Err(TryRecvError::Disconnected)) => false,
            Some(Err(TryRecvError::Empty)) | None => return expired,
        };
        self.pending_check = None;
        if has_image && self.cooled_down(now) {
            self.last_shown_at = Some(now);
            self.visible_until = Some(now + HINT_DURATION);
            return true;
        }
        expired
    }

    pub(super) fn is_visible(&self) -> bool {
        self.visible_until.is_some()
    }

    fn cooled_down(&self, now: Instant) -> bool {
        self.last_shown_at
            .is_none_or(|shown_at| now.saturating_duration_since(shown_at) >= HINT_COOLDOWN)
    }
}
