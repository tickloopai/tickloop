//! The views attached to one screen the host serves, a hosted Tick's terminal or a Shell task's
//! view, and the one-time grants that let them in.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use tickhost_connect::hosted::CONTROL_TIMEOUT;
use uuid::Uuid;

use crate::pty::TerminalSize;

/// How long a view grant waits for its view to connect.
pub const VIEW_GRANT_TIMEOUT: Duration = CONTROL_TIMEOUT;

/// One-time permissions for views to attach to a screen: each admits the view it names once
/// before it expires, whatever grants were issued since.
#[derive(Default)]
pub(crate) struct ViewGrants {
    expiries: HashMap<Uuid, Instant>,
}

impl ViewGrants {
    /// Lets `view` attach until the grant expires; expired grants go.
    pub(crate) fn allow(&mut self, view: Uuid, now: Instant) {
        self.expiries.retain(|_, expires| now < *expires);
        self.expiries.insert(view, now + VIEW_GRANT_TIMEOUT);
    }

    /// Takes the grant of `view` if it has not expired; a grant works once.
    pub(crate) fn claim(&mut self, view: Uuid, now: Instant) -> bool {
        self.expiries
            .remove(&view)
            .is_some_and(|expires| now < expires)
    }
}

/// The views attached to one screen. The screen takes the size of the view that typed last, or
/// of the newest view while none of them typed; without views it keeps its size.
#[derive(Default)]
pub(crate) struct Views {
    /// Orders attaches and input; a view is named by the moment it attached.
    clock: u64,
    attached: HashMap<u64, AttachedView>,
}

struct AttachedView {
    size: TerminalSize,
    attached_at: u64,
    typed_at: Option<u64>,
}

impl Views {
    pub(crate) fn attach(&mut self, size: TerminalSize) -> u64 {
        let view = self.advance();
        self.attached.insert(
            view,
            AttachedView {
                size,
                attached_at: view,
                typed_at: None,
            },
        );
        view
    }

    pub(crate) fn typed(&mut self, view: u64) {
        let now = self.advance();
        if let Some(attached) = self.attached.get_mut(&view) {
            attached.typed_at = Some(now);
        }
    }

    pub(crate) fn resized(&mut self, view: u64, size: TerminalSize) {
        if let Some(attached) = self.attached.get_mut(&view) {
            attached.size = size;
        }
    }

    pub(crate) fn detach(&mut self, view: u64) {
        self.attached.remove(&view);
    }

    pub(crate) fn len(&self) -> usize {
        self.attached.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.attached.is_empty()
    }

    pub(crate) fn leading_size(&self) -> Option<TerminalSize> {
        self.attached
            .values()
            .max_by_key(|view| (view.typed_at, view.attached_at))
            .map(|view| view.size)
    }

    fn advance(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_view_grant_is_claimed_once_by_its_own_view_before_it_expires() {
        let now = Instant::now();
        let (earlier, later) = (Uuid::new_v4(), Uuid::new_v4());
        let mut grants = ViewGrants::default();
        grants.allow(earlier, now);
        grants.allow(later, now);
        assert!(!grants.claim(Uuid::new_v4(), now), "a view nobody allowed");
        assert!(grants.claim(later, now));
        assert!(!grants.claim(later, now), "a grant works once");
        assert!(
            grants.claim(earlier, now),
            "a newer grant leaves an earlier one usable"
        );
        grants.allow(earlier, now);
        assert!(
            !grants.claim(earlier, now + VIEW_GRANT_TIMEOUT),
            "an expired grant admits nothing"
        );
        grants.allow(later, now);
        grants.allow(earlier, now + VIEW_GRANT_TIMEOUT);
        assert_eq!(
            grants.expiries.keys().collect::<Vec<_>>(),
            [&earlier],
            "expired grants go"
        );
    }

    #[test]
    fn the_screen_follows_the_view_that_typed_last_or_else_the_newest() {
        let size = |rows| TerminalSize::validated(rows, 80).unwrap();
        let mut views = Views::default();
        assert_eq!(views.leading_size(), None, "no view sizes the screen");
        let first = views.attach(size(20));
        let second = views.attach(size(24));
        assert_eq!(
            views.leading_size(),
            Some(size(24)),
            "the newest view while none typed"
        );
        views.typed(first);
        assert_eq!(views.leading_size(), Some(size(20)));
        let third = views.attach(size(30));
        assert_eq!(
            views.leading_size(),
            Some(size(20)),
            "a newer view does not take over from one that typed"
        );
        views.resized(second, size(26));
        assert_eq!(
            views.leading_size(),
            Some(size(20)),
            "another view's resize changes only that view"
        );
        views.resized(first, size(22));
        assert_eq!(views.leading_size(), Some(size(22)));
        views.typed(second);
        assert_eq!(views.leading_size(), Some(size(26)));
        views.detach(second);
        assert_eq!(
            views.leading_size(),
            Some(size(22)),
            "then the view that typed before it"
        );
        views.detach(first);
        assert_eq!(views.leading_size(), Some(size(30)), "then the newest");
        views.detach(third);
        assert!(views.is_empty());
        assert_eq!(views.leading_size(), None);
    }
}
