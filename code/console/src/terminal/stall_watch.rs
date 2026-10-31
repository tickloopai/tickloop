//! Records Console work that stays unfinished, so a View that stops responding leaves the
//! handler, feed step and AgentAccess request it waits on in `logs/console/` of the
//! configuration root. Nothing is written until a phase outlasts the threshold.

use std::{
    borrow::Cow,
    collections::HashMap,
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use tickloop_code_access::{
    AccessError, AgentAccess, CommandEnvelope, CommandReceipt, DisplayReceipt, DisplayUpdate,
    Observation, ObservationRequest,
};
use tickloop_harness::base::private_fs::{PrivateDirectory, unix_time_milliseconds};

static WATCH: OnceLock<StallWatch> = OnceLock::new();
/// Earlier Views' logs kept when a View starts watching; older ones are removed.
const KEPT_LOG_FILES: usize = 20;
const LOG_PREFIX: &str = "console-";
const LOG_EXTENSION: &str = ".log";

struct StallWatch {
    directory: PathBuf,
    next_phase: AtomicU64,
    phases: Mutex<HashMap<u64, Phase>>,
    log: Mutex<Option<File>>,
}

struct Phase {
    label: Cow<'static, str>,
    started: Instant,
    reported: bool,
}

/// An unfinished phase; dropping it ends the phase.
#[must_use]
pub(super) struct PhaseGuard(Option<(&'static StallWatch, u64)>);

impl Drop for PhaseGuard {
    fn drop(&mut self) {
        if let Some((watch, id)) = self.0 {
            watch.leave(id);
        }
    }
}

/// Starts watching for stalls, logging under `config_root`; only the first call takes effect.
/// Unit tests never install it, so their Views log nothing into temporary roots.
#[cfg_attr(test, allow(dead_code))]
pub(super) fn install(config_root: &Path) {
    const STALL_THRESHOLD: Duration = Duration::from_secs(5);
    const CHECK_INTERVAL: Duration = Duration::from_secs(1);
    let directory = PrivateDirectory::trusted(config_root.to_owned())
        .join("logs")
        .join("console");
    if WATCH
        .set(StallWatch::new(directory.path().to_owned()))
        .is_ok()
    {
        let _ = std::thread::Builder::new()
            .name("console-stall-watch".into())
            .spawn(|| {
                if let Some(watch) = WATCH.get() {
                    watch.remove_old_logs(KEPT_LOG_FILES);
                }
                loop {
                    std::thread::sleep(CHECK_INTERVAL);
                    if let Some(watch) = WATCH.get() {
                        watch.report_stalls(STALL_THRESHOLD);
                    }
                }
            });
    }
}

/// Marks `label` as in progress until the returned guard drops; free when not installed.
pub(super) fn enter(label: impl Into<Cow<'static, str>>) -> PhaseGuard {
    WATCH
        .get()
        .map_or(PhaseGuard(None), |watch| watch.enter(label.into()))
}

/// [`enter`] for a label that must be formatted, which happens only while installed.
pub(super) fn enter_with(label: impl FnOnce() -> String) -> PhaseGuard {
    WATCH
        .get()
        .map_or(PhaseGuard(None), |watch| watch.enter(label().into()))
}

/// The leading identifier of `value`'s Debug form, such as `Submit` for a command; formatting
/// stops there, so large payloads are never rendered.
pub(super) fn variant_name(value: &dyn std::fmt::Debug) -> String {
    struct Leading(String);
    impl std::fmt::Write for Leading {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            let end = text
                .find(|character: char| !(character.is_alphanumeric() || character == '_'))
                .unwrap_or(text.len());
            self.0.push_str(&text[..end]);
            if end < text.len() {
                Err(std::fmt::Error)
            } else {
                Ok(())
            }
        }
    }
    let mut leading = Leading(String::new());
    let _ = std::fmt::write(&mut leading, format_args!("{value:?}"));
    leading.0
}

impl StallWatch {
    fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            next_phase: AtomicU64::new(1),
            phases: Mutex::new(HashMap::new()),
            log: Mutex::new(None),
        }
    }

    fn enter(&'static self, label: Cow<'static, str>) -> PhaseGuard {
        let id = self.next_phase.fetch_add(1, Ordering::Relaxed);
        self.phases.lock().unwrap().insert(
            id,
            Phase {
                label,
                started: Instant::now(),
                reported: false,
            },
        );
        PhaseGuard(Some((self, id)))
    }

    fn leave(&self, id: u64) {
        let Some(phase) = self.phases.lock().unwrap().remove(&id) else {
            return;
        };
        if phase.reported {
            self.write(&format!(
                "resumed after {:.1}s: {}",
                phase.started.elapsed().as_secs_f64(),
                phase.label
            ));
        }
    }

    fn report_stalls(&self, threshold: Duration) {
        let stalled = {
            let mut phases = self.phases.lock().unwrap();
            phases
                .values_mut()
                .filter(|phase| !phase.reported && phase.started.elapsed() >= threshold)
                .map(|phase| {
                    phase.reported = true;
                    format!(
                        "stalled {:.1}s: {}",
                        phase.started.elapsed().as_secs_f64(),
                        phase.label
                    )
                })
                .collect::<Vec<_>>()
        };
        for line in stalled {
            self.write(&line);
        }
    }

    fn write(&self, line: &str) {
        let mut log = self.log.lock().unwrap();
        if log.is_none() {
            *log = self.open().ok();
        }
        if let Some(file) = log.as_mut() {
            let now = time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default();
            let _ = writeln!(file, "{now} {line}");
        }
    }

    /// Removes all but the newest `kept` logs earlier Views left; links and other files stay.
    fn remove_old_logs(&self, kept: usize) {
        let Ok(entries) = std::fs::read_dir(&self.directory) else {
            return;
        };
        let mut logs = entries
            .flatten()
            .filter(|entry| {
                entry.file_name().to_str().is_some_and(|name| {
                    name.starts_with(LOG_PREFIX) && name.ends_with(LOG_EXTENSION)
                })
            })
            .filter_map(|entry| {
                let metadata = entry.metadata().ok()?;
                metadata
                    .file_type()
                    .is_file()
                    .then(|| Some((metadata.modified().ok()?, entry.path())))
                    .flatten()
            })
            .collect::<Vec<_>>();
        logs.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
        for (_, path) in logs.into_iter().skip(kept) {
            let _ = std::fs::remove_file(path);
        }
    }

    fn open(&self) -> std::io::Result<File> {
        PrivateDirectory::trusted(self.directory.clone()).create()?;
        OpenOptions::new()
            .append(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.directory.join(format!(
                "{LOG_PREFIX}{}-{}{LOG_EXTENSION}",
                std::process::id(),
                unix_time_milliseconds()
            )))
    }
}

/// AgentAccess that marks each request as a phase, so a reply that never comes is logged with
/// the request it answers.
pub(super) struct WatchedAccess(pub(super) std::sync::Arc<dyn AgentAccess>);

#[async_trait::async_trait]
impl AgentAccess for WatchedAccess {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        let _phase = enter_with(|| match &query {
            ObservationRequest::Agent { detail, .. } => {
                format!("AgentAccess observe Agent {}", variant_name(detail))
            }
            query => format!("AgentAccess observe {}", variant_name(query)),
        });
        self.0.observe(query).await
    }

    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        let _phase =
            enter_with(|| format!("AgentAccess execute {}", variant_name(&command.command)));
        self.0.execute(command).await
    }

    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        let _phase = enter_with(|| format!("AgentAccess show_view {}", variant_name(&update)));
        self.0.show_view(update).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_lines(directory: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return Vec::new();
        };
        entries
            .flatten()
            .flat_map(|entry| {
                std::fs::read_to_string(entry.path())
                    .unwrap()
                    .lines()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn a_phase_past_the_threshold_is_logged_once_and_again_when_it_ends() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("logs").join("console");
        let watch: &'static StallWatch = Box::leak(Box::new(StallWatch::new(directory.clone())));

        let quick = watch.enter("main loop: input".into());
        drop(quick);
        let stalled = watch.enter("AgentAccess observe Agent Events".into());
        watch.report_stalls(Duration::ZERO);
        watch.report_stalls(Duration::ZERO);
        drop(stalled);

        let lines = log_lines(&directory);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].contains("stalled ")
                && lines[0].ends_with(": AgentAccess observe Agent Events")
        );
        assert!(
            lines[1].contains("resumed after ")
                && lines[1].ends_with(": AgentAccess observe Agent Events")
        );
    }

    #[test]
    fn nothing_is_written_while_no_phase_stalls() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("logs").join("console");
        let watch: &'static StallWatch = Box::leak(Box::new(StallWatch::new(directory.clone())));

        let _phase = watch.enter("main loop: animation".into());
        watch.report_stalls(Duration::from_secs(3600));

        assert!(!directory.exists());
    }

    #[test]
    fn starting_keeps_only_the_newest_logs_of_earlier_views() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("logs").join("console");
        std::fs::create_dir_all(&directory).unwrap();
        let epoch = std::time::SystemTime::UNIX_EPOCH;
        for index in 0..4u64 {
            let file = File::create(directory.join(format!("console-{index}-0.log"))).unwrap();
            file.set_modified(epoch + Duration::from_secs(index + 1))
                .unwrap();
        }
        std::fs::write(directory.join("notes.txt"), "kept").unwrap();
        let watch: &'static StallWatch = Box::leak(Box::new(StallWatch::new(directory.clone())));

        watch.remove_old_logs(2);

        let mut names = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, ["console-2-0.log", "console-3-0.log", "notes.txt"]);
    }

    #[test]
    fn variant_names_stop_before_the_payload() {
        #[derive(Debug)]
        #[allow(dead_code)]
        enum Command {
            Submit { text: String },
            Stop,
        }
        assert_eq!(
            variant_name(&Command::Submit {
                text: "x".repeat(1 << 20)
            }),
            "Submit"
        );
        assert_eq!(variant_name(&Command::Stop), "Stop");
    }
}
