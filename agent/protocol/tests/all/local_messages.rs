//! Local messages the terminal recognizes in a conversation.

use tickloop_agent_protocol::SubagentReportMessage;

#[test]
fn a_subagent_report_reads_back_as_its_note_and_report() {
    for note in [
        None,
        Some("SECURITY WARNING: auto review flagged this subagent's report. Reason: risk"),
    ] {
        let message = SubagentReportMessage {
            note: note.map(str::to_owned),
            report: "Found three call sites.\n\n  Indented detail".into(),
        };
        let text = message.text();
        assert!(text.contains("NOT a message from the user"), "{text}");
        assert_eq!(SubagentReportMessage::parse(&text), Some(message));
    }
    assert_eq!(SubagentReportMessage::parse("A plain Agent message"), None);
}
