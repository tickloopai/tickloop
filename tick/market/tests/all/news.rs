use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::json;
use tick_market::{
    args::{NewsArgs, OnceArgs, OutputFormat},
    commands::news,
    config::Settings,
    metadata::TickKind,
};
use tick_provider::{
    error::Result,
    news::{NewsApi, NewsArticle, NewsItem},
};
use tick_sdk::stream::TickStream;

#[derive(Default)]
struct News {
    symbols: Mutex<Vec<Option<String>>>,
    articles: Mutex<Vec<String>>,
}

#[async_trait]
impl NewsApi for News {
    async fn news(&self, symbol: Option<&str>) -> Result<Vec<NewsItem>> {
        self.symbols.lock().unwrap().push(symbol.map(str::to_owned));
        Ok(serde_json::from_value(json!([
            {"id":"1","title":"较早的中文资讯","source":"Source","published_at":"2026-09-10T10:00:00+08:00","url":"https://example.com/1"},
            {"id":"3","title":"最新的中文资讯","source":"Source","published_at":"2026-09-10T03:00:00Z","url":"https://example.com/3"},
            {"id":"2","title":"中间的中文资讯","source":"Source","published_at":"2026-09-10T02:30:00Z","url":"https://example.com/2"}
        ])).unwrap())
    }

    async fn article(&self, id: &str) -> Result<NewsArticle> {
        self.articles.lock().unwrap().push(id.to_owned());
        Ok(NewsArticle {
            id: id.into(),
            title: "正文标题".into(),
            source: "Source".into(),
            published_at: "2026-09-10T02:30:00Z".into(),
            url: "https://example.com/2".into(),
            body: "## 中文小标题\n\n- 第一条新闻正文。\n- [官方来源](https://example.com)".into(),
        })
    }
}

#[tokio::test]
async fn news_once_normalizes_symbol_sorts_timestamps_and_limits_both_sources() {
    let api = News::default();
    for symbol in [Some("700".to_owned()), None] {
        let args = NewsArgs {
            symbol,
            limit: 2,
            output: OnceArgs {
                once: true,
                format: OutputFormat::Json,
            },
        };
        let mut output = Vec::new();
        let mut events = TickStream::in_memory(TickKind::News, "longbridge");
        news::run_once(
            &args,
            &api,
            &Settings::default(),
            false,
            &mut output,
            &mut events,
        )
        .await
        .unwrap();
        let rows: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 2);
        assert_eq!(rows[0]["id"], "3");
        assert_eq!(rows[1]["id"], "2");
        assert_eq!(rows[0]["title"], "最新的中文资讯");
        assert_eq!(rows[0]["source"], "Source");
        assert!(rows[0].get("published_at").is_some());
        assert!(!output.contains(&0x1b));
    }
    assert_eq!(*api.symbols.lock().unwrap(), [Some("700.HK".into()), None]);
    assert!(api.articles.lock().unwrap().is_empty());
}

#[tokio::test]
async fn news_detail_returns_to_the_selected_item_without_extra_business_events() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};
    use tick_provider::api::{Quote, QuoteApi};

    struct NoQuotes;
    #[async_trait]
    impl QuoteApi for NoQuotes {
        async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
            panic!("news must not request quotes")
        }
    }
    let api = News::default();
    let (keys, input) = tokio::sync::mpsc::unbounded_channel();
    let keyboard = tokio::spawn(async move {
        for code in [
            KeyCode::Down,
            KeyCode::Enter,
            KeyCode::Down,
            KeyCode::Backspace,
            KeyCode::Enter,
            KeyCode::Backspace,
            KeyCode::Char('q'),
        ] {
            tokio::time::sleep(std::time::Duration::from_millis(45)).await;
            keys.send(Ok(Event::Key(KeyEvent::new(code, KeyModifiers::NONE))))
                .unwrap();
        }
    });
    let args = NewsArgs {
        symbol: Some("700".into()),
        limit: 3,
        output: OnceArgs::default(),
    };
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
    let mut events = TickStream::in_memory(TickKind::News, "longbridge");
    news::run_live(
        &args,
        &NoQuotes,
        &api,
        &Settings::default(),
        &mut terminal,
        tokio_stream::wrappers::UnboundedReceiverStream::new(input),
        &mut events,
    )
    .await
    .unwrap();
    keyboard.await.unwrap();
    assert_eq!(*api.articles.lock().unwrap(), ["2"]);
    let records: Vec<serde_json::Value> = events.records().all();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1]["data"]["articles"]["2"]["title"], "正文标题");
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .filter(|symbol| *symbol != " ")
        .collect();
    assert!(screen.contains("中间的中文资讯"));
    assert!(!screen.contains("中文小标题"));
}

#[tokio::test(start_paused = true)]
async fn periodic_news_refresh_preserves_selection_by_article_id() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};
    use tick_provider::api::{Quote, QuoteApi};

    struct UpdatingNews(News);
    #[async_trait]
    impl NewsApi for UpdatingNews {
        async fn news(&self, symbol: Option<&str>) -> Result<Vec<NewsItem>> {
            let mut items = self.0.news(symbol).await?;
            if self.0.symbols.lock().unwrap().len() > 1 {
                items.push(NewsItem {
                    id: "4".into(),
                    title: "新增资讯".into(),
                    source: "Source".into(),
                    published_at: "2026-09-10T04:00:00Z".into(),
                    url: "https://example.com/4".into(),
                });
            }
            Ok(items)
        }
        async fn article(&self, id: &str) -> Result<NewsArticle> {
            self.0.article(id).await
        }
    }
    #[async_trait]
    impl QuoteApi for UpdatingNews {
        async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
            panic!("news must not request quotes")
        }
    }
    let api = UpdatingNews(News::default());
    let (keys, input) = tokio::sync::mpsc::unbounded_channel();
    let keyboard = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        keys.send(Ok(Event::Key(KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        ))))
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(31)).await;
        for code in [KeyCode::Enter, KeyCode::Backspace, KeyCode::Char('q')] {
            keys.send(Ok(Event::Key(KeyEvent::new(code, KeyModifiers::NONE))))
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    });
    let args = NewsArgs {
        symbol: None,
        limit: 3,
        output: OnceArgs::default(),
    };
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
    let mut events = TickStream::in_memory(TickKind::News, "longbridge");
    news::run_live(
        &args,
        &api,
        &api,
        &Settings::default(),
        &mut terminal,
        tokio_stream::wrappers::UnboundedReceiverStream::new(input),
        &mut events,
    )
    .await
    .unwrap();
    keyboard.await.unwrap();
    assert_eq!(*api.0.articles.lock().unwrap(), ["2"]);
    let records: Vec<serde_json::Value> = events.records().all();
    assert_eq!(records.len(), 3);
    assert_eq!(records[1]["data"]["items"][0]["id"], "4");
    assert_eq!(records[2]["data"]["articles"]["2"]["title"], "正文标题");
}

#[tokio::test]
async fn news_renderer_applies_resolved_styles_to_selection_markdown_and_blank_cells() {
    use crossterm::event::KeyCode;
    use ratatui::{
        Terminal,
        backend::TestBackend,
        style::{Color, Modifier, Style},
    };
    use tick_market::runtime::{LiveView, ViewState};
    use tick_provider::api::{Quote, QuoteApi};

    struct StyledNews(News);
    #[async_trait]
    impl NewsApi for StyledNews {
        async fn news(&self, symbol: Option<&str>) -> Result<Vec<NewsItem>> {
            self.0.news(symbol).await
        }
        async fn article(&self, id: &str) -> Result<NewsArticle> {
            let mut article = self.0.article(id).await?;
            article.title = "ArticleTitle".into();
            article.body = "PlainText 中文 **StrongText** *EmphasisText* [LinkText](https://example.com) `InlineCode`".into();
            Ok(article)
        }
    }
    #[async_trait]
    impl QuoteApi for StyledNews {
        async fn quotes(&self, _: &[String]) -> Result<Vec<Quote>> {
            panic!("news must not request quotes")
        }
    }
    let mut settings = Settings::default();
    let mut styles = settings.styles();
    styles.canvas = Style::reset().bg(Color::Indexed(101));
    styles.text = Style::new()
        .fg(Color::Indexed(102))
        .add_modifier(Modifier::BOLD | Modifier::ITALIC);
    styles.panel = Style::new().bg(Color::Indexed(103));
    styles.title = Style::new()
        .fg(Color::Indexed(104))
        .remove_modifier(Modifier::BOLD);
    styles.strong = Style::new()
        .fg(Color::Indexed(105))
        .remove_modifier(Modifier::ITALIC)
        .add_modifier(Modifier::DIM);
    styles.emphasis = Style::new()
        .fg(Color::Indexed(106))
        .remove_modifier(Modifier::BOLD)
        .add_modifier(Modifier::UNDERLINED);
    styles.link = Style::new()
        .fg(Color::Indexed(107))
        .remove_modifier(Modifier::BOLD | Modifier::ITALIC)
        .add_modifier(Modifier::REVERSED);
    styles.inline_code = Style::new()
        .fg(Color::Indexed(108))
        .remove_modifier(Modifier::BOLD | Modifier::ITALIC)
        .add_modifier(Modifier::CROSSED_OUT);
    styles.selection = Style::new()
        .fg(Color::Indexed(109))
        .bg(Color::Indexed(110))
        .remove_modifier(Modifier::ITALIC)
        .add_modifier(Modifier::UNDERLINED);
    styles.muted_text = Style::new()
        .fg(Color::Indexed(111))
        .remove_modifier(Modifier::ITALIC);
    settings.renderer_styles = Some(styles);
    let api = StyledNews(News::default());
    let args = NewsArgs {
        symbol: None,
        limit: 3,
        output: OnceArgs::default(),
    };
    let mut view = news::NewsView::new(args, &api, api.news(None).await.unwrap());
    let mut state = ViewState::default();
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
    terminal
        .draw(|frame| view.draw(frame, &mut state, &settings))
        .unwrap();
    let list = terminal.backend().buffer();
    let selected = rendered_cell(list, "最");
    assert_eq!(selected.fg, Color::Indexed(109));
    assert_eq!(selected.bg, Color::Indexed(110));
    assert!(
        selected
            .modifier
            .contains(Modifier::BOLD | Modifier::UNDERLINED)
    );
    assert!(!selected.modifier.contains(Modifier::ITALIC));
    assert_eq!(list[(17, 2)].symbol(), " ");
    assert_eq!(
        list[(17, 2)].bg,
        Color::Indexed(110),
        "selection covers column gaps"
    );
    assert_eq!(
        list[(78, 17)].bg,
        Color::Indexed(103),
        "panel covers unused rows"
    );
    let heading = rendered_cell(list, "Published");
    assert_eq!(heading.fg, Color::Indexed(104));
    assert!(!heading.modifier.contains(Modifier::BOLD));

    view.on_key(KeyCode::Enter, &api, &mut state).await.unwrap();
    terminal
        .draw(|frame| {
            view.draw(frame, &mut state, &settings);
            let cells = &frame.buffer_mut().content;
            let chinese_index = cells.iter().position(|cell| cell.symbol() == "中").unwrap();
            let continuation = &cells[chinese_index + 1];
            assert_eq!(continuation.bg, Color::Indexed(103));
            assert_eq!(continuation.modifier, Modifier::BOLD | Modifier::ITALIC);
        })
        .unwrap();
    let article = terminal.backend().buffer();
    for (text, foreground, modifiers) in [
        ("ArticleTitle", 104, Modifier::ITALIC),
        ("PlainText", 102, Modifier::BOLD | Modifier::ITALIC),
        ("StrongText", 105, Modifier::BOLD | Modifier::DIM),
        ("EmphasisText", 106, Modifier::ITALIC | Modifier::UNDERLINED),
        ("LinkText", 107, Modifier::REVERSED),
        ("InlineCode", 108, Modifier::CROSSED_OUT),
    ] {
        let cell = rendered_cell(article, text);
        assert_eq!(cell.fg, Color::Indexed(foreground), "{text}");
        assert_eq!(cell.bg, Color::Indexed(103), "{text}");
        assert_eq!(cell.modifier, modifiers, "{text}");
    }
    assert_eq!(article[(78, 17)].symbol(), " ");
    assert_eq!(article[(78, 17)].bg, Color::Indexed(103));
    assert_eq!(
        rendered_cell(article, "Backspace Back").fg,
        Color::Indexed(111)
    );
}

fn rendered_cell<'a>(buffer: &'a ratatui::buffer::Buffer, text: &str) -> &'a ratatui::buffer::Cell {
    buffer
        .content
        .iter()
        .enumerate()
        .find_map(|(index, cell)| {
            let rendered: String = buffer
                .content
                .iter()
                .skip(index)
                .take(text.chars().count())
                .map(|cell| cell.symbol())
                .collect();
            (rendered == text).then_some(cell)
        })
        .unwrap_or_else(|| panic!("missing rendered text: {text}"))
}
