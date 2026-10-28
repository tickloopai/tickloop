//! Monitor cards: the live screen of a hosted Tick or a Shell, shown under its row in the
//! Monitor list. Each open card is one view attached to the Tick in the Tickhost; its terminal
//! takes the card's size. A focused card takes the user's raw input bytes and hands them to the
//! Tick, so it reads the keys its own terminal would send.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tick_protocol::InstanceRef;
use tickhost_connect::{
    host_error::HostError,
    hosted::{
        self, ATTACH_INPUT_BYTES, ATTACH_REQUEST_FRAME_BYTES, BUSINESS_FRAME_BYTES,
        CAPABILITY_DATA, CONTROL_TIMEOUT, HostConnection, MAX_COLUMNS, MAX_ROWS, MIN_COLUMNS,
        MIN_ROWS,
    },
};
use tickloop_harness::protocol::HostedViewGrant;
use tokio::sync::{Notify, mpsc};
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

use super::{
    display::{DisplayProfile, Glyph},
    style::{Colour, Role, RoleLine, Style},
};

/// The rows a card shows of a Tick that declares no minimum, a Terminal Tick and a Shell.
pub(super) const DEFAULT_CARD_ROWS: u16 = tick_protocol::MIN_SCREEN_ROWS;
/// The rule Tickloop draws between a card's row and the Tick's screen.
const RULE_ROWS: usize = 1;
/// Each shown card also needs its own row in the list above it.
const CARD_ROW: usize = 1;
/// A lone Esc leaves the card once nothing else follows it within this time; a second Esc within
/// it sends one Esc to the Tick instead.
pub(super) const ESCAPE_WINDOW: Duration = Duration::from_millis(400);
/// The bytes of one key's escape sequence arrive together; an Esc with nothing after it for this
/// long was pressed alone.
const SEQUENCE_DELAY: Duration = Duration::from_millis(50);
/// A bracketed paste whose end marker has not come after this long of nothing more pasted is
/// over, so a lost marker never traps the focus in the card.
pub(super) const PASTE_IDLE: Duration = Duration::from_millis(1500);
/// The most input a card holds while its view attaches; more, and all input after it, is
/// dropped.
const EARLY_INPUT_BYTES: usize = 64 * 1024;
/// The requests a card's view may have waiting; what is typed beyond them while the Tickhost
/// takes nothing is dropped.
const CARD_REQUESTS: usize = 64;
/// A Tickhost that takes none of a view's requests for this long stopped reading it: the view
/// ends and its connection closes, which detaches it.
const CARD_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// A Tick redraws on every resize its terminal gets, so a view sends its resizes at least this
/// far apart: dragging Monitor's separator or resizing the terminal never floods the Tick.
pub(super) const CARD_RESIZE_INTERVAL: Duration = Duration::from_millis(100);
/// At most this many card redraws a second, however fast frames arrive.
pub(super) const CARD_FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// Esc goes back to the Monitor list, whose own hints then offer Space to show other cards.
pub(super) const FOCUSED_CARD_HINT: &str = "esc back to list · esc esc send esc";
/// A Shell's card is read-only, so leaving is all its keys do.
pub(super) const FOCUSED_SHELL_CARD_HINT: &str = "esc back to list";

const ESCAPE: u8 = 0x1b;
/// A control sequence still unfinished at this length is no key's.
const LONGEST_KEY_SEQUENCE: usize = 32;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
/// Focus reports the Console's terminal sends; they are the Console's, never the Tick's.
const FOCUS_GAINED: &[u8] = b"\x1b[I";
const FOCUS_LOST: &[u8] = b"\x1b[O";
/// While a card has the focus, the terminal reports legacy keys, the ones a Tick's own terminal
/// sends; leaving pops these flags again.
const PUSH_LEGACY_KEYS: &str = "\x1b[>0u";
const POP_LEGACY_KEYS: &str = "\x1b[<u";

/// A card's Tick screen size, in terminal rows and columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CardSize {
    pub(super) rows: u16,
    pub(super) columns: u16,
}

impl CardSize {
    /// The size a view asks the Tick terminal for, within the bounds the host accepts.
    fn requested(self) -> Value {
        json!({
            "rows": self.rows.clamp(MIN_ROWS, MAX_ROWS),
            "columns": self.columns.clamp(MIN_COLUMNS, MAX_COLUMNS),
        })
    }
}

/// What the Tickhost has shown a card so far.
pub(super) struct CardScreen {
    pub(super) parser: vt100::Parser,
    /// Why the view ended, once it did.
    pub(super) ended: Option<ViewEnd>,
}

/// Why a card's view ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ViewEnd {
    /// The Tickhost said the Tick ended.
    TickEnded,
    /// The view itself ended, for this reason; its Tick may run on.
    Failed(String),
}

/// What a card's rule says about its view, in the colour it says it in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CardNote {
    pub(super) text: String,
    pub(super) role: Role,
    /// The card's Tick ended: this outranks anything else the rule could say.
    pub(super) ended: bool,
}

impl CardNote {
    pub(super) fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            role: Role::MutedText,
            ended: false,
        }
    }

    /// How the card's Tick ended, in the colour of its row's state mark.
    pub(super) fn ended(text: String, role: Role) -> Self {
        Self {
            text,
            role,
            ended: true,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum CardRequest {
    Input(Vec<u8>),
    Resize(CardSize),
    /// Sends a resize still waiting for [`CARD_RESIZE_INTERVAL`] at once, as when a drag ends.
    FlushResize,
}

/// Spaces one view's resizes [`CARD_RESIZE_INTERVAL`] apart: the first change after a quiet
/// spell goes at once, and of the changes that follow within the interval only the latest goes,
/// when it is over or at a flush.
#[derive(Debug)]
struct ResizePacing {
    /// The size the Tick's terminal was last given.
    sent: CardSize,
    sent_at: Option<tokio::time::Instant>,
    waiting: Option<CardSize>,
}

impl ResizePacing {
    fn new(attached: CardSize) -> Self {
        Self {
            sent: attached,
            sent_at: None,
            waiting: None,
        }
    }

    /// Asks for `size` at `now`; returns it when it is to be sent at once.
    fn request(&mut self, size: CardSize, now: tokio::time::Instant) -> Option<CardSize> {
        self.waiting = None;
        if size == self.sent {
            return None;
        }
        if self
            .sent_at
            .is_some_and(|sent_at| now < sent_at + CARD_RESIZE_INTERVAL)
        {
            self.waiting = Some(size);
            return None;
        }
        Some(self.mark_sent(size, now))
    }

    /// When the waiting size is due.
    fn deadline(&self) -> Option<tokio::time::Instant> {
        self.waiting?;
        self.sent_at.map(|sent_at| sent_at + CARD_RESIZE_INTERVAL)
    }

    /// The waiting size once its interval is over.
    fn due(&mut self, now: tokio::time::Instant) -> Option<CardSize> {
        if self.deadline().is_some_and(|deadline| now >= deadline) {
            self.flush(now)
        } else {
            None
        }
    }

    fn flush(&mut self, now: tokio::time::Instant) -> Option<CardSize> {
        let size = self.waiting.take()?;
        Some(self.mark_sent(size, now))
    }

    fn mark_sent(&mut self, size: CardSize, now: tokio::time::Instant) -> CardSize {
        self.sent = size;
        self.sent_at = Some(now);
        size
    }
}

/// One card's view attached to its Tick. Dropping it detaches the view.
pub(super) struct CardView {
    size: CardSize,
    pub(super) screen: Arc<Mutex<CardScreen>>,
    requests: mpsc::Sender<CardRequest>,
}

impl CardView {
    /// Attaches a viewer of the Tick `grant` names at `size`. The grant is single-use and soon
    /// expires, so the view attaches right after the grant arrives.
    pub(super) async fn attach(
        grant: &HostedViewGrant,
        size: CardSize,
        redraw: Arc<Notify>,
    ) -> Result<Self, HostError> {
        let mut connection = HostConnection::connect(
            &grant.endpoint,
            tickhost_connect::hosted::Role::Viewer {
                instance: grant.instance.clone(),
                view: grant.view,
            },
            &[CAPABILITY_DATA],
        )
        .await?;
        if connection.epoch() != grant.host_epoch {
            return Err(HostError::before(
                "EpochChanged",
                "the Tickhost was replaced since this view was granted",
            ));
        }
        connection
            .request("attach", json!({"size": size.requested()}), CONTROL_TIMEOUT)
            .await?;
        let (events, writer) = connection.into_parts();
        let screen = Arc::new(Mutex::new(CardScreen {
            parser: vt100::Parser::new(size.rows.max(1), size.columns.max(1), 0),
            ended: None,
        }));
        let reader = tokio::spawn(read_frames(events, screen.clone(), redraw.clone()));
        let (requests, queued) = mpsc::channel(CARD_REQUESTS);
        tokio::spawn(write_requests(
            writer,
            queued,
            size,
            reader.abort_handle(),
            screen.clone(),
            redraw,
            CARD_WRITE_TIMEOUT,
        ));
        Ok(Self {
            size,
            screen,
            requests,
        })
    }

    /// A view connected to nothing, whose requests the caller reads.
    #[cfg(test)]
    pub(super) fn detached(size: CardSize) -> (Self, mpsc::Receiver<CardRequest>) {
        let (requests, queued) = mpsc::channel(CARD_REQUESTS);
        let screen = Arc::new(Mutex::new(CardScreen {
            parser: vt100::Parser::new(size.rows.max(1), size.columns.max(1), 0),
            ended: None,
        }));
        (
            Self {
                size,
                screen,
                requests,
            },
            queued,
        )
    }

    /// Asks for `size`, which reaches the Tick paced by [`CARD_RESIZE_INTERVAL`]; a size the
    /// full queue cannot take is asked for again by the next frame.
    pub(super) fn resize(&mut self, size: CardSize) {
        if size != self.size && self.requests.try_send(CardRequest::Resize(size)).is_ok() {
            self.size = size;
        }
    }

    /// Sends the size last asked for at once, without waiting out the pacing interval.
    pub(super) fn flush_resize(&self) {
        let _ = self.requests.try_send(CardRequest::FlushResize);
    }

    pub(super) fn input(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            let _ = self.requests.try_send(CardRequest::Input(bytes));
        }
    }
}

/// Paints every frame into the card's terminal model at the size the frame names and asks for a
/// redraw; the Console's state is never locked while a frame is read.
async fn read_frames(
    mut events: tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    screen: Arc<Mutex<CardScreen>>,
    redraw: Arc<Notify>,
) {
    let ended = loop {
        let event = match hosted::read_frame(&mut events, BUSINESS_FRAME_BYTES).await {
            Ok(Some(event)) => event,
            Ok(None) => break ViewEnd::Failed("the Tickhost closed this view".to_owned()),
            Err(error) => break ViewEnd::Failed(super::safe_line(&error.message)),
        };
        match event["event"].as_str() {
            Some("frame") => {
                let dimension = |name: &str| {
                    event["size"][name]
                        .as_u64()
                        .and_then(|value| u16::try_from(value).ok())
                        .unwrap_or(1)
                        .max(1)
                };
                let (rows, columns) = (dimension("rows"), dimension("columns"));
                let data = event["data"].as_str().unwrap_or_default();
                let Ok(mut screen) = screen.lock() else {
                    return;
                };
                if screen.parser.screen().size() != (rows, columns) {
                    screen.parser.screen_mut().set_size(rows, columns);
                }
                screen.parser.process(data.as_bytes());
            }
            Some("gone") => break ViewEnd::TickEnded,
            Some("bubbling" | "bubble") => continue,
            _ => {
                break ViewEnd::Failed(super::safe_line(
                    event["error"]["message"].as_str().unwrap_or("view ended"),
                ));
            }
        }
        redraw.notify_one();
    };
    if let Ok(mut screen) = screen.lock() {
        screen.ended = Some(ended);
    }
    redraw.notify_one();
}

/// Sends the card's input in order and its resizes paced by [`ResizePacing`], starting from the
/// `attached` size; once the card goes, detaches the view. A write the Tickhost takes nothing of
/// within `write_timeout` ends the view and closes its connection.
async fn write_requests(
    mut writer: tokio::net::unix::OwnedWriteHalf,
    mut queued: mpsc::Receiver<CardRequest>,
    attached: CardSize,
    reader: tokio::task::AbortHandle,
    screen: Arc<Mutex<CardScreen>>,
    redraw: Arc<Notify>,
    write_timeout: Duration,
) {
    let mut pacing = ResizePacing::new(attached);
    loop {
        let deadline = pacing.deadline();
        let waiting_due = async move {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        let queued_request = tokio::select! {
            request = queued.recv() => match request {
                Some(request) => Some(request),
                None => break,
            },
            () = waiting_due => None,
        };
        let now = tokio::time::Instant::now();
        let request = match queued_request {
            Some(CardRequest::Resize(size)) => pacing.request(size, now).map(CardRequest::Resize),
            Some(CardRequest::FlushResize) => pacing.flush(now).map(CardRequest::Resize),
            Some(input) => Some(input),
            None => pacing.due(now).map(CardRequest::Resize),
        };
        let Some(request) = request else {
            continue;
        };
        let sent = match request {
            CardRequest::Input(bytes) => {
                let mut sent = Ok(());
                for chunk in bytes.chunks(ATTACH_INPUT_BYTES) {
                    sent = send(
                        &mut writer,
                        json!({"method":"input","params":{"data":chunk}}),
                        write_timeout,
                    )
                    .await;
                    if sent.is_err() {
                        break;
                    }
                }
                sent
            }
            CardRequest::Resize(size) => {
                send(
                    &mut writer,
                    json!({"method":"resize","params":{"size":size.requested()}}),
                    write_timeout,
                )
                .await
            }
            CardRequest::FlushResize => Ok(()),
        };
        if let Err(error) = sent {
            if let Ok(mut screen) = screen.lock() {
                screen
                    .ended
                    .get_or_insert(ViewEnd::Failed(super::safe_line(&error.message)));
            }
            redraw.notify_one();
            reader.abort();
            return;
        }
    }
    let _ = send(&mut writer, json!({"method":"detach"}), write_timeout).await;
    reader.abort();
}

async fn send(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    request: Value,
    write_timeout: Duration,
) -> Result<(), HostError> {
    tokio::time::timeout(
        write_timeout,
        hosted::write_frame(writer, &request, ATTACH_REQUEST_FRAME_BYTES),
    )
    .await
    .unwrap_or_else(|_| {
        Err(HostError::after(
            "Timeout",
            "the Tickhost stopped taking this view's input",
        ))
    })
}

/// Where an open card's view stands.
pub(super) enum CardState {
    /// The view grant is on its way.
    Opening,
    Attached(CardView),
    Failed(String),
}

pub(super) struct OpenCard {
    pub(super) instance: InstanceRef,
    /// When the card opened, among this Chat's cards; the most recent ones stay shown.
    sequence: u64,
    pub(super) state: CardState,
    /// Input typed into the focused card before its view attached; it reaches the Tick then.
    early_input: Vec<u8>,
    /// Typed input outgrew [`EARLY_INPUT_BYTES`]; everything after it is dropped too, so the
    /// Tick never gets later keys without the earlier ones.
    early_input_overflowed: bool,
}

/// The cards open in the current Chat and the one with the focus.
#[derive(Default)]
pub(super) struct MonitorCards {
    chat: Option<Uuid>,
    open: Vec<OpenCard>,
    focused: Option<InstanceRef>,
    next_sequence: u64,
    keys: CardKeys,
    redraw: Arc<Notify>,
    painter_started: bool,
}

impl MonitorCards {
    pub(super) fn redraw(&self) -> Arc<Notify> {
        self.redraw.clone()
    }

    /// Whether the card painter still has to start; it starts once.
    pub(super) fn take_painter_start(&mut self) -> bool {
        !std::mem::replace(&mut self.painter_started, true)
    }

    pub(super) fn cards(&self) -> &[OpenCard] {
        &self.open
    }

    pub(super) fn is_open(&self, instance: &InstanceRef) -> bool {
        self.open.iter().any(|card| &card.instance == instance)
    }

    pub(super) fn card(&self, instance: &InstanceRef) -> Option<&OpenCard> {
        self.open.iter().find(|card| &card.instance == instance)
    }

    pub(super) fn card_mut(&mut self, instance: &InstanceRef) -> Option<&mut OpenCard> {
        self.open.iter_mut().find(|card| &card.instance == instance)
    }

    /// Opens `instance`'s card in `chat`; cards of another Chat close first. Returns false when
    /// it was open already.
    pub(super) fn open(&mut self, chat: Uuid, instance: InstanceRef) -> bool {
        if self.chat != Some(chat) {
            self.close_all();
            self.chat = Some(chat);
        }
        if self.is_open(&instance) {
            return false;
        }
        self.next_sequence += 1;
        self.open.push(OpenCard {
            instance,
            sequence: self.next_sequence,
            state: CardState::Opening,
            early_input: Vec::new(),
            early_input_overflowed: false,
        });
        true
    }

    /// Closes the card, which detaches its view; a focused card loses the focus.
    pub(super) fn close(&mut self, instance: &InstanceRef) {
        self.open.retain(|card| &card.instance != instance);
        if self.focused.as_ref() == Some(instance) {
            self.leave_focus();
        }
    }

    pub(super) fn close_all(&mut self) {
        self.open.clear();
        self.leave_focus();
    }

    /// Closes the cards that do not belong to `chat`, or whose rows `listed` no longer has.
    pub(super) fn retain(&mut self, chat: Option<Uuid>, listed: impl Fn(&InstanceRef) -> bool) {
        if self.chat != chat {
            self.close_all();
            self.chat = chat;
            return;
        }
        let gone: Vec<InstanceRef> = self
            .open
            .iter()
            .filter(|card| !listed(&card.instance))
            .map(|card| card.instance.clone())
            .collect();
        for instance in gone {
            self.close(&instance);
        }
    }

    /// Stores the view of a card still waiting for it; false when the card closed meanwhile, and
    /// the view is dropped, which detaches it.
    pub(super) fn attached(&mut self, chat: Uuid, instance: &InstanceRef, view: CardView) -> bool {
        match self.card_mut(instance) {
            Some(card) if matches!(card.state, CardState::Opening) => {
                view.input(std::mem::take(&mut card.early_input));
                card.state = CardState::Attached(view);
                self.chat == Some(chat)
            }
            _ => false,
        }
    }

    pub(super) fn failed(&mut self, instance: &InstanceRef, reason: String) {
        if let Some(card) = self.card_mut(instance)
            && matches!(card.state, CardState::Opening)
        {
            card.state = CardState::Failed(reason);
        }
        if self.focused.as_ref() == Some(instance) {
            self.leave_focus();
        }
    }

    /// Sends `bytes` to the focused card's Tick, or holds them until its view attaches.
    pub(super) fn input(&mut self, bytes: Vec<u8>) {
        let Some(focused) = self.focused.clone() else {
            return;
        };
        let Some(card) = self.card_mut(&focused) else {
            return;
        };
        match &card.state {
            CardState::Attached(view) => view.input(bytes),
            CardState::Opening => {
                card.early_input_overflowed |=
                    card.early_input.len() + bytes.len() > EARLY_INPUT_BYTES;
                if !card.early_input_overflowed {
                    card.early_input.extend(bytes);
                }
            }
            CardState::Failed(_) => {}
        }
    }

    pub(super) fn focus(&mut self, instance: InstanceRef) {
        if self.is_open(&instance) {
            self.focused = Some(instance);
            self.keys = CardKeys::default();
        }
    }

    pub(super) fn leave_focus(&mut self) {
        self.focused = None;
        self.keys = CardKeys::default();
    }

    pub(super) fn focused(&self) -> Option<&InstanceRef> {
        self.focused.as_ref()
    }

    /// The focused card's view, once attached.
    pub(super) fn focused_view(&self) -> Option<&CardView> {
        let focused = self.focused.as_ref()?;
        match &self.card(focused)?.state {
            CardState::Attached(view) => Some(view),
            _ => None,
        }
    }

    /// Which card stays shown first when not all fit: the focused one, then the most recently
    /// opened.
    pub(super) fn priority(&self, card: &OpenCard) -> u64 {
        if self.focused.as_ref() == Some(&card.instance) {
            u64::MAX
        } else {
            card.sequence
        }
    }

    pub(super) fn keys_deadline(&self) -> Option<Instant> {
        self.keys.deadline()
    }

    pub(super) fn keys_mut(&mut self) -> &mut CardKeys {
        &mut self.keys
    }
}

/// The input modes a focused card's Tick asked its terminal for; the real terminal takes them
/// while the card has the focus, so the keys it sends are the ones the Tick expects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct CardModes {
    pub(super) application_cursor: bool,
    pub(super) application_keypad: bool,
}

impl CardModes {
    pub(super) fn of(screen: &vt100::Screen) -> Self {
        Self {
            application_cursor: screen.application_cursor(),
            application_keypad: screen.application_keypad(),
        }
    }

    /// `modes` as one byte, so they can be held in an atomic: bit 0 says a card has them.
    pub(super) fn to_bits(modes: Option<Self>) -> u8 {
        modes.map_or(0, |modes| {
            1 | u8::from(modes.application_cursor) << 1 | u8::from(modes.application_keypad) << 2
        })
    }

    pub(super) fn from_bits(bits: u8) -> Option<Self> {
        (bits & 1 != 0).then_some(Self {
            application_cursor: bits & 2 != 0,
            application_keypad: bits & 4 != 0,
        })
    }
}

pub(super) fn mode_changes(from: Option<CardModes>, to: Option<CardModes>) -> String {
    let mut changes = String::new();
    if from.is_none() && to.is_some() {
        changes.push_str(PUSH_LEGACY_KEYS);
    }
    let (before, after) = (from.unwrap_or_default(), to.unwrap_or_default());
    if before.application_cursor != after.application_cursor {
        changes.push_str(if after.application_cursor {
            "\x1b[?1h"
        } else {
            "\x1b[?1l"
        });
    }
    if before.application_keypad != after.application_keypad {
        changes.push_str(if after.application_keypad {
            "\x1b="
        } else {
            "\x1b>"
        });
    }
    if from.is_some() && to.is_none() {
        changes.push_str(POP_LEGACY_KEYS);
    }
    changes
}

/// What one open card asks of the Monitor list's height.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CardDemand {
    pub(super) min_rows: u16,
    pub(super) priority: u64,
}

/// One open card as a Monitor frame shows it.
pub(super) struct CardFrame<'a> {
    pub(super) instance: &'a InstanceRef,
    pub(super) demand: CardDemand,
    pub(super) focused: bool,
    /// The Tick's screen once the view attached.
    pub(super) screen: Option<&'a vt100::Screen>,
    /// What the rule says about the view, such as that it is opening or how its Tick ended.
    pub(super) note: Option<CardNote>,
}

/// The rows a shown card gets: the Tick's screen rows and how many of them fit on screen; fewer
/// fit only when even one card's minimum does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CardRows {
    pub(super) screen: u16,
    pub(super) shown: u16,
}

impl CardRows {
    pub(super) fn is_cropped(self) -> bool {
        self.shown < self.screen
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct CardAllocation {
    /// Per demand, in the order given: its rows when shown, `None` when collapsed.
    pub(super) cards: Vec<Option<CardRows>>,
    pub(super) collapsed: usize,
}

/// Shares a list viewport `height` rows tall among open cards. `list_rows` is what the list
/// needs without cards, the cards' own rows included. Every shown card gets at least its
/// minimum below its row and a rule; when not all fit, the ones of highest priority stay and the
/// others collapse behind one `+N collapsed` line. When even one does not fit, the first stays
/// shown, cropped. Height the list leaves over goes to the shown cards evenly.
pub(super) fn allocate(height: usize, list_rows: usize, demands: &[CardDemand]) -> CardAllocation {
    let mut order: Vec<usize> = (0..demands.len()).collect();
    order.sort_by_key(|index| std::cmp::Reverse(demands[*index].priority));
    let need = |index: usize| CARD_ROW + RULE_ROWS + usize::from(demands[index].min_rows);
    let collapsed_line = |shown: usize| usize::from(shown < demands.len());
    let mut shown = 0;
    let mut used = 0;
    while shown < demands.len() && used + need(order[shown]) + collapsed_line(shown + 1) <= height {
        used += need(order[shown]);
        shown += 1;
    }
    let mut cards = vec![None; demands.len()];
    if demands.is_empty() {
        return CardAllocation {
            cards,
            collapsed: 0,
        };
    }
    if shown == 0 {
        let first = order[0];
        let screen = demands[first].min_rows;
        let room = height.saturating_sub(CARD_ROW + RULE_ROWS + collapsed_line(1));
        cards[first] = Some(CardRows {
            screen,
            shown: screen.min(u16::try_from(room).unwrap_or(u16::MAX)),
        });
        return CardAllocation {
            cards,
            collapsed: demands.len() - 1,
        };
    }
    let mut kept: Vec<usize> = order[..shown].to_vec();
    kept.sort_unstable();
    let card_rows: usize = kept
        .iter()
        .map(|index| RULE_ROWS + usize::from(demands[*index].min_rows))
        .sum();
    let spare = height.saturating_sub(list_rows + card_rows + collapsed_line(shown));
    let (each, remainder) = (spare / shown, spare % shown);
    for (position, index) in kept.into_iter().enumerate() {
        let extra = each + usize::from(position < remainder);
        let screen = demands[index]
            .min_rows
            .saturating_add(u16::try_from(extra).unwrap_or(u16::MAX));
        cards[index] = Some(CardRows {
            screen,
            shown: screen,
        });
    }
    CardAllocation {
        cards,
        collapsed: demands.len() - shown,
    }
}

/// The line `+N collapsed` under the list while open cards do not fit.
pub(super) fn collapsed_line(collapsed: usize) -> RoleLine {
    RoleLine::role(format!("+{collapsed} collapsed"), Role::MutedText)
}

/// The rule between a card's row and the Tick's screen: light, or heavy in the accent colour
/// for the focused card, with a short `note` such as how its Tick ended.
pub(super) fn rule_line(
    width: usize,
    focused: bool,
    note: Option<&CardNote>,
    display: DisplayProfile,
) -> RoleLine {
    let (glyph, role) = if focused {
        (display.glyph(Glyph::FocusedCardRule), Role::Accent)
    } else {
        (display.glyph(Glyph::CardRule), Role::Subtle)
    };
    let mut line = RoleLine::default();
    let lead = glyph.repeat(2.min(width));
    line.push(lead, Some(role));
    if let Some(note) = note.filter(|_| width > 6) {
        let text = super::conversation_picker::ellipsize(&note.text, width.saturating_sub(5));
        line.push(" ", Some(role));
        line.push(text, Some(note.role));
        line.push(" ", Some(role));
    }
    let rest = width.saturating_sub(line.text().width());
    line.push(glyph.repeat(rest), Some(role));
    line
}

/// Row `row` of the Tick's screen as `width` columns: cut at the card's edge, blank beyond the
/// Tick's screen, every cell in its own colours and attributes. A wide character that would
/// spill past the edge leaves that column blank.
pub(super) fn screen_line(screen: &vt100::Screen, row: u16, width: u16) -> RoleLine {
    let (rows, columns) = screen.size();
    let mut line = RoleLine::default();
    let mut column = 0;
    while column < width {
        let cell = (row < rows && column < columns)
            .then(|| screen.cell(row, column))
            .flatten();
        let style = cell.map(cell_style).unwrap_or_default();
        match cell {
            Some(cell) if cell.is_wide() => {
                if column + 1 >= width {
                    line.push_literal(" ", style);
                    column += 1;
                } else {
                    line.push_literal(cell.contents(), style);
                    column += 2;
                }
            }
            Some(cell) if cell.has_contents() && !cell.is_wide_continuation() => {
                line.push_literal(cell.contents(), style);
                column += 1;
            }
            _ => {
                line.push_literal(" ", style);
                column += 1;
            }
        }
    }
    line
}

fn cell_style(cell: &vt100::Cell) -> Style {
    Style {
        foreground: colour(cell.fgcolor()),
        background: colour(cell.bgcolor()),
        bold: cell.bold(),
        dim: cell.dim(),
        italic: cell.italic(),
        underline: cell.underline(),
        reversed: cell.inverse(),
        crossed_out: false,
    }
}

fn colour(colour: vt100::Color) -> Option<Colour> {
    match colour {
        vt100::Color::Default => None,
        vt100::Color::Idx(index) => Some(Colour::Indexed(index)),
        vt100::Color::Rgb(red, green, blue) => {
            Some(Colour::Rgb(super::style::Rgb { red, green, blue }))
        }
    }
}

/// A mouse report in the SGR encoding the Console's terminal uses: `ESC [ < code ; column ; row`
/// then `M` for a press or motion and `m` for a release. Columns and rows count from 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SgrMouse {
    pub(super) code: u16,
    pub(super) column: u16,
    pub(super) row: u16,
    pub(super) pressed: bool,
}

const MOUSE_MOTION: u16 = 32;
const MOUSE_WHEEL: u16 = 64;
const MOUSE_BUTTONS: u16 = 3;
const MOUSE_RELEASE: u16 = 3;
/// The legacy mouse encodings carry each value in one byte after this offset.
const LEGACY_MOUSE_OFFSET: u16 = 32;

impl SgrMouse {
    fn parse(sequence: &[u8]) -> Option<Self> {
        let body = sequence.strip_prefix(b"\x1b[<")?;
        let (last, parameters) = body.split_last()?;
        let pressed = match last {
            b'M' => true,
            b'm' => false,
            _ => return None,
        };
        let mut fields = std::str::from_utf8(parameters).ok()?.split(';');
        let mut next = || fields.next()?.parse::<u16>().ok();
        let (code, column, row) = (next()?, next()?, next()?);
        Some(Self {
            code,
            column,
            row,
            pressed,
        })
    }

    fn is_motion(self) -> bool {
        self.code & MOUSE_MOTION != 0
    }

    fn is_wheel(self) -> bool {
        self.code & MOUSE_WHEEL != 0
    }

    /// A button pressed down, the kind of report that leaves or enters a card.
    pub(super) fn is_click(self) -> bool {
        self.pressed && !self.is_motion() && !self.is_wheel()
    }

    /// The report as the Console's own mouse event, as crossterm decodes it.
    pub(super) fn console_event(self) -> crossterm::event::MouseEvent {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        const SHIFT: u16 = 4;
        const ALT: u16 = 8;
        const CONTROL: u16 = 16;
        let button = match self.code & MOUSE_BUTTONS {
            0 => Some(MouseButton::Left),
            1 => Some(MouseButton::Middle),
            2 => Some(MouseButton::Right),
            _ => None,
        };
        let kind = if self.is_wheel() {
            match self.code & MOUSE_BUTTONS {
                0 => MouseEventKind::ScrollUp,
                1 => MouseEventKind::ScrollDown,
                2 => MouseEventKind::ScrollLeft,
                _ => MouseEventKind::ScrollRight,
            }
        } else {
            match (button, self.is_motion(), self.pressed) {
                (Some(button), true, _) => MouseEventKind::Drag(button),
                (None, true, _) => MouseEventKind::Moved,
                (Some(button), false, true) => MouseEventKind::Down(button),
                (button, false, _) => MouseEventKind::Up(button.unwrap_or(MouseButton::Left)),
            }
        };
        let mut modifiers = KeyModifiers::NONE;
        for (bit, modifier) in [
            (SHIFT, KeyModifiers::SHIFT),
            (ALT, KeyModifiers::ALT),
            (CONTROL, KeyModifiers::CONTROL),
        ] {
            if self.code & bit != 0 {
                modifiers |= modifier;
            }
        }
        MouseEvent {
            kind,
            column: self.column.saturating_sub(1),
            row: self.row.saturating_sub(1),
            modifiers,
        }
    }

    /// The report a Tick asked for with its mouse `mode` and `encoding`, at `column` and `row`
    /// of its screen counted from 0; `None` when it asked for no such report.
    pub(super) fn for_tick(
        self,
        column: u16,
        row: u16,
        mode: vt100::MouseProtocolMode,
        encoding: vt100::MouseProtocolEncoding,
    ) -> Option<Vec<u8>> {
        use vt100::{MouseProtocolEncoding, MouseProtocolMode};
        let button_held = self.code & MOUSE_BUTTONS != MOUSE_RELEASE;
        let wanted = match mode {
            MouseProtocolMode::None => false,
            MouseProtocolMode::Press => self.pressed && !self.is_motion(),
            MouseProtocolMode::PressRelease => !self.is_motion(),
            MouseProtocolMode::ButtonMotion => !self.is_motion() || button_held,
            MouseProtocolMode::AnyMotion => true,
        };
        if !wanted {
            return None;
        }
        let (column, row) = (column + 1, row + 1);
        if encoding == MouseProtocolEncoding::Sgr {
            let end = if self.pressed { 'M' } else { 'm' };
            return Some(format!("\x1b[<{};{column};{row}{end}", self.code).into_bytes());
        }
        let code = if self.pressed || self.is_wheel() {
            self.code
        } else {
            self.code & !MOUSE_BUTTONS | MOUSE_RELEASE
        };
        let mut report = b"\x1b[M".to_vec();
        for value in [code, column, row] {
            let value = value + LEGACY_MOUSE_OFFSET;
            match encoding {
                MouseProtocolEncoding::Utf8 => {
                    let character = char::from_u32(u32::from(value))?;
                    let mut buffer = [0; 4];
                    report.extend(character.encode_utf8(&mut buffer).as_bytes());
                }
                _ => report.push(u8::try_from(value).ok()?),
            }
        }
        Some(report)
    }
}

/// What a read of the user's input does while a card has the focus, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum CardInput {
    /// Bytes the Tick receives as they came.
    Bytes(Vec<u8>),
    Mouse(SgrMouse),
    /// The Console's terminal gained (`true`) or lost the focus.
    Focus(bool),
    /// The focus leaves the card; nothing read after it reaches the Tick.
    Leave,
}

/// Splits a focused card's raw input into what reaches the Tick and what leaves the card. A lone
/// Esc leaves once nothing follows it within [`ESCAPE_WINDOW`]; a second Esc within it sends one
/// Esc and keeps the focus; any other key after it sends both. Escape sequences such as arrows,
/// Alt-keys and every other byte, Ctrl keys included, reach the Tick unchanged. Text inside a
/// bracketed paste is never a key; its markers reach the Tick only when it asked for them.
/// Mouse reports and the terminal's focus reports are never sent as they came, and an Esc held
/// before them stays held.
#[derive(Debug, Default)]
pub(super) struct CardKeys {
    /// An escape sequence not ended yet, perhaps a lone Esc, and when its Esc arrived.
    sequence: Vec<u8>,
    sequence_at: Option<Instant>,
    /// When a lone Esc was pressed that has not yet left the card or gone to the Tick.
    escape_at: Option<Instant>,
    in_paste: bool,
    /// When the paste last brought bytes, and whether the Tick got its start marker.
    paste_at: Option<Instant>,
    paste_markers: bool,
    /// Pasted bytes held back while they could still begin the paste's end marker.
    paste_tail: Vec<u8>,
}

impl CardKeys {
    /// The items `input`, read at `now`, makes; `paste_markers` is whether the Tick asked for
    /// bracketed paste.
    pub(super) fn feed(
        &mut self,
        input: &[u8],
        now: Instant,
        paste_markers: bool,
    ) -> Vec<CardInput> {
        let mut items = Vec::new();
        if self.in_paste && !input.is_empty() {
            self.paste_at = Some(now);
        }
        for &byte in input {
            if self.in_paste {
                self.paste_byte(byte, &mut items);
                continue;
            }
            if !self.sequence.is_empty() {
                if self.sequence == [ESCAPE] && byte == ESCAPE {
                    let at = self.sequence_at.unwrap_or(now);
                    self.lone_escape(at, &mut items);
                    self.sequence_at = Some(now);
                    continue;
                }
                match sequence_step(&self.sequence, byte) {
                    SequenceStep::Continues => {
                        self.sequence.push(byte);
                        continue;
                    }
                    SequenceStep::Ends => {
                        self.sequence.push(byte);
                        let sequence = std::mem::take(&mut self.sequence);
                        self.sequence_at = None;
                        self.token(sequence, now, paste_markers, &mut items);
                        continue;
                    }
                    SequenceStep::Breaks => {
                        let sequence = std::mem::take(&mut self.sequence);
                        self.sequence_at = None;
                        if sequence == [ESCAPE] {
                            // An Esc and a key in one read: Alt and that key, both for the Tick.
                            self.token(vec![ESCAPE, byte], now, paste_markers, &mut items);
                            continue;
                        }
                        self.token(sequence, now, paste_markers, &mut items);
                    }
                }
            }
            if byte == ESCAPE {
                self.sequence.push(byte);
                self.sequence_at = Some(now);
            } else {
                self.token(vec![byte], now, paste_markers, &mut items);
            }
        }
        if self.in_paste {
            self.flush_paste_tail(&mut items);
        }
        items
    }

    /// When [`Self::expire`] next has something to decide.
    pub(super) fn deadline(&self) -> Option<Instant> {
        let sequence = self.sequence_at.map(|at| at + SEQUENCE_DELAY);
        let escape = self.escape_at.map(|at| at + ESCAPE_WINDOW);
        let paste = self.paste_at.map(|at| at + PASTE_IDLE);
        [sequence, escape, paste].into_iter().flatten().min()
    }

    /// Settles what waited for time to tell: a held Esc with nothing after it is a lone Esc, an
    /// unfinished sequence reaches the Tick as it is, a lone Esc nothing followed leaves, and a
    /// paste that went quiet without its end marker ends.
    pub(super) fn expire(&mut self, now: Instant) -> Vec<CardInput> {
        let mut items = Vec::new();
        if let Some(at) = self.paste_at
            && now >= at + PASTE_IDLE
        {
            let tail = std::mem::take(&mut self.paste_tail);
            push_bytes(&mut items, &tail);
            if self.paste_markers {
                push_bytes(&mut items, PASTE_END);
            }
            self.end_paste();
        }
        if let Some(at) = self.sequence_at
            && now >= at + SEQUENCE_DELAY
        {
            let sequence = std::mem::take(&mut self.sequence);
            self.sequence_at = None;
            if sequence == [ESCAPE] {
                self.lone_escape(at, &mut items);
            } else {
                self.forward(sequence, &mut items);
            }
        }
        if let Some(at) = self.escape_at
            && now >= at + ESCAPE_WINDOW
        {
            self.escape_at = None;
            items.push(CardInput::Leave);
        }
        items
    }

    fn lone_escape(&mut self, at: Instant, items: &mut Vec<CardInput>) {
        if self.escape_at.take().is_some() {
            push_bytes(items, &[ESCAPE]);
        } else {
            self.escape_at = Some(at);
        }
    }

    /// One whole key or report. A key is sent after any Esc held before it; a report is no key,
    /// so the Esc stays held.
    fn token(
        &mut self,
        token: Vec<u8>,
        now: Instant,
        paste_markers: bool,
        items: &mut Vec<CardInput>,
    ) {
        if is_kitty_escape(&token) {
            self.lone_escape(now, items);
            return;
        }
        if token == FOCUS_GAINED || token == FOCUS_LOST {
            items.push(CardInput::Focus(token == FOCUS_GAINED));
            return;
        }
        if let Some(mouse) = SgrMouse::parse(&token) {
            items.push(CardInput::Mouse(mouse));
            return;
        }
        if self.escape_at.take().is_some() {
            push_bytes(items, &[ESCAPE]);
        }
        if token == PASTE_START {
            self.in_paste = true;
            self.paste_at = Some(now);
            self.paste_markers = paste_markers;
            if paste_markers {
                push_bytes(items, PASTE_START);
            }
            return;
        }
        push_bytes(items, &token);
    }

    fn forward(&mut self, bytes: Vec<u8>, items: &mut Vec<CardInput>) {
        if self.escape_at.take().is_some() {
            push_bytes(items, &[ESCAPE]);
        }
        push_bytes(items, &bytes);
    }

    /// One pasted byte; the end marker reaches the Tick only when the start marker did.
    fn paste_byte(&mut self, byte: u8, items: &mut Vec<CardInput>) {
        self.paste_tail.push(byte);
        if self.paste_tail.ends_with(PASTE_END) {
            let mut text = std::mem::take(&mut self.paste_tail);
            text.truncate(text.len() - PASTE_END.len());
            push_bytes(items, &text);
            if self.paste_markers {
                push_bytes(items, PASTE_END);
            }
            self.end_paste();
        }
    }

    fn end_paste(&mut self) {
        self.in_paste = false;
        self.paste_at = None;
        self.paste_markers = false;
    }

    /// Sends the pasted bytes read so far, except an end that may begin the end marker.
    fn flush_paste_tail(&mut self, items: &mut Vec<CardInput>) {
        let held = (1..PASTE_END.len())
            .rev()
            .find(|length| self.paste_tail.ends_with(&PASTE_END[..*length]))
            .unwrap_or(0);
        let sent: Vec<u8> = self
            .paste_tail
            .drain(..self.paste_tail.len() - held)
            .collect();
        push_bytes(items, &sent);
    }
}

fn push_bytes(items: &mut Vec<CardInput>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if let Some(CardInput::Bytes(last)) = items.last_mut() {
        last.extend_from_slice(bytes);
    } else {
        items.push(CardInput::Bytes(bytes.to_vec()));
    }
}

/// How a byte read after the start of an escape sequence relates to it.
enum SequenceStep {
    Continues,
    Ends,
    /// The byte belongs to no sequence begun so far, which therefore ended before it.
    Breaks,
}

fn sequence_step(sequence: &[u8], byte: u8) -> SequenceStep {
    match sequence {
        [ESCAPE] => match byte {
            b'[' | b'O' => SequenceStep::Continues,
            _ => SequenceStep::Breaks,
        },
        // SS3, the application cursor and keypad keys: one byte ends it.
        [ESCAPE, b'O'] => SequenceStep::Ends,
        _ if sequence.len() >= LONGEST_KEY_SEQUENCE => SequenceStep::Breaks,
        _ => match byte {
            0x20..=0x3f => SequenceStep::Continues,
            0x40..=0x7e => SequenceStep::Ends,
            _ => SequenceStep::Breaks,
        },
    }
}

/// Whether `sequence` is Esc pressed or repeated in the kitty keyboard protocol's encoding, as a
/// terminal may still send it before the card's legacy keys take effect.
fn is_kitty_escape(sequence: &[u8]) -> bool {
    let Some(parameters) = sequence
        .strip_prefix(b"\x1b[")
        .and_then(|rest| rest.strip_suffix(b"u"))
    else {
        return false;
    };
    let Ok(parameters) = std::str::from_utf8(parameters) else {
        return false;
    };
    let mut fields = parameters.split(';');
    let code = fields.next().and_then(|field| field.split(':').next());
    let (modifiers, event) = match fields.next() {
        Some(field) => field.split_once(':').unwrap_or((field, "1")),
        None => ("1", "1"),
    };
    const KITTY_LOCKS: u16 = 64 | 128;
    let plain = modifiers
        .parse::<u16>()
        .ok()
        .and_then(|modifiers| modifiers.checked_sub(1))
        .is_some_and(|modifiers| modifiers & !KITTY_LOCKS == 0);
    code == Some("27") && plain && matches!(event, "1" | "2")
}

#[cfg(test)]
#[path = "monitor_cards_tests.rs"]
mod tests;
