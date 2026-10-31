use super::*;

fn size(rows: u16, columns: u16) -> ScreenSize {
    ScreenSize { rows, columns }
}

/// Frames a Tick screen as the host does: whole at first and after a resize, then only what
/// changed.
struct HostScreen {
    parser: vt100::Parser,
    shown: Option<vt100::Screen>,
}

impl HostScreen {
    fn new(rows: u16, columns: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, columns, 0),
            shown: None,
        }
    }

    fn frame(&mut self, output: &[u8]) -> (ScreenSize, Vec<u8>) {
        self.parser.process(output);
        let current = self.parser.screen();
        let mut bytes = match &self.shown {
            Some(shown) if shown.size() == current.size() => current.contents_diff(shown),
            _ => current.contents_formatted(),
        };
        match &self.shown {
            Some(shown) if shown.size() == current.size() => {
                bytes.extend(current.input_mode_diff(shown));
            }
            _ => bytes.extend(current.input_mode_formatted()),
        }
        self.shown = Some(current.clone());
        let (rows, columns) = current.size();
        (size(rows, columns), bytes)
    }
}

/// A terminal of `terminal`'s size that showed something else before.
fn terminal(terminal: ScreenSize) -> vt100::Parser {
    let mut parser = vt100::Parser::new(terminal.rows, terminal.columns, 0);
    parser.process(&b"stale".repeat(usize::from(terminal.rows) * usize::from(terminal.columns)));
    parser
}

fn rows(terminal: &vt100::Parser) -> Vec<String> {
    let (_, columns) = terminal.screen().size();
    terminal.screen().rows(0, columns).collect()
}

#[test]
fn a_smaller_terminal_shows_the_top_left_of_the_screen_cut_at_its_edges() {
    let mut host = HostScreen::new(6, 20);
    let (tick, frame) =
        host.frame("first line is long\r\nabcdefghi腾 wide\r\nthird\r\nfourth\r\nfifth".as_bytes());
    let small = size(4, 10);
    let mut view = ViewScreen::new(small);
    let mut shown = terminal(small);
    shown.process(&view.frame(tick, &frame));
    assert_eq!(
        rows(&shown),
        ["first line", "abcdefghi", "third", "fourth"],
        "the top-left part, never scrolled"
    );
    assert_eq!(
        shown.screen().cell(1, 9).unwrap().contents(),
        "",
        "a wide character that would spill past the edge is left out"
    );
    assert!(
        shown.screen().hide_cursor(),
        "a cursor outside the part shown is hidden"
    );

    let (tick, change) = host.frame(b"\x1b[2;1HABC\x1b[1;3H");
    shown.process(&view.frame(tick, &change));
    assert_eq!(rows(&shown)[..2], ["first line", "ABCdefghi"]);
    assert!(!shown.screen().hide_cursor());
    assert_eq!(shown.screen().cursor_position(), (0, 2));
}

#[test]
fn a_larger_terminal_shows_the_whole_screen_with_blank_space_beyond_it() {
    let mut host = HostScreen::new(3, 10);
    let (tick, frame) = host.frame(b"\x1b[1;31m0123456789\x1b[mabc");
    let large = size(6, 20);
    let mut view = ViewScreen::new(large);
    let mut shown = terminal(large);
    shown.process(&view.frame(tick, &frame));
    assert_eq!(
        rows(&shown),
        ["0123456789", "abc", "", "", "", ""],
        "a wrapped line stays as the Tick drew it and the rest is blank"
    );
    assert_eq!(
        shown.screen().cell(0, 0).unwrap().fgcolor(),
        vt100::Color::Idx(1)
    );
    assert_eq!(
        shown.screen().cell(1, 0).unwrap().fgcolor(),
        vt100::Color::Default
    );
    assert_eq!(shown.screen().cursor_position(), (1, 3));
    assert!(!shown.screen().hide_cursor());
}

#[test]
fn a_terminal_of_the_screen_size_gets_the_frames_as_drawn_and_a_redraw_when_either_resizes() {
    let mut host = HostScreen::new(3, 10);
    let same = size(3, 10);
    let mut view = ViewScreen::new(same);
    let mut shown = terminal(same);
    let (tick, frame) = host.frame(b"hello");
    let drawn = view.frame(tick, &frame);
    assert_eq!(drawn, frame, "the host's frame as it is");
    shown.process(&drawn);

    let larger = size(5, 12);
    shown.screen_mut().set_size(larger.rows, larger.columns);
    shown.process(&view.resize_terminal(larger));
    let (tick, change) = host.frame(b" world");
    shown.process(&view.frame(tick, &change));
    assert_eq!(rows(&shown), ["hello worl", "d", "", "", ""]);

    shown.screen_mut().set_size(same.rows, same.columns);
    shown.process(&view.resize_terminal(same));
    let (tick, change) = host.frame(b"!");
    let drawn = view.frame(tick, &change);
    assert_eq!(
        drawn, change,
        "back at the same size, changes are drawn as they come"
    );
    shown.process(&drawn);
    assert_eq!(shown.screen().contents(), host.parser.screen().contents());

    host.parser.screen_mut().set_size(4, 12);
    let (tick, frame) = host.frame(b"");
    assert_eq!(tick, size(4, 12));
    shown.process(&view.frame(tick, &frame));
    assert_eq!(
        rows(&shown),
        ["hello worl", "d!", ""],
        "a resized screen is cut to fit"
    );
}

const ESC: &[u8] = b"\x1b";

fn tick_screen(output: &[u8]) -> vt100::Parser {
    let mut parser = vt100::Parser::new(24, 80, 0);
    parser.process(output);
    parser
}

/// Feeds `input` at `at` and settles a held lone Esc once nothing followed it.
fn press(
    keys: &mut FullScreenKeys,
    input: &[u8],
    at: Instant,
    tick: &vt100::Screen,
) -> Vec<KeyAction> {
    let mut actions = keys.feed(input, at, tick);
    if let Some(due) = keys.deadline() {
        actions.extend(keys.expire(due, tick));
    }
    actions
}

#[test]
fn a_lone_esc_reaches_the_tick_and_a_second_within_a_second_hides_the_view() {
    let tick = tick_screen(b"");
    let mut keys = FullScreenKeys::new(ViewInput::Interactive);
    let start = Instant::now();
    assert_eq!(
        press(&mut keys, ESC, start, tick.screen()),
        [KeyAction::Input(ESC.to_vec())]
    );
    let hint = keys
        .hint_until(start)
        .expect("the hint shows after the first Esc");
    assert_eq!(hint, start + FULL_SCREEN_ESCAPE_WINDOW);
    assert_eq!(
        keys.hint_until(start + FULL_SCREEN_ESCAPE_WINDOW),
        None,
        "the hint goes once a second Esc would no longer hide"
    );
    let second = start + Duration::from_millis(700);
    assert_eq!(
        press(&mut keys, ESC, second, tick.screen()),
        [KeyAction::Hide],
        "the second Esc never reaches the Tick"
    );
}

#[test]
fn an_esc_after_the_window_or_after_another_key_reaches_the_tick() {
    let tick = tick_screen(b"");
    let mut keys = FullScreenKeys::new(ViewInput::Interactive);
    let start = Instant::now();
    press(&mut keys, ESC, start, tick.screen());
    let late = start + FULL_SCREEN_ESCAPE_WINDOW + Duration::from_millis(100);
    assert_eq!(
        press(&mut keys, ESC, late, tick.screen()),
        [KeyAction::Input(ESC.to_vec())]
    );
    let typed = late + Duration::from_millis(200);
    assert_eq!(
        press(&mut keys, b"i", typed, tick.screen()),
        [KeyAction::Input(b"i".to_vec())]
    );
    assert_eq!(keys.hint_until(typed), None, "another key ends the pair");
    assert_eq!(
        press(
            &mut keys,
            ESC,
            typed + Duration::from_millis(100),
            tick.screen()
        ),
        [KeyAction::Input(ESC.to_vec())]
    );
}

#[test]
fn ctrl_keys_and_escape_sequences_reach_the_tick_unchanged() {
    let tick = tick_screen(b"");
    let mut keys = FullScreenKeys::new(ViewInput::Interactive);
    let now = Instant::now();
    assert_eq!(
        press(&mut keys, b"\x03\x02\x1b[A\x1bx", now, tick.screen()),
        [KeyAction::Input(b"\x03\x02\x1b[A\x1bx".to_vec())]
    );
    assert_eq!(keys.hint_until(now), None);
}

#[test]
fn a_read_only_screen_hides_on_a_lone_esc_and_takes_no_other_key() {
    let tick = tick_screen(b"");
    let mut keys = FullScreenKeys::new(ViewInput::ReadOnly { stoppable: false });
    let now = Instant::now();
    assert_eq!(
        press(&mut keys, b"\x1b[AG\x03", now, tick.screen()),
        [KeyAction::Input(b"\x1b[AG".to_vec())],
        "keys go to the host to scroll, and Ctrl+C stops nothing it cannot"
    );
    assert_eq!(press(&mut keys, ESC, now, tick.screen()), [KeyAction::Hide]);
    assert_eq!(keys.hint_until(now), None);

    let mut keys = FullScreenKeys::new(ViewInput::ReadOnly { stoppable: true });
    assert_eq!(
        press(&mut keys, b"G\x03\x1b[A", now, tick.screen()),
        [
            KeyAction::Input(b"G".to_vec()),
            KeyAction::Interrupt,
            KeyAction::Input(b"\x1b[A".to_vec()),
        ],
        "Ctrl+C stops a task its View may stop, and the view stays"
    );
}

#[test]
fn clicks_reach_a_tick_only_when_it_asked_for_mouse_reports() {
    let click = b"\x1b[<0;5;3M";
    let plain = tick_screen(b"");
    let mut keys = FullScreenKeys::new(ViewInput::Interactive);
    let now = Instant::now();
    assert_eq!(press(&mut keys, click, now, plain.screen()), []);

    let reporting = tick_screen(b"\x1b[?1000h\x1b[?1006h");
    assert_eq!(
        press(&mut keys, click, now, reporting.screen()),
        [KeyAction::Input(click.to_vec())]
    );
    let small = {
        let mut parser = vt100::Parser::new(2, 2, 0);
        parser.process(b"\x1b[?1000h\x1b[?1006h");
        parser
    };
    assert_eq!(
        press(&mut keys, click, now, small.screen()),
        [],
        "a click beyond a smaller screen reaches nothing"
    );
    assert_eq!(
        press(&mut keys, b"\x1b[I", now, reporting.screen()),
        [],
        "the terminal's own focus reports never reach the Tick"
    );
}

#[test]
fn the_hint_sits_on_the_bottom_row_and_leaves_the_ticks_cursor_and_colours() {
    let mut shown = vt100::Parser::new(4, 30, 0);
    shown.process(b"\x1b[31mred\x1b[2;2H");
    shown.process(&hint_overlay(size(4, 30), HIDE_HINT, INVERSE));
    assert_eq!(shown.screen().contents_between(3, 0, 3, 30), HIDE_HINT);
    assert!(shown.screen().cell(3, 0).unwrap().inverse());
    assert_eq!(shown.screen().cursor_position(), (1, 1));
    shown.process(b"x");
    assert_eq!(
        shown.screen().cell(1, 1).unwrap().fgcolor(),
        vt100::Color::Idx(1),
        "the Tick keeps drawing in its own colours"
    );

    let mut narrow = vt100::Parser::new(2, 10, 0);
    narrow.process(&hint_overlay(size(2, 10), HIDE_HINT, INVERSE));
    assert_eq!(narrow.screen().contents_between(1, 0, 1, 10), "Press esc ");
}

#[test]
fn a_shells_screen_always_shows_its_keys_and_a_ticks_only_the_esc_hint() {
    let terminal = size(5, 60);
    let text = |bytes: Vec<u8>| {
        let mut shown = vt100::Parser::new(5, 60, 0);
        shown.process(&bytes);
        shown.screen().contents_between(4, 0, 4, 60)
    };
    assert_eq!(
        text(overlay(
            ViewInput::ReadOnly { stoppable: true },
            false,
            terminal
        )),
        STOPPABLE_SHELL_HINT
    );
    assert_eq!(
        text(overlay(
            ViewInput::ReadOnly { stoppable: false },
            false,
            terminal
        )),
        SHELL_HINT
    );
    assert!(overlay(ViewInput::Interactive, false, terminal).is_empty());
    assert_eq!(
        text(overlay(ViewInput::Interactive, true, terminal)),
        HIDE_HINT
    );
}
