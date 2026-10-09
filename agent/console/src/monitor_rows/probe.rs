//! Probes of Monitor members' host state. Probing reads host state only: it never joins,
//! subscribes, reads Tick content or calls the model.

use super::*;

/// A host whose members' state cannot be read now.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ProbeError(pub String);

/// Reads the host state of a Chat's members of one Tick type.
#[async_trait::async_trait]
pub trait TickProbe: Send + Sync {
    /// The Tick type whose instances this probe reports on.
    fn tick_type(&self) -> &'static str;
    /// The current host state of `chat`'s member `instances`. A host that cannot be read reports
    /// an error, so earlier findings stay instead of being wiped.
    async fn probe(
        &self,
        chat: Uuid,
        instances: &[InstanceRef],
    ) -> Result<MemberFindings, ProbeError>;
}

/// What a host probe found for members no subscription reports on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MemberFindings {
    pub states: HashMap<InstanceRef, MemberState>,
    /// Process samples of the members whose host takes them.
    pub resources: HashMap<InstanceRef, TickResources>,
    /// What each member's host calls it now, for members named by what they run.
    pub titles: HashMap<InstanceRef, String>,
    /// When each protocol member last sent a Tick Update, as its host saw it without anyone
    /// subscribing or reading the update.
    pub updates: HashMap<InstanceRef, u64>,
}

/// Probes the Herdr members of the View's own Herdr session.
pub struct HerdrProbe {
    access: Arc<tickhost_connect::access::TickAccess>,
}

impl HerdrProbe {
    pub fn new(access: Arc<tickhost_connect::access::TickAccess>) -> Self {
        Self { access }
    }

    /// When a protocol Tick in a Pane last sent a Tick Update; reading this reads no content.
    async fn update_time(&self, instance: &InstanceRef) -> Option<u64> {
        let session = self.access.session(instance).await.ok()?;
        tokio::time::timeout(UPDATE_PROBE_TIMEOUT, session.status())
            .await
            .ok()?
            .ok()?
            .updated_at_ms
    }
}

/// How long a probe waits for one Tick's update time before leaving it out.
const UPDATE_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[async_trait::async_trait]
impl TickProbe for HerdrProbe {
    fn tick_type(&self) -> &'static str {
        tick_protocol::HERDR_INSTANCE_TYPE
    }

    async fn probe(
        &self,
        _chat: Uuid,
        instances: &[InstanceRef],
    ) -> Result<MemberFindings, ProbeError> {
        let (located, uncertain) = self
            .access
            .locate_all()
            .await
            .map_err(|error| ProbeError(error.to_string()))?;
        let mut findings = MemberFindings::default();
        for instance in instances {
            let location = located
                .iter()
                .find(|location| &location.instance == instance);
            let state = match location {
                Some(location) => MemberState::Live {
                    protocol: location.protocol,
                },
                None if uncertain > 0 => MemberState::IdentityUncertain,
                None => MemberState::Gone,
            };
            findings.states.insert(instance.clone(), state);
            if let Some(name) = location.and_then(|location| location.name.clone()) {
                findings.titles.insert(instance.clone(), name);
            }
            if location.is_some_and(|location| location.protocol)
                && let Some(updated_at_ms) = self.update_time(instance).await
            {
                findings.updates.insert(instance.clone(), updated_at_ms);
            }
        }
        Ok(findings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tickhost_connect::{
        access::TickAccess,
        herdr::{HerdrContext, HerdrPort},
        host_error::HostError,
    };
    use tokio_util::sync::CancellationToken;

    /// One Pane running `make dev`.
    struct MakePane;

    #[async_trait::async_trait]
    impl HerdrPort for MakePane {
        async fn request(
            &self,
            method: &str,
            _: Value,
            _: CancellationToken,
        ) -> Result<Value, HostError> {
            let me = std::process::id();
            Ok(match method {
                "pane.list" => {
                    json!({"panes":[{"pane_id":"p1","terminal_id":"t1","workspace_id":"w1","tab_id":"tab1","cwd":"/tmp","foreground_cwd":"/tmp"}]})
                }
                "pane.process_info" => {
                    json!({"process_info":{"foreground_process_group_id":me,"shell_pid":1,"foreground_processes":[{"pid":me,"argv":["/usr/bin/make","dev"],"name":"gnumake"}]}})
                }
                method => panic!("unexpected Herdr method {method}"),
            })
        }
    }

    #[tokio::test]
    async fn a_probe_names_live_programs_by_what_they_run() {
        let access = Arc::new(TickAccess::with_host(
            HerdrContext::without_view("/tmp/probe-herdr.sock".into()),
            Arc::new(MakePane),
        ));
        let (located, _) = access.locate_all().await.unwrap();
        let member = located[0].instance.clone();
        let findings = HerdrProbe::new(access)
            .probe(Uuid::nil(), std::slice::from_ref(&member))
            .await
            .unwrap();
        assert_eq!(
            findings.states[&member],
            MemberState::Live { protocol: false }
        );
        assert_eq!(findings.titles[&member], "make dev");
    }
}
