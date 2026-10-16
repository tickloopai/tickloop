use std::{cell::Cell, io::Write, sync::Arc, time::Duration};

use crossterm::event::{Event, KeyCode};
use futures_util::Stream;
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::Rect,
    style::Style,
    widgets::{Block, Borders},
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tickloop_ui::{
    format::{pad, width, write_text},
    markdown,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{
    args::CommunityArgs,
    config::Settings,
    output::TableData,
    runtime::{LiveView, NoMarketApi, ViewState},
    tools::Tools,
};
use tick_protocol::SeriesEnd;
use tick_provider::{
    api::{MarketEvent, QuoteServices},
    community::{CommunityApi, MemberPost, MemberProfile, reply_outcome_unknown},
    error::{Error, Result},
};
use tick_sdk::{stream::TickStream, tools::ToolCall};

const HEADERS: &[&str] = &["Published At", "Kind", "Title", "Text", "Topic Id", "Url"];
const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
/// A reply runs on the Panel's UI path, so it may hold the Panel only this long.
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);
const POSTS_TOOL: &str = "posts";
const REPLY_TOOL: &str = "reply";

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReplyArgs {
    /// The `topic_id` of one of the followed member's posts.
    topic_id: i64,
    /// Plain text; Markdown is not rendered, and symbols such as TSLA.US become stock links.
    body: String,
}

pub fn tools() -> Tools {
    tools_for(None)
}

fn tools_for(source: Option<(Arc<dyn CommunityApi>, i64)>) -> Tools {
    Tools::default()
        .query::<EmptyArgs>(
            POSTS_TOOL,
            "Read the followed member and their newest posts again, in this Tick's state shape.",
            move |_: &QuoteServices, _| {
                let source = source.clone();
                Box::pin(async move {
                    let (api, member_id) =
                        source.ok_or_else(|| Error::usage("community service is not attached"))?;
                    Ok(load(api.as_ref(), member_id).await?.content())
                })
            },
        )
        .action::<ReplyArgs>(
            REPLY_TOOL,
            "Publicly reply under one of the followed member's posts, as the logged-in Longbridge user. A topic_id missing from this Tick's state is checked against a fresh read first. Sent once and never retried; returns the posted reply. A failure that says the outcome is unknown may still have posted the reply, so check the post before replying again.",
        )
        .series("/posts", SeriesEnd::First)
        .recover_with(POSTS_TOOL)
}

/// What the Tick shows and publishes: the member and their newest posts, newest first.
struct Feed {
    member: MemberProfile,
    posts: Vec<MemberPost>,
}

impl Feed {
    fn lists(&self, topic_id: i64) -> bool {
        self.posts.iter().any(|post| post.topic_id == topic_id)
    }

    fn content(&self) -> Value {
        json!({"member": self.member, "posts": self.posts})
    }

    fn table(&self) -> TableData {
        TableData {
            headers: HEADERS.to_vec(),
            rows: self
                .posts
                .iter()
                .map(|post| {
                    vec![
                        json!(post.published_at),
                        json!(post.kind.label()),
                        json!(post.title),
                        json!(post.text),
                        json!(post.topic_id),
                        json!(post.url),
                    ]
                })
                .collect(),
        }
    }
}

async fn load(api: &dyn CommunityApi, member_id: i64) -> Result<Feed> {
    let (member, posts) = tokio::try_join!(api.profile(member_id), api.posts(member_id))?;
    Ok(Feed { member, posts })
}

pub async fn run_once(
    args: &CommunityArgs,
    api: &dyn CommunityApi,
    settings: &Settings,
    color: bool,
    output: &mut impl Write,
    events: &mut TickStream,
) -> Result<()> {
    let feed = load(api, args.member_id).await?;
    events.publish(feed.content())?;
    feed.table()
        .write(args.output.format, settings, color, output)
}

/// The list row's headline: the title of an article, otherwise the post's opening text.
fn headline(post: &MemberPost) -> String {
    let text = if post.title.is_empty() {
        &post.text
    } else {
        &post.title
    };
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn format_time(published_at: &str, is_wide: bool) -> String {
    OffsetDateTime::parse(published_at, &Rfc3339).map_or_else(
        |_| published_at.to_owned(),
        |time| {
            if is_wide {
                format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}",
                    time.year(),
                    time.month() as u8,
                    time.day(),
                    time.hour(),
                    time.minute()
                )
            } else {
                format!("{:02}-{:02}", time.month() as u8, time.day())
            }
        },
    )
}

struct CommunityView {
    member_id: i64,
    api: Arc<dyn CommunityApi>,
    feed: Feed,
    selected_topic: Option<i64>,
    open_topic: Option<i64>,
    detail_scroll: Cell<usize>,
    page_size: Cell<usize>,
}

impl CommunityView {
    fn new(member_id: i64, api: Arc<dyn CommunityApi>, feed: Feed) -> Self {
        let selected_topic = feed.posts.first().map(|post| post.topic_id);
        Self {
            member_id,
            api,
            feed,
            selected_topic,
            open_topic: None,
            detail_scroll: Cell::new(0),
            page_size: Cell::new(1),
        }
    }

    fn selected_index(&self) -> usize {
        self.feed
            .posts
            .iter()
            .position(|post| Some(post.topic_id) == self.selected_topic)
            .unwrap_or(0)
    }

    /// Shows a newer read of the feed, keeping the selected post when it is still listed and
    /// otherwise the selected position; an open post that disappeared closes.
    fn show(&mut self, feed: Feed) {
        let previous_index = self.selected_index();
        self.feed = feed;
        if !self
            .selected_topic
            .is_some_and(|topic| self.feed.lists(topic))
        {
            let posts = &self.feed.posts;
            self.selected_topic = posts
                .get(previous_index.min(posts.len().saturating_sub(1)))
                .map(|post| post.topic_id);
        }
        if self.open_topic.is_some_and(|topic| !self.feed.lists(topic)) {
            self.open_topic = None;
        }
    }

    async fn reply(&mut self, args: ReplyArgs) -> Result<Value> {
        let body = args.body.trim();
        if body.is_empty() {
            return Err(Error::usage("a reply needs text"));
        }
        if !self.feed.lists(args.topic_id) {
            let feed = load(self.api.as_ref(), self.member_id).await?;
            self.show(feed);
        }
        if !self.feed.lists(args.topic_id) {
            return Err(Error::usage(format!(
                "topic {} is not one of {}'s newest posts; read the Tick and reply to a listed topic_id",
                args.topic_id, self.feed.member.nickname
            )));
        }
        let reply = tokio::time::timeout(REPLY_TIMEOUT, self.api.reply(args.topic_id, body))
            .await
            .map_err(|_| reply_outcome_unknown("no answer before the reply timeout"))??;
        serde_json::to_value(reply).map_err(|error| Error::usage(error.to_string()))
    }

    fn draw_post(&self, frame: &mut Frame<'_>, post: &MemberPost, settings: &Settings) {
        let area = frame.area();
        let styles = settings.styles();
        let block = Block::default()
            .title(format!(
                " {} · {} ",
                self.feed.member.nickname,
                post.kind.label()
            ))
            .borders(Borders::ALL)
            .style(styles.panel_base())
            .border_style(styles.border);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let heading = if post.title.is_empty() {
            String::new()
        } else {
            format!("# {}\n\n", post.title)
        };
        let markdown = format!(
            "{heading}{}\n\n{} · topic {}\n\n[Open on Longbridge]({})",
            post.text.replace('\n', "\n\n"),
            format_time(&post.published_at, true),
            post.topic_id,
            post.url
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
                "Backspace Back · j/k Scroll · q Quit",
                inner.width as usize,
                settings.ambiguous_width,
                styles.muted_text,
            );
        }
    }
}

#[async_trait::async_trait]
impl LiveView for CommunityView {
    fn tools(&self) -> Tools {
        tools_for(Some((self.api.clone(), self.member_id)))
    }
    fn tick(&self) -> crate::metadata::TickKind {
        crate::metadata::TickKind::Community
    }
    fn title(&self) -> String {
        format!("Community · {}", self.feed.member.nickname)
    }
    fn table(&self) -> TableData {
        self.feed.table()
    }
    fn content(&self) -> Value {
        self.feed.content()
    }
    fn priority(&self) -> &[usize] {
        &[3, 0, 1]
    }
    fn on_market(&mut self, _: &MarketEvent) {}
    fn refresh_interval(&self) -> Option<Duration> {
        Some(REFRESH_INTERVAL)
    }
    async fn refresh(&mut self, _: &QuoteServices) -> Result<()> {
        let feed = load(self.api.as_ref(), self.member_id).await?;
        self.show(feed);
        Ok(())
    }
    async fn on_key(
        &mut self,
        key: KeyCode,
        _: &QuoteServices,
        state: &mut ViewState,
    ) -> Result<bool> {
        if self.open_topic.is_some() {
            let scroll = self.detail_scroll.get();
            let page = self.page_size.get();
            let next_scroll = match key {
                KeyCode::Backspace => {
                    self.open_topic = None;
                    scroll
                }
                KeyCode::Down | KeyCode::Char('j') => scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => scroll.saturating_sub(1),
                KeyCode::PageDown => scroll.saturating_add(page),
                KeyCode::PageUp => scroll.saturating_sub(page),
                KeyCode::Home | KeyCode::Char('g') => 0,
                KeyCode::End | KeyCode::Char('G') => usize::MAX,
                _ => scroll,
            };
            self.detail_scroll.set(next_scroll);
            return Ok(true);
        }
        let selected = self.selected_index();
        let last = self.feed.posts.len().saturating_sub(1);
        let next = match key {
            KeyCode::Enter => {
                if let Some(post) = self.feed.posts.get(selected) {
                    self.open_topic = Some(post.topic_id);
                    self.detail_scroll.set(0);
                }
                return Ok(true);
            }
            KeyCode::Down | KeyCode::Char('j') => selected + 1,
            KeyCode::Up | KeyCode::Char('k') => selected.saturating_sub(1),
            KeyCode::PageDown => selected + self.page_size.get(),
            KeyCode::PageUp => selected.saturating_sub(self.page_size.get()),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => last,
            _ => return Ok(false),
        }
        .min(last);
        state.selected = next;
        self.selected_topic = self.feed.posts.get(next).map(|post| post.topic_id);
        Ok(true)
    }
    async fn act(&mut self, call: ToolCall) -> Result<Value> {
        match call.tool.as_str() {
            REPLY_TOOL => self.reply(call.arguments()?).await,
            tool => Err(Error::usage(format!(
                "this Tick offers no action Tick Tool {tool:?}"
            ))),
        }
    }
    fn help(&self) -> &'static str {
        "Enter  Read selected post\nBackspace  Back to list\nj / k / Ctrl+N / Ctrl+P / arrows  Move / scroll\nPageUp / PageDown  Page\ng / G  First / last\nq / Ctrl+C  Quit\n?  Help"
    }
    fn draw(&self, frame: &mut Frame<'_>, state: &mut ViewState, settings: &Settings) {
        if let Some(post) = self
            .open_topic
            .and_then(|topic| self.feed.posts.iter().find(|post| post.topic_id == topic))
        {
            self.draw_post(frame, post, settings);
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
        let is_wide = inner.width >= 65;
        let time_width = if is_wide { 16 } else { 5 };
        let kind_width = 7;
        let text_width = (inner.width as usize).saturating_sub(time_width + kind_width + 4);
        let visible = inner.height.saturating_sub(2) as usize;
        self.page_size.set(visible.max(1));
        let posts = &self.feed.posts;
        state.selected = self.selected_index();
        state.offset = state.offset.min(posts.len().saturating_sub(visible));
        if state.selected < state.offset {
            state.offset = state.selected;
        }
        if state.selected >= state.offset + visible {
            state.offset = state.selected.saturating_sub(visible.saturating_sub(1));
        }
        for (row, post) in std::iter::once(None)
            .chain(posts.iter().skip(state.offset).take(visible).map(Some))
            .enumerate()
        {
            let values = match post {
                None => [
                    HEADERS[0].to_owned(),
                    HEADERS[1].to_owned(),
                    "Post".to_owned(),
                ],
                Some(post) => [
                    format_time(&post.published_at, is_wide),
                    post.kind.label().to_owned(),
                    headline(post),
                ],
            };
            let style = if row == 0 { styles.title } else { Style::new() };
            let mut x = inner.x;
            for (value, columns) in values.iter().zip([time_width, kind_width, text_width]) {
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
                state.focus_row = Some(inner.y + row as u16);
                frame.buffer_mut().set_style(
                    Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
                    styles.selection,
                );
            }
        }
        if posts.is_empty() {
            write_text(
                frame.buffer_mut(),
                inner.x,
                inner.y + 1,
                "No posts",
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
    args: &CommunityArgs,
    api: Arc<dyn CommunityApi>,
    settings: &Settings,
    terminal: &mut Terminal<B>,
    mut input: S,
    events: &mut TickStream,
) -> Result<()> {
    let Some(feed) =
        crate::runtime::cancellable(load(api.as_ref(), args.member_id), &mut input).await?
    else {
        return Ok(());
    };
    let mut view = CommunityView::new(args.member_id, api, feed);
    let (_market_sender, market) = tokio::sync::mpsc::unbounded_channel();
    crate::runtime::run(
        &mut view,
        &NoMarketApi,
        settings,
        terminal,
        input,
        market,
        events,
    )
    .await
}
