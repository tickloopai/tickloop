use std::{fs, path::PathBuf};

pub fn assert_frame(frame: &str, height: usize, title: &str) {
    let lines = frame.lines().collect::<Vec<_>>();
    let top = lines
        .iter()
        .rposition(|line| !line.is_empty() && line.chars().all(|character| character == '▔'))
        .expect("The Input Dialog must have its shared top rule");
    let dialog_rows = height - top;
    assert!(
        dialog_rows <= height.saturating_sub(2).max(1),
        "The dialog must preserve the upper content budget:\n{frame}"
    );
    assert_eq!(lines.len(), height, "{frame}");
    assert!(
        lines[top].chars().all(|character| character == '▔') && !lines[top].is_empty(),
        "The Input Dialog must start with its shared top rule:\n{frame}"
    );
    assert!(
        lines[top + 1].contains(title),
        "{title} must follow the shared top rule:\n{frame}"
    );
    assert!(
        !lines[top..]
            .iter()
            .any(|line| line.contains("────") || line.contains("Subagents")),
        "The dialog must replace the entire composer and its dividers:\n{frame}"
    );
    assert!(
        lines[top..]
            .iter()
            .any(|line| line.trim_start().starts_with('❯')),
        "The dialog must retain the shared selection marker:\n{frame}"
    );
    assert!(
        lines.last().is_some_and(|line| !line.trim().is_empty()),
        "Actions must occupy the last dialog row:\n{frame}"
    );
}

pub fn record_frame(label: &str, frame: &str) {
    let Some(directory) = std::env::var_os("TICKLOOP_MODEL_DIALOG_EVIDENCE_DIR") else {
        return;
    };
    let directory = PathBuf::from(directory);
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join(format!("{label}.txt")), frame).unwrap();
}
