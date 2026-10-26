use tickloop_harness::protocol::{EventSourceBinding, LoopId, LoopState};
use tickloop_harness::transcript::Line;
use uuid::Uuid;
pub fn linked(lines: Vec<Line>, loop_id: LoopId) -> Vec<Line> {
    linked_at(lines, loop_id, "https://tickloop.ai/artifact")
}

fn linked_at(lines: Vec<Line>, loop_id: LoopId, origin: &str) -> Vec<Line> {
    let binding = EventSourceBinding {
        origin: origin.into(),
        account_id: "owner".into(),
        artifact_id: "doc".into(),
        binding_id: Uuid::new_v4(),
        client_instance_id: Uuid::new_v4(),
        epoch: 1,
        resume_required: false,
        last_request_at_ms: None,
    };
    let last = lines
        .iter()
        .rposition(|line| matches!(line, Line::System(entry) if entry.loop_record.as_ref().is_some_and(|record| record.loop_id == loop_id)))
        .expect("the Loop is recorded");
    let mut lines = lines;
    let Line::System(entry) = &mut lines[last] else {
        unreachable!();
    };
    let record = entry.loop_record.as_mut().unwrap();
    record.event_sources = vec![serde_json::to_value(&binding).unwrap()];
    record.state = serde_json::to_value(LoopState::Idle).unwrap();
    lines
}
