use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    buffer::{Buffer, CellDiffOption},
    layout::Rect,
    text::{Line, Text},
    widgets::Paragraph,
};
use tickloop_ui::hyperlink::{OSC8_START, link_rows, linked, split_rows, supported};

const TARGET: &str = "https://example.com/oauth2/device?user_code=ABCD-EFGH&client_id=tickloop";

fn environment(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let pairs = pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect::<Vec<_>>();
    move |name| {
        pairs
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, value)| value.clone())
    }
}

/// What a terminal backend writes, kept where the test can read it.
#[derive(Clone, Default)]
struct Written(Arc<Mutex<Vec<u8>>>);

impl Written {
    fn since(&self, start: usize) -> String {
        String::from_utf8(self.0.lock().unwrap()[start..].to_vec()).unwrap()
    }

    fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

impl Write for Written {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A frame of `heading` above `rows`, each on its own line.
fn content(heading: &str, rows: &[String]) -> Paragraph<'static> {
    let mut lines = vec![Line::raw(heading.to_owned())];
    lines.extend(rows.iter().map(|row| Line::raw(row.clone())));
    Paragraph::new(Text::from(lines))
}

#[test]
fn terminals_that_show_links_are_detected_like_claude_code() {
    for supported_terminal in [
        &[
            ("TERM_PROGRAM", "iTerm.app"),
            ("TERM_PROGRAM_VERSION", "3.5.0"),
        ][..],
        &[
            ("TERM_PROGRAM", "WezTerm"),
            ("TERM_PROGRAM_VERSION", "20240203"),
        ],
        &[
            ("TERM_PROGRAM", "vscode"),
            ("TERM_PROGRAM_VERSION", "1.90.0"),
        ],
        &[("TERM_PROGRAM", "ghostty")],
        &[("VTE_VERSION", "7600")],
        &[("TERM", "alacritty")],
        &[("TERM", "xterm-kitty")],
        &[("TERM_PROGRAM", "tmux"), ("LC_TERMINAL", "iTerm2")],
        &[("WT_SESSION", "1")],
        &[("CI", "1"), ("TERM_PROGRAM", "kitty")],
        &[("FORCE_HYPERLINK", "1")],
        &[("FORCE_HYPERLINK", ""), ("TERM_PROGRAM", "ghostty")],
    ] {
        assert!(
            supported(environment(supported_terminal)),
            "{supported_terminal:?}"
        );
    }
    for plain_terminal in [
        &[][..],
        &[("TERM_PROGRAM", "Apple_Terminal")],
        &[
            ("TERM_PROGRAM", "vscode"),
            ("TERM_PROGRAM_VERSION", "1.71.0"),
        ],
        &[("VTE_VERSION", "0.50.0")],
        &[("VTE_VERSION", "4901")],
        &[("CI", "1"), ("TERM_PROGRAM", "WezTerm")],
        &[("FORCE_HYPERLINK", "0"), ("TERM_PROGRAM", "WezTerm")],
        &[
            ("FORCE_COLOR", "0"),
            ("TERM_PROGRAM", "vscode"),
            ("TERM_PROGRAM_VERSION", "1.90.0"),
        ],
        &[("TERM", "dumb"), ("WT_SESSION", "1")],
    ] {
        assert!(
            !supported(environment(plain_terminal)),
            "{plain_terminal:?}"
        );
    }
}

#[test]
fn text_splits_into_rows_by_the_columns_it_takes() {
    assert_eq!(split_rows("abcdefghij", 4), ["abcd", "efgh", "ij"]);
    assert_eq!(split_rows("宽字符", 3), ["宽", "字", "符"]);
    assert_eq!(split_rows("abc", 10), ["abc"]);
}

#[test]
fn every_row_of_a_wrapped_link_is_written_as_a_link_to_the_whole_target() {
    let area = Rect::new(0, 0, 30, 6);
    let output = Written::default();
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(output.clone()),
        TerminalOptions {
            viewport: Viewport::Fixed(area),
        },
    )
    .unwrap();
    let rows = split_rows(TARGET, usize::from(area.width));
    assert!(rows.len() > 1);

    terminal
        .draw(|frame| {
            frame.render_widget(content("Open", &rows), area);
            assert!(link_rows(frame.buffer_mut(), area, &rows, TARGET));
        })
        .unwrap();

    let written = output.since(0);
    for row in &rows {
        assert!(
            written.contains(&linked(TARGET, row)),
            "{row:?} is written as a link to the whole target: {written:?}"
        );
    }
    assert_eq!(written.matches(OSC8_START).count(), rows.len() * 2);
    assert!(written.contains("Open"), "{written:?}");

    let before = output.len();
    terminal
        .draw(|frame| {
            frame.render_widget(content("Open", &rows), area);
            link_rows(frame.buffer_mut(), area, &rows, TARGET);
        })
        .unwrap();
    let redrawn = output.since(before);
    assert!(
        !redrawn.contains(OSC8_START),
        "an unchanged link is not written again: {redrawn:?}"
    );
}

#[test]
fn a_link_that_is_not_shown_or_could_break_out_of_its_escape_leaves_the_buffer_alone() {
    let area = Rect::new(0, 0, 30, 6);
    let rows = split_rows(TARGET, usize::from(area.width));
    let drawn = || {
        let mut buffer = Buffer::empty(area);
        ratatui::widgets::Widget::render(content("Open", &rows), area, &mut buffer);
        buffer
    };
    let mut buffer = drawn();
    assert!(!link_rows(
        &mut buffer,
        area,
        &rows,
        "https://example.com/\x07\x1b]8;;evil"
    ));
    assert_eq!(buffer, drawn());
    assert!(!link_rows(
        &mut buffer,
        area,
        &split_rows("https://elsewhere.example/", 30),
        TARGET
    ));
    assert_eq!(buffer, drawn());
    assert!(
        buffer
            .content
            .iter()
            .all(|cell| cell.diff_option == CellDiffOption::None)
    );
}
