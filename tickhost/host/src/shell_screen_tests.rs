use super::*;

fn size(rows: u16, columns: u16) -> TerminalSize {
    TerminalSize::validated(rows, columns).unwrap()
}

/// The rows a view scrolled `back` lines shows, trailing blanks trimmed.
fn lines_at(screen: &mut ShellScreen, back: usize) -> Vec<String> {
    let (_, shown) = screen.frame_since(back, None);
    let (rows, columns) = shown.size();
    shown
        .rows(0, columns)
        .take(usize::from(rows))
        .map(|line| line.trim_end().to_owned())
        .collect()
}

fn lines(screen: &mut ShellScreen) -> Vec<String> {
    lines_at(screen, 0)
}

#[test]
fn the_header_names_the_task_with_its_status_beside_the_key_command() {
    let mut screen = ShellScreen::new(
        Some("Run the tests\nwith output".into()),
        "cd /work/tree && cargo test\n  -p console".into(),
        "/work/tree".into(),
    );
    screen.redraw(size(12, 44), b"compiling\nrunning 3 tests\n");
    let shown = lines(&mut screen);
    assert!(
        shown[0].starts_with("$ cargo test   -p console ") && shown[0].ends_with(" running 0s"),
        "{}",
        shown[0]
    );
    assert_eq!(UnicodeWidthStr::width(shown[0].as_str()), 44);
    assert_eq!(
        shown[1..5],
        [
            "Run the tests",
            "  cd /work/tree && cargo test",
            "    -p console",
            "in /work/tree",
        ]
    );
    assert_eq!(shown[5], "─".repeat(44));
    assert_eq!(shown[6..8], ["compiling", "running 3 tests"]);
    assert_eq!(
        shown[11], "",
        "the last row is left for the view's key hints"
    );

    screen.append(b"ok\n");
    screen.end("exit 0".into(), None, true);
    let shown = lines(&mut screen);
    assert!(shown[0].ends_with("exit 0"), "{}", shown[0]);
    assert_eq!(shown[8], "ok");
    screen.end("exit 2".into(), None, false);
    assert!(lines(&mut screen)[0].ends_with("exit 2"));
    screen.end("signal 15".into(), Some("cancelled"), false);
    assert!(lines(&mut screen)[0].ends_with("stopped"));
    screen.end("signal 9".into(), Some("output_limit_exceeded"), false);
    assert!(lines(&mut screen)[0].ends_with("signal 9 · output_limit_exceeded"));
}

#[test]
fn a_command_that_says_no_more_than_its_key_command_shows_once() {
    let mut screen = ShellScreen::new(None, "make test".into(), "/".into());
    screen.redraw(size(8, 40), b"");
    let shown = lines(&mut screen);
    assert!(shown[0].starts_with("$ make test"));
    assert_eq!(shown[1], "in /");
    assert_eq!(shown[2], "─".repeat(40));
}

#[test]
fn the_running_time_moves_on_until_the_task_ends() {
    let mut screen = ShellScreen::new(None, "sleep 100".into(), "/".into());
    screen.redraw(size(6, 40), b"");
    let start = screen.started;
    assert!(!screen.tick(start), "nothing changed yet");
    assert!(screen.tick(start + std::time::Duration::from_secs(65)));
    assert!(lines(&mut screen)[0].ends_with("running 1m 5s"));
    assert!(!screen.tick(start + std::time::Duration::from_millis(65_400)));
    screen.end("exit 0".into(), None, true);
    assert!(!screen.tick(start + std::time::Duration::from_secs(80)));
}

#[test]
fn a_narrow_screen_gives_the_status_its_own_row() {
    let mut screen = ShellScreen::new(None, "make".into(), "/".into());
    screen.redraw(size(8, 20), b"out\n");
    let shown = lines(&mut screen);
    assert_eq!(shown[..2], ["$ make", "running 0s"]);
}

#[test]
fn a_short_screen_gives_up_the_rule_directory_description_and_full_command_first() {
    let command = format!("cd /a && {}", "x".repeat(200));
    let mut screen = ShellScreen::new(Some("Build".into()), command, "/".into());
    screen.redraw(size(30, 50), b"");
    let full_rows = lines(&mut screen)
        .iter()
        .filter(|line| line.starts_with("  cd /a"))
        .count();
    assert_eq!(full_rows, 1, "the full command starts on one row");
    let wrapped = lines(&mut screen)
        .iter()
        .filter(|line| line.contains('x') && !line.starts_with('$'))
        .count();
    assert_eq!(wrapped, COMMAND_ROWS, "and takes at most its rows");
    assert!(
        lines(&mut screen)
            .iter()
            .all(|line| UnicodeWidthStr::width(line.as_str()) <= 50)
    );

    screen.redraw(size(4, 50), b"out\n");
    let shown = lines(&mut screen);
    assert!(shown[0].starts_with("$ xxx") && shown[0].ends_with("running 0s"));
    assert_eq!(
        shown[1..],
        [
            &format!("  cd /a && {}{ELLIPSIS}", "x".repeat(38)),
            "out",
            ""
        ],
        "the full command stays after the description gave way"
    );
    screen.redraw(size(3, 50), b"out\n");
    assert_eq!(lines(&mut screen)[1..], ["out", ""]);
    screen.redraw(size(2, 50), b"out\n");
    assert_eq!(lines(&mut screen)[1], "out", "no room for a hint row");
}

#[test]
fn a_view_scrolls_back_through_the_output_and_keeps_its_lines_as_more_arrive() {
    let mut screen = ShellScreen::new(None, "seq 100".into(), "/".into());
    let output = (1..=100)
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    screen.redraw(size(8, 40), output.as_bytes());
    assert_eq!(screen.output_rows(), 4);
    assert_eq!(
        lines(&mut screen)[3..7],
        ["97", "98", "99", "100"],
        "a following view shows the newest lines"
    );
    let history = screen.history_rows();
    assert_eq!(history, 96);
    assert_eq!(lines_at(&mut screen, 10)[3..7], ["87", "88", "89", "90"]);
    assert_eq!(
        lines(&mut screen)[6],
        "100",
        "scrolling one view leaves the newest lines for the others"
    );
    assert_eq!(screen.append(b"101\n102\n"), 2);
    assert_eq!(lines_at(&mut screen, 12)[3..7], ["87", "88", "89", "90"]);
}

#[test]
fn scroll_keys_move_a_view_and_stop_at_either_end() {
    assert_eq!(
        scroll_keys(b"\x1b[A\x10x\x1b[B\x0e\x1b[5~\x1b[6~\x1b[H\x1b[FG"),
        [
            Scroll::Lines(-1),
            Scroll::Lines(-1),
            Scroll::Lines(1),
            Scroll::Lines(1),
            Scroll::Pages(-1),
            Scroll::Pages(1),
            Scroll::Oldest,
            Scroll::Newest,
            Scroll::Newest,
        ]
    );
    assert_eq!(scrolled(0, Scroll::Lines(-1), 50, 10), 1, "up goes back");
    assert_eq!(
        scrolled(0, Scroll::Lines(1), 50, 10),
        0,
        "the newest is the end"
    );
    assert_eq!(scrolled(5, Scroll::Pages(-1), 50, 10), 14);
    assert_eq!(scrolled(45, Scroll::Pages(-1), 50, 10), 50);
    assert_eq!(scrolled(5, Scroll::Oldest, 50, 10), 50);
    assert_eq!(scrolled(30, Scroll::Newest, 50, 10), 0);
}

#[test]
fn control_characters_in_the_command_are_shown_as_spaces_not_run() {
    let mut screen = ShellScreen::new(None, "printf '\x1b[2J'\tdone".into(), "/".into());
    screen.redraw(size(10, 60), b"");
    assert!(lines(&mut screen)[0].starts_with("$ printf ' [2J' done"));
}
