use std::{
    io::Write,
    sync::{Arc, RwLock},
};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::json;
use tick_market::{
    commands::reader::{ReadDocument, ReaderView, read_document, run_live},
    config::Settings,
    metadata::TickKind,
    reader::{Document, html_to_markdown, load_document},
    runtime::{LiveView, NoMarketApi, ViewState},
};
use tick_sdk::{stream::TickStream, tools::ToolCall};

#[tokio::test]
async fn local_original_keeps_complete_sections_tables_and_source() {
    let mut file = tempfile::Builder::new().suffix(".md").tempfile().unwrap();
    write!(file,"# Results\n\nIntro.\n\n## Revenue\n\n| Item | Actual |\n| --- | --- |\n| Sales | 12 |\n\n## Outlook\n\nSpending will rise.\n").unwrap();
    let document = load_document(file.path().to_str().unwrap()).await.unwrap();
    assert_eq!(document.sections.len(), 3);
    assert!(document.sections[1].body.contains("| Sales | 12 |"));
    assert!(document.sections[2].body.contains("Spending will rise."));
    assert!(document.origin.ends_with(".md"));
    assert_eq!(document.published_at, None);
    assert!(!document.retrieved_at.is_empty());
}

#[test]
fn html_conversion_preserves_financial_tables_and_rejects_executable_content() {
    let (title, body) = html_to_markdown(
        "<html><head><title>Results</title><style>hidden-css</style></head><body><h1>Results</h1><p>Full context before the table.</p><table><tr><th>Metric</th><th>2026</th></tr><tr><td>Sales</td><td>12.8</td></tr></table><p>Context after.</p><a href='/report.pdf'>Original</a><script>malicious-code</script></body></html>",
        "https://example.com/ir/results",
    );
    assert_eq!(title.as_deref(), Some("Results"));
    assert!(body.contains("Full context before"));
    assert!(body.contains("| Sales | 12.8 |"));
    assert!(body.contains("Context after"));
    assert!(body.contains("https://example.com/report.pdf"));
    assert!(!body.contains("malicious-code"));
    assert!(!body.contains("hidden-css"));
}

#[tokio::test]
async fn citations_pin_document_revision_even_after_new_version_is_opened() {
    let old = Document::from_text(
        "https://example.com/report",
        "FY2025 Q4",
        "# Results\n\nOld guidance.",
    );
    let new = Document::from_text(
        "https://example.com/report",
        "FY2026 Q1",
        "# Results\n\nNew guidance.",
    );
    let library = Arc::new(RwLock::new(vec![old.clone(), new.clone()]));
    let args = ReadDocument {
        document_id: old.id.clone(),
        revision: Some(old.revision.clone()),
        section: Some("s001".into()),
        offset: 0,
        max_characters: None,
    };
    let value = read_document(&library, &args).unwrap();
    assert!(value["text"].as_str().unwrap().contains("Old guidance"));
    let mut view = ReaderView::new(library.clone());
    let result = view
        .act(ToolCall {
            tool: "locate".into(),
            args: json!({"document_id":old.id,"revision":old.revision,"section":"s001"}),
        })
        .await
        .unwrap();
    assert_eq!(result["document"]["title"], "FY2025 Q4");
    assert!(
        read_document(
            &library,
            &ReadDocument {
                revision: Some("missing-revision".into()),
                ..args
            }
        )
        .is_err()
    );
}

#[test]
fn document_queries_page_unicode_without_losing_or_duplicating_characters() {
    let document =
        Document::from_text("demo://unicode", "财报", "# 财报\n\n收入增长。现金流改善。");
    let library = Arc::new(RwLock::new(vec![document.clone()]));
    let mut args = ReadDocument {
        document_id: document.id,
        revision: Some(document.revision),
        section: Some("s001".into()),
        offset: 0,
        max_characters: Some(4),
    };
    let mut text = String::new();
    loop {
        let chunk = read_document(&library, &args).unwrap();
        text.push_str(chunk["text"].as_str().unwrap());
        if let Some(next) = chunk["next_offset"].as_u64() {
            args.offset = next as usize;
        } else {
            break;
        }
    }
    assert_eq!(text, "# 财报\n\n收入增长。现金流改善。");
}

/// Runs the demo Reader on `keys` until they run out and returns the last screen with how many
/// Tick Updates it published.
async fn read_with(keys: Vec<KeyEvent>) -> (String, usize) {
    let library = tick_market::reader::demo_library();
    let input = futures_util::stream::iter(keys.into_iter().map(|key| Ok(Event::Key(key))));
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    let mut updates = TickStream::in_memory(TickKind::Reader, "longbridge");
    run_live(
        library,
        &Settings::default(),
        &mut terminal,
        input,
        &mut updates,
    )
    .await
    .unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    (text, updates.records().len())
}

#[tokio::test]
async fn reader_backspace_and_control_navigation_do_not_quit_or_publish_cursor_updates() {
    let (text, updates) = read_with(vec![
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
    ])
    .await;
    assert!(
        text.contains("/qj"),
        "q is text and Backspace edits: {text}"
    );
    assert!(!text.contains("/qjk"), "{text}");
    assert!(
        !text.contains("NORMAL") && !text.contains("INSERT"),
        "{text}"
    );
    assert_eq!(updates, 1, "navigation is not a business update");
}

#[tokio::test]
async fn ctrl_c_cancels_the_reader_search_and_the_tick_keeps_running() {
    let (text, _) = read_with(vec![
        KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
    ])
    .await;
    assert!(!text.contains("/z"), "the search was cancelled: {text}");
    assert!(
        text.contains("s001 · "),
        "the next key opened the table of contents: {text}"
    );
}

#[tokio::test]
async fn opening_another_document_keeps_prior_revision_and_reports_load_failures() {
    let library = tick_market::reader::demo_library();
    let original = library.read().unwrap()[0].revision.clone();
    let mut file = tempfile::NamedTempFile::new().unwrap();
    writeln!(file, "# Second report\n\nComplete original.").unwrap();
    let mut view = ReaderView::new(library.clone());
    view.act(ToolCall {
        tool: "open_document".into(),
        args: json!({"location":file.path(),"symbol":"DEMO.US","fiscal_period":"FY2025Q4"}),
    })
    .await
    .unwrap();
    assert_eq!(library.read().unwrap().len(), 2);
    assert_eq!(library.read().unwrap()[0].revision, original);
    let result = view
        .act(ToolCall {
            tool: "open_document".into(),
            args: json!({"location":"/no-such-original/report.pdf"}),
        })
        .await;
    assert!(result.is_err());
    assert_eq!(library.read().unwrap().len(), 2);
    let mut state = ViewState::default();
    view.on_key(KeyCode::Backspace, &NoMarketApi, &mut state)
        .await
        .unwrap();
}

fn draw(view: &ReaderView, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| view.draw(frame, &mut ViewState::default(), &Settings::default()))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

#[tokio::test]
async fn locate_offset_is_visible_and_invalid_location_leaves_the_current_document_intact() {
    let body = format!(
        "# Long original\n\n{}NEEDLE_TARGET belongs to this paragraph.",
        "Earlier paragraph.\n\n".repeat(60)
    );
    let document = Document::from_text("demo://long", "Long original", &body);
    let other = Document::from_text(
        "demo://other",
        "Different original",
        "# Different\n\nOther text.",
    );
    let library = Arc::new(RwLock::new(vec![document.clone(), other.clone()]));
    let mut view = ReaderView::new(library);
    let offset = body[..body.find("NEEDLE_TARGET").unwrap()].chars().count();
    view.act(ToolCall{tool:"locate".into(),args:json!({"document_id":document.id,"revision":document.revision,"section":"s001","offset":offset})}).await.unwrap();
    let screen = draw(&view, 80, 18);
    assert!(screen.contains("NEEDLE_TARGET"), "{screen}");
    let result = view
        .act(ToolCall {
            tool: "locate".into(),
            args: json!({"document_id":other.id,"revision":other.revision,"max_characters":0}),
        })
        .await;
    assert!(result.is_err());
    let after = draw(&view, 80, 18);
    assert!(after.contains("Long original"), "{after}");
    assert!(!after.contains("Different original"), "{after}");
}

#[tokio::test]
async fn reader_search_finds_text_across_panel_wraps_and_scroll_citations_follow_visible_sections()
{
    for width in [40, 100] {
        let document = Document::from_text(
            "demo://wrap",
            "Wrapped original",
            &format!(
                "# Intro\n\nPreamble.\n\n## Findings\n\n{}NEEDLE_TARGET\n\n{}",
                "x".repeat(26),
                "Following context.\n\n".repeat(30)
            ),
        );
        let mut view = ReaderView::new(Arc::new(RwLock::new(vec![document])));
        draw(&view, width, 18);
        let mut state = ViewState::default();
        view.on_key(KeyCode::Char('/'), &NoMarketApi, &mut state)
            .await
            .unwrap();
        for character in "NEEDLE_TARGET".chars() {
            view.on_key(KeyCode::Char(character), &NoMarketApi, &mut state)
                .await
                .unwrap();
        }
        view.on_key(KeyCode::Enter, &NoMarketApi, &mut state)
            .await
            .unwrap();
        let found = draw(&view, width, 18);
        assert!(found.contains("1 matches"), "width {width}: {found}");
        view.on_key(KeyCode::Char('c'), &NoMarketApi, &mut state)
            .await
            .unwrap();
        let cited = draw(&view, 100, 18);
        assert!(cited.contains("s002"), "{cited}");
    }
}

#[tokio::test]
async fn reopening_identical_original_updates_explicit_context_and_returns_stored_metadata() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    writeln!(file, "# Results\n\nSame original.").unwrap();
    let document = load_document(file.path().to_str().unwrap()).await.unwrap();
    let library = Arc::new(RwLock::new(vec![document]));
    let mut view = ReaderView::new(library.clone());
    let result = view
        .act(ToolCall {
            tool: "open_document".into(),
            args: json!({"location":file.path(),"symbol":"AAPL.US","fiscal_period":"FY2026Q2"}),
        })
        .await
        .unwrap();
    assert_eq!(library.read().unwrap().len(), 1);
    assert_eq!(
        result["fiscal_period"],
        view.content()["documents"][0]["fiscal_period"]
    );
    assert_eq!(result["fiscal_period"], "FY2026Q2");
}

#[tokio::test]
async fn persisted_original_revision_restores_after_source_changes_and_library_restarts() {
    use tick_market::reader::DocumentCache;
    let directory = tempfile::tempdir().unwrap();
    let cache = DocumentCache::new(directory.path().join("cache"));
    let origin = directory.path().join("report.md");
    std::fs::write(&origin, "# Results\n\nRevenue was 12.\n").unwrap();
    let library = Arc::new(RwLock::new(Vec::new()));
    let mut view = ReaderView::new(library).with_document_cache(cache.clone());
    let original = view
        .act(ToolCall {
            tool: "open_document".into(),
            args: json!({"location":origin,"symbol":"AAPL.US","fiscal_period":"FY2026-Q2"}),
        })
        .await
        .unwrap();
    std::fs::write(&origin, "# Results\n\nRevenue was 15.\n").unwrap();
    let current = view
        .act(ToolCall {
            tool: "open_document".into(),
            args: json!({"location":origin,"symbol":"AAPL.US","fiscal_period":"FY2026-Q3"}),
        })
        .await
        .unwrap();
    assert_ne!(original["revision"], current["revision"]);
    std::fs::remove_file(&origin).unwrap();
    let library = Arc::new(RwLock::new(Vec::new()));
    let mut restarted = ReaderView::new(library).with_document_cache(cache);
    let old = restarted.act(ToolCall { tool: "locate".into(), args: json!({"document_id":original["id"],"revision":original["revision"],"section":"s001"}) }).await.unwrap();
    assert!(old["text"].as_str().unwrap().contains("Revenue was 12."));
    assert!(!old["text"].as_str().unwrap().contains("Revenue was 15."));
    assert_eq!(old["document"]["fiscal_period"], "FY2026-Q2");
}

#[test]
fn cached_original_rejects_corrupt_contents_and_identity_path_traversal() {
    use tick_market::reader::DocumentCache;
    let directory = tempfile::tempdir().unwrap();
    let cache = DocumentCache::new(directory.path().to_owned());
    let original = Document::from_text(
        "https://issuer.example/report",
        "Results",
        "# Revenue\n\n12 million.",
    );
    cache.save(&original).unwrap();
    let mut altered = original.clone();
    altered.sections[0].body = "Changed evidence".into();
    assert!(cache.save(&altered).is_err());
    std::fs::write(
        directory
            .path()
            .join(&original.id)
            .join(format!("{}.json", original.revision)),
        serde_json::to_vec(&altered).unwrap(),
    )
    .unwrap();
    assert!(cache.load(&original.id, &original.revision).is_err());
    assert!(cache.load("../outside", &original.revision).is_err());
}

#[tokio::test]
async fn following_period_metric_opens_matching_text_and_chat_keeps_exact_citation() {
    use tick_market::{
        reader::DocumentCache,
        research::{ResearchContext, ResearchDocument, ResearchEvidence, ResearchTarget},
    };
    let directory = tempfile::tempdir().unwrap();
    let origin = directory.path().join("report.md");
    std::fs::write(&origin, "# Results\n\nIntroduction.\n\n## Performance\n\nTotal net sales were 109.4 billion for fiscal 2026 third quarter.\n\n## Outlook\n\nUnchanged.\n").unwrap();
    let context = ResearchContext {
        symbol: "AAPL.US".into(),
        period_id: "FY2026-Q3".into(),
        fiscal_period_end: Some("2026-06-27".into()),
        documents: vec![ResearchDocument {
            title: "Original".into(),
            location: origin.display().to_string(),
            kind: "release".into(),
            published_at: None,
            verification: "metadata_match_body_not_verified".into(),
            match_basis: "test fixture".into(),
        }],
        evidence: Some(ResearchEvidence {
            metric_key: Some("revenue".into()),
            search_terms: vec!["total net sales".into()],
            ..ResearchEvidence::default()
        }),
        ..ResearchContext::default()
    };
    let mut view = ReaderView::new(Arc::new(RwLock::new(Vec::new())))
        .with_document_cache(DocumentCache::new(directory.path().join("cache")));
    view.act(ToolCall {
        tool: "set_context".into(),
        args: json!({"context":context}),
    })
    .await
    .unwrap();
    let mut terminal = Terminal::new(TestBackend::new(90, 18)).unwrap();
    let mut state = ViewState::default();
    terminal
        .draw(|frame| view.draw(frame, &mut state, &Settings::default()))
        .unwrap();
    view.on_key(KeyCode::Char('a'), &NoMarketApi, &mut state)
        .await
        .unwrap();
    let navigation = view.take_navigation().unwrap();
    assert_eq!(navigation.target, ResearchTarget::Chat);
    assert_eq!(navigation.context.period_id, "FY2026-Q3");
    let evidence = navigation.context.evidence.unwrap();
    assert!(
        evidence.text.contains("Total net sales"),
        "{}",
        evidence.text
    );
    assert_eq!(evidence.section.as_deref(), Some("s002"));
    assert!(evidence.document_id.is_some() && evidence.revision.is_some());
    let pinned = view.act(ToolCall {tool:"locate".into(),args:json!({"document_id":evidence.document_id,"revision":evidence.revision,"section":evidence.section,"offset":evidence.offset,"max_characters":evidence.text.chars().count()})}).await.unwrap();
    assert_eq!(pinned["text"], evidence.text);
}

#[tokio::test]
async fn an_explicit_original_survives_period_round_trips_and_cache_only_restart() {
    use tick_market::{
        reader::DocumentCache,
        research::{ResearchContext, ResearchDocument},
    };
    let directory = tempfile::tempdir().unwrap();
    let cache = DocumentCache::new(directory.path().join("cache"));
    let original = directory.path().join("issuer-q3.md");
    std::fs::write(&original, "# FY2026 Q3\n\nRevenue is 109.4 billion.\n").unwrap();
    let mut view =
        ReaderView::new(Arc::new(RwLock::new(Vec::new()))).with_document_cache(cache.clone());
    view.act(ToolCall {
        tool: "open_document".into(),
        args: json!({"location":original,"symbol":"AAPL.US","fiscal_period":"FY2026-Q3"}),
    })
    .await
    .unwrap();
    let second = directory.path().join("issuer-q2.md");
    std::fs::write(&second, "# FY2026 Q2\n\nRevenue is 111.2 billion.\n").unwrap();
    let context_for = |period: &str, location: String| ResearchContext {
        symbol: "AAPL.US".into(),
        period_id: period.into(),
        documents: vec![ResearchDocument {
            title: "Original".into(),
            location,
            kind: "release".into(),
            published_at: None,
            verification: "explicit_user_original".into(),
            match_basis: "test fixture".into(),
        }],
        ..ResearchContext::default()
    };
    let q2 = context_for("FY2026-Q2", second.display().to_string());
    view.act(ToolCall {
        tool: "set_context".into(),
        args: json!({"context":q2}),
    })
    .await
    .unwrap();
    std::fs::remove_file(original).unwrap();
    let q3 = context_for(
        "FY2026-Q3",
        directory
            .path()
            .join("unavailable-provider-original.md")
            .display()
            .to_string(),
    );
    let cached = view
        .act(ToolCall {
            tool: "set_context".into(),
            args: json!({"context":q3}),
        })
        .await
        .unwrap();
    assert_eq!(cached["cached_original"], true);
    assert_eq!(cached["document"]["fiscal_period"], "FY2026-Q3");
    assert!(cached["document"]["retrieved_at"].is_string());
    let mut restarted =
        ReaderView::new(Arc::new(RwLock::new(Vec::new()))).with_document_cache(cache);
    let restored = restarted
        .act(ToolCall {
            tool: "set_context".into(),
            args: json!({"context":q3}),
        })
        .await
        .unwrap();
    assert_eq!(restored["cached_original"], true);
    assert_eq!(
        restored["document"]["revision"],
        cached["document"]["revision"]
    );
    let mut wrong_period = q3;
    wrong_period.period_id = "FY2026-Q4".into();
    assert!(
        restarted
            .act(ToolCall {
                tool: "set_context".into(),
                args: json!({"context":wrong_period})
            })
            .await
            .is_err()
    );
    assert_eq!(restarted.research_context().unwrap().period_id, "FY2026-Q3");
}

#[tokio::test]
async fn a_whitespace_only_visible_section_cannot_create_a_blank_chat_citation() {
    use tick_market::reader::DocumentSection;
    let mut document = Document::from_text(
        "https://issuer.example/report",
        "Empty extraction",
        "# Results\n\nPlaceholder.",
    );
    document.symbol = Some("AAPL.US".into());
    document.fiscal_period = Some("FY2026-Q3".into());
    document.sections = vec![DocumentSection {
        id: "s001".into(),
        title: "Empty section".into(),
        body: "   \n\n \t\n".into(),
    }];
    let mut view = ReaderView::new(Arc::new(RwLock::new(vec![document])));
    let mut state = ViewState::default();
    view.on_key(KeyCode::Char('a'), &NoMarketApi, &mut state)
        .await
        .unwrap();
    assert!(view.take_navigation().is_none());
    let mut terminal = Terminal::new(TestBackend::new(100, 18)).unwrap();
    terminal
        .draw(|frame| view.draw(frame, &mut state, &Settings::default()))
        .unwrap();
    let visible = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(visible.contains("No nonempty original paragraph"));
}
