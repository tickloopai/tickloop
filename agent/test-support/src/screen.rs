use unicode_width::UnicodeWidthChar;

pub fn capture_output(
    mut reader: impl std::io::Read,
    output: std::sync::Arc<std::sync::Mutex<String>>,
) {
    let mut pending = Vec::new();
    let mut chunk = [0_u8; 8192];
    while let Ok(count) = reader.read(&mut chunk) {
        if count == 0 {
            break;
        }
        pending.extend_from_slice(&chunk[..count]);
        loop {
            match std::str::from_utf8(&pending) {
                Ok(text) => {
                    output.lock().unwrap().push_str(text);
                    pending.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    output
                        .lock()
                        .unwrap()
                        .push_str(std::str::from_utf8(&pending[..valid]).unwrap());
                    pending.drain(..valid);
                    match error.error_len() {
                        Some(invalid) => {
                            output.lock().unwrap().push('\u{fffd}');
                            pending.drain(..invalid);
                        }
                        None => break,
                    }
                }
            }
        }
    }
    if !pending.is_empty() {
        output
            .lock()
            .unwrap()
            .push_str(&String::from_utf8_lossy(&pending));
    }
}

#[test]
fn capture_output_preserves_utf8_characters_split_across_reads() {
    struct SingleByte(std::io::Cursor<Vec<u8>>);
    impl std::io::Read for SingleByte {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            std::io::Read::read(&mut self.0, &mut buffer[..1])
        }
    }
    let output = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    capture_output(
        SingleByte(std::io::Cursor::new(
            "GLM · Context — ✓".as_bytes().to_vec(),
        )),
        output.clone(),
    );
    assert_eq!(*output.lock().unwrap(), "GLM · Context — ✓");
}

/// A row's text without the `⎿` connector or the indentation a response's rows have.
#[allow(dead_code)]
pub fn response_line(line: &str) -> &str {
    line.trim().trim_start_matches('⎿').trim_start()
}

/// The queued inputs a frame lists above the input box, as Claude Code shows them: `  ❯ text`,
/// or `  ! command` for a queued command.
#[allow(dead_code)]
pub fn queued_inputs(frame: &str) -> Vec<&str> {
    frame.lines().filter(|line| is_queued_input(line)).collect()
}

/// `frame` split before its first queued input: the conversation, then the queue and the rows
/// below it.
#[allow(dead_code)]
pub fn split_at_queue(frame: &str) -> Option<(&str, &str)> {
    let mut offset = 0;
    for line in frame.split_inclusive('\n') {
        if is_queued_input(line) {
            return Some(frame.split_at(offset));
        }
        offset += line.len();
    }
    None
}

/// Whether `line` lists a queued input rather than echoing a sent one.
#[allow(dead_code)]
pub fn is_queued_input(line: &str) -> bool {
    line.starts_with("  ❯ ") || line.starts_with("  ! ")
}

#[allow(dead_code)]
pub fn message_flow(frame: &str) -> &str {
    match frame.split_once('\n') {
        Some((header, flow)) if header.starts_with("❯ ") => flow,
        _ => frame,
    }
}

/// Reconstructs completed Agent frames, including rows retained by differential redraws.
pub fn visible_screen(output: &str, width: usize, height: usize) -> String {
    Screen::reconstruct(output, width, height).text(false)
}

/// What a frame shows where the selection fill begins, since the fill alone marks the selected
/// row of a list.
pub const SELECTED_MARK: char = '▌';

/// The background parameters the Agent's two themes send for the selection fill, at 256 colours
/// and in true colour.
pub const SELECTION_BACKGROUNDS: [&str; 4] =
    ["48;5;24", "48;5;153", "48;2;38;79;120", "48;2;180;213;255"];

/// [`visible_screen`] with the selection fill made visible: in each row, the first cell painted
/// with the selection background shows [`SELECTED_MARK`] when that cell is blank, as the indent
/// of a list's selected row is. A Monitor row whose first column holds its update square or `⟳`
/// keeps that column off the selection, so its mark falls on the second column. Where something
/// else paints the middle of a selected row, such as a Monitor row's update bar, the selection
/// that resumes after it shows no second mark.
pub fn visible_screen_with_selection(output: &str, width: usize, height: usize) -> String {
    Screen::reconstruct(output, width, height).text(true)
}

struct Screen {
    rows: Vec<Vec<char>>,
    /// Whether each cell was painted with the selection background.
    selected: Vec<Vec<bool>>,
}

impl Screen {
    fn reconstruct(output: &str, width: usize, height: usize) -> Self {
        let end = output
            .rfind("\x1b[?2026l")
            .map_or(output.len(), |end| end + "\x1b[?2026l".len());
        static CONTROLS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let controls = CONTROLS.get_or_init(|| {
            regex::Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07]*?(?:\x07|\x1b\\)").unwrap()
        });
        let mut rows = vec![vec![' '; width]; height];
        let mut selected = vec![vec![false; width]; height];
        let (mut row, mut column) = (0usize, 0usize);
        let (mut top, mut bottom) = (0usize, height - 1);
        let mut on_selection = false;
        let mut offset = 0;
        for control in controls.find_iter(&output[..end]) {
            for character in output[offset..control.start()].chars() {
                match character {
                    '\r' => column = 0,
                    '\n' => row = (row + 1).min(height - 1),
                    character if !character.is_control() => {
                        let cells = character.width().unwrap_or(0);
                        if cells > 0 && row < height && column < width {
                            let covered = column..(column + cells).min(width);
                            rows[row][column] = character;
                            rows[row][column + 1..covered.end].fill('\0');
                            selected[row][covered.clone()].fill(on_selection);
                            column += cells;
                        }
                    }
                    _ => {}
                }
            }
            let sequence = control.as_str();
            if let Some(csi) = sequence.strip_prefix("\x1b[") {
                let action = csi.as_bytes()[csi.len() - 1] as char;
                let parameters = &csi[..csi.len() - 1];
                let values: Vec<usize> = parameters
                    .split(';')
                    .map(|value| value.parse().unwrap_or(0))
                    .collect();
                let first = values[0];
                match action {
                    'm' if SELECTION_BACKGROUNDS.contains(&parameters) => on_selection = true,
                    'm' if first == 0 || first == 49 || parameters.starts_with("48;") => {
                        on_selection = false
                    }
                    'H' | 'f' => {
                        row = first.saturating_sub(1).min(height - 1);
                        column = values
                            .get(1)
                            .copied()
                            .unwrap_or(1)
                            .saturating_sub(1)
                            .min(width - 1);
                    }
                    'J' if first == 2 => {
                        rows.iter_mut().for_each(|line| line.fill(' '));
                        selected.iter_mut().for_each(|line| line.fill(false));
                    }
                    'K' => {
                        let start = if first == 2 { 0 } else { column.min(width) };
                        rows[row][start..].fill(' ');
                        selected[row][start..].fill(false);
                    }
                    'r' => {
                        top = first.saturating_sub(1).min(height - 1);
                        bottom = values
                            .get(1)
                            .copied()
                            .filter(|value| *value > 0)
                            .unwrap_or(height)
                            .saturating_sub(1)
                            .min(height - 1);
                    }
                    'S' => {
                        for _ in 0..first.max(1).min(bottom - top + 1) {
                            rows[top..=bottom].rotate_left(1);
                            rows[bottom].fill(' ');
                            selected[top..=bottom].rotate_left(1);
                            selected[bottom].fill(false);
                        }
                    }
                    _ => {}
                }
            }
            offset = control.end();
        }
        Self { rows, selected }
    }

    fn text(self, marks_selection: bool) -> String {
        self.rows
            .into_iter()
            .zip(self.selected)
            .map(|(mut line, selected)| {
                if marks_selection
                    && let Some(column) = selected.iter().position(|selected| *selected)
                    && line[column] == ' '
                {
                    line[column] = SELECTED_MARK;
                }
                line.into_iter()
                    .filter(|character| *character != '\0')
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[test]
fn the_selection_fill_shows_as_a_mark_where_its_run_begins_on_a_blank_cell() {
    let output = "\x1b[?2026h\x1b[2J\x1b[1;1H  plain\x1b[2;1H\x1b[48;5;24m  ◉ chosen   \x1b[0m\x1b[3;1H\x1b[48;2;180;213;255m❯ option\x1b[0m\x1b[4;1Hleft \x1b[48;5;24m  row\x1b[0m\x1b[?2026l";
    assert_eq!(
        visible_screen_with_selection(output, 20, 4),
        "  plain\n▌ ◉ chosen\n❯ option\nleft ▌ row"
    );
    assert_eq!(
        visible_screen(output, 20, 4),
        "  plain\n  ◉ chosen\n❯ option\nleft   row",
        "the plain screen keeps the blank cells"
    );
    let banded = "\x1b[?2026h\x1b[2J\x1b[1;1H\x1b[48;5;24m \x1b[48;5;173m \x1b[48;5;239m◉ a \x1b[48;5;24m b  \x1b[0m\x1b[?2026l";
    assert_eq!(
        visible_screen_with_selection(banded, 20, 1),
        "▌ ◉ a  b",
        "the selection resuming after a band shows no second mark"
    );
    let led = "\x1b[?2026h\x1b[2J\x1b[1;1H\x1b[38;5;37m⟳\x1b[48;5;24m ◉ quotes  \x1b[0m\x1b[2;1H\x1b[48;5;173m \x1b[48;5;24m ◉ news  \x1b[0m\x1b[?2026l";
    assert_eq!(
        visible_screen_with_selection(led, 20, 2),
        "⟳▌◉ quotes\n ▌◉ news",
        "a Monitor row's first column stays off the selection"
    );
}

#[test]
fn retained_rows_and_cleared_rows_are_both_reflected_in_the_screen() {
    let output = "\x1b[?2026h\x1b[2J\x1b[1;1HProvider\x1b[2;1H旧内容\x1b[3;1HFooter\x1b[?2026l\x1b[?2026h\x1b[2;1H\x1b[2KModel\x1b[?2026l";
    assert_eq!(visible_screen(output, 20, 3), "Provider\nModel\nFooter");
    let scrolled = format!("{output}\x1b[?2026h\x1b[1;2r\x1b[1S\x1b[r\x1b[?2026l");
    assert_eq!(visible_screen(&scrolled, 20, 3), "Model\n\nFooter");
}

#[test]
fn osc_hyperlinks_preserve_labels_and_later_frame_updates() {
    for end in ["\x07", "\x1b\\"] {
        let first = format!(
            "\x1b[?2026h\x1b[2J\x1b[1;1HBrowser login\x1b[3;1HCancel login\x1b[2;1H\x1b]8;;https://first.test{end}First link\x1b]8;;{end}\x1b[?2026l"
        );
        let second = format!(
            "\x1b[?2026h\x1b[3;1H\x1b[2KSubmit callback\x1b[2;1H\x1b[2K\x1b]8;;https://second.test{end}Second link\x1b]8;;{end}\x1b[?2026l"
        );
        assert_eq!(
            visible_screen(&format!("{first}{second}"), 40, 3),
            "Browser login\nSecond link\nSubmit callback"
        );
    }
}
