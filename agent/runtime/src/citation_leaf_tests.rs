use super::*;
use tickloop_agent_protocol::{Content, Message, Role};
use tickloop_agent_transcript::{ChainEntry, CitationSourcesEntry, RetainedSnapshotEntry};

fn id(value: u64) -> Uuid {
    Uuid::from_u128(value.into())
}

fn node(number: u64, parent: Option<u64>, run: u64, role: Role, content: Vec<Content>) -> Line {
    let entry = serde_json::from_value::<ChainEntry>(serde_json::json!({
        "uuid":id(number), "parentUuid":parent.map(id), "chatId":Uuid::nil(),
        "timestamp":tickloop_agent_base::timestamp::format_timestamp(number * 1000),
        "version":"fixture", "runId":(run != 0).then(|| id(run)),
        "message":Message {role, content},
    }))
    .unwrap();
    Line::chain(role, entry)
}

fn text(number: u64, parent: Option<u64>) -> Line {
    node(
        number,
        parent,
        0,
        Role::User,
        vec![Content::Text {
            text: "text".into(),
        }],
    )
}

fn uses(number: u64, parent: u64, run: u64, calls: &[&str]) -> Line {
    node(
        number,
        Some(parent),
        run,
        Role::Assistant,
        calls
            .iter()
            .map(|call| Content::ToolUse {
                id: (*call).into(),
                name: "WebSearch".into(),
                input: serde_json::json!({"query":"evidence"}),
            })
            .collect(),
    )
}

fn result(number: u64, parent: u64, run: u64, continued: bool, calls: &[&str]) -> Line {
    let mut line = node(
        number,
        Some(parent),
        run,
        Role::User,
        calls
            .iter()
            .map(|call| Content::ToolResult {
                tool_use_id: (*call).into(),
                content: (*call).into(),
                is_error: false,
            })
            .collect(),
    );
    if let Line::User(entry) = &mut line {
        entry.continues_message = continued;
    }
    line
}

fn call(name: &str, run: u64, message: u64, number: u64, aliases: &[u64]) -> CitationCall {
    CitationCall {
        call_id: name.into(),
        run_id: id(run),
        message_id: id(message),
        numbers: vec![number],
        aliases: aliases.iter().copied().map(id).collect(),
    }
}

fn sources() -> Vec<CitationSource> {
    (1..=4)
        .map(|number| CitationSource {
            number,
            url: format!("https://evidence.test/{number}"),
            title: format!("Source {number}"),
            site: "evidence.test".into(),
            snippet: format!("Evidence {number}"),
        })
        .collect()
}

fn metadata(calls: Vec<CitationCall>) -> Line {
    Line::CitationSources(CitationSourcesEntry {
        chat_id: Uuid::nil(),
        sources: sources(),
        calls,
    })
}

fn at(lines: &[Line], leaf: u64) -> VisibleCitations {
    visible_citations_at(lines, id(leaf), None).unwrap()
}

fn numbers(visible: &VisibleCitations) -> Vec<u64> {
    visible.sources.iter().map(|source| source.number).collect()
}

fn boundary(number: u64, logical_parent: u64) -> Line {
    let mut line = node(
        number,
        None,
        0,
        Role::System,
        vec![Content::Text {
            text: "compact".into(),
        }],
    );
    if let Line::System(entry) = &mut line {
        entry.subtype = Some(tickloop_agent_transcript::COMPACT_BOUNDARY_SUBTYPE.into());
        entry.logical_parent_uuid = Some(id(logical_parent));
        entry.compact_metadata = Some(
            serde_json::from_value(serde_json::json!({"trigger":"manual","preTokens":10})).unwrap(),
        );
    }
    line
}

fn summary(number: u64, parent: u64) -> Line {
    let mut line = text(number, Some(parent));
    if let Line::User(entry) = &mut line {
        entry.is_compact_summary = true;
    }
    line
}

#[test]
fn association_without_a_result_and_a_wrong_call_id_are_invisible() {
    let lines = vec![
        metadata(vec![
            call("a", 100, 3, 1, &[]),
            call("wrong", 100, 3, 2, &[]),
        ]),
        text(1, None),
        uses(2, 1, 100, &["a"]),
        result(3, 2, 100, false, &["a"]),
    ];
    assert!(at(&lines, 2).calls.is_empty());
    assert_eq!(numbers(&at(&lines, 3)), [1]);
    assert_eq!(at(&lines, 3).calls[0].call_id, "a");
}

#[test]
fn a_physical_leaf_before_a_continuation_cannot_see_its_later_result() {
    let lines = vec![
        metadata(vec![
            call("a", 100, 3, 1, &[]),
            call("b", 100, 3, 2, &[]),
            call("physical-b", 100, 4, 3, &[]),
        ]),
        text(1, None),
        uses(2, 1, 100, &["a", "b"]),
        result(3, 2, 100, false, &["a"]),
        result(4, 3, 100, true, &["b"]),
    ];
    assert_eq!(numbers(&at(&lines, 3)), [1]);
    assert_eq!(numbers(&at(&lines, 4)), [1, 2]);
    assert_eq!(at(&lines, 3), at(&lines[..4], 3));
    let mut physical = lines.clone();
    if let Line::CitationSources(table) = &mut physical[0] {
        table.calls[1].message_id = id(4);
    }
    assert_eq!(numbers(&at(&physical, 4)), [1, 2]);
}

#[test]
fn a_family_root_table_does_not_make_future_sources_visible() {
    let mut lines = vec![
        metadata(vec![call("a", 100, 3, 1, &[]), call("b", 100, 3, 2, &[])]),
        text(1, None),
        uses(2, 1, 100, &["a", "b"]),
        result(3, 2, 100, false, &["a"]),
        result(4, 3, 100, true, &["b"]),
    ];
    if let Line::CitationSources(table) = &mut lines[0] {
        table.sources.clear();
    }
    let root = sources();
    assert_eq!(
        numbers(&visible_citations_at(&lines, id(3), Some(&root)).unwrap()),
        [1]
    );
    assert_eq!(
        numbers(&visible_citations_at(&lines, id(4), Some(&root)).unwrap()),
        [1, 2]
    );
}

#[test]
fn summaries_use_aliases_without_following_the_logical_parent() {
    let lines = vec![
        metadata(vec![call("a", 100, 3, 1, &[5])]),
        text(1, None),
        uses(2, 1, 100, &["a"]),
        result(3, 2, 100, false, &["a"]),
        boundary(4, 3),
        summary(5, 4),
    ];
    assert!(at(&lines, 4).calls.is_empty());
    assert_eq!(numbers(&at(&lines, 5)), [1]);
}

#[test]
fn directed_summaries_and_rewritten_entries_keep_their_own_leaf_boundaries() {
    for keeps_before in [true, false] {
        let mut kept = result(6, 5, 100, false, &["a"]);
        if let Line::User(entry) = &mut kept {
            entry.original_message_id = Some(id(3));
        }
        let mut summarized = summary(7, 5);
        let mut lines = vec![
            metadata(vec![call("a", 100, 3, 1, &[]), call("b", 100, 4, 2, &[7])]),
            boundary(5, 4),
        ];
        if keeps_before {
            if let Line::User(entry) = &mut summarized {
                entry.parent_uuid = Some(id(6));
            }
            lines.extend([kept, summarized]);
            assert_eq!(numbers(&at(&lines, 6)), [1]);
            assert_eq!(numbers(&at(&lines, 7)), [1, 2]);
        } else {
            if let Line::User(entry) = &mut kept {
                entry.parent_uuid = Some(id(7));
            }
            lines.extend([summarized, kept]);
            assert_eq!(numbers(&at(&lines, 7)), [2]);
            assert_eq!(numbers(&at(&lines, 6)), [1, 2]);
        }
    }
}

#[test]
fn branches_rewind_and_retained_snapshots_do_not_revive_abandoned_calls() {
    let table = metadata(vec![call("a", 100, 3, 1, &[]), call("b", 200, 5, 2, &[])]);
    let lines = vec![
        table.clone(),
        text(1, None),
        uses(2, 1, 100, &["a"]),
        result(3, 2, 100, false, &["a"]),
        uses(4, 1, 200, &["b"]),
        result(5, 4, 200, false, &["b"]),
        Line::RetainedSnapshot(RetainedSnapshotEntry {
            chat_id: Uuid::nil(),
            lines: vec![table],
        }),
        text(6, Some(3)),
    ];
    assert!(at(&lines, 1).calls.is_empty());
    assert_eq!(numbers(&at(&lines, 5)), [2]);
    assert_eq!(numbers(&at(&lines, 6)), [1]);
    let compacted = vec![
        Line::RetainedSnapshot(RetainedSnapshotEntry {
            chat_id: Uuid::nil(),
            lines: vec![metadata(vec![call("a", 100, 3, 1, &[8])])],
        }),
        boundary(7, 6),
        summary(8, 7),
        text(9, Some(8)),
    ];
    assert_eq!(numbers(&at(&compacted, 9)), [1]);
}

#[test]
fn old_parallel_orphans_and_reused_call_ids_are_scoped_to_their_batches() {
    let lines = vec![
        metadata(vec![
            call("a", 100, 3, 1, &[]),
            call("b", 100, 4, 2, &[]),
            call("b", 200, 6, 3, &[]),
            call("foreign", 300, 8, 4, &[]),
        ]),
        text(1, None),
        uses(2, 1, 100, &["a", "b", "foreign"]),
        result(3, 2, 100, false, &["a"]),
        result(8, 2, 300, false, &["foreign"]),
        result(4, 2, 100, false, &["b"]),
        uses(5, 3, 200, &["b"]),
        result(6, 5, 200, false, &["b"]),
        text(7, Some(6)),
    ];
    assert_eq!(numbers(&at(&lines, 3)), [1]);
    let visible = at(&lines, 7);
    assert_eq!(numbers(&visible), [1, 2, 3]);
    assert_eq!(
        visible
            .calls
            .iter()
            .filter(|call| call.call_id == "b")
            .count(),
        2
    );
    assert_eq!(numbers(&at(&lines, 5)), [1, 2]);
}

#[test]
fn history_without_call_metadata_keeps_the_existing_orphan_recovery() {
    let stored = crate::transcript::StoredTranscript {
        lines: vec![
            text(1, None),
            uses(2, 1, 100, &["a", "b"]),
            result(3, 2, 100, false, &["a"]),
            result(4, 2, 100, false, &["b"]),
            uses(5, 3, 200, &["b"]),
            result(6, 5, 200, false, &["b"]),
            text(7, Some(6)),
        ],
        ..Default::default()
    };
    let (loaded, complete) = crate::transcript::assemble(&stored, Some(id(7)));
    assert!(complete);
    assert_eq!(
        loaded
            .unwrap()
            .chain
            .iter()
            .map(|entry| entry.uuid)
            .collect::<Vec<_>>(),
        [1, 2, 3, 5, 6, 7].map(id)
    );
}

#[test]
fn future_orphans_and_preserved_segments_cannot_change_an_earlier_leaf() {
    let mut lines = vec![
        metadata(vec![call("a", 100, 3, 1, &[]), call("b", 100, 5, 2, &[])]),
        text(1, None),
        uses(2, 1, 100, &["a", "b"]),
        result(3, 2, 100, false, &["a"]),
        text(4, Some(3)),
        result(5, 2, 100, false, &["b"]),
    ];
    let early = at(&lines, 4);
    assert_eq!(numbers(&early), [1]);
    assert_eq!(early, at(&lines[..5], 4));
    let mut compact = boundary(6, 5);
    if let Line::System(entry) = &mut compact {
        entry.compact_metadata.as_mut().unwrap().preserved_segment =
            Some(tickloop_agent_transcript::PreservedSegmentEntry {
                head_uuid: id(2),
                tail_uuid: id(3),
                anchor_uuid: id(7),
            });
    }
    lines.extend([compact, summary(7, 6), text(9, Some(7))]);
    assert_eq!(at(&lines, 3), at(&lines[..4], 3));
}

#[test]
fn legacy_preserved_segments_share_the_runtime_relinking_rules() {
    let mut compact = boundary(6, 5);
    if let Line::System(entry) = &mut compact {
        entry.compact_metadata.as_mut().unwrap().preserved_segment =
            Some(tickloop_agent_transcript::PreservedSegmentEntry {
                head_uuid: id(4),
                tail_uuid: id(5),
                anchor_uuid: id(7),
            });
    }
    let lines = vec![
        metadata(vec![call("a", 100, 3, 1, &[7]), call("b", 200, 5, 2, &[])]),
        text(1, None),
        uses(2, 1, 100, &["a"]),
        result(3, 2, 100, false, &["a"]),
        uses(4, 3, 200, &["b"]),
        result(5, 4, 200, false, &["b"]),
        compact,
        summary(7, 6),
        text(8, Some(7)),
    ];
    assert_eq!(numbers(&at(&lines, 3)), [1]);
    assert_eq!(numbers(&at(&lines, 8)), [1, 2]);
}

#[test]
fn missing_leaves_and_incomplete_ancestry_are_errors_and_plain_history_stays_plain() {
    assert_eq!(
        visible_citations_at(&[], id(1), None),
        Err(CitationQueryError::LeafNotFound(id(1)))
    );
    assert_eq!(
        visible_citations_at(&[text(1, Some(999))], id(1), None),
        Err(CitationQueryError::IncompleteHistory(id(1)))
    );
    assert_eq!(at(&[text(1, None)], 1), VisibleCitations::default());
}
