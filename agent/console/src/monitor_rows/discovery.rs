//! Workspace discovery for explicitly adding existing instances to a Chat's Monitor, and probes
//! of members' host state. Listing a Workspace reads protocol Ticks' current state to report it,
//! but neither listing nor probing joins, subscribes or calls the model.

use super::*;
use tickhost_connect::page::{Page, PageCursor};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredTick {
    /// Absent while the host cannot verify the program's identity; such rows cannot be added.
    pub instance: Option<InstanceRef>,
    pub title: String,
    pub kind: String,
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DiscoveryError {
    #[error("the Workspace changed; list it again from the first page")]
    CursorExpired,
    #[error("{0}")]
    Unavailable(String),
}

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
    ) -> Result<MemberFindings, DiscoveryError>;
}

/// Lists existing instances that can be added to a Chat's Monitor, and probes its members.
#[async_trait::async_trait]
pub trait TickDiscovery: TickProbe {
    async fn page(
        &self,
        cursor: Option<PageCursor>,
    ) -> Result<Page<DiscoveredTick>, DiscoveryError>;
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

/// The programs of the View's current Herdr Workspace.
pub struct HerdrDiscovery {
    access: Arc<tickhost_connect::access::TickAccess>,
}

impl HerdrDiscovery {
    pub fn new(access: Arc<tickhost_connect::access::TickAccess>) -> Self {
        Self { access }
    }
}

#[async_trait::async_trait]
impl TickDiscovery for HerdrDiscovery {
    async fn page(
        &self,
        cursor: Option<PageCursor>,
    ) -> Result<Page<DiscoveredTick>, DiscoveryError> {
        let mut input = serde_json::json!({});
        if let Some(cursor) = cursor {
            input["cursor"] = serde_json::json!(cursor);
        }
        let unavailable = |error: tickhost_connect::host_error::HostError| {
            if error.code == tickhost_connect::page::PageError::CursorExpired.code() {
                DiscoveryError::CursorExpired
            } else {
                DiscoveryError::Unavailable(error.to_string())
            }
        };
        let prepared = self
            .access
            .prepare("read_workspace", &input)
            .await
            .map_err(unavailable)?;
        let listing = self
            .access
            .execute(prepared, CancellationToken::new())
            .await
            .map_err(unavailable)?;
        let malformed =
            || DiscoveryError::Unavailable("Herdr returned a malformed Workspace listing".into());
        let items = listing["ticks"]
            .as_array()
            .ok_or_else(malformed)?
            .iter()
            .map(|tick| DiscoveredTick {
                instance: serde_json::from_value(tick["tick_instance_id"].clone()).ok(),
                title: tick["title"]
                    .as_str()
                    .filter(|title| !title.is_empty())
                    .or_else(|| tick["pane_id"].as_str())
                    .unwrap_or_default()
                    .to_owned(),
                kind: match tick["source"].as_str() {
                    Some("protocol") => "Tick",
                    Some("terminal") => "Terminal",
                    _ => "Program",
                }
                .into(),
                state: tick["state"].as_str().unwrap_or_default().replace('_', " "),
            })
            .collect();
        Ok(Page {
            generation: listing["generation"].as_str().ok_or_else(malformed)?.into(),
            total: listing["total"].as_u64().ok_or_else(malformed)? as usize,
            items,
            next: serde_json::from_value(listing["next_cursor"].clone())
                .map_err(|_| malformed())?,
        })
    }
}

impl HerdrDiscovery {
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
impl TickProbe for HerdrDiscovery {
    fn tick_type(&self) -> &'static str {
        tick_protocol::HERDR_INSTANCE_TYPE
    }

    async fn probe(
        &self,
        _chat: Uuid,
        instances: &[InstanceRef],
    ) -> Result<MemberFindings, DiscoveryError> {
        let (located, uncertain) = self
            .access
            .locate_all()
            .await
            .map_err(|error| DiscoveryError::Unavailable(error.to_string()))?;
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
        let findings = HerdrDiscovery::new(access)
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
