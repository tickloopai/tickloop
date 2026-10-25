use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{UpdateActivity, UpdateState};

const WRITE_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateStage {
    Downloading,
    Verifying,
    #[default]
    Installing,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct UpdateProgress {
    pub stage: UpdateStage,
    pub percent: Option<u8>,
    pub downloaded_bytes: Option<u64>,
}

impl UpdateProgress {
    pub(crate) fn download(downloaded: u64, total: Option<u64>) -> Self {
        Self {
            stage: UpdateStage::Downloading,
            percent: total
                .filter(|total| *total > 0)
                .map(|total| ((u128::from(downloaded) * 100 / u128::from(total)).min(100)) as u8),
            downloaded_bytes: Some(downloaded),
        }
    }
}

pub(crate) struct ProgressReporter<'a> {
    state: &'a mut UpdateState,
    path: &'a Path,
    last_write: Option<Instant>,
}

impl<'a> ProgressReporter<'a> {
    pub(crate) fn new(state: &'a mut UpdateState, path: &'a Path) -> Self {
        Self {
            state,
            path,
            last_write: None,
        }
    }

    pub(crate) fn report(&mut self, progress: UpdateProgress) -> Result<()> {
        if self.state.progress == Some(progress) {
            return Ok(());
        }
        let stage_changed = self
            .state
            .progress
            .is_none_or(|last| last.stage != progress.stage);
        if !stage_changed
            && progress.percent != Some(100)
            && self
                .last_write
                .is_some_and(|last| last.elapsed() < WRITE_INTERVAL)
        {
            return Ok(());
        }
        self.state.activity = Some(UpdateActivity::Installing);
        self.state.progress = Some(progress);
        self.state.write(self.path)?;
        self.last_write = Some(Instant::now());
        Ok(())
    }

    pub(crate) fn stage(&mut self, stage: UpdateStage) -> Result<()> {
        self.report(UpdateProgress {
            stage,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentage_handles_unknown_zero_and_large_sizes() {
        assert_eq!(UpdateProgress::download(58, Some(100)).percent, Some(58));
        assert_eq!(UpdateProgress::download(0, Some(0)).percent, None);
        assert_eq!(UpdateProgress::download(58, None).percent, None);
        assert_eq!(
            UpdateProgress::download(u64::MAX, Some(u64::MAX)).percent,
            Some(100)
        );
    }

    #[test]
    fn reports_are_throttled_but_completion_and_stage_changes_are_immediate() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("status.json");
        let mut state = UpdateState::default();
        let mut reporter = ProgressReporter::new(&mut state, &path);
        reporter
            .report(UpdateProgress::download(0, Some(100)))
            .unwrap();
        reporter
            .report(UpdateProgress::download(50, Some(100)))
            .unwrap();
        let saved: UpdateState = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.progress.unwrap().percent, Some(0));
        reporter
            .report(UpdateProgress::download(100, Some(100)))
            .unwrap();
        let saved: UpdateState = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.progress.unwrap().percent, Some(100));
        reporter.stage(UpdateStage::Verifying).unwrap();
        let saved: UpdateState = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            saved.progress.unwrap(),
            UpdateProgress {
                stage: UpdateStage::Verifying,
                ..Default::default()
            }
        );
    }
}
