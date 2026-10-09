use std::{cell::Cell, collections::BTreeMap, io::Write, time::Duration};

use crossterm::event::{Event, KeyCode};
use futures_util::Stream;
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::Rect,
    style::Style,
    widgets::{Block, Borders},
};
use serde_json::json;
use tickloop_ui::{
    format::{pad, width, write_text},
    markdown,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    args::NewsArgs,
    config::Settings,
    output::TableData,
    runtime::{LiveView, ViewState},
};
use tick_provider::{
    api::{MarketEvent, QuoteServices, normalize_symbol},
    error::Result,
    news::{NewsApi, NewsArticle, NewsItem},
};
use tick_sdk::stream::TickStream;

const HEADERS: &[&str] = &["Published At", "Source", "Title", "Id", "Url"];
const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

pub fn table(items: &[NewsItem]) -> TableData {
    TableData {
        headers: HEADERS.to_vec(),
        rows: items
            .iter()
            .map(|item| {
                vec![
                    json!(item.published_at),
                    json!(item.source),
                    json!(item.title),
                    json!(item.id),
                    json!(item.url),
                ]
            })
            .collect(),
    }
}

async fn load_items(args: &NewsArgs, api: &dyn NewsApi) -> Result<Vec<NewsItem>> {
    let symbol = args.symbol.as_deref().map(normalize_symbol);
    let items = api.news(symbol.as_deref()).await?;
    Ok(sorted_items(items, args.limit))
}

fn sorted_items(mut items: Vec<NewsItem>, limit: usize) -> Vec<NewsItem> {
    items.sort_by_key(|item| {
        std::cmp::Reverse(OffsetDateTime::parse(&item.published_at, &Rfc3339).ok())
    });
    items.truncate(limit);
    items
}

pub async fn run_once(
    args: &NewsArgs,
    api: &dyn NewsApi,
    settings: &Settings,
    color: bool,
    output: &mut impl Write,
    events: &mut TickStream,
) -> Result<()> {
    let items = load_items(args, api).await?;
    let table = table(&items);
    events.publish(json!({"items": table.json(), "articles": {}}))?;
    table.write(args.output.format, settings, color, output)
}

pub struct NewsView<'a> {
    args: NewsArgs,
    api: &'a dyn NewsApi,
    items: Vec<NewsItem>,
    articles: BTreeMap<String, NewsArticle>,
    selected_id: Option<String>,
    open_article: Option<String>,
    detail_scroll: Cell<usize>,
    page_size: Cell<usize>,
}

impl<'a> NewsView<'a> {
    pub fn new(args: NewsArgs, api: &'a dyn NewsApi, items: Vec<NewsItem>) -> Self {
        let items = sorted_items(items, args.limit);
        let selected_id = items.first().map(|item| item.id.clone());
        Self {
            args,
            api,
            items,
            articles: BTreeMap::new(),
            selected_id,
            open_article: None,
            detail_scroll: Cell::new(0),
            page_size: Cell::new(1),
        }
    }

    fn selected_index(&self) -> usize {
        self.items
            .iter()
            .position(|item| Some(&item.id) == self.selected_id.as_ref())
            .unwrap_or(0)
    }

    fn draw_article(&self, frame: &mut Frame<'_>, article: &NewsArticle, settings: &Settings) {
        let area = frame.area();
        let styles = settings.styles();
        let block = Block::default()
            .title(" News article ")
            .borders(Borders::ALL)
            .style(styles.panel_base())
            .border_style(styles.border);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let markdown = format!(
            "# {}\n\n{}\n\n[Original article]({})",
            article.title, article.body, article.url
        );
        let lines = markdown::render(
            &markdown,
            inner.width as usize,
            settings.ambiguous_width,
            &styles,
        );
        let visible = inner.height.saturating_sub(1) as usize;
        self.page_size.set(visible.max(1));
        let offset = self
            .detail_scroll
            .get()
            .min(lines.len().saturating_sub(visible));
        self.detail_scroll.set(offset);
        for (index, line) in lines.iter().skip(offset).take(visible).enumerate() {
            let mut x = inner.x;
            for span in &line.spans {
                write_text(
                    frame.buffer_mut(),
                    x,
                    inner.y + index as u16,
                    &span.content,
                    inner.right().saturating_sub(x) as usize,
                    settings.ambiguous_width,
                    span.style,
                );
                x = x.saturating_add(width(&span.content, settings.ambiguous_width) as u16);
            }
        }
        if inner.height > 0 {
            write_text(
                frame.buffer_mut(),
                inner.x,
                inner.bottom() - 1,
                "Esc Back  ·  j/k Scroll  ·  q Quit",
                inner.width as usize,
                settings.ambiguous_width,
                styles.muted_text,
            );
        }
    }
}

#[async_trait::async_trait]
impl LiveView for NewsView<'_> {
    fn tick(&self) -> crate::metadata::TickKind {
        crate::metadata::TickKind::News
    }
    fn title(&self) -> String {
        self.args.symbol.as_deref().map_or_else(
            || "News · Market".into(),
            |symbol| format!("News · {}", normalize_symbol(symbol)),
        )
    }
    fn table(&self) -> TableData {
        table(&self.items)
    }
    fn content(&self) -> serde_json::Value {
        json!({"items": self.table().json(), "articles": self.articles})
    }
    fn priority(&self) -> &[usize] {
        &[2, 0, 1]
    }
    fn on_market(&mut self, _: &MarketEvent) {}
    fn refresh_interval(&self) -> Option<Duration> {
        Some(REFRESH_INTERVAL)
    }
    async fn refresh(&mut self, _: &QuoteServices) -> Result<()> {
        let previous_index = self.selected_index();
        self.items = load_items(&self.args, self.api).await?;
        if !self
            .items
            .iter()
            .any(|item| Some(&item.id) == self.selected_id.as_ref())
        {
            self.selected_id = self
                .items
                .get(previous_index.min(self.items.len().saturating_sub(1)))
                .map(|item| item.id.clone());
        }
        Ok(())
    }
    async fn on_key(
        &mut self,
        key: KeyCode,
        _: &QuoteServices,
        state: &mut ViewState,
    ) -> Result<bool> {
        if self.open_article.is_some() {
            match key {
                KeyCode::Esc => self.open_article = None,
                KeyCode::Down | KeyCode::Char('j') => self
                    .detail_scroll
                    .set(self.detail_scroll.get().saturating_add(1)),
                KeyCode::Up | KeyCode::Char('k') => self
                    .detail_scroll
                    .set(self.detail_scroll.get().saturating_sub(1)),
                KeyCode::PageDown => self.detail_scroll.set(
                    self.detail_scroll
                        .get()
                        .saturating_add(self.page_size.get()),
                ),
                KeyCode::PageUp => self.detail_scroll.set(
                    self.detail_scroll
                        .get()
                        .saturating_sub(self.page_size.get()),
                ),
                KeyCode::Home | KeyCode::Char('g') => self.detail_scroll.set(0),
                KeyCode::End | KeyCode::Char('G') => self.detail_scroll.set(usize::MAX),
                _ => (),
            }
            return Ok(true);
        }
        let selected = self.selected_index();
        let next = match key {
            KeyCode::Enter => {
                if let Some(item) = self.items.get(selected) {
                    if !self.articles.contains_key(&item.id) {
                        self.articles
                            .insert(item.id.clone(), self.api.article(&item.id).await?);
                    }
                    self.open_article = Some(item.id.clone());
                    self.detail_scroll.set(0);
                }
                return Ok(true);
            }
            KeyCode::Down | KeyCode::Char('j') => selected.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => selected.saturating_sub(1),
            KeyCode::PageDown => selected.saturating_add(self.page_size.get()),
            KeyCode::PageUp => selected.saturating_sub(self.page_size.get()),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => self.items.len().saturating_sub(1),
            _ => return Ok(false),
        }
        .min(self.items.len().saturating_sub(1));
        state.selected = next;
        self.selected_id = self.items.get(next).map(|item| item.id.clone());
        Ok(true)
    }
    fn help(&self) -> &'static str {
        "Enter  Read selected article\nEsc  Back to list\nj / k / Ctrl+N / Ctrl+P / arrows  Move / scroll\nPageUp / PageDown  Page\ng / G  First / last\nq / Ctrl+C  Quit\n?  Help"
    }
    fn draw(&self, frame: &mut Frame<'_>, state: &mut ViewState, settings: &Settings) {
        if let Some(article) = self
            .open_article
            .as_ref()
            .and_then(|id| self.articles.get(id))
        {
            self.draw_article(frame, article, settings);
            return;
        }
        let styles = settings.styles();
        let block = Block::default()
            .title(format!(" {} ", self.title()))
            .borders(Borders::ALL)
            .style(styles.panel_base())
            .border_style(styles.border);
        let inner = block.inner(frame.area());
        frame.render_widget(block, frame.area());
        if inner.width == 0 || inner.height < 2 {
            return;
        }
        let time_width = if inner.width >= 65 { 16 } else { 5 };
        let source_width = if inner.width >= 65 { 14 } else { 8 };
        let title_width = (inner.width as usize).saturating_sub(time_width + source_width + 4);
        let visible = inner.height.saturating_sub(2) as usize;
        self.page_size.set(visible.max(1));
        state.selected = self.selected_index();
        state.offset = state.offset.min(self.items.len().saturating_sub(visible));
        if state.selected < state.offset {
            state.offset = state.selected;
        }
        if state.selected >= state.offset + visible {
            state.offset = state.selected.saturating_sub(visible.saturating_sub(1));
        }
        for (row, item) in std::iter::once(None)
            .chain(self.items.iter().skip(state.offset).take(visible).map(Some))
            .enumerate()
        {
            let time = item.map_or_else(
                || HEADERS[0].to_owned(),
                |item| {
                    OffsetDateTime::parse(&item.published_at, &Rfc3339)
                        .ok()
                        .map_or_else(
                            || item.published_at.clone(),
                            |time| {
                                if time_width > 5 {
                                    format!(
                                        "{:04}-{:02}-{:02} {:02}:{:02}",
                                        time.year(),
                                        time.month() as u8,
                                        time.day(),
                                        time.hour(),
                                        time.minute()
                                    )
                                } else {
                                    format!("{:02}:{:02}", time.hour(), time.minute())
                                }
                            },
                        )
                },
            );
            let values = [
                time.as_str(),
                item.map_or(HEADERS[1], |item| item.source.as_str()),
                item.map_or(HEADERS[2], |item| item.title.as_str()),
            ];
            let style = if row == 0 { styles.title } else { Style::new() };
            let mut x = inner.x;
            for (value, columns) in values
                .into_iter()
                .zip([time_width, source_width, title_width])
            {
                write_text(
                    frame.buffer_mut(),
                    x,
                    inner.y + row as u16,
                    &pad(value, columns, settings.ambiguous_width, false),
                    inner.right().saturating_sub(x) as usize,
                    settings.ambiguous_width,
                    style,
                );
                x = x.saturating_add(columns as u16 + 2);
            }
            if row > 0 && state.offset + row - 1 == state.selected {
                frame.buffer_mut().set_style(
                    Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
                    styles.selection,
                );
            }
        }
        if self.items.is_empty() {
            write_text(
                frame.buffer_mut(),
                inner.x,
                inner.y + 1,
                "No news",
                inner.width as usize,
                settings.ambiguous_width,
                styles.muted_text,
            );
        }
        write_text(
            frame.buffer_mut(),
            inner.x,
            inner.bottom() - 1,
            "Enter Read  ·  ? Help  ·  q Quit",
            inner.width as usize,
            settings.ambiguous_width,
            styles.muted_text,
        );
    }
}

pub async fn run_live<B: Backend, S: Stream<Item = std::io::Result<Event>> + Unpin>(
    args: &NewsArgs,
    quote_api: &QuoteServices,
    news_api: &dyn NewsApi,
    settings: &Settings,
    terminal: &mut Terminal<B>,
    mut input: S,
    events: &mut TickStream,
) -> Result<()> {
    let Some(items) = crate::runtime::cancellable(load_items(args, news_api), &mut input).await?
    else {
        return Ok(());
    };
    let mut view = NewsView::new(args.clone(), news_api, items);
    let (_market_sender, market) = tokio::sync::mpsc::unbounded_channel();
    crate::runtime::run(
        &mut view, quote_api, settings, terminal, input, market, events,
    )
    .await
}
