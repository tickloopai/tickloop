pub mod provider_transport;
pub mod tick_admission_contract;

pub fn model_script_requests(
    snapshot: &tickloop_agent_protocol::ChatSnapshot,
) -> Vec<&tickloop_agent_protocol::RequestUsageRecord> {
    snapshot
        .request_usage
        .entries()
        .iter()
        .filter(|entry| entry.kind != tickloop_agent_protocol::ModelRequestKind::Title)
        .collect()
}

/// How long a real-time test waits for a `high` Tick Subscription to deliver: the first delivery
/// comes one period after subscribing, plus a margin for a loaded machine.
pub fn high_delivery_wait() -> std::time::Duration {
    tickloop_agent_protocol::TickSubscriptionFrequency::High
        .period()
        .expect("high is periodic")
        + std::time::Duration::from_secs(15)
}
