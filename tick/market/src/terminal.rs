use crossterm::{
    cursor::{Hide, Show},
    event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::{Stream, StreamExt};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    fs::File,
    io::{IsTerminal, Stdout},
    os::fd::AsRawFd,
};
use tick_provider::error::{Error, Result};

static TERMINAL_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static PANIC_HOOK: std::sync::Once = std::sync::Once::new();
static PANIC_TTY: std::sync::Mutex<Option<File>> = std::sync::Mutex::new(None);
static PANIC_TERMIOS: std::sync::Mutex<Option<libc::termios>> = std::sync::Mutex::new(None);
const INPUT_POLL_TIMEOUT_MS: i32 = 50;
const ESCAPE_SEQUENCE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(35);

pub struct TerminalSession {
    pub terminal: Terminal<crate::viewport::ViewportBackend<CrosstermBackend<Stdout>>>,
    raw: bool,
    alternate: bool,
}

pub struct VisibleTerminalSession {
    pub terminal: Terminal<crate::viewport::ViewportBackend<CrosstermBackend<File>>>,
    tty: File,
    original_termios: libc::termios,
}

impl VisibleTerminalSession {
    pub fn enter(maximum: Option<ratatui::layout::Size>) -> Result<Self> {
        let tty = open_controlling_tty().map_err(|error| {
            Error::usage(format!("Native Tick needs a controlling TTY: {error}"))
        })?;
        let fd = tty.as_raw_fd();
        if unsafe { libc::tcgetpgrp(fd) } != unsafe { libc::getpgrp() } {
            return Err(Error::usage(
                "Native Tick must run in the controlling TTY's foreground process group",
            ));
        }
        let terminal = Terminal::new(crate::viewport::ViewportBackend::with_tty(
            CrosstermBackend::new(tty.try_clone()?),
            maximum,
            tty.try_clone()?,
        ))?;
        let mut original_termios: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut original_termios) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut raw_termios = original_termios;
        unsafe {
            libc::cfmakeraw(&mut raw_termios);
        }
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw_termios) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut session = Self {
            terminal,
            tty,
            original_termios,
        };
        if let Err(error) = execute!(session.terminal.backend_mut(), EnterAlternateScreen, Hide) {
            let _ = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &original_termios) };
            return Err(error.into());
        }
        *PANIC_TTY.lock().unwrap_or_else(|error| error.into_inner()) =
            Some(session.tty.try_clone()?);
        *PANIC_TERMIOS
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(original_termios);
        install_panic_hook();
        TERMINAL_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(session)
    }
}

impl Drop for VisibleTerminalSession {
    fn drop(&mut self) {
        TERMINAL_ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed);
        let _ =
            unsafe { libc::tcsetattr(self.tty.as_raw_fd(), libc::TCSANOW, &self.original_termios) };
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen, Show);
        *PANIC_TTY.lock().unwrap_or_else(|error| error.into_inner()) = None;
        *PANIC_TERMIOS
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
    }
}

impl TerminalSession {
    pub fn enter() -> Result<Self> {
        Self::enter_with_size(None)
    }

    pub fn enter_with_size(maximum: Option<ratatui::layout::Size>) -> Result<Self> {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return Err(Error::usage(
                "interactive mode requires a TTY; use --once for redirected output",
            ));
        }
        let terminal = Terminal::new(crate::viewport::ViewportBackend::new(
            CrosstermBackend::new(std::io::stdout()),
            maximum,
        ))?;
        let mut session = Self {
            terminal,
            raw: false,
            alternate: false,
        };
        enable_raw_mode()?;
        session.raw = true;
        session.alternate = true;
        execute!(session.terminal.backend_mut(), EnterAlternateScreen, Hide)?;
        install_panic_hook();
        TERMINAL_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(session)
    }
}

fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if TERMINAL_ACTIVE.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = disable_raw_mode();
                if let Some(tty) = PANIC_TTY
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_mut()
                {
                    if let Some(original) = PANIC_TERMIOS
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .as_ref()
                    {
                        let _ =
                            unsafe { libc::tcsetattr(tty.as_raw_fd(), libc::TCSANOW, original) };
                    }
                    let _ = execute!(tty, LeaveAlternateScreen, Show);
                } else {
                    let _ = execute!(std::io::stdout(), LeaveAlternateScreen, Show);
                }
            }
            previous(info);
        }));
    });
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        TERMINAL_ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed);
        if self.raw {
            let _ = disable_raw_mode();
        }
        if self.alternate {
            let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen, Show);
        }
    }
}

pub fn input_events() -> Result<std::pin::Pin<Box<dyn Stream<Item = std::io::Result<Event>>>>> {
    let keyboard = EventStream::new();
    #[cfg(unix)]
    {
        let interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        let terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let signal_event = |_| {
            Ok(Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )))
        };
        let interrupt = tokio_stream::wrappers::SignalStream::new(interrupt).map(signal_event);
        let terminate = tokio_stream::wrappers::SignalStream::new(terminate).map(signal_event);
        Ok(Box::pin(futures_util::stream::select(
            keyboard,
            futures_util::stream::select(interrupt, terminate),
        )))
    }
    #[cfg(not(unix))]
    {
        Ok(Box::pin(keyboard))
    }
}

pub fn visible_input_events()
-> Result<std::pin::Pin<Box<dyn Stream<Item = std::io::Result<Event>>>>> {
    use std::io::Read;
    let mut tty = open_controlling_tty()?;
    let (sender, receiver) = tokio::sync::mpsc::channel(64);
    tokio::task::spawn_blocking(move || {
        let mut escape = Vec::new();
        let mut escape_started = None;
        let mut utf8 = Vec::new();
        loop {
            if sender.is_closed() {
                return;
            }
            let mut poll = libc::pollfd {
                fd: tty.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let poll_timeout = if escape.is_empty() {
                INPUT_POLL_TIMEOUT_MS
            } else {
                ESCAPE_SEQUENCE_TIMEOUT.as_millis() as i32
            };
            let ready = unsafe { libc::poll(&mut poll, 1, poll_timeout) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                let _ = sender.blocking_send(Err(error));
                return;
            }
            if ready == 0 {
                if !escape.is_empty() {
                    escape.clear();
                    escape_started = None;
                    if sender
                        .blocking_send(Ok(Event::Key(KeyEvent::new(
                            KeyCode::Esc,
                            KeyModifiers::NONE,
                        ))))
                        .is_err()
                    {
                        return;
                    }
                }
                continue;
            }
            let mut bytes = [0u8; 256];
            let read = match tty.read(&mut bytes) {
                Ok(read) => read,
                Err(error) => {
                    let _ = sender.blocking_send(Err(error));
                    return;
                }
            };
            if read == 0 {
                return;
            }
            for byte in &bytes[..read] {
                if escape_started.is_some_and(|started: tokio::time::Instant| {
                    started.elapsed() >= ESCAPE_SEQUENCE_TIMEOUT
                }) {
                    escape.clear();
                    escape_started = None;
                    if sender
                        .blocking_send(Ok(Event::Key(KeyEvent::new(
                            KeyCode::Esc,
                            KeyModifiers::NONE,
                        ))))
                        .is_err()
                    {
                        return;
                    }
                }
                let event = if !escape.is_empty() {
                    escape.push(*byte);
                    match escape.as_slice() {
                        b"\x1b[A" => {
                            Some(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)))
                        }
                        b"\x1b[B" => {
                            Some(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)))
                        }
                        b"\x1b[C" => Some(Event::Key(KeyEvent::new(
                            KeyCode::Right,
                            KeyModifiers::NONE,
                        ))),
                        b"\x1b[D" => {
                            Some(Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)))
                        }
                        b"\x1b[H" => {
                            Some(Event::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE)))
                        }
                        b"\x1b[F" => {
                            Some(Event::Key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE)))
                        }
                        b"\x1b[5~" => Some(Event::Key(KeyEvent::new(
                            KeyCode::PageUp,
                            KeyModifiers::NONE,
                        ))),
                        b"\x1b[6~" => Some(Event::Key(KeyEvent::new(
                            KeyCode::PageDown,
                            KeyModifiers::NONE,
                        ))),
                        _ if escape.len() >= 4
                            || !b"\x1b[".starts_with(&escape) && !escape.starts_with(b"\x1b[") =>
                        {
                            Some(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
                        }
                        _ => None,
                    }
                } else if *byte == 0x1b {
                    escape.push(*byte);
                    escape_started = Some(tokio::time::Instant::now());
                    None
                } else if *byte == 3 {
                    Some(Event::Key(KeyEvent::new(
                        KeyCode::Char('c'),
                        KeyModifiers::CONTROL,
                    )))
                } else if *byte == 14 {
                    Some(Event::Key(KeyEvent::new(
                        KeyCode::Char('n'),
                        KeyModifiers::CONTROL,
                    )))
                } else if *byte == 16 {
                    Some(Event::Key(KeyEvent::new(
                        KeyCode::Char('p'),
                        KeyModifiers::CONTROL,
                    )))
                } else if *byte == b'\r' || *byte == b'\n' {
                    Some(Event::Key(KeyEvent::new(
                        KeyCode::Enter,
                        KeyModifiers::NONE,
                    )))
                } else if *byte == 8 || *byte == 127 {
                    Some(Event::Key(KeyEvent::new(
                        KeyCode::Backspace,
                        KeyModifiers::NONE,
                    )))
                } else if *byte == b'\t' {
                    Some(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)))
                } else if !utf8.is_empty() || *byte >= 128 {
                    utf8.push(*byte);
                    match std::str::from_utf8(&utf8) {
                        Ok(character) => character.chars().next().map(|character| {
                            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
                        }),
                        Err(error) if error.error_len().is_none() => None,
                        Err(_) => {
                            utf8.clear();
                            None
                        }
                    }
                } else if byte.is_ascii_graphic() || *byte == b' ' {
                    Some(Event::Key(KeyEvent::new(
                        KeyCode::Char(*byte as char),
                        KeyModifiers::NONE,
                    )))
                } else {
                    None
                };
                if let Some(event) = event {
                    escape.clear();
                    escape_started = None;
                    utf8.clear();
                    if sender.blocking_send(Ok(event)).is_err() {
                        return;
                    }
                }
            }
        }
    });
    let keyboard = tokio_stream::wrappers::ReceiverStream::new(receiver);
    #[cfg(unix)]
    {
        let interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        let terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let resize = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
        let quit = |_| {
            Ok(Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )))
        };
        let resize =
            tokio_stream::wrappers::SignalStream::new(resize).map(|_| Ok(Event::Resize(0, 0)));
        Ok(Box::pin(futures_util::stream::select(
            keyboard,
            futures_util::stream::select(
                resize,
                futures_util::stream::select(
                    tokio_stream::wrappers::SignalStream::new(interrupt).map(quit),
                    tokio_stream::wrappers::SignalStream::new(terminate).map(quit),
                ),
            ),
        )))
    }
    #[cfg(not(unix))]
    {
        Ok(Box::pin(keyboard))
    }
}

fn open_controlling_tty() -> std::io::Result<File> {
    File::options()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .or_else(|_| {
            let fd = unsafe { libc::dup(libc::STDERR_FILENO) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let tty = unsafe { std::os::fd::FromRawFd::from_raw_fd(fd) };
            if !std::io::IsTerminal::is_terminal(&tty) {
                return Err(std::io::Error::other("stderr is not a TTY"));
            }
            Ok(tty)
        })
}
