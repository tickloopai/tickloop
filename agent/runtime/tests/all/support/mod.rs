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
