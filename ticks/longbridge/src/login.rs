//! The Longbridge login screens and the `tick-longbridge auth` status commands.

use std::{
    io::Write,
    process::{Command, Stdio},
    thread,
};

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
use tick_packages::{OAuthToken, PackageCredentials};
use tick_provider::{
    error::{Error, Result},
    exit_code,
    preferences::Language,
};
use tickloop_ui::hyperlink;

use crate::{
    AUTH_HINT,
    auth::{self, DeviceLogin, LoginNotice, now_seconds},
    cli::AuthAction,
    region::{Issuers, Region},
};

const MINIMUM_COLUMNS: u16 = 36;
const MINIMUM_ROWS: u16 = 10;
const CARD_COLUMNS: u16 = 76;
const CARD_ROWS: u16 = 18;
/// The card's left and right borders.
const CARD_BORDER_COLUMNS: u16 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginOutcome {
    Authorized,
    Cancelled,
}

pub fn inspect(
    action: AuthAction,
    credentials: &PackageCredentials,
    output: &mut impl Write,
) -> Result<()> {
    match action {
        AuthAction::Status => match saved_login(credentials)? {
            SavedLogin::None => writeln!(output, "Not logged in; {AUTH_HINT}.")?,
            SavedLogin::Unreadable(error) => return Err(error),
            SavedLogin::Current(token) => {
                writeln!(output, "Logged in")?;
                writeln!(output, "Expires: {}", expiry_text(token.expires_at))?;
                write_region(output, &token)?;
            }
            SavedLogin::Renewable(token) => {
                writeln!(output, "Logged in; {ACCESS_TOKEN_RENEWS}")?;
                writeln!(
                    output,
                    "Access token expired: {}",
                    expiry_text(token.expires_at)
                )?;
                write_region(output, &token)?;
            }
            SavedLogin::Expired(token) => {
                writeln!(output, "Authorization expired; {AUTH_HINT}.")?;
                writeln!(output, "Expires: {}", expiry_text(token.expires_at))?;
                write_region(output, &token)?;
            }
        },
        AuthAction::Logout => {
            // Without a token there is nothing to lock, and nothing is created.
            if credentials.oauth_token_path().exists() {
                credentials
                    .lock_oauth()
                    .and_then(|files| files.remove_token())
                    .map_err(|error| Error::usage(error.to_string()))?;
            }
            writeln!(output, "Logged out")?;
        }
    }
    Ok(())
}

fn write_region(output: &mut impl Write, token: &OAuthToken) -> Result<()> {
    writeln!(
        output,
        "Region: {}",
        Region::of_issuer(&token.issuer).as_str()
    )?;
    Ok(())
}

/// What the saved token says about the login, read without renewing it.
enum SavedLogin {
    None,
    Unreadable(Error),
    Current(OAuthToken),
    /// The access token lapsed, and its refresh token renews it when a Tick opens.
    Renewable(OAuthToken),
    Expired(OAuthToken),
}

const ACCESS_TOKEN_RENEWS: &str = "the access token is renewed when a Tick opens";

fn saved_login(credentials: &PackageCredentials) -> Result<SavedLogin> {
    Ok(match auth::saved_token(credentials) {
        Err(error) if error.code == exit_code::NOT_AUTHORIZED => {
            if credentials.oauth_token_path().exists() {
                SavedLogin::Unreadable(error)
            } else {
                SavedLogin::None
            }
        }
        Err(error) => return Err(error),
        Ok(token) if token.expires_at > now_seconds() => SavedLogin::Current(token),
        Ok(token) if token.refresh_token().is_some() => SavedLogin::Renewable(token),
        Ok(token) => SavedLogin::Expired(token),
    })
}

fn expiry_text(expires_at: u64) -> String {
    time::OffsetDateTime::from_unix_timestamp(expires_at as i64)
        .ok()
        .and_then(|date| {
            date.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| expires_at.to_string())
}

/// How the waiting screen hands over the verification page besides showing it.
pub struct PageAccess<O: FnOnce(&str) -> bool> {
    /// Opens the page in a browser and says whether it did; the login goes on either way.
    pub open: O,
    /// Whether the terminal shows OSC 8 links, so that every row of a wrapped link opens the
    /// whole page.
    pub links: bool,
}

impl PageAccess<fn(&str) -> bool> {
    /// The system's browser and what this terminal's environment says about links.
    pub fn of_this_terminal() -> Self {
        Self {
            open: open_in_browser,
            links: hyperlink::supported(|name| std::env::var(name).ok()),
        }
    }
}

/// Whether `page` can be opened and linked: an https address with a host and no credentials, and
/// nothing that could break out of an escape sequence.
fn is_safe_page(page: &str) -> bool {
    !page.chars().any(char::is_control)
        && reqwest::Url::parse(page).is_ok_and(|url| {
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
        })
}

/// Opens the verification page in the system's browser, as the Agent's own device logins do, and
/// says whether it started the browser.
pub fn open_in_browser(page: &str) -> bool {
    if !is_safe_page(page) {
        return false;
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let Ok(mut child) = Command::new(opener)
        .arg(page)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    thread::spawn(move || child.wait());
    true
}

/// The login screens, logging in with `issuers` and opening the verification page in the
/// system's browser.
pub async fn run<B, S>(
    settings: &Settings,
    credentials: &PackageCredentials,
    issuers: &Issuers,
    terminal: &mut Terminal<B>,
    input: S,
) -> Result<LoginOutcome>
where
    B: Backend,
    S: Stream<Item = std::io::Result<Event>> + Unpin,
{
    run_with_page_access(
        settings,
        credentials,
        issuers,
        terminal,
        input,
        PageAccess::of_this_terminal(),
    )
    .await
}

/// The login screens, logging in with `issuers` and handing over the verification page through
/// `page_access`. The login is saved in `credentials`, where Tickloop keeps it too.
pub async fn run_with_page_access<B, S, O>(
    settings: &Settings,
    credentials: &PackageCredentials,
    issuers: &Issuers,
    terminal: &mut Terminal<B>,
    mut input: S,
    page_access: PageAccess<O>,
) -> Result<LoginOutcome>
where
    B: Backend,
    S: Stream<Item = std::io::Result<Event>> + Unpin,
    O: FnOnce(&str) -> bool,
{
    let saved = match auth::saved_token(credentials) {
        Ok(token) => Some(token),
        Err(error) if error.code == exit_code::NOT_AUTHORIZED => None,
        Err(error) => return Err(error),
    };
    let mut region = saved
        .as_ref()
        .map_or(Region::Cn, |token| Region::of_issuer(&token.issuer));
    if let Some(token) = saved.filter(|token| token.expires_at > now_seconds()) {
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
                    KeyCode::Enter => return Ok(LoginOutcome::Authorized),
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

    clear(terminal)?;
    draw(terminal, settings, Screen::Starting(region))?;
    let login = begin_or_cancel(
        DeviceLogin::start(credentials, issuers.of(region)),
        &mut input,
    )
    .await?;
    let Some(login) = login else {
        return Ok(LoginOutcome::Cancelled);
    };
    let opened = (page_access.open)(login.verification_uri());
    let links = page_access.links;

    clear(terminal)?;
    let (notices, mut updates) = tokio::sync::mpsc::unbounded_channel();
    let completion = login.finish(&notices);
    tokio::pin!(completion);
    let mut status = waiting_text(settings.language).to_owned();
    let mut refresh = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        draw(
            terminal,
            settings,
            Screen::Waiting {
                login: &login,
                opened,
                links,
                status: &status,
            },
        )?;
        tokio::select! {
            result = &mut completion => {
                result?;
                clear(terminal)?;
                draw(terminal, settings, Screen::Authorized)?;
                return Ok(LoginOutcome::Authorized);
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
        /// Whether the verification page was opened in the browser.
        opened: bool,
        /// Whether the terminal shows OSC 8 links.
        links: bool,
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

    // The card widens for a verification link longer than it, so the link stays on one line
    // where the terminal is wide enough.
    let card_columns = match &screen {
        Screen::Waiting { login, .. } => CARD_COLUMNS.max(
            u16::try_from(Line::raw(login.verification_uri()).width())
                .unwrap_or(u16::MAX)
                .saturating_add(CARD_BORDER_COLUMNS),
        ),
        _ => CARD_COLUMNS,
    };
    let card = centered(area, card_columns, CARD_ROWS);
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
        Screen::Waiting {
            login,
            opened,
            links,
            status,
        } => render_waiting(frame, inner, settings, login, opened, links, status),
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
    opened: bool,
    links: bool,
    status: &str,
) {
    let styles = settings.styles();
    let body = body_area(area);
    let width = usize::from(body.width);
    let fits = |link: &str| Line::raw(link).width() <= width;
    let complete = login.verification_uri();
    let plain = login.plain_verification_uri();
    let is_linked = links && is_safe_page(complete);
    // A terminal detects only the row it is clicked on of a link wrapped across rows, unless
    // every row is an OSC 8 link to the whole page. Without those links, a page with the code
    // filled in that does not fit gives way to the plain page, where the user enters the code.
    let link = if fits(complete) || is_linked || !fits(plain) {
        complete
    } else {
        plain
    };
    let has_code = link != plain;
    let link_rows = hyperlink::split_rows(link, width);
    let open = match (settings.language, opened) {
        (Language::ZhCn, true) => "1  已在浏览器中打开；没有打开时请访问",
        (Language::ZhCn, false) => "1  在浏览器中打开",
        (Language::En, true) => "1  Opened in your browser; if it did not open, visit",
        (Language::En, false) => "1  Open in your browser",
    };
    let footer = match settings.language {
        Language::ZhCn => "q 取消",
        Language::En => "q cancel",
    };
    let confirm = match (settings.language, has_code) {
        (Language::ZhCn, true) => "2  核对设备码并批准",
        (Language::ZhCn, false) => "2  输入设备码并批准",
        (Language::En, true) => "2  Confirm and approve this code",
        (Language::En, false) => "2  Enter this code and approve",
    };
    let remaining = login.remaining().as_secs();
    let minutes = remaining / 60;
    let seconds = remaining % 60;
    let mut lines = vec![Line::styled(open, styles.muted_text)];
    lines.extend(
        link_rows
            .iter()
            .map(|row| Line::styled(row.as_str(), styles.link)),
    );
    lines.extend([
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
        Paragraph::new(Text::from(lines))
            .style(styles.overlay_base())
            .wrap(Wrap { trim: false }),
        body,
    );
    if is_linked {
        hyperlink::link_rows(frame.buffer_mut(), body, &link_rows, complete);
    }
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
        (Language::ZhCn, LoginNotice::SlowerPolling) => "Longbridge 要求降低检查频率",
        (Language::En, notice) => notice.message(),
    }
}
