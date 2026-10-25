use std::sync::{Arc, Mutex};

use tickloop_agent_access::AccessError;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(crate) struct DaemonLifecycle {
    shutdown: CancellationToken,
    phase: Arc<Mutex<Phase>>,
}

#[derive(Clone, Copy)]
enum Phase {
    Serving,
    Stopping,
    Handoff,
    Activating,
}

pub(crate) struct StopPermit(CancellationToken);

impl Drop for StopPermit {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl DaemonLifecycle {
    pub(crate) fn new(shutdown: CancellationToken) -> Self {
        Self {
            shutdown,
            phase: Arc::new(Mutex::new(Phase::Serving)),
        }
    }

    pub(crate) fn shutdown(&self) -> &CancellationToken {
        &self.shutdown
    }

    pub(crate) fn reserve_stop(&self) -> Result<StopPermit, AccessError> {
        let mut phase = self.phase.lock().unwrap();
        if matches!(*phase, Phase::Activating) {
            return Err(AccessError::unavailable(
                "This daemon transferred ownership; retry daemon stop against the running successor",
            ));
        }
        *phase = Phase::Stopping;
        Ok(StopPermit(self.shutdown.clone()))
    }

    pub(crate) fn begin_handoff(&self) -> Result<(), String> {
        let mut phase = self.phase.lock().map_err(|error| error.to_string())?;
        if !matches!(*phase, Phase::Serving) || self.shutdown.is_cancelled() {
            return Err("Daemon stop was already requested; activation is cancelled".into());
        }
        *phase = Phase::Handoff;
        Ok(())
    }

    pub(crate) fn begin_activation(&self) -> Result<(), String> {
        let mut phase = self.phase.lock().map_err(|error| error.to_string())?;
        if !matches!(*phase, Phase::Handoff) || self.shutdown.is_cancelled() {
            return Err("Daemon stop was already requested; activation is cancelled".into());
        }
        *phase = Phase::Activating;
        Ok(())
    }

    /// Only a failed durable commit may return the still-authoritative daemon to service.
    pub(crate) fn cancel_handoff(&self) {
        let mut phase = self.phase.lock().unwrap();
        if matches!(*phase, Phase::Handoff) {
            *phase = Phase::Serving;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_stop_prevents_handoff_before_the_reply_is_written() {
        let lifecycle = DaemonLifecycle::new(CancellationToken::new());
        let stop = lifecycle.reserve_stop().unwrap();
        assert!(!lifecycle.shutdown().is_cancelled());
        assert!(lifecycle.begin_handoff().is_err());
        drop(stop);
        assert!(lifecycle.shutdown().is_cancelled());
    }

    #[test]
    fn activated_handoff_refuses_stop_on_the_old_connection_without_acknowledging() {
        let lifecycle = DaemonLifecycle::new(CancellationToken::new());
        lifecycle.begin_handoff().unwrap();
        lifecycle.begin_activation().unwrap();
        assert!(lifecycle.reserve_stop().is_err());
        assert!(!lifecycle.shutdown().is_cancelled());
    }

    #[test]
    fn stop_can_cancel_a_handoff_until_its_successor_is_released() {
        let lifecycle = DaemonLifecycle::new(CancellationToken::new());
        lifecycle.begin_handoff().unwrap();
        let stop = lifecycle.reserve_stop().unwrap();
        assert!(lifecycle.begin_activation().is_err());
        drop(stop);
        assert!(lifecycle.shutdown().is_cancelled());
        let lifecycle = DaemonLifecycle::new(CancellationToken::new());
        lifecycle.begin_handoff().unwrap();
        lifecycle.cancel_handoff();
        drop(lifecycle.reserve_stop().unwrap());
        assert!(lifecycle.shutdown().is_cancelled());
    }

    #[test]
    fn concurrent_stop_and_activation_have_only_one_winner() {
        for _ in 0..100 {
            let lifecycle = DaemonLifecycle::new(CancellationToken::new());
            lifecycle.begin_handoff().unwrap();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let contender = lifecycle.clone();
            let ready = barrier.clone();
            let stop = std::thread::spawn(move || {
                ready.wait();
                contender.reserve_stop().map(drop).is_ok()
            });
            barrier.wait();
            let handed_off = lifecycle.begin_activation().is_ok();
            assert_ne!(handed_off, stop.join().unwrap());
        }
    }
}
