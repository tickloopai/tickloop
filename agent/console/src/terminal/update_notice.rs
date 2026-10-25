use tickloop_agent_updater::{UpdateNotice, UpdateStage};
use unicode_width::UnicodeWidthStr;

use super::console::display_single_line;
use super::display::DisplayProfile;
use super::style::{Role, RoleLine};

const BAR_COLUMNS: usize = 10;
const COMPACT_BAR_COLUMNS: usize = 4;
const MOVING_COLUMNS: usize = 3;
const FRAME_INTERVAL_MS: u64 = 150;
const BYTES_PER_MEGABYTE: f64 = 1024.0 * 1024.0;

pub(super) fn next_frame_ms(
    notice: Option<&UpdateNotice>,
    display: DisplayProfile,
    now_ms: u64,
) -> Option<u64> {
    let moving = match notice? {
        UpdateNotice::Checking => true,
        UpdateNotice::Installing { progress } => progress.percent.is_none(),
        _ => false,
    };
    (moving && !display.preferences().prefers_reduced_motion)
        .then_some(now_ms + FRAME_INTERVAL_MS - now_ms % FRAME_INTERVAL_MS)
}

pub(super) fn line(
    notice: &UpdateNotice,
    width: usize,
    display: DisplayProfile,
    now_ms: u64,
) -> RoleLine {
    let reduced_motion = display.preferences().prefers_reduced_motion;
    let (messages, role) = match notice {
        UpdateNotice::Checking => (
            progress_messages(
                "Checking for updates…",
                "Checking…",
                None,
                None,
                now_ms,
                reduced_motion,
            ),
            Role::MutedText,
        ),
        UpdateNotice::Installing { progress } => {
            let stage = match progress.stage {
                UpdateStage::Downloading => "Downloading",
                UpdateStage::Verifying => "Verifying",
                UpdateStage::Installing => "Installing",
            };
            (
                progress_messages(
                    &format!("Updating Tickloop · {stage}"),
                    stage,
                    progress.percent,
                    progress.downloaded_bytes,
                    now_ms,
                    reduced_motion,
                ),
                Role::MutedText,
            )
        }
        UpdateNotice::Ready { version } => (
            vec![
                format!(
                    "Installed {} · switching automatically",
                    display_single_line(version)
                ),
                "Update ready".into(),
            ],
            Role::Success,
        ),
        UpdateNotice::PendingAgent => (
            vec!["Agent update pending".into(), "Update pending".into()],
            Role::MutedText,
        ),
        UpdateNotice::Deferred { reason } => (
            vec![
                format!("Update pending · {}", display_single_line(reason)),
                "Update pending".into(),
            ],
            Role::MutedText,
        ),
        UpdateNotice::Failed => (
            vec![
                "Update failed · tickloop update".into(),
                "Update failed".into(),
            ],
            Role::Warning,
        ),
    };
    messages
        .into_iter()
        .find(|text| UnicodeWidthStr::width(text.as_str()) <= width)
        .map_or_else(RoleLine::default, |text| RoleLine::role(text, role))
}

fn progress_messages(
    full: &str,
    compact: &str,
    percent: Option<u8>,
    downloaded: Option<u64>,
    now_ms: u64,
    reduced_motion: bool,
) -> Vec<String> {
    let detail = if let Some(percent) = percent {
        format!(" {}%", percent.min(100))
    } else if let Some(bytes) = downloaded {
        format!(" {:.1} MB", bytes as f64 / BYTES_PER_MEGABYTE)
    } else {
        String::new()
    };
    let bar = progress_bar(BAR_COLUMNS, percent, now_ms, reduced_motion);
    let short_bar = progress_bar(COMPACT_BAR_COLUMNS, percent, now_ms, reduced_motion);
    vec![
        format!("{full} {bar}{detail}"),
        format!("{compact} {short_bar}{detail}"),
        format!("Update {short_bar}{detail}"),
        format!("{short_bar}{detail}"),
        short_bar,
    ]
}

fn progress_bar(columns: usize, percent: Option<u8>, now_ms: u64, reduced_motion: bool) -> String {
    let filled = percent.map(|percent| columns * usize::from(percent.min(100)) / 100);
    let travel = columns.saturating_sub(MOVING_COLUMNS);
    let step = if reduced_motion {
        0
    } else {
        (now_ms / FRAME_INTERVAL_MS) as usize % (travel * 2).max(1)
    };
    let start = if step <= travel {
        step
    } else {
        travel * 2 - step
    };
    let cells: String = (0..columns)
        .map(|index| {
            let active = filled
                .map_or((start..start + MOVING_COLUMNS).contains(&index), |filled| {
                    index < filled
                });
            if active { '━' } else { '─' }
        })
        .collect();
    format!("[{cells}]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_agent_settings::DisplayPreferences;
    use tickloop_agent_updater::UpdateProgress;

    #[test]
    fn a_pending_agent_does_not_animate_an_update_in_progress() {
        let display = DisplayProfile::default();
        let notice = UpdateNotice::PendingAgent;
        let first = line(&notice, 120, display, 0).text();
        assert_eq!(first, "Agent update pending");
        assert_eq!(first, line(&notice, 120, display, FRAME_INTERVAL_MS).text());
        assert_eq!(next_frame_ms(Some(&notice), display, 0), None);
    }

    #[test]
    fn downloading_renders_a_real_bar_and_narrow_layouts_preserve_the_percentage() {
        let notice = UpdateNotice::Installing {
            progress: UpdateProgress {
                stage: UpdateStage::Downloading,
                percent: Some(58),
                downloaded_bytes: Some(58_000_000),
            },
        };
        let display = DisplayProfile::default();
        let wide = line(&notice, 120, display, 0).text();
        assert!(wide.contains("Downloading [━━━━━─────] 58%"), "{wide}");
        for width in [10, 20, 30] {
            let text = line(&notice, width, display, 0).text();
            assert!(text.contains("58%"), "{width}: {text}");
            assert!(text.contains('['), "{width}: {text}");
            assert!(UnicodeWidthStr::width(text.as_str()) <= width);
        }
        assert_eq!(next_frame_ms(Some(&notice), display, 0), None);
    }

    #[test]
    fn unknown_download_size_and_installation_animate_and_respect_reduced_motion() {
        let display = DisplayProfile::default();
        for notice in [
            UpdateNotice::Checking,
            UpdateNotice::Installing {
                progress: UpdateProgress::default(),
            },
            UpdateNotice::Installing {
                progress: UpdateProgress {
                    stage: UpdateStage::Downloading,
                    percent: None,
                    downloaded_bytes: Some(1_048_576),
                },
            },
        ] {
            let first = line(&notice, 120, display, 0).text();
            let second = line(&notice, 120, display, FRAME_INTERVAL_MS).text();
            assert_ne!(first, second);
            assert!(!first.contains('%'));
            assert_eq!(
                next_frame_ms(Some(&notice), display, 0),
                Some(FRAME_INTERVAL_MS)
            );
            let reduced = DisplayProfile::new(DisplayPreferences {
                prefers_reduced_motion: true,
            });
            assert_eq!(
                line(&notice, 120, reduced, 0).text(),
                line(&notice, 120, reduced, FRAME_INTERVAL_MS).text()
            );
            assert_eq!(next_frame_ms(Some(&notice), reduced, 0), None);
        }
    }
}
