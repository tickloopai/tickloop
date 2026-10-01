use std::io::{self, Write};

use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};

pub(super) const RENDERER_VARIABLE: &str = "TICKLOOP_AGENT_TUI";
pub(super) const DISABLE_MOUSE_VARIABLE: &str = "TICKLOOP_AGENT_DISABLE_MOUSE";
/// Lines one wheel notch moves the conversation.
pub(super) const WHEEL_SCROLL_LINES: usize = 3;

/// Button presses and the wheel (1000), motion while a button is held (1002),
/// SGR encoding (1006). Any-motion tracking (1003) is left off: nothing here
/// reacts to hover, and it would report every pointer move.
const ENABLE_WHEEL_REPORTING: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const DISABLE_WHEEL_REPORTING: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";
pub(super) const STARTUP_RENDERER: Renderer = Renderer::Fullscreen;

const SYNCHRONIZED_UPDATE_BEGIN: &str = "\x1b[?2026h";
const SYNCHRONIZED_UPDATE_END: &str = "\x1b[?2026l";

/// Drawing surface the console paints on. `Fullscreen` takes the terminal's
/// alternate screen buffer; `Inline` paints in the main buffer alongside the
/// output of other commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Renderer {
    Inline,
    Fullscreen,
}

impl Renderer {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Fullscreen => "fullscreen",
        }
    }

    pub(super) fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "inline" => Some(Self::Inline),
            "fullscreen" => Some(Self::Fullscreen),
            _ => None,
        }
    }

    pub(super) fn uses_alternate_screen(self) -> bool {
        matches!(self, Self::Fullscreen)
    }

    /// `mouse` only applies to the alternate screen. The main buffer keeps its
    /// native scrollback, where the terminal's own wheel handling is correct and
    /// capturing it would take away selection for nothing.
    pub(super) fn enter(self, mouse: bool, output: &mut impl Write) -> io::Result<()> {
        if !self.uses_alternate_screen() {
            return Ok(());
        }
        execute!(output, EnterAlternateScreen)?;
        if mouse {
            write!(output, "{ENABLE_WHEEL_REPORTING}")?;
            output.flush()?;
        }
        Ok(())
    }

    pub(super) fn leave(self, mouse: bool, output: &mut impl Write) -> io::Result<()> {
        if !self.uses_alternate_screen() {
            return Ok(());
        }
        if mouse {
            write!(output, "{DISABLE_WHEEL_REPORTING}")?;
            output.flush()?;
        }
        execute!(output, LeaveAlternateScreen)
    }
}

pub(super) struct StartupChoice {
    pub renderer: Renderer,
    pub notice: Option<String>,
}

/// Chooses the startup renderer from the configured value. An unusable value
/// never fails startup: it falls back to the startup default and explains
/// itself without echoing the value back to the terminal.
pub(super) fn startup_choice(configured: Option<&str>) -> StartupChoice {
    let Some(configured) = configured.map(str::trim).filter(|value| !value.is_empty()) else {
        return StartupChoice {
            renderer: STARTUP_RENDERER,
            notice: None,
        };
    };
    match Renderer::parse(configured) {
        Some(renderer) => StartupChoice {
            renderer,
            notice: None,
        },
        None => StartupChoice {
            renderer: STARTUP_RENDERER,
            notice: Some(format!(
                "{RENDERER_VARIABLE} must be inline or fullscreen; started in the {} renderer",
                STARTUP_RENDERER.name()
            )),
        },
    }
}

/// tmux parses and proxies every byte but does not implement DEC 2026, so the
/// markers are suppressed there rather than forwarded to a terminal that will
/// not honour them.
pub(super) fn supports_synchronized_output() -> bool {
    std::env::var_os("TMUX").is_none()
}

/// Without capture, terminals translate the wheel into arrow keys on the
/// alternate screen, which the input editor reads as history navigation. The
/// cost of capturing is the terminal's own click-and-drag selection, so this
/// stays switchable.
pub(super) fn captures_mouse() -> bool {
    !matches!(
        std::env::var(DISABLE_MOUSE_VARIABLE).as_deref(),
        Ok("1") | Ok("true")
    )
}

/// Wraps one frame in DEC 2026 so the terminal presents it as a single update.
/// The closing marker is written even when the frame fails, so a failed draw
/// cannot leave the terminal inside a synchronized update.
pub(super) fn write_synchronized<W: Write + ?Sized>(
    output: &mut W,
    frame: impl FnOnce(&mut W) -> io::Result<()>,
) -> io::Result<()> {
    write!(output, "{SYNCHRONIZED_UPDATE_BEGIN}")?;
    let drawn = frame(output);
    let closed = write!(output, "{SYNCHRONIZED_UPDATE_END}").and_then(|()| output.flush());
    drawn.and(closed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_names_round_trip_through_parse() {
        for renderer in [Renderer::Inline, Renderer::Fullscreen] {
            assert_eq!(Renderer::parse(renderer.name()), Some(renderer));
        }
    }

    #[test]
    fn parse_accepts_surrounding_whitespace_and_rejects_other_spellings() {
        assert_eq!(Renderer::parse("  fullscreen "), Some(Renderer::Fullscreen));
        for value in ["", "default", "classic", "Fullscreen", "full screen", "1"] {
            assert_eq!(Renderer::parse(value), None, "{value} should not parse");
        }
    }

    #[test]
    fn only_fullscreen_uses_the_alternate_screen() {
        assert!(Renderer::Fullscreen.uses_alternate_screen());
        assert!(!Renderer::Inline.uses_alternate_screen());
    }

    #[test]
    fn inline_writes_nothing_and_never_captures_the_mouse() {
        for mouse in [false, true] {
            let mut output = Vec::new();
            Renderer::Inline.enter(mouse, &mut output).unwrap();
            Renderer::Inline.leave(mouse, &mut output).unwrap();
            assert!(output.is_empty(), "mouse={mouse}: {output:?}");
        }
    }

    #[test]
    fn fullscreen_switches_screen_buffers_without_the_mouse() {
        let mut output = Vec::new();
        Renderer::Fullscreen.enter(false, &mut output).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "\x1b[?1049h");

        let mut output = Vec::new();
        Renderer::Fullscreen.leave(false, &mut output).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "\x1b[?1049l");
    }

    #[test]
    fn mouse_reporting_includes_drags_but_not_hover() {
        let mut output = Vec::new();
        Renderer::Fullscreen.enter(true, &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(
            output.contains("\x1b[?1002h"),
            "without 1002 terminals never report a drag, so selection cannot work: {output:?}"
        );
        assert!(
            !output.contains("\x1b[?1003h"),
            "any-motion tracking would report every move: {output:?}"
        );
    }

    #[test]
    fn fullscreen_captures_the_mouse_inside_the_alternate_screen() {
        let mut entered = Vec::new();
        Renderer::Fullscreen.enter(true, &mut entered).unwrap();
        assert_eq!(
            String::from_utf8(entered).unwrap(),
            "\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1006h"
        );

        let mut left = Vec::new();
        Renderer::Fullscreen.leave(true, &mut left).unwrap();
        assert_eq!(
            String::from_utf8(left).unwrap(),
            "\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?1049l",
            "the mouse must be released before the screen"
        );
    }

    #[test]
    fn unset_and_blank_configuration_start_in_the_default_renderer() {
        for configured in [None, Some(""), Some("   ")] {
            let choice = startup_choice(configured);
            assert_eq!(choice.renderer, STARTUP_RENDERER);
            assert!(choice.notice.is_none(), "{configured:?}");
        }
    }

    #[test]
    fn configuration_selects_either_renderer_without_a_notice() {
        for renderer in [Renderer::Inline, Renderer::Fullscreen] {
            let choice = startup_choice(Some(renderer.name()));
            assert_eq!(choice.renderer, renderer);
            assert!(choice.notice.is_none());
        }
    }

    #[test]
    fn unusable_configuration_falls_back_without_echoing_the_value() {
        let choice = startup_choice(Some("\x1b[31mdefault"));
        assert_eq!(choice.renderer, STARTUP_RENDERER);
        let notice = choice
            .notice
            .expect("an unusable value should explain itself");
        assert!(notice.contains(RENDERER_VARIABLE), "{notice}");
        assert!(!notice.contains('\x1b'), "{notice}");
        assert!(!notice.contains("default"), "{notice}");
    }

    #[test]
    fn a_synchronized_frame_is_wrapped_in_paired_markers() {
        let mut output = Vec::new();
        write_synchronized(&mut output, |output| write!(output, "frame")).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\x1b[?2026hframe\x1b[?2026l"
        );
    }

    #[test]
    fn a_failed_frame_still_closes_the_synchronized_update() {
        let mut output = Vec::new();
        let result = write_synchronized(&mut output, |output| {
            write!(output, "partial")?;
            Err(io::Error::other("frame failed"))
        });
        assert!(result.is_err());
        assert!(
            String::from_utf8(output).unwrap().ends_with("\x1b[?2026l"),
            "a failed frame must not leave the terminal inside a synchronized update"
        );
    }
}
