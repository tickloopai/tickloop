//! Workspace discovery for explicitly adding existing instances to a Chat's Monitor, and probes
//! of members' host state. Listing a Workspace reads protocol Ticks' current state to report it,
//! but neither listing nor probing joins, subscribes or calls the model.

use super::*;
use tick_connect::page::{Page, PageCursor};
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

#[async_trait::async_trait]
pub trait TickDiscovery: Send + Sync {
    /// The Tick type whose instances this discovery lists and probes.
    fn tick_type(&self) -> &'static str;
    async fn page(
        &self,
        cursor: Option<PageCursor>,
    ) -> Result<Page<DiscoveredTick>, DiscoveryError>;
    /// The current host state of `instances`. A host that cannot be read reports an error, so
    /// earlier findings stay instead of being wiped.
    async fn probe(
        &self,
        instances: &[InstanceRef],
    ) -> Result<HashMap<InstanceRef, MemberState>, DiscoveryError>;
}

/// The programs of the View's current Herdr Workspace.
pub struct HerdrDiscovery {
    access: Arc<tick_connect::access::TickAccess>,
}

impl HerdrDiscovery {
    pub fn new(access: Arc<tick_connect::access::TickAccess>) -> Self {
        Self { access }
    }
}

#[async_trait::async_trait]
impl TickDiscovery for HerdrDiscovery {
    fn tick_type(&self) -> &'static str {
        "herdr"
    }

    async fn page(
        &self,
        cursor: Option<PageCursor>,
    ) -> Result<Page<DiscoveredTick>, DiscoveryError> {
        let mut input = serde_json::json!({});
        if let Some(cursor) = cursor {
            input["cursor"] = serde_json::json!(cursor);
        }
        let unavailable = |error: tick_connect::host_error::HostError| {
            if error.code == tick_connect::page::PageError::CursorExpired.code() {
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

    async fn probe(
        &self,
        instances: &[InstanceRef],
    ) -> Result<HashMap<InstanceRef, MemberState>, DiscoveryError> {
        let (located, uncertain) = self
            .access
            .locate_all()
            .await
            .map_err(|error| DiscoveryError::Unavailable(error.to_string()))?;
        Ok(instances
            .iter()
            .map(|instance| {
                let state = match located
                    .iter()
                    .find(|location| &location.instance == instance)
                {
                    Some(location) => MemberState::Live {
                        protocol: location.protocol,
                    },
                    None if uncertain > 0 => MemberState::IdentityUncertain,
                    None => MemberState::Gone,
                };
                (instance.clone(), state)
            })
            .collect())
    }
}
