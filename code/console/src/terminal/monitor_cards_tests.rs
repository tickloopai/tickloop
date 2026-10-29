use super::*;
use vt100::{MouseProtocolEncoding, MouseProtocolMode};

fn bytes(data: &[u8]) -> Vec<CardInput> {
    vec![CardInput::Bytes(data.to_vec())]
}

fn after(start: Instant, milliseconds: u64) -> Instant {
    start + Duration::from_millis(milliseconds)
}

#[test]
fn a_lone_escape_reaches_the_tick_as_soon_as_it_is_known_to_be_alone() {
    let start = Instant::now();
    let mut keys = CardKeys::default();
    assert_eq!(keys.feed(b"ab\x1b", start, false, false), bytes(b"ab"));
    assert_eq!(keys.deadline(), Some(after(start, 50)));
    assert!(
        keys.expire(after(start, 40), false).is_empty(),
        "maybe an arrow"
    );
    assert_eq!(keys.expire(after(start, 50), false), bytes(b"\x1b"));
    assert_eq!(keys.deadline(), None, "nothing waits to leave");
    assert!(
        keys.expire(after(start, 5000), false).is_empty(),
        "the focus stays"
    );
}

#[test]
fn a_second_escape_within_300_ms_leaves_and_never_reaches_the_tick() {
    let start = Instant::now();
    let mut keys = CardKeys::default();
    let mut items = keys.feed(b"\x1b\x1b", start, false, false);
    items.extend(keys.expire(after(start, 60), false));
    assert_eq!(
        items,
        vec![CardInput::Bytes(b"\x1b".to_vec()), CardInput::Leave],
        "Esc Esc in one read: the first reaches the Tick, the second leaves"
    );

    let mut keys = CardKeys::default();
    assert!(keys.feed(b"\x1b", start, false, false).is_empty());
    assert_eq!(keys.expire(after(start, 60), false), bytes(b"\x1b"));
    assert!(
        keys.feed(b"\x1b", after(start, 299), false, false)
            .is_empty()
    );
    assert_eq!(
        keys.expire(after(start, 360), false),
        vec![CardInput::Leave],
        "a second Esc 299 ms later"
    );

    let mut keys = CardKeys::default();
    assert!(keys.feed(b"\x1b", start, false, false).is_empty());
    assert_eq!(keys.expire(after(start, 60), false), bytes(b"\x1b"));
    assert!(
        keys.feed(b"\x1b", after(start, 300), false, false)
            .is_empty()
    );
    assert_eq!(
        keys.expire(after(start, 360), false),
        bytes(b"\x1b"),
        "300 ms later it is another first Esc"
    );
    assert!(
        keys.feed(b"\x1b", after(start, 400), false, false)
            .is_empty()
    );
    assert_eq!(
        keys.expire(after(start, 460), false),
        vec![CardInput::Leave],
        "and the next one leaves"
    );
}

#[test]
fn another_key_between_two_escapes_keeps_them_both_for_the_tick() {
    let start = Instant::now();
    let mut keys = CardKeys::default();
    assert!(keys.feed(b"\x1b", start, false, false).is_empty());
    assert_eq!(keys.expire(after(start, 60), false), bytes(b"\x1b"));
    assert_eq!(
        keys.feed(b"i", after(start, 100), false, false),
        bytes(b"i")
    );
    assert!(
        keys.feed(b"\x1b", after(start, 150), false, false)
            .is_empty()
    );
    assert_eq!(keys.expire(after(start, 210), false), bytes(b"\x1b"));

    let mut keys = CardKeys::default();
    assert_eq!(
        keys.feed(b"\x1bx", start, false, false),
        bytes(b"\x1bx"),
        "Esc and a key in one read are Alt and that key"
    );
    assert_eq!(
        keys.feed(b"\x1b\x1b[A", start, false, false),
        bytes(b"\x1b\x1b[A"),
        "Esc then an arrow"
    );
    assert!(keys.expire(after(start, 1000), false).is_empty());
}

#[test]
fn a_read_only_card_leaves_on_a_lone_escape() {
    let start = Instant::now();
    let mut keys = CardKeys::default();
    assert!(keys.feed(b"\x1b", start, false, true).is_empty());
    assert_eq!(keys.expire(after(start, 60), true), vec![CardInput::Leave]);
    assert_eq!(
        keys.feed(b"\x1b[A", start, false, true),
        bytes(b"\x1b[A"),
        "an arrow is no Esc"
    );
    assert_eq!(keys.escape(start, true), vec![CardInput::Leave]);
}

#[test]
fn sequences_and_control_keys_reach_the_tick_unchanged() {
    let start = Instant::now();
    let mut keys = CardKeys::default();
    let typed = b"\x02\x03\x1d\x1bOA\x1b[1;5C\x1bjq\x7f\xe8\x85\xbe";
    assert_eq!(keys.feed(typed, start, false, false), bytes(typed));
    let mut split = keys.feed(b"\x1b", start, false, false);
    split.extend(keys.feed(b"[B", after(start, 5), false, false));
    assert_eq!(split, bytes(b"\x1b[B"), "an arrow split across reads");
    assert!(keys.expire(after(start, 1000), false).is_empty());
}

#[test]
fn focus_reports_go_to_the_console_and_a_kitty_escape_counts_as_escape() {
    let start = Instant::now();
    let mut keys = CardKeys::default();
    assert_eq!(
        keys.feed(b"a\x1b[Ib\x1b[O", start, false, false),
        vec![
            CardInput::Bytes(b"a".to_vec()),
            CardInput::Focus(true),
            CardInput::Bytes(b"b".to_vec()),
            CardInput::Focus(false),
        ]
    );
    assert_eq!(keys.feed(b"\x1b[27u", start, false, false), bytes(b"\x1b"));
    assert_eq!(
        keys.feed(b"\x1b[27;1:2u", after(start, 100), false, false),
        vec![CardInput::Leave],
        "a repeated kitty Esc is the second of a pair"
    );
}

#[test]
fn pasted_text_is_never_a_key_and_its_markers_reach_only_a_tick_that_asked_for_them() {
    let start = Instant::now();
    let mut keys = CardKeys::default();
    assert_eq!(
        keys.feed(b"\x1b[200~a\x1bb\x1b[201~", start, true, false),
        bytes(b"\x1b[200~a\x1bb\x1b[201~")
    );
    assert_eq!(keys.deadline(), None, "an Esc inside the paste is text");

    let mut keys = CardKeys::default();
    let mut sent = keys.feed(b"\x1b[200~one\x1b[2", start, false, false);
    sent.extend(keys.feed(b"01~two", start, false, false));
    assert_eq!(
        sent,
        vec![
            CardInput::Bytes(b"one".to_vec()),
            CardInput::Bytes(b"two".to_vec())
        ],
        "markers dropped even when the end marker spans reads"
    );
}

#[test]
fn mouse_and_focus_reports_between_two_escapes_neither_reach_the_tick_nor_break_the_pair() {
    let start = Instant::now();
    let mut keys = CardKeys::default();
    assert!(keys.feed(b"\x1b", start, false, false).is_empty());
    assert_eq!(keys.expire(after(start, 60), false), bytes(b"\x1b"));
    assert_eq!(
        keys.feed(b"\x1b[<64;3;4M", after(start, 100), false, false),
        vec![CardInput::Mouse(SgrMouse {
            code: 64,
            column: 3,
            row: 4,
            pressed: true
        })],
        "a wheel sends no Esc to the Tick"
    );
    assert_eq!(
        keys.feed(b"\x1b[O", after(start, 150), false, false),
        vec![CardInput::Focus(false)]
    );
    assert_eq!(
        keys.feed(b"\x1b", after(start, 200), false, false),
        Vec::new()
    );
    assert_eq!(
        keys.expire(after(start, 260), false),
        vec![CardInput::Leave],
        "the second Esc still leaves"
    );

    let mut keys = CardKeys::default();
    assert_eq!(
        keys.feed(b"\x1b\x1b[<0;1;1M", start, false, false),
        vec![
            CardInput::Bytes(b"\x1b".to_vec()),
            CardInput::Mouse(SgrMouse {
                code: 0,
                column: 1,
                row: 1,
                pressed: true
            })
        ],
        "an Esc and a click in one read: the Esc was alone, the click is the Console's"
    );
}

#[test]
fn a_paste_whose_end_never_arrives_ends_once_it_goes_quiet() {
    let start = Instant::now();
    let mut keys = CardKeys::default();
    assert_eq!(
        keys.feed(b"\x1b[200~text\x1b[20", start, true, false),
        bytes(b"\x1b[200~text")
    );
    assert_eq!(keys.deadline(), Some(start + PASTE_IDLE));
    assert!(
        keys.expire(start + PASTE_IDLE - Duration::from_millis(1), false)
            .is_empty()
    );
    assert_eq!(
        keys.expire(start + PASTE_IDLE, false),
        bytes(b"\x1b[20\x1b[201~"),
        "the held tail and the end marker the Tick waits for"
    );
    assert_eq!(keys.deadline(), None);
    assert_eq!(
        keys.feed(b"x\x1b", start + PASTE_IDLE, false, false),
        bytes(b"x")
    );
    assert!(keys.deadline().is_some(), "keys are keys again");

    let mut keys = CardKeys::default();
    assert_eq!(keys.feed(b"\x1b[200~a", start, false, false), bytes(b"a"));
    assert_eq!(
        keys.feed(b"b", after(start, 1200), false, false),
        bytes(b"b")
    );
    assert_eq!(
        keys.deadline(),
        Some(after(start, 1200) + PASTE_IDLE),
        "each pasted read restarts the wait"
    );
    assert!(
        keys.expire(after(start, 1200) + PASTE_IDLE, false)
            .is_empty()
    );
    assert_eq!(
        keys.feed(b"c", after(start, 5000), false, false),
        bytes(b"c")
    );
    assert_eq!(keys.deadline(), None, "the paste ended");
}

#[test]
fn mouse_reports_are_parsed_and_never_forwarded_as_they_came() {
    let start = Instant::now();
    let mut keys = CardKeys::default();
    assert_eq!(
        keys.feed(b"a\x1b[<0;12;5Mb\x1b[<0;12;5m", start, false, false),
        vec![
            CardInput::Bytes(b"a".to_vec()),
            CardInput::Mouse(SgrMouse {
                code: 0,
                column: 12,
                row: 5,
                pressed: true
            }),
            CardInput::Bytes(b"b".to_vec()),
            CardInput::Mouse(SgrMouse {
                code: 0,
                column: 12,
                row: 5,
                pressed: false
            }),
        ]
    );
}

fn click(code: u16, pressed: bool) -> SgrMouse {
    SgrMouse {
        code,
        column: 50,
        row: 9,
        pressed,
    }
}

#[test]
fn a_mouse_report_reaches_the_tick_only_as_its_mode_and_encoding_ask() {
    let press = click(0, true);
    assert!(press.is_click());
    assert!(!click(32, true).is_click() && !click(64, true).is_click());
    assert_eq!(
        press.for_tick(3, 1, MouseProtocolMode::None, MouseProtocolEncoding::Sgr),
        None
    );
    assert_eq!(
        press.for_tick(3, 1, MouseProtocolMode::Press, MouseProtocolEncoding::Sgr),
        Some(b"\x1b[<0;4;2M".to_vec()),
        "coordinates from the card's origin"
    );
    assert_eq!(
        click(0, false).for_tick(3, 1, MouseProtocolMode::Press, MouseProtocolEncoding::Sgr),
        None,
        "press mode reports no release"
    );
    assert_eq!(
        click(0, false).for_tick(
            3,
            1,
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Default
        ),
        Some(vec![0x1b, b'[', b'M', 32 + 3, 32 + 4, 32 + 2]),
        "a legacy release names button 3"
    );
    assert_eq!(
        click(32, true).for_tick(
            0,
            0,
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Sgr
        ),
        None
    );
    assert!(
        click(32, true)
            .for_tick(
                0,
                0,
                MouseProtocolMode::ButtonMotion,
                MouseProtocolEncoding::Sgr
            )
            .is_some()
    );
}

fn demand(min_rows: u16, priority: u64) -> CardDemand {
    CardDemand {
        min_rows,
        max_rows: None,
        priority,
    }
}

fn growing(min_rows: u16, max_rows: u16, priority: u64) -> CardDemand {
    CardDemand {
        min_rows,
        max_rows: Some(max_rows),
        priority,
    }
}

fn rows(screen: u16) -> Option<CardRows> {
    Some(CardRows {
        screen,
        shown: screen,
    })
}

#[test]
fn cards_get_exactly_their_minimum_and_leave_the_rest_below_the_list() {
    assert_eq!(
        allocate(30, 4, &[demand(6, 1), demand(9, 2)]),
        CardAllocation {
            cards: vec![rows(6), rows(9)],
        }
    );
    assert_eq!(
        allocate(16, 2, &[demand(6, 1)]).cards,
        vec![rows(6)],
        "a card that declares no maximum never grows"
    );
}

#[test]
fn spare_height_goes_fairly_to_cards_that_declare_a_maximum() {
    // 40 - 4 list rows - (1 + 6) - (1 + 6) - (1 + 8) = 13 spare rows.
    assert_eq!(
        allocate(40, 4, &[growing(6, 60, 1), demand(6, 2), growing(8, 60, 3)]).cards,
        vec![rows(13), rows(6), rows(14)],
        "shared evenly between the growing cards, the first ones taking what is left"
    );
    assert_eq!(
        allocate(40, 4, &[growing(6, 8, 1), demand(6, 2), growing(8, 60, 3)]).cards,
        vec![rows(8), rows(6), rows(19)],
        "a card stops at its maximum and the other takes the rest"
    );
    assert_eq!(
        allocate(40, 4, &[growing(6, 7, 1), growing(8, 9, 2)]).cards,
        vec![rows(7), rows(9)],
        "what no card can use stays empty"
    );
    assert_eq!(
        allocate(15, 3, &[growing(6, 60, 1), growing(6, 60, u64::MAX)]),
        CardAllocation {
            // 15 - 3 list rows - (1 + 6) = 5 spare rows.
            cards: vec![None, rows(11)],
        },
        "no card grows into another one's minimum: the other collapses"
    );
}

#[test]
fn cards_that_do_not_fit_collapse_keeping_the_highest_priority() {
    // Each card needs its row, a rule and 6 screen rows: 8; two need 16, and no line tells of
    // the others.
    assert_eq!(
        allocate(16, 3, &[demand(6, 1), demand(6, u64::MAX), demand(6, 2)]),
        CardAllocation {
            cards: vec![None, rows(6), rows(6)],
        }
    );
    assert_eq!(
        allocate(15, 3, &[demand(6, 1), demand(6, u64::MAX), demand(6, 2)]),
        CardAllocation {
            cards: vec![None, rows(6), None],
        },
        "the focused card stays first"
    );
}

#[test]
fn a_card_whose_minimum_does_not_fit_is_cropped() {
    let allocation = allocate(6, 1, &[demand(9, 1)]);
    assert_eq!(
        allocation.cards,
        vec![Some(CardRows {
            screen: 9,
            shown: 4
        })]
    );
    assert!(allocation.cards[0].unwrap().is_cropped());
    assert_eq!(allocate(0, 0, &[]).cards, Vec::new());
}

fn styled_screen(rows: u16, columns: u16, output: &[u8]) -> vt100::Parser {
    let mut parser = vt100::Parser::new(rows, columns, 0);
    parser.process(output);
    parser
}

#[test]
fn a_screen_row_keeps_every_cell_style_and_is_cut_at_the_card_edge() {
    let parser = styled_screen(
        2,
        12,
        b"\x1b[1;31mred\x1b[0m \x1b[38;5;200mpink\x1b[0m\x1b[48;2;1;2;3mx\x1b[0m\r\nab\xe8\x85\xbe",
    );
    let line = screen_line(parser.screen(), 0, 10);
    assert_eq!(line.text(), "red pinkx ");
    let style_of = |text: &str| {
        line.spans
            .iter()
            .find(|span| span.text.contains(text))
            .and_then(|span| span.literal)
            .unwrap()
    };
    let red = style_of("red");
    assert!(red.bold);
    assert_eq!(red.foreground, Some(Colour::Indexed(1)));
    assert_eq!(style_of("pink").foreground, Some(Colour::Indexed(200)));
    assert_eq!(
        style_of("x").background,
        Some(Colour::Rgb(super::super::style::Rgb {
            red: 1,
            green: 2,
            blue: 3
        }))
    );
    let rendered = line
        .resolve(Default::default(), 10)
        .render(super::super::style::ColourDepth::Indexed256);
    assert!(rendered.contains("\x1b[1m\x1b[31mred"), "{rendered:?}");
    assert!(rendered.contains("\x1b[38;5;200mpink"), "{rendered:?}");

    assert_eq!(
        screen_line(parser.screen(), 1, 3).text(),
        "ab ",
        "a wide character at the edge leaves its column blank"
    );
    assert_eq!(screen_line(parser.screen(), 1, 4).text(), "ab腾");
    assert_eq!(
        screen_line(parser.screen(), 5, 4).text(),
        "    ",
        "blank beyond the Tick's screen"
    );
}

#[test]
fn the_focused_card_rule_is_heavy_and_accented() {
    let display = DisplayProfile::default();
    let quiet = rule_line(12, false, None, display);
    assert_eq!(quiet.text(), "────────────");
    assert!(
        quiet
            .spans
            .iter()
            .all(|span| span.role == Some(Role::Subtle))
    );
    let focused = rule_line(12, true, None, display);
    assert_eq!(focused.text(), "━━━━━━━━━━━━");
    assert!(
        focused
            .spans
            .iter()
            .all(|span| span.role == Some(Role::Accent))
    );
    let noted = rule_line(
        24,
        false,
        Some(&CardNote::plain("terminal too short")),
        display,
    );
    assert_eq!(noted.text(), "── terminal too short ──");
}

#[test]
fn a_focused_card_turns_on_legacy_keys_and_its_ticks_input_modes_until_it_leaves() {
    let tick = CardModes {
        application_cursor: true,
        application_keypad: false,
    };
    assert_eq!(mode_changes(None, Some(tick)), "\x1b[>0u\x1b[?1h");
    assert_eq!(mode_changes(Some(tick), Some(tick)), "");
    assert_eq!(mode_changes(Some(tick), None), "\x1b[?1l\x1b[<u");
    assert_eq!(mode_changes(None, None), "");
}

#[test]
fn cards_close_with_their_chat_and_their_rows_and_lose_the_focus_with_them() {
    let chat = Uuid::from_u128(1);
    let first: InstanceRef = "hosted:one".parse().unwrap();
    let second: InstanceRef = "hosted:two".parse().unwrap();
    let mut cards = MonitorCards::default();
    assert!(cards.open(chat, first.clone()));
    assert!(!cards.open(chat, first.clone()), "already open");
    assert!(cards.open(chat, second.clone()));
    cards.focus(second.clone());
    assert_eq!(cards.priority(cards.card(&second).unwrap()), u64::MAX);
    assert!(cards.priority(cards.card(&first).unwrap()) < u64::MAX);
    cards.retain(Some(chat), |instance| instance == &first);
    assert!(!cards.is_open(&second) && cards.focused().is_none());
    cards.retain(Some(Uuid::from_u128(2)), |_| true);
    assert!(cards.cards().is_empty(), "another Chat shows no cards");
}

#[test]
fn the_revealed_or_previewed_card_comes_right_after_the_focused_one() {
    let chat = Uuid::from_u128(1);
    let [focused, user, agent, closed]: [InstanceRef; 4] = [
        "hosted:focused",
        "hosted:user",
        "hosted:agent",
        "hosted:closed",
    ]
    .map(|instance| instance.parse().unwrap());
    let mut cards = MonitorCards::default();
    cards.open(chat, focused.clone());
    cards.open_as(chat, agent.clone(), CardOrigin::Agent);
    cards.open(chat, user.clone());
    cards.focus(focused.clone());
    let priority = |cards: &MonitorCards, instance: &InstanceRef| {
        cards.priority(cards.card(instance).unwrap())
    };
    assert!(priority(&cards, &user) > priority(&cards, &agent));

    assert!(cards.reveal(&agent));
    assert_eq!(cards.revealed(), Some(&agent));
    assert_eq!(cards.card(&agent).unwrap().origin, CardOrigin::Agent);
    assert!(priority(&cards, &agent) > priority(&cards, &user));
    assert!(priority(&cards, &agent) < priority(&cards, &focused));
    assert!(cards.end_reveal());
    assert!(cards.is_open(&agent), "a revealed card stays open");
    assert_eq!(cards.card(&agent).unwrap().origin, CardOrigin::Agent);
    assert!(priority(&cards, &user) > priority(&cards, &agent));
    assert!(!cards.end_reveal(), "nothing is revealed any more");

    assert!(!cards.reveal(&closed), "a card not open is not opened");
    assert!(!cards.is_open(&closed) && cards.revealed().is_none());

    assert!(
        !cards.preview(chat, agent.clone()),
        "Tab on an open card reveals it"
    );
    assert_eq!(cards.revealed(), Some(&agent));
    assert_eq!(cards.card(&agent).unwrap().origin, CardOrigin::Agent);
    assert!(cards.end_reveal());
    assert!(cards.is_open(&agent));

    assert!(
        cards.preview(chat, closed.clone()),
        "Tab opens a card not open"
    );
    assert_eq!(cards.card(&closed).unwrap().origin, CardOrigin::Preview);
    assert!(priority(&cards, &closed) > priority(&cards, &user));
    assert!(cards.end_reveal());
    assert!(
        !cards.is_open(&closed),
        "a card open only as a preview closes"
    );

    cards.preview(chat, closed.clone());
    cards.open(chat, closed.clone());
    assert!(cards.end_reveal());
    assert_eq!(
        cards.card(&closed).unwrap().origin,
        CardOrigin::User,
        "a preview the user kept stays"
    );

    cards.reveal(&user);
    cards.close(&user);
    assert_eq!(
        cards.revealed(),
        None,
        "closing the revealed card ends the reveal"
    );
}

#[test]
fn a_chats_cards_are_remembered_without_a_preview_until_the_console_forgets_the_chat() {
    let chat = Uuid::from_u128(1);
    let shown: InstanceRef = "hosted:shown".parse().unwrap();
    let used: InstanceRef = "hosted:used".parse().unwrap();
    let hidden: InstanceRef = "hosted:hidden".parse().unwrap();
    let previewed: InstanceRef = "hosted:previewed".parse().unwrap();
    let gone: InstanceRef = "hosted:gone".parse().unwrap();
    let mut cards = MonitorCards::default();
    cards.open(chat, shown.clone());
    cards.open_as(chat, used.clone(), CardOrigin::Agent);
    cards.open(chat, gone.clone());
    cards.open(chat, hidden.clone());
    cards.hide(&hidden);
    cards.preview(chat, previewed.clone());
    cards.focus(shown.clone());
    let sequences: Vec<u64> = [&shown, &used]
        .iter()
        .map(|instance| cards.card(instance).unwrap().sequence)
        .collect();

    cards.remember();
    assert!(cards.cards().is_empty() && cards.focused().is_none());
    assert_eq!(cards.revealed(), None, "the preview is not remembered");
    assert!(cards.agent_may_show(&hidden), "no Chat is shown");
    let restored = cards.restore(chat, |_| true, |instance| instance != &gone);
    assert_eq!(restored, [shown.clone(), used.clone()]);
    assert_eq!(
        [&shown, &used].map(|instance| {
            let card = cards.card(instance).unwrap();
            assert!(matches!(card.state, CardState::Opening));
            (card.origin, card.sequence)
        }),
        [
            (CardOrigin::User, sequences[0]),
            (CardOrigin::Agent, sequences[1])
        ]
    );
    assert!(cards.focused().is_none(), "the keyboard is not given back");
    assert!(!cards.agent_may_show(&hidden));

    cards.remember();
    let restored = cards.restore(chat, |instance| instance != &hidden, |_| true);
    assert_eq!(restored, [shown.clone(), used.clone()]);
    assert!(
        cards.agent_may_show(&hidden),
        "a hidden row no longer listed is forgotten"
    );

    cards.hide(&used);
    cards.remember();
    cards.forget_chats(|known| known != chat);
    assert!(cards.restore(chat, |_| true, |_| true).is_empty());
    assert!(
        cards.agent_may_show(&used),
        "a forgotten Chat starts afresh"
    );
}

#[test]
fn input_typed_before_the_view_attaches_is_held_for_it() {
    let chat = Uuid::from_u128(1);
    let card: InstanceRef = "hosted:early".parse().unwrap();
    let mut cards = MonitorCards::default();
    cards.open(chat, card.clone());
    cards.focus(card.clone());
    cards.input(b"ab".to_vec());
    cards.input(vec![b'c'; EARLY_INPUT_BYTES]);
    cards.input(b"d".to_vec());
    assert_eq!(
        cards.card(&card).unwrap().early_input,
        b"ab",
        "input beyond the limit is dropped, and everything after it"
    );
}

#[tokio::test]
async fn a_view_whose_tickhost_stops_reading_ends_and_closes_its_connection() {
    use tokio::io::AsyncReadExt;
    let (local, mut remote) = tokio::net::UnixStream::pair().unwrap();
    let (_unread, writer) = local.into_split();
    let screen = Arc::new(Mutex::new(CardScreen::new(vt100::Parser::new(6, 30, 0))));
    let (requests, queued) = mpsc::channel(CARD_REQUESTS);
    let reader = tokio::spawn(std::future::pending::<()>());
    let writing = tokio::spawn(write_requests(
        writer,
        queued,
        CardSize {
            rows: 6,
            columns: 30,
        },
        reader.abort_handle(),
        screen.clone(),
        Arc::new(Notify::new()),
        Duration::from_millis(200),
    ));
    for _ in 0..CARD_REQUESTS {
        requests
            .try_send(CardRequest::Input(vec![b'x'; ATTACH_INPUT_BYTES]))
            .unwrap();
    }
    assert!(
        requests
            .try_send(CardRequest::Input(b"more".to_vec()))
            .is_err(),
        "the queue is bounded"
    );
    tokio::time::timeout(Duration::from_secs(10), writing)
        .await
        .expect("a write the Tickhost never takes times out")
        .unwrap();
    assert!(screen.lock().unwrap().ended.is_some());
    assert!(reader.await.unwrap_err().is_cancelled());
    let mut written = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), remote.read_to_end(&mut written))
        .await
        .expect("the view's connection closes, which detaches it")
        .unwrap();
}

fn size(columns: u16) -> CardSize {
    CardSize { rows: 6, columns }
}

#[test]
fn a_lone_resize_goes_at_once_and_a_burst_sends_only_its_latest_size_per_interval() {
    let start = tokio::time::Instant::now();
    let at = |milliseconds| start + Duration::from_millis(milliseconds);
    let mut pacing = ResizePacing::new(size(40));
    assert_eq!(pacing.request(size(40), at(0)), None, "the Tick has it");
    assert_eq!(
        pacing.request(size(39), at(0)),
        Some(size(39)),
        "a lone change"
    );
    assert_eq!(pacing.request(size(38), at(10)), None);
    assert_eq!(pacing.request(size(37), at(20)), None);
    assert_eq!(pacing.deadline(), Some(at(100)));
    assert_eq!(pacing.due(at(99)), None);
    assert_eq!(pacing.due(at(100)), Some(size(37)), "only the latest size");
    assert_eq!(pacing.deadline(), None);
    assert_eq!(
        pacing.request(size(36), at(150)),
        None,
        "still within the interval"
    );
    assert_eq!(
        pacing.request(size(37), at(160)),
        None,
        "back to the size the Tick has"
    );
    assert_eq!(pacing.deadline(), None, "so nothing waits");
    assert_eq!(
        pacing.request(size(30), at(400)),
        Some(size(30)),
        "a quiet spell ended"
    );
}

#[test]
fn a_flush_sends_the_waiting_size_at_once() {
    let start = tokio::time::Instant::now();
    let at = |milliseconds| start + Duration::from_millis(milliseconds);
    let mut pacing = ResizePacing::new(size(40));
    assert_eq!(pacing.flush(at(0)), None, "nothing waits");
    pacing.request(size(39), at(0));
    pacing.request(size(30), at(5));
    assert_eq!(pacing.flush(at(6)), Some(size(30)));
    assert_eq!(pacing.deadline(), None);
    assert_eq!(
        pacing.request(size(29), at(50)),
        None,
        "the flush counts as a send"
    );
}

/// The resizes a view sent its Tickhost, read from the other end of its connection.
async fn sent_resizes(
    remote: &mut tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
) -> Vec<u64> {
    let mut columns = Vec::new();
    while let Ok(Ok(Some(frame))) = tokio::time::timeout(
        Duration::from_millis(1),
        hosted::read_frame(remote, ATTACH_REQUEST_FRAME_BYTES),
    )
    .await
    {
        if frame["method"] == "resize" {
            columns.push(frame["params"]["size"]["columns"].as_u64().unwrap());
        }
    }
    columns
}

fn spawn_writer(
    local: tokio::net::UnixStream,
) -> (mpsc::Sender<CardRequest>, tokio::task::JoinHandle<()>) {
    let (_unread, writer) = local.into_split();
    let screen = Arc::new(Mutex::new(CardScreen::new(vt100::Parser::new(6, 40, 0))));
    let (requests, queued) = mpsc::channel(CARD_REQUESTS);
    let reader = tokio::spawn(std::future::pending::<()>());
    let writing = tokio::spawn(write_requests(
        writer,
        queued,
        size(40),
        reader.abort_handle(),
        screen,
        Arc::new(Notify::new()),
        CARD_WRITE_TIMEOUT,
    ));
    (requests, writing)
}

#[tokio::test(start_paused = true)]
async fn dragging_across_many_columns_sends_a_few_resizes_ending_with_the_final_size() {
    let (local, remote) = tokio::net::UnixStream::pair().unwrap();
    let (remote, _remote_writer) = remote.into_split();
    let mut remote = tokio::io::BufReader::new(remote);
    let (requests, _writing) = spawn_writer(local);
    let columns: Vec<u16> = (20..60).collect();
    for column in &columns {
        requests
            .send(CardRequest::Resize(size(*column)))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(CARD_RESIZE_INTERVAL).await;
    let sent = sent_resizes(&mut remote).await;
    assert!(
        sent.len() <= 1 + columns.len() * 10 / 100 + 1,
        "{} changes over {}ms sent {sent:?}",
        columns.len(),
        columns.len() * 10
    );
    assert_eq!(sent.first(), Some(&20), "the first change goes at once");
    assert_eq!(
        sent.last(),
        Some(&59),
        "the final size goes once the drag stops"
    );
    assert_eq!(sent.iter().filter(|column| **column == 59).count(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_release_sends_the_final_size_at_once_and_a_lone_change_never_waits() {
    let (local, remote) = tokio::net::UnixStream::pair().unwrap();
    let (remote, _remote_writer) = remote.into_split();
    let mut remote = tokio::io::BufReader::new(remote);
    let (requests, _writing) = spawn_writer(local);
    requests.send(CardRequest::Resize(size(30))).await.unwrap();
    tokio::task::yield_now().await;
    assert_eq!(
        sent_resizes(&mut remote).await,
        [30],
        "a lone change goes at once"
    );
    requests.send(CardRequest::Resize(size(31))).await.unwrap();
    requests.send(CardRequest::Resize(size(32))).await.unwrap();
    requests.send(CardRequest::FlushResize).await.unwrap();
    tokio::task::yield_now().await;
    assert_eq!(
        sent_resizes(&mut remote).await,
        [32],
        "the release sends the final size without waiting out the interval"
    );
    tokio::time::sleep(CARD_RESIZE_INTERVAL * 2).await;
    assert_eq!(
        sent_resizes(&mut remote).await,
        Vec::<u64>::new(),
        "and only once"
    );
}

#[test]
fn an_ended_ticks_note_takes_its_rows_state_colour() {
    let display = DisplayProfile::default();
    let note = CardNote::ended("exited 101".into(), Role::ErrorMark);
    let rule = rule_line(24, false, Some(&note), display);
    assert_eq!(rule.text(), "── exited 101 ──────────");
    assert!(
        rule.spans
            .iter()
            .any(|span| span.text.contains("exited 101") && span.role == Some(Role::ErrorMark))
    );
}
