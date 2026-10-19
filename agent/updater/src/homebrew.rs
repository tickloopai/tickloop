use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::process::Stdio;

use anyhow::Result;
use tokio::io::unix::AsyncFd;

use crate::{ProgressReporter, UpdateProgress, UpdateStage, run_process_with_output};

const TERMINAL_COLUMNS: u16 = 160;
const TERMINAL_ROWS: u16 = 24;
const MAX_RECORD_BYTES: usize = 8192;

pub(crate) async fn run(
    command: &mut tokio::process::Command,
    progress: &mut ProgressReporter<'_>,
) -> Result<()> {
    let (master, slave) = terminal()?;
    command
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    let master = AsyncFd::new(master)?;
    run_process_with_output(command, async {
        let mut parser = OutputParser::default();
        let mut buffer = [0; 4096];
        loop {
            let mut ready = master.readable().await?;
            let count = match ready.try_io(|fd| fd.get_ref().read(&mut buffer)) {
                Ok(Ok(count)) => count,
                Ok(Err(error)) if error.raw_os_error() == Some(libc::EIO) => break,
                Ok(Err(error)) => return Err(error.into()),
                Err(_) => continue,
            };
            if count == 0 {
                break;
            }
            io::stdout().write_all(&buffer[..count])?;
            parser.feed(&buffer[..count], progress)?;
        }
        parser.finish(progress)
    })
    .await
}

fn terminal() -> io::Result<(File, File)> {
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: TERMINAL_ROWS,
        ws_col: TERMINAL_COLUMNS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    for file in [&master, &slave] {
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    if unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((master, slave))
}

#[derive(Default)]
struct OutputParser {
    record: Vec<u8>,
    escape: bool,
    csi: bool,
    overflow: bool,
}

impl OutputParser {
    fn feed(&mut self, bytes: &[u8], progress: &mut ProgressReporter<'_>) -> Result<()> {
        for &byte in bytes {
            if self.escape {
                if !self.csi && byte == b'[' {
                    self.csi = true;
                } else if !self.csi || (0x40..=0x7e).contains(&byte) {
                    self.escape = false;
                    self.csi = false;
                    if byte != b'm' {
                        self.finish(progress)?;
                    }
                }
            } else if byte == 0x1b {
                self.escape = true;
            } else if matches!(byte, b'\r' | b'\n') {
                self.finish(progress)?;
            } else if self.record.len() < MAX_RECORD_BYTES {
                self.record.push(byte);
            } else {
                self.overflow = true;
            }
        }
        Ok(())
    }

    fn finish(&mut self, progress: &mut ProgressReporter<'_>) -> Result<()> {
        let update = (!self.overflow)
            .then(|| parse_record(&String::from_utf8_lossy(&self.record)))
            .flatten();
        self.record.clear();
        self.overflow = false;
        if let Some(update) = update {
            progress.report(update)?;
        }
        Ok(())
    }
}

fn parse_record(record: &str) -> Option<UpdateProgress> {
    let text = record.trim();
    // Older Homebrew versions forward curl's percentage bar; newer versions print size pairs.
    if let Some(number) = text.strip_suffix('%') {
        let number = number.trim_start_matches(['#', ' ']);
        if let Ok(percent) = number.parse::<f64>()
            && percent.is_finite()
            && (0.0..=100.0).contains(&percent)
        {
            return Some(UpdateProgress {
                stage: UpdateStage::Downloading,
                percent: Some(percent as u8),
                downloaded_bytes: None,
            });
        }
    }
    for (phase, stage) in [
        ("Downloading", UpdateStage::Downloading),
        ("Downloaded", UpdateStage::Downloading),
        ("Verifying", UpdateStage::Verifying),
    ] {
        if let Some((_, sizes)) = text.rsplit_once(phase) {
            let sizes = sizes.trim().trim_end_matches(']');
            if let Some((fetched, total)) = sizes.split_once('/') {
                let downloaded = parse_size(fetched)?;
                let mut update = UpdateProgress::download(downloaded, parse_size(total));
                update.stage = stage;
                if stage != UpdateStage::Downloading {
                    update.percent = None;
                }
                return Some(update);
            }
            if text.starts_with("==> Downloading ") {
                return Some(UpdateProgress {
                    stage,
                    ..Default::default()
                });
            }
        }
    }
    if text.starts_with("==> Pouring ")
        || text.starts_with("==> Installing ")
        || text.starts_with("==> Upgrading ")
        || text.starts_with("==> Moving ")
    {
        return Some(UpdateProgress::default());
    }
    None
}

fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let number_end = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let value: f64 = text[..number_end].parse().ok()?;
    let multiplier = match text[number_end..].trim() {
        "B" => 1.0,
        "KB" => 1000.0,
        "MB" => 1000.0 * 1000.0,
        "GB" => 1000.0 * 1000.0 * 1000.0,
        _ => return None,
    };
    (value.is_finite() && value >= 0.0).then_some((value * multiplier) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UpdateState;

    #[tokio::test]
    async fn cancelling_terminal_output_stops_the_installers_descendants() {
        let root = tempfile::tempdir().unwrap();
        let started = root.path().join("started");
        let finished = root.path().join("finished");
        let task_started = started.clone();
        let task_finished = finished.clone();
        let path = root.path().join("status.json");
        let task = tokio::spawn(async move {
            let mut state = UpdateState::default();
            let mut reporter = ProgressReporter::new(&mut state, &path);
            let mut command = tokio::process::Command::new("/bin/sh");
            command
                .args([
                    "-c",
                    "printf started > \"$1\"; (sleep 1; printf leaked > \"$2\") & wait",
                    "installer",
                ])
                .arg(task_started)
                .arg(task_finished);
            run(&mut command, &mut reporter).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !started.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!finished.exists());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        assert!(
            !finished.exists(),
            "a cancelled Homebrew process left a descendant running"
        );
    }

    #[test]
    fn reads_curl_bars_and_homebrew_size_pairs_without_treating_other_numbers_as_progress() {
        for (text, expected) in [
            ("################# 58.7%", Some(58)),
            (
                "⠋ Formula tickloop (1.2.3) ### Downloading 29.0MB/ 50.0MB",
                Some(58),
            ),
            (
                "✔︎ Formula tickloop (1.2.3) Downloaded 50.0MB/ 50.0MB",
                Some(100),
            ),
            ("⠋ Formula tickloop Downloading 500.0KB/ 1.0MB", Some(50)),
            ("⠋ Formula tickloop Downloading 29.0MB/-------", None),
        ] {
            let progress = parse_record(text).unwrap();
            assert_eq!(progress.stage, UpdateStage::Downloading, "{text}");
            assert_eq!(progress.percent, expected, "{text}");
        }
        for text in [
            "100% of tests passed",
            "invalid 58.0%",
            "101.0%",
            "NaN%",
            "1 package upgraded",
        ] {
            assert_eq!(parse_record(text), None, "{text}");
        }
        assert_eq!(
            parse_record("==> Pouring tickloop--1.2.3.tar.gz")
                .unwrap()
                .stage,
            UpdateStage::Installing
        );
    }

    #[test]
    fn parser_handles_fragmented_terminal_output_and_bounds_long_records() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("status.json");
        let mut state = UpdateState::default();
        let mut reporter = ProgressReporter::new(&mut state, &path);
        let mut parser = OutputParser::default();
        for part in [b"\x1b[3".as_slice(), b"2m### 58.", b"0%\x1b[0m\r"] {
            parser.feed(part, &mut reporter).unwrap();
        }
        let saved: UpdateState = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.progress.unwrap().percent, Some(58));
        parser
            .feed(&vec![b'x'; MAX_RECORD_BYTES + 1], &mut reporter)
            .unwrap();
        parser
            .feed(b"\r\x1b[2K==> Pouring tickloop\r", &mut reporter)
            .unwrap();
        let saved: UpdateState = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.progress.unwrap().stage, UpdateStage::Installing);
    }

    #[tokio::test]
    async fn installer_has_terminal_output_without_keyboard_input_and_progress_is_visible_while_running()
     {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("status.json");
        let mut state = UpdateState::default();
        let mut reporter = ProgressReporter::new(&mut state, &path);
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", "test -t 1 && test -t 2 && ! test -t 0 || exit 42; printf '\\r######## 58.0%%\\r'; sleep 0.5; printf '==> Pouring tickloop\\n'"]);
        let observe = async {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if std::fs::read(&path)
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<UpdateState>(&bytes).ok())
                        .and_then(|state| state.progress)
                        .is_some_and(|progress| progress.percent == Some(58))
                    {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        };
        let (result, ()) = tokio::join!(run(&mut command, &mut reporter), observe);
        result.unwrap();
        assert_eq!(state.progress.unwrap().stage, UpdateStage::Installing);
        let mut failing = tokio::process::Command::new("/bin/sh");
        failing.args(["-c", "exit 7"]);
        assert!(
            run(&mut failing, &mut ProgressReporter::new(&mut state, &path))
                .await
                .unwrap_err()
                .to_string()
                .contains("7")
        );
    }
}
