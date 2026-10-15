//! The Longbridge login screens and the `tick-longbridge auth` status commands.

use std::{io::Write, path::Path};

use crossterm::event::{Event, KeyCode, KeyEventKind};
use futures_util::{Stream, StreamExt};
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::Rect,
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use tick_market::{config::Settings, runtime::is_exit};
use tick_provider::{
    error::{Error, Result},
    preferences::{Language, Region},
};

use crate::{
    auth::{
        check_noninteractive,
        device::{
            AuthEndpoints, DeviceLogin, LoginNotice, begin_login, complete_login, save_region,
        },
        storage::{AuthStorage, now_seconds},
    },
    cli::AuthAction,
};

const MINIMUM_COLUMNS: u16 = 36;
const MINIMUM_ROWS: u16 = 10;
const CARD_COLUMNS: u16 = 76;
const CARD_ROWS: u16 = 18;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginOutcome {
    Authorized(Region),
    Cancelled,
}

pub fn inspect(
    action: AuthAction,
    storage: &AuthStorage,
    region: Option<Region>,
    output: &mut impl Write,
) -> Result<()> {
    match action {
        AuthAction::Status => {
            match storage.token()? {
                None => writeln!(output, "Not logged in. Run `tick-longbridge auth`.")?,
                Some(token) => {
                    writeln!(
                        output,
                        "{}",
                        if token.expires_at > now_seconds() {
                            "Logged in"
                        } else {
                            "Authorization expired; run `tick-longbridge auth`"
                        }
                    )?;
                    let expires =
                        time::OffsetDateTime::from_unix_timestamp(token.expires_at as i64)
                            .ok()
                            .and_then(|date| {
                                date.format(&time::format_description::well_known::Rfc3339)
                                    .ok()
                            })
                            .unwrap_or_else(|| token.expires_at.to_string());
                    writeln!(output, "Expires: {expires}")?;
                }
            }
            writeln!(
                output,
                "Region: {}",
                region.map_or("not configured", Region::as_str)
            )?;
        }
        AuthAction::Check => check_noninteractive(storage)?,
        AuthAction::Logout => {
            storage.logout()?;
            writeln!(output, "Logged out")?;
        }
    }
    Ok(())
}

pub async fn run<B, S>(
    settings: &Settings,
    config_path: &Path,
    storage: &AuthStorage,
    endpoints: Option<AuthEndpoints>,
    terminal: &mut Terminal<B>,
    mut input: S,
) -> Result<LoginOutcome>
where
    B: Backend,
    S: Stream<Item = std::io::Result<Event>> + Unpin,
{
    let mut region = settings.region.unwrap_or(Region::Cn);
    if let Some(token) = storage
        .token()?
        .filter(|token| token.expires_at > now_seconds())
    {
        loop {
            draw(
                terminal,
                settings,
                Screen::SignedIn {
                    region,
                    expires_at: token.expires_at,
                },
            )?;
            match next_event(&mut input).await? {
                None => return Ok(LoginOutcome::Cancelled),
                Some(event) if is_exit(&event) => return Ok(LoginOutcome::Cancelled),
                Some(Event::Key(key)) if key.kind != KeyEventKind::Release => match key.code {
                    KeyCode::Enter => return Ok(LoginOutcome::Authorized(region)),
                    KeyCode::Char('r') => break,
                    _ => {}
                },
                Some(Event::Resize(_, _)) => terminal
                    .clear()
                    .map_err(|error| Error::usage(error.to_string()))?,
                Some(_) => {}
            }
        }
    }
    loop {
        draw(terminal, settings, Screen::Region(region))?;
        match next_event(&mut input).await? {
            None => return Ok(LoginOutcome::Cancelled),
            Some(event) if is_exit(&event) => return Ok(LoginOutcome::Cancelled),
            Some(Event::Key(key)) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                    region = match region {
                        Region::Cn => Region::Global,
                        Region::Global => Region::Cn,
                    };
                }
                KeyCode::Enter => break,
                _ => {}
            },
            Some(Event::Resize(_, _)) => terminal
                .clear()
                .map_err(|error| Error::usage(error.to_string()))?,
            Some(_) => {}
        }
    }

    save_region(config_path, region)?;
    clear(terminal)?;
    draw(terminal, settings, Screen::Starting(region))?;
    let login =
        begin_or_cancel(begin_login(region, storage, endpoints.clone()), &mut input).await?;
    let Some(login) = login else {
        return Ok(LoginOutcome::Cancelled);
    };

    clear(terminal)?;
    let (notices, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let completion = complete_login(region, storage, endpoints, &login, &notices);
    tokio::pin!(completion);
    let mut status = waiting_text(settings.language).to_owned();
    let mut refresh = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        draw(
            terminal,
            settings,
            Screen::Waiting {
                login: &login,
                status: &status,
            },
        )?;
        tokio::select! {
            result = &mut completion => {
                result?;
                clear(terminal)?;
                draw(terminal, settings, Screen::Authorized)?;
                return Ok(LoginOutcome::Authorized(region));
            }
            notice = updates.recv() => {
                if let Some(notice) = notice {
                    status = notice_text(settings.language, notice).to_owned();
                }
            }
            _ = refresh.tick() => {}
            event = input.next() => match event {
                None => return Ok(LoginOutcome::Cancelled),
                Some(Err(error)) => return Err(error.into()),
                Some(Ok(event)) if is_exit(&event) => return Ok(LoginOutcome::Cancelled),
                Some(Ok(Event::Resize(_, _))) => terminal
                    .clear()
                    .map_err(|error| Error::usage(error.to_string()))?,
                Some(Ok(_)) => {}
            }
        }
    }
}

fn clear<B: Backend>(terminal: &mut Terminal<B>) -> Result<()> {
    terminal
        .clear()
        .map_err(|error| Error::usage(error.to_string()))
}

async fn begin_or_cancel<T, S>(
    future: impl std::future::Future<Output = Result<T>>,
    input: &mut S,
) -> Result<Option<T>>
where
    S: Stream<Item = std::io::Result<Event>> + Unpin,
{
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return result.map(Some),
            event = input.next() => match event {
                None => return Ok(None),
                Some(Err(error)) => return Err(error.into()),
                Some(Ok(event)) if is_exit(&event) => return Ok(None),
                Some(Ok(_)) => {}
            }
        }
    }
}

async fn next_event<S>(input: &mut S) -> Result<Option<Event>>
where
    S: Stream<Item = std::io::Result<Event>> + Unpin,
{
    input.next().await.transpose().map_err(Into::into)
}

enum Screen<'a> {
    SignedIn {
        region: Region,
        expires_at: u64,
    },
    Region(Region),
    Starting(Region),
    Waiting {
        login: &'a DeviceLogin,
        status: &'a str,
    },
    Authorized,
}

fn draw<B: Backend>(
    terminal: &mut Terminal<B>,
    settings: &Settings,
    screen: Screen<'_>,
) -> Result<()> {
    terminal
        .draw(|frame| render(frame, settings, screen))
        .map_err(|error| Error::usage(error.to_string()))?;
    Ok(())
}

fn render(frame: &mut Frame<'_>, settings: &Settings, screen: Screen<'_>) {
    let area = frame.area();
    let styles = settings.styles();
    frame.render_widget(Clear, area);
    frame.buffer_mut().set_style(area, styles.canvas);
    if area.width < MINIMUM_COLUMNS || area.height < MINIMUM_ROWS {
        let message = match settings.language {
            Language::ZhCn => format!(
                "连接 Longbridge\n\n需要 {}×{}\n当前 {}×{}\n\n请扩大 Panel · q 退出",
                MINIMUM_COLUMNS, MINIMUM_ROWS, area.width, area.height
            ),
            Language::En => format!(
                "Connect Longbridge\n\nNeed {}x{}\nCurrent {}x{}\n\nResize panel · q quit",
                MINIMUM_COLUMNS, MINIMUM_ROWS, area.width, area.height
            ),
        };
        frame.render_widget(
            Paragraph::new(message).style(styles.canvas.patch(styles.text)),
            area,
        );
        return;
    }

    let card = centered(area, CARD_COLUMNS, CARD_ROWS);
    frame.buffer_mut().set_style(card, styles.overlay_base());
    let title = match settings.language {
        Language::ZhCn => " 连接 Longbridge ",
        Language::En => " Connect Longbridge ",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(styles.border)
        .title(Span::styled(title, styles.title));
    let inner = block.inner(card);
    frame.render_widget(block.style(styles.overlay_base()), card);

    match screen {
        Screen::SignedIn { region, expires_at } => {
            render_signed_in(frame, inner, settings, region, expires_at);
        }
        Screen::Region(region) => render_region(frame, inner, settings, region),
        Screen::Starting(region) => render_starting(frame, inner, settings, region),
        Screen::Waiting { login, status } => render_waiting(frame, inner, settings, login, status),
        Screen::Authorized => render_authorized(frame, inner, settings),
    }
}

fn render_signed_in(
    frame: &mut Frame<'_>,
    area: Rect,
    settings: &Settings,
    region: Region,
    expires_at: u64,
) {
    let styles = settings.styles();
    let expires = time::OffsetDateTime::from_unix_timestamp(expires_at as i64)
        .ok()
        .and_then(|date| {
            date.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| expires_at.to_string());
    let (heading, detail, hint) = match settings.language {
        Language::ZhCn => (
            "已登录 Longbridge".to_owned(),
            format!("接入区域 {} · 有效期至 {expires}", region.as_str()),
            "Enter 继续    r 重新授权    q 退出",
        ),
        Language::En => (
            "Signed in to Longbridge".to_owned(),
            format!("Region {} · valid until {expires}", region.as_str()),
            "Enter continue    r sign in again    q quit",
        ),
    };
    let text = Text::from(vec![
        Line::styled(heading, styles.title),
        Line::raw(""),
        Line::styled(detail, styles.text),
        Line::raw(""),
        Line::styled(hint, styles.muted_text),
    ]);
    frame.render_widget(
        Paragraph::new(text)
            .style(styles.overlay_base())
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_region(frame: &mut Frame<'_>, area: Rect, settings: &Settings, selected: Region) {
    let styles = settings.styles();
    let (heading, hint, cn_description, global_description) = match settings.language {
        Language::ZhCn => (
            "选择接入区域",
            "Enter 确认    q 退出",
            "中国大陆网络",
            "全球网络",
        ),
        Language::En => (
            "Choose access region",
            "Enter confirm    q quit",
            "Mainland China network",
            "Global network",
        ),
    };
    let option = |region, label: &'static str, description: &'static str| {
        let active = region == selected;
        Line::from(vec![
            Span::styled(
                if active { "> " } else { "  " },
                if active {
                    styles.selection
                } else {
                    styles.text
                },
            ),
            Span::styled(
                format!("{label:<12}"),
                if active {
                    styles.selection
                } else {
                    styles.text
                },
            ),
            Span::styled(description, styles.muted_text),
        ])
    };
    let text = Text::from(vec![
        Line::styled(heading, styles.strong),
        Line::default(),
        option(Region::Cn, "中国大陆", cn_description),
        Line::default(),
        option(Region::Global, "Global", global_description),
    ]);
    frame.render_widget(
        Paragraph::new(text).style(styles.overlay_base()),
        body_area(area),
    );
    render_footer(frame, area, settings, hint);
}

fn render_starting(frame: &mut Frame<'_>, area: Rect, settings: &Settings, region: Region) {
    let styles = settings.styles();
    let region_name = match region {
        Region::Cn => "中国大陆",
        Region::Global => "Global",
    };
    let message = match settings.language {
        Language::ZhCn => format!("正在通过 {region_name} 接入点申请设备码…"),
        Language::En => format!("Requesting a device code through {region_name}…"),
    };
    frame.render_widget(
        Paragraph::new(message).style(styles.overlay_base().patch(styles.muted_text)),
        body_area(area),
    );
    render_footer(
        frame,
        area,
        settings,
        match settings.language {
            Language::ZhCn => "q 取消",
            Language::En => "q cancel",
        },
    );
}

fn render_waiting(
    frame: &mut Frame<'_>,
    area: Rect,
    settings: &Settings,
    login: &DeviceLogin,
    status: &str,
) {
    let styles = settings.styles();
    let (open, confirm, footer) = match settings.language {
        Language::ZhCn => ("1  在浏览器中打开", "2  核对设备码并批准", "q 取消"),
        Language::En => (
            "1  Open in your browser",
            "2  Confirm and approve this code",
            "q cancel",
        ),
    };
    let remaining = login.remaining().as_secs();
    let minutes = remaining / 60;
    let seconds = remaining % 60;
    let content = Text::from(vec![
        Line::styled(open, styles.muted_text),
        Line::styled(login.verification_uri(), styles.link),
        Line::default(),
        Line::styled(confirm, styles.muted_text),
        Line::styled(login.user_code(), styles.strong),
        Line::default(),
        Line::from(vec![
            Span::styled(status, styles.text),
            Span::styled(format!("  ·  {minutes:02}:{seconds:02}"), styles.muted_text),
        ]),
    ]);
    frame.render_widget(
        Paragraph::new(content)
            .style(styles.overlay_base())
            .wrap(Wrap { trim: false }),
        body_area(area),
    );
    render_footer(frame, area, settings, footer);
}

fn render_authorized(frame: &mut Frame<'_>, area: Rect, settings: &Settings) {
    let styles = settings.styles();
    let (heading, body) = match settings.language {
        Language::ZhCn => ("授权成功", "Longbridge 凭据已保存，正在继续。"),
        Language::En => (
            "Authorized",
            "Longbridge credentials are saved. Continuing.",
        ),
    };
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::styled(heading, styles.strong),
            Line::default(),
            Line::styled(body, styles.text),
        ]))
        .style(styles.overlay_base()),
        body_area(area),
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, settings: &Settings, text: &str) {
    let styles = settings.styles();
    frame.render_widget(
        Paragraph::new(text).style(styles.status_base().patch(styles.muted_text)),
        Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
    );
}

fn body_area(area: Rect) -> Rect {
    Rect::new(
        area.x,
        area.y + 1,
        area.width,
        area.height.saturating_sub(3),
    )
}

fn centered(area: Rect, maximum_width: u16, maximum_height: u16) -> Rect {
    let width = area.width.min(maximum_width);
    let height = area.height.min(maximum_height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn waiting_text(language: Language) -> &'static str {
    match language {
        Language::ZhCn => "等待网页确认",
        Language::En => "Waiting for browser approval",
    }
}

fn notice_text(language: Language, notice: LoginNotice) -> &'static str {
    match (language, notice) {
        (Language::ZhCn, LoginNotice::TemporaryFailureRetry) => "授权服务暂时不可用，正在自动重试",
        (Language::ZhCn, LoginNotice::EndpointUnavailable) => "正在改用另一个授权端点",
        (Language::ZhCn, LoginNotice::SlowerPolling) => "Longbridge 要求降低检查频率",
        (Language::En, notice) => notice.message(),
    }
}
