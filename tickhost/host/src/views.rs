//! The views attached to one screen the host serves, a hosted Tick's terminal or a Shell task's
//! view, and the one-time grants that let them in.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use tick_protocol::{BubbledKey, KeySignal};
use tickhost_connect::hosted::CONTROL_TIMEOUT;
use tokio::sync::{mpsc, watch};
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

/// What a Tick asked of the terminal its views show that views act on: whether it bubbles the
/// keys it does not use, and whether it wants focus reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ViewModes {
    pub(crate) bubbling: bool,
    pub(crate) focus_reports: bool,
}

/// The views attached to one screen. The screen takes the size of the view that typed last, or
/// of the newest view while none of them typed; without views it keeps its size. The same view
/// gets the keys a Tick bubbles. Every listening view watches the Tick's [`ViewModes`]: a change
/// is never lost, however many keys a view leaves unread.
pub(crate) struct Views {
    /// Orders attaches and input; a view is named by the moment it attached.
    clock: u64,
    attached: HashMap<u64, AttachedView>,
    modes: watch::Sender<ViewModes>,
}

impl Default for Views {
    fn default() -> Self {
        Self {
            clock: 0,
            attached: HashMap::new(),
            modes: watch::channel(ViewModes::default()).0,
        }
    }
}

struct AttachedView {
    size: TerminalSize,
    attached_at: u64,
    typed_at: Option<u64>,
    /// Where this view takes the keys the Tick bubbles, once it listens; a view that does not
    /// take them in time misses them.
    keys: Option<mpsc::Sender<BubbledKey>>,
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
                keys: None,
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
        self.leading().map(|view| view.size)
    }

    fn leading(&self) -> Option<&AttachedView> {
        self.attached
            .values()
            .max_by_key(|view| (view.typed_at, view.attached_at))
    }

    /// Sends `view` the keys the Tick bubbles from now on, and watches its modes.
    pub(crate) fn listen(
        &mut self,
        view: u64,
        keys: mpsc::Sender<BubbledKey>,
    ) -> watch::Receiver<ViewModes> {
        if let Some(attached) = self.attached.get_mut(&view) {
            attached.keys = Some(keys);
        }
        self.modes.subscribe()
    }

    /// Records a key-bubbling signal of the Tick: whether it bubbles keys becomes one of its
    /// modes, and a bubbled key, while it bubbles keys, goes to the leading view.
    pub(crate) fn signal(&mut self, signal: KeySignal) {
        match signal {
            KeySignal::Bubbling(enabled) => self.change_modes(|modes| modes.bubbling = enabled),
            KeySignal::Key(key) if self.modes.borrow().bubbling => {
                if let Some(keys) = self.leading().and_then(|view| view.keys.as_ref()) {
                    let _ = keys.try_send(key);
                }
            }
            KeySignal::Key(_) => {}
        }
    }

    /// Records whether the Tick wants focus reports.
    pub(crate) fn focus_reports(&mut self, enabled: bool) {
        self.change_modes(|modes| modes.focus_reports = enabled);
    }

    fn change_modes(&self, change: impl FnOnce(&mut ViewModes)) {
        self.modes.send_if_modified(|modes| {
            let before = *modes;
            change(modes);
            *modes != before
        });
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

    #[test]
    fn bubbled_keys_go_to_the_leading_view_and_every_view_watches_the_modes() {
        let size = TerminalSize::validated(10, 40).unwrap();
        let mut views = Views::default();
        let first = views.attach(size);
        let (first_keys, mut first_received) = mpsc::channel(8);
        let mut first_modes = views.listen(first, first_keys);
        assert_eq!(*first_modes.borrow_and_update(), ViewModes::default());
        views.signal(KeySignal::Key(BubbledKey::Down));
        assert!(
            first_received.try_recv().is_err(),
            "a key bubbled while bubbling is off goes nowhere"
        );
        views.signal(KeySignal::Bubbling(true));
        assert!(first_modes.has_changed().unwrap());
        assert!(first_modes.borrow_and_update().bubbling);
        views.signal(KeySignal::Bubbling(true));
        assert!(!first_modes.has_changed().unwrap(), "only changes count");

        let second = views.attach(size);
        let (second_keys, mut second_received) = mpsc::channel(8);
        let mut second_modes = views.listen(second, second_keys);
        assert!(
            second_modes.borrow_and_update().bubbling,
            "a new view learns it"
        );
        views.signal(KeySignal::Key(BubbledKey::Down));
        assert_eq!(
            second_received.try_recv(),
            Ok(BubbledKey::Down),
            "the newest view while none typed"
        );
        assert!(first_received.try_recv().is_err());
        views.typed(first);
        views.signal(KeySignal::Key(BubbledKey::Esc));
        assert_eq!(
            first_received.try_recv(),
            Ok(BubbledKey::Esc),
            "the view that typed last"
        );
        assert!(second_received.try_recv().is_err());

        views.focus_reports(true);
        assert_eq!(
            *second_modes.borrow_and_update(),
            ViewModes {
                bubbling: true,
                focus_reports: true
            }
        );
        views.signal(KeySignal::Bubbling(false));
        assert!(!first_modes.borrow_and_update().bubbling);
        views.signal(KeySignal::Key(BubbledKey::Esc));
        assert!(first_received.try_recv().is_err());
    }

    #[test]
    fn a_flood_of_bubbled_keys_never_hides_a_change_of_modes() {
        let size = TerminalSize::validated(10, 40).unwrap();
        let mut views = Views::default();
        let view = views.attach(size);
        let (keys, _unread) = mpsc::channel(2);
        let mut modes = views.listen(view, keys);
        views.signal(KeySignal::Bubbling(true));
        for _ in 0..100 {
            views.signal(KeySignal::Key(BubbledKey::Tab));
        }
        views.signal(KeySignal::Bubbling(false));
        assert!(modes.has_changed().unwrap());
        assert_eq!(*modes.borrow_and_update(), ViewModes::default());
    }
}
