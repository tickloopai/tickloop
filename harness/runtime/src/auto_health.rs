use std::time::Duration;

use tokio::time::Instant;

use tickloop_harness_model::judgment::JevConnectionVersion;
use tickloop_harness_protocol::AutoReviewerHealth;

const COOLDOWN: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
pub(super) struct AttemptPermit {
    generation: u64,
    probe: bool,
}

#[derive(Default)]
pub(super) struct AutoHealth {
    connection: Option<Option<JevConnectionVersion>>,
    generation: u64,
    cooldown_until: Option<Instant>,
    probe_in_flight: bool,
}

impl AutoHealth {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.connection = None;
        self.cooldown_until = None;
        self.probe_in_flight = false;
    }

    pub fn begin(
        &mut self,
        connection: Option<JevConnectionVersion>,
        now: Instant,
    ) -> Option<AttemptPermit> {
        if self.connection != Some(connection) {
            self.reset();
            self.connection = Some(connection);
        }
        match self.cooldown_until {
            Some(until) if until > now => None,
            Some(_) if self.probe_in_flight => None,
            Some(_) => {
                self.probe_in_flight = true;
                Some(AttemptPermit {
                    generation: self.generation,
                    probe: true,
                })
            }
            None => Some(AttemptPermit {
                generation: self.generation,
                probe: false,
            }),
        }
    }

    pub fn begin_bound(
        &mut self,
        connection: Option<JevConnectionVersion>,
        generation_before_binding: u64,
        now: Instant,
    ) -> Option<AttemptPermit> {
        if self.generation != generation_before_binding && self.connection != Some(connection) {
            return None;
        }
        self.begin(connection, now)
    }

    pub fn failure(&mut self, permit: AttemptPermit, now: Instant) {
        if permit.generation != self.generation {
            return;
        }
        self.cooldown_until = Some(now + COOLDOWN);
        self.probe_in_flight = false;
    }

    pub fn success(&mut self, permit: AttemptPermit) {
        if permit.generation != self.generation {
            return;
        }
        self.cooldown_until = None;
        self.probe_in_flight = false;
    }

    pub fn cancel(&mut self, permit: AttemptPermit) {
        if permit.generation == self.generation && permit.probe {
            self.probe_in_flight = false;
        }
    }

    pub fn status(&self, now: Instant) -> AutoReviewerHealth {
        if self.probe_in_flight {
            AutoReviewerHealth::Probing
        } else if let Some(until) = self.cooldown_until.filter(|until| *until > now) {
            AutoReviewerHealth::CoolingDown {
                remaining_seconds: until.duration_since(now).as_secs().max(1),
            }
        } else {
            AutoReviewerHealth::Ready
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn failed_connection_cools_and_only_one_probe_runs_after_expiry() {
        let mut health = AutoHealth::default();
        let now = Instant::now();
        let first = health.begin(None, now).unwrap();
        health.failure(first, now);
        assert!(matches!(
            health.status(now),
            AutoReviewerHealth::CoolingDown { .. }
        ));
        assert!(health.begin(None, now).is_none());
        tokio::time::advance(COOLDOWN).await;
        let probe = health.begin(None, Instant::now()).unwrap();
        assert!(health.begin(None, Instant::now()).is_none());
        health.success(probe);
        assert_eq!(health.status(Instant::now()), AutoReviewerHealth::Ready);
    }

    #[tokio::test(start_paused = true)]
    async fn reset_prevents_old_failure_or_recovery_from_mutating_new_connection() {
        let mut health = AutoHealth::default();
        let old = health.begin(None, Instant::now()).unwrap();
        health.reset();
        let new = health.begin(None, Instant::now()).unwrap();
        health.failure(old, Instant::now());
        health.success(old);
        assert_eq!(health.status(Instant::now()), AutoReviewerHealth::Ready);
        health.failure(new, Instant::now());
        assert!(matches!(
            health.status(Instant::now()),
            AutoReviewerHealth::CoolingDown { .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_probe_preserves_cooldown_without_restart() {
        let mut health = AutoHealth::default();
        let first = health.begin(None, Instant::now()).unwrap();
        health.failure(first, Instant::now());
        tokio::time::advance(COOLDOWN).await;
        let probe = health.begin(None, Instant::now()).unwrap();
        health.cancel(probe);
        assert!(health.begin(None, Instant::now()).is_some());
    }
}
