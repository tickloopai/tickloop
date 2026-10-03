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

#[allow(dead_code)]
pub fn message_flow(frame: &str) -> &str {
    match frame.split_once('\n') {
        Some((header, flow)) if header.starts_with("❯ ") => flow,
        _ => frame,
    }
}

/// Reconstructs completed Agent frames, including rows retained by differential redraws.
pub fn visible_screen(output: &str, width: usize, height: usize) -> String {
    let end = output
        .rfind("\x1b[?2026l")
        .map_or(output.len(), |end| end + "\x1b[?2026l".len());
    let controls =
        regex::Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07]*(?:\x07|\x1b\\)").unwrap();
    let mut rows = vec![vec![' '; width]; height];
    let (mut row, mut column) = (0usize, 0usize);
    let (mut top, mut bottom) = (0usize, height - 1);
    let mut offset = 0;
    for control in controls.find_iter(&output[..end]) {
        for character in output[offset..control.start()].chars() {
            match character {
                '\r' => column = 0,
                '\n' => row = (row + 1).min(height - 1),
                character if !character.is_control() => {
                    let cells = character.width().unwrap_or(0);
                    if cells > 0 && row < height && column < width {
                        rows[row][column] = character;
                        rows[row][column + 1..(column + cells).min(width)].fill('\0');
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
                'H' | 'f' => {
                    row = first.saturating_sub(1).min(height - 1);
                    column = values
                        .get(1)
                        .copied()
                        .unwrap_or(1)
                        .saturating_sub(1)
                        .min(width - 1);
                }
                'J' if first == 2 => rows.iter_mut().for_each(|line| line.fill(' ')),
                'K' => {
                    let start = if first == 2 { 0 } else { column.min(width) };
                    rows[row][start..].fill(' ');
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
                    }
                }
                _ => {}
            }
        }
        offset = control.end();
    }
    rows.into_iter()
        .map(|line| {
            line.into_iter()
                .filter(|character| *character != '\0')
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn retained_rows_and_cleared_rows_are_both_reflected_in_the_screen() {
    let output = "\x1b[?2026h\x1b[2J\x1b[1;1HProvider\x1b[2;1H旧内容\x1b[3;1HFooter\x1b[?2026l\x1b[?2026h\x1b[2;1H\x1b[2KModel\x1b[?2026l";
    assert_eq!(visible_screen(output, 20, 3), "Provider\nModel\nFooter");
    let scrolled = format!("{output}\x1b[?2026h\x1b[1;2r\x1b[1S\x1b[r\x1b[?2026l");
    assert_eq!(visible_screen(&scrolled, 20, 3), "Model\n\nFooter");
}
