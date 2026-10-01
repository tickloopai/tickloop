//! Semantic styling for the terminal.
//!
//! Rendering code never names a colour. It names a [`Role`] — what the text is
//! in the interface — and the active [`Theme`] resolves that into a [`Style`]
//! at the point where bytes reach the terminal. Body text stays free of escape
//! sequences: styling and hyperlinks travel in the span structure, not inside
//! the string.
//!
//! The palette and the quantisation choice are recorded in
//! `docs/design/agent-ui.md`, which cites the Claude Code source it reproduces.

use unicode_width::UnicodeWidthChar;

use super::hyperlink::Hyperlink;

/// A 24-bit colour. Theme values are always literal RGB; the depth the terminal
/// actually supports is applied when the escape sequence is emitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Rgb {
    pub(super) red: u8,
    pub(super) green: u8,
    pub(super) blue: u8,
}

impl Rgb {
    const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }
}

/// Visual attributes of a span. The eight fields correspond one-to-one with the
/// workbench client's `Property` set so that handing theme ownership to the
/// Layout client later is a mechanical mapping rather than a translation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Style {
    pub(super) foreground: Option<Rgb>,
    pub(super) background: Option<Rgb>,
    pub(super) bold: bool,
    pub(super) dim: bool,
    pub(super) italic: bool,
    pub(super) underline: bool,
    pub(super) reversed: bool,
    pub(super) crossed_out: bool,
}

impl Style {
    const PLAIN: Self = Self {
        foreground: None,
        background: None,
        bold: false,
        dim: false,
        italic: false,
        underline: false,
        reversed: false,
        crossed_out: false,
    };

    pub(super) const fn foreground(colour: Rgb) -> Self {
        Self {
            foreground: Some(colour),
            ..Self::PLAIN
        }
    }

    pub(super) const fn background(colour: Rgb) -> Self {
        Self {
            background: Some(colour),
            ..Self::PLAIN
        }
    }

    pub(super) fn is_plain(&self) -> bool {
        *self == Self::default()
    }

    /// Layers `over` on top: fields it sets win, fields it leaves unset are
    /// inherited, matching how the workbench composes region and state roles.
    fn patch(self, over: Self) -> Self {
        Self {
            foreground: over.foreground.or(self.foreground),
            background: over.background.or(self.background),
            bold: self.bold || over.bold,
            dim: self.dim || over.dim,
            italic: self.italic || over.italic,
            underline: self.underline || over.underline,
            reversed: self.reversed || over.reversed,
            crossed_out: self.crossed_out || over.crossed_out,
        }
    }
}

/// What a span is in the interface. Content roles are neutral names for the
/// Claude Code theme keys they reproduce; chrome roles share their names with
/// the workbench client's roles. Decorations such as bold belong to the role,
/// never to the call site. The mapping is recorded in `docs/design/agent-ui.md`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum Role {
    Text,
    /// Secondary text. Claude Code's `dimColor` is this colour, not ANSI dim.
    MutedText,
    RunningShade0,
    RunningShade1,
    RunningShade2,
    RunningShade3,
    RunningShade4,
    Subtle,
    Accent,
    Success,
    Error,
    Warning,
    Suggestion,
    InverseText,
    Badge,
    Permission,
    PlanMode,
    AutoAccept,
    PromptBorder,
    ShellBorder,
    UserMessageBg,
    ShellMessageBg,
    Selection,
    DiffAdded,
    DiffRemoved,
    DiffAddedDim,
    DiffRemovedDim,
    DiffAddedWord,
    DiffRemovedWord,
    Border,
    StatusBar,
    StatusPanel,
    ModeIndicator,
    Title,
    /// Dialog and menu headings: bold in the permission colour, as
    /// Claude Code's `Dialog` renders its title.
    DialogTitle,
    ToolName,
    /// Markdown strong text and headings below the first level.
    Strong,
    /// Markdown emphasis and quoted text.
    Emphasis,
    StrongEmphasis,
    /// Markdown first-level heading: bold, italic and underlined.
    Heading,
    /// The dim bar in front of each quoted Markdown line.
    QuoteBar,
}

impl Role {
    pub(super) const ALL: [Self; ROLE_COUNT] = [
        Self::Text,
        Self::MutedText,
        Self::RunningShade0,
        Self::RunningShade1,
        Self::RunningShade2,
        Self::RunningShade3,
        Self::RunningShade4,
        Self::Subtle,
        Self::Accent,
        Self::Success,
        Self::Error,
        Self::Warning,
        Self::Suggestion,
        Self::InverseText,
        Self::Badge,
        Self::Permission,
        Self::PlanMode,
        Self::AutoAccept,
        Self::PromptBorder,
        Self::ShellBorder,
        Self::UserMessageBg,
        Self::ShellMessageBg,
        Self::Selection,
        Self::DiffAdded,
        Self::DiffRemoved,
        Self::DiffAddedDim,
        Self::DiffRemovedDim,
        Self::DiffAddedWord,
        Self::DiffRemovedWord,
        Self::Border,
        Self::StatusBar,
        Self::StatusPanel,
        Self::ModeIndicator,
        Self::Title,
        Self::DialogTitle,
        Self::ToolName,
        Self::Strong,
        Self::Emphasis,
        Self::StrongEmphasis,
        Self::Heading,
        Self::QuoteBar,
    ];
}

const ROLE_COUNT: usize = 41;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum ThemeName {
    #[default]
    ClaudeCodeDark,
    ClaudeCodeLight,
}

impl ThemeName {
    pub(super) const ALL: [Self; 2] = [Self::ClaudeCodeDark, Self::ClaudeCodeLight];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::ClaudeCodeDark => "Claude Code · Dark",
            Self::ClaudeCodeLight => "Claude Code · Light",
        }
    }

    const fn palette(self) -> &'static [Style; ROLE_COUNT] {
        match self {
            Self::ClaudeCodeDark => &DARK_PALETTE,
            Self::ClaudeCodeLight => &LIGHT_PALETTE,
        }
    }
}

const fn fg(red: u8, green: u8, blue: u8) -> Style {
    Style::foreground(Rgb::new(red, green, blue))
}

const fn bg(red: u8, green: u8, blue: u8) -> Style {
    Style::background(Rgb::new(red, green, blue))
}

const fn strong(red: u8, green: u8, blue: u8) -> Style {
    Style {
        bold: true,
        ..fg(red, green, blue)
    }
}

const fn italic(red: u8, green: u8, blue: u8) -> Style {
    Style {
        italic: true,
        ..fg(red, green, blue)
    }
}

const fn strong_italic(red: u8, green: u8, blue: u8) -> Style {
    Style {
        italic: true,
        ..strong(red, green, blue)
    }
}

const fn heading(red: u8, green: u8, blue: u8) -> Style {
    Style {
        underline: true,
        ..strong_italic(red, green, blue)
    }
}

const fn dim(red: u8, green: u8, blue: u8) -> Style {
    Style {
        dim: true,
        ..fg(red, green, blue)
    }
}

/// Values reproduced from `darkTheme` in the Claude Code source. Order matches
/// [`Role::ALL`].
const DARK_PALETTE: [Style; ROLE_COUNT] = [
    fg(255, 255, 255), // Text            <- text
    fg(153, 153, 153), // MutedText       <- inactive (dimColor)
    fg(144, 148, 146), // RunningShade0
    fg(172, 175, 173), // RunningShade1
    fg(200, 202, 201), // RunningShade2
    fg(228, 228, 228), // RunningShade3
    fg(255, 255, 255), // RunningShade4
    fg(80, 80, 80),    // Subtle          <- subtle
    fg(215, 119, 87),  // Accent          <- claude
    fg(78, 186, 101),  // Success         <- success
    fg(255, 107, 128), // Error           <- error
    fg(255, 193, 7),   // Warning         <- warning
    fg(177, 185, 249), // Suggestion      <- suggestion
    fg(0, 0, 0),       // InverseText     <- inverseText
    fg(0, 204, 204),   // Badge           <- background
    fg(177, 185, 249), // Permission      <- permission
    fg(72, 150, 140),  // PlanMode        <- planMode
    fg(175, 135, 255), // AutoAccept      <- autoAccept
    fg(136, 136, 136), // PromptBorder    <- promptBorder
    fg(253, 93, 177),  // ShellBorder     <- bashBorder
    bg(55, 55, 55),    // UserMessageBg   <- userMessageBackground
    bg(65, 60, 65),    // ShellMessageBg  <- bashMessageBackgroundColor
    bg(38, 79, 120),   // Selection       <- selectionBg
    bg(34, 92, 43),    // DiffAdded       <- diffAdded
    bg(122, 41, 54),   // DiffRemoved     <- diffRemoved
    bg(71, 88, 74),    // DiffAddedDim    <- diffAddedDimmed
    bg(105, 72, 77),   // DiffRemovedDim  <- diffRemovedDimmed
    bg(56, 166, 96),   // DiffAddedWord   <- diffAddedWord
    bg(179, 89, 107),  // DiffRemovedWord <- diffRemovedWord
    fg(153, 153, 153), // Border          <- Divider default: dimColor
    Style {
        background: Some(Rgb::new(38, 49, 43)),
        ..fg(153, 153, 153)
    }, // StatusBar
    bg(32, 41, 37),    // StatusPanel
    fg(153, 153, 153), // ModeIndicator   <- footer text: dimColor
    strong(255, 255, 255), // Title       <- <Text bold>
    strong(177, 185, 249), // DialogTitle <- Dialog: bold, permission
    strong(255, 255, 255), // ToolName    <- <Text bold>
    strong(255, 255, 255), // Strong         <- chalk.bold
    italic(255, 255, 255), // Emphasis       <- chalk.italic
    strong_italic(255, 255, 255), // StrongEmphasis <- chalk.bold.italic
    heading(255, 255, 255), // Heading        <- chalk.bold.italic.underline
    dim(255, 255, 255), // QuoteBar       <- chalk.dim
];

/// Values reproduced from `lightTheme` in the Claude Code source.
const LIGHT_PALETTE: [Style; ROLE_COUNT] = [
    fg(0, 0, 0),       // Text
    fg(102, 102, 102), // MutedText
    fg(121, 123, 122), // RunningShade0
    fg(91, 92, 91),    // RunningShade1
    fg(61, 61, 61),    // RunningShade2
    fg(30, 31, 30),    // RunningShade3
    fg(0, 0, 0),       // RunningShade4
    fg(175, 175, 175), // Subtle
    fg(215, 119, 87),  // Accent (same in both themes)
    fg(44, 122, 57),   // Success
    fg(171, 43, 63),   // Error
    fg(150, 108, 30),  // Warning
    fg(87, 105, 247),  // Suggestion
    fg(255, 255, 255), // InverseText
    fg(0, 153, 153),   // Badge
    fg(87, 105, 247),  // Permission
    fg(0, 102, 102),   // PlanMode
    fg(135, 0, 255),   // AutoAccept
    fg(153, 153, 153), // PromptBorder
    fg(255, 0, 135),   // ShellBorder
    bg(240, 240, 240), // UserMessageBg
    bg(250, 245, 250), // ShellMessageBg
    bg(180, 213, 255), // Selection
    bg(105, 219, 124), // DiffAdded
    bg(255, 168, 180), // DiffRemoved
    bg(199, 225, 203), // DiffAddedDim
    bg(253, 210, 216), // DiffRemovedDim
    bg(47, 157, 68),   // DiffAddedWord
    bg(209, 69, 75),   // DiffRemovedWord
    fg(102, 102, 102), // Border
    Style {
        background: Some(Rgb::new(230, 237, 232)),
        ..fg(102, 102, 102)
    }, // StatusBar
    bg(242, 246, 243), // StatusPanel
    fg(102, 102, 102), // ModeIndicator
    strong(0, 0, 0),   // Title
    strong(87, 105, 247), // DialogTitle
    strong(0, 0, 0),   // ToolName
    strong(0, 0, 0),   // Strong
    italic(0, 0, 0),   // Emphasis
    strong_italic(0, 0, 0), // StrongEmphasis
    heading(0, 0, 0),  // Heading
    dim(0, 0, 0),      // QuoteBar
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Theme {
    name: ThemeName,
}

impl Theme {
    pub(super) const fn new(name: ThemeName) -> Self {
        Self { name }
    }

    pub(super) fn name(self) -> ThemeName {
        self.name
    }

    pub(super) fn style(self, role: Role) -> Style {
        let index = Role::ALL
            .iter()
            .position(|candidate| *candidate == role)
            .expect("every role appears in Role::ALL");
        self.name.palette()[index]
    }
}

/// How many colours the terminal is asked to render. Matching Claude Code, 256
/// is the baseline and true colour is opt-in through `COLORTERM`; there is no
/// automatic step down to a 16-colour palette.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum ColourDepth {
    /// Non-interactive output. No escape sequences are emitted at all.
    None,
    #[default]
    Indexed256,
    TrueColour,
}

impl ColourDepth {
    pub(super) fn detect(interactive: bool, colour_term: Option<&str>) -> Self {
        if !interactive {
            return Self::None;
        }
        match colour_term.map(str::trim) {
            Some("truecolor" | "24bit") => Self::TrueColour,
            _ => Self::Indexed256,
        }
    }
}

const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

const fn cube_index(component: u8) -> usize {
    if component < 48 {
        0
    } else if component < 115 {
        1
    } else if component < 155 {
        2
    } else if component < 195 {
        3
    } else if component < 235 {
        4
    } else {
        5
    }
}

const fn squared_distance(left: u8, right: u8) -> u32 {
    let left = left as i32;
    let right = right as i32;
    let difference = left - right;
    (difference * difference) as u32
}

/// Port of `ansi_colours::ansi256_from_rgb`, the same algorithm Claude Code
/// carries in `native-ts/color-diff`. It compares the 6x6x6 cube candidate
/// against the grey ramp candidate and takes whichever is closer, which beats
/// the simpler segmenting quantiser on every palette entry where the two
/// disagree — see `docs/design/agent-ui.md`.
const fn quantise(colour: Rgb) -> u8 {
    let (red, green, blue) = (colour.red, colour.green, colour.blue);
    let (red_level, green_level, blue_level) =
        (cube_index(red), cube_index(green), cube_index(blue));
    let cube = 16 + 36 * red_level + 6 * green_level + blue_level;
    // `Math.round(sum / 3)` over integers, which rounds halves up.
    let grey = (red as u32 + green as u32 + blue as u32 + 1) / 3;
    if grey < 5 {
        return 16;
    }
    if grey > 244 && red_level == green_level && green_level == blue_level {
        return cube as u8;
    }
    let ramp_step = if grey < 8 { 0 } else { (grey - 8 + 5) / 10 };
    let ramp_step = if ramp_step > 23 { 23 } else { ramp_step };
    let grey_value = (8 + ramp_step * 10) as u8;
    let cube_distance = squared_distance(red, CUBE_LEVELS[red_level])
        + squared_distance(green, CUBE_LEVELS[green_level])
        + squared_distance(blue, CUBE_LEVELS[blue_level]);
    let grey_distance = squared_distance(red, grey_value)
        + squared_distance(green, grey_value)
        + squared_distance(blue, grey_value);
    if grey_distance < cube_distance {
        (232 + ramp_step) as u8
    } else {
        cube as u8
    }
}

/// The quantiser measures Euclidean distance in RGB, which is not perceptual.
/// Two palette entries collapse onto the grey ramp, which would make a success
/// message indistinguishable from body text and an added diff line
/// indistinguishable from its surroundings. Both are forced onto the colour
/// cube instead. Recorded in `docs/design/agent-ui.md`.
const fn manual_override(colour: Rgb) -> Option<u8> {
    match (colour.red, colour.green, colour.blue) {
        (44, 122, 57) => Some(29), // Success / light
        (34, 92, 43) => Some(22),  // DiffAdded / dark
        _ => None,
    }
}

pub(super) const fn indexed(colour: Rgb) -> u8 {
    match manual_override(colour) {
        Some(index) => index,
        None => quantise(colour),
    }
}

fn push_colour(sequence: &mut String, colour: Rgb, depth: ColourDepth, background: bool) {
    let lead = if background { 48 } else { 38 };
    match depth {
        ColourDepth::None => {}
        ColourDepth::TrueColour => sequence.push_str(&format!(
            "\x1b[{lead};2;{};{};{}m",
            colour.red, colour.green, colour.blue
        )),
        ColourDepth::Indexed256 => {
            sequence.push_str(&format!("\x1b[{lead};5;{}m", indexed(colour)))
        }
    }
}

/// Escape sequence that turns `style` on. Emits nothing when the terminal is
/// not being coloured or the style carries no attributes.
pub(super) fn open(style: &Style, depth: ColourDepth) -> String {
    if depth == ColourDepth::None || style.is_plain() {
        return String::new();
    }
    let mut sequence = String::new();
    if style.bold {
        sequence.push_str("\x1b[1m");
    }
    // Claude Code's own text layer treats bold and dim as mutually exclusive;
    // emitting both leaves terminals disagreeing about which wins.
    if style.dim && !style.bold {
        sequence.push_str("\x1b[2m");
    }
    if style.italic {
        sequence.push_str("\x1b[3m");
    }
    if style.underline {
        sequence.push_str("\x1b[4m");
    }
    if style.reversed {
        sequence.push_str("\x1b[7m");
    }
    if style.crossed_out {
        sequence.push_str("\x1b[9m");
    }
    if let Some(colour) = style.foreground {
        push_colour(&mut sequence, colour, depth, false);
    }
    if let Some(colour) = style.background {
        push_colour(&mut sequence, colour, depth, true);
    }
    sequence
}

/// Escape sequence that returns the terminal to its default appearance.
pub(super) fn close(style: &Style, depth: ColourDepth) -> &'static str {
    if depth == ColourDepth::None || style.is_plain() {
        ""
    } else {
        "\x1b[0m"
    }
}

/// A run of text carrying one style and, optionally, the link it opens.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Span {
    pub(super) text: String,
    pub(super) style: Style,
    pub(super) link: Option<Hyperlink>,
}

impl Span {
    pub(super) fn plain(text: impl Into<String>) -> Self {
        Self::styled(text, Style::default())
    }

    pub(super) fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
            link: None,
        }
    }

    pub(super) fn linked(mut self, link: Option<Hyperlink>) -> Self {
        self.link = link;
        self
    }
}

/// One screen row as styled runs. Producers build these instead of bare
/// strings; the escape sequences are added only when the row is rendered, so
/// width arithmetic always runs on visible text.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct StyledLine {
    pub(super) spans: Vec<Span>,
    pub(super) link: Option<String>,
}

impl StyledLine {
    pub(super) fn plain(text: impl Into<String>) -> Self {
        let text = text.into();
        if text.is_empty() {
            return Self::default();
        }
        Self {
            spans: vec![Span::plain(text)],
            link: None,
        }
    }

    pub(super) fn push(&mut self, span: Span) {
        if span.text.is_empty() {
            return;
        }
        if let Some(last) = self.spans.last_mut()
            && last.style == span.style
            && last.link == span.link
        {
            last.text.push_str(&span.text);
            return;
        }
        self.spans.push(span);
    }

    /// Renders the row, opening and closing each styled run. Runs are closed
    /// with a full reset rather than the individual attribute-off codes: dim
    /// and bold share the reset code `\x1b[22m`, so closing a dim run that way
    /// makes some terminals render an immediately following bold run dim too.
    /// Claude Code works around the same defect by separating the two runs into
    /// independent boxes. A linked run is wrapped in OSC 8 outside its style.
    pub(super) fn render(&self, depth: ColourDepth) -> String {
        let mut rendered = String::new();
        if let Some(link) = &self.link {
            rendered.push_str("\x1b]8;;");
            rendered.push_str(link);
            rendered.push_str("\x1b\\");
        }
        for span in &self.spans {
            if let Some(link) = &span.link {
                rendered.push_str(&link.open());
            }
            let opening = open(&span.style, depth);
            rendered.push_str(&opening);
            rendered.push_str(&span.text);
            if !opening.is_empty() {
                rendered.push_str(close(&span.style, depth));
            }
            if span.link.is_some() {
                rendered.push_str(&Hyperlink::close());
            }
        }
        if self.link.is_some() {
            rendered.push_str("\x1b]8;;\x1b\\");
        }
        rendered
    }
}

/// A run of text named by role rather than by colour. Frames and the
/// conversation are kept in this form until the moment they are written, so a
/// theme switch restyles content that was produced earlier.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct RoleSpan {
    pub(super) text: String,
    pub(super) role: Option<Role>,
    pub(super) link: Option<Hyperlink>,
}

/// One screen row described by roles. `fill` paints the whole row width with a
/// background role, as Claude Code's message boxes do.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct RoleLine {
    pub(super) spans: Vec<RoleSpan>,
    pub(super) fill: Option<Role>,
    pub(super) link: Option<String>,
}

impl RoleLine {
    pub(super) fn plain(text: impl Into<String>) -> Self {
        let mut line = Self::default();
        line.push(text, None);
        line
    }

    pub(super) fn role(text: impl Into<String>, role: Role) -> Self {
        let mut line = Self::default();
        line.push(text, Some(role));
        line
    }

    pub(super) fn linked(text: impl Into<String>, role: Role, link: &str) -> Self {
        let mut line = Self::role(text, role);
        line.link = Some(link.to_owned());
        line
    }

    #[cfg(test)]
    pub(super) fn with(mut self, text: impl Into<String>, role: Role) -> Self {
        self.push(text, Some(role));
        self
    }

    pub(super) fn filled(mut self, role: Role) -> Self {
        self.fill = Some(role);
        self
    }

    pub(super) fn push(&mut self, text: impl Into<String>, role: Option<Role>) {
        self.push_linked(text, role, None);
    }

    pub(super) fn push_linked(
        &mut self,
        text: impl Into<String>,
        role: Option<Role>,
        link: Option<Hyperlink>,
    ) {
        let text = text.into();
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.spans.last_mut()
            && last.role == role
            && last.link == link
        {
            last.text.push_str(&text);
            return;
        }
        self.spans.push(RoleSpan { text, role, link });
    }

    pub(super) fn text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }

    /// Cuts the row to `width` display columns without splitting a wide
    /// character, keeping each surviving character's role.
    pub(super) fn truncate(&mut self, width: usize) {
        let mut column = 0;
        let mut kept = Vec::new();
        for span in self.spans.drain(..) {
            let mut text = String::new();
            let mut full = false;
            for character in span.text.chars() {
                let character_width = character.width().unwrap_or(0);
                if column + character_width > width {
                    full = true;
                    break;
                }
                text.push(character);
                column += character_width;
            }
            if !text.is_empty() {
                kept.push(RoleSpan {
                    text,
                    role: span.role,
                    link: span.link,
                });
            }
            if full {
                break;
            }
        }
        self.spans = kept;
    }

    /// Resolves roles against `theme`. Every span starts from the body text
    /// role, as every Claude Code `<Text>` inherits the theme's text colour;
    /// a filled row is padded to `width` so its background spans the row.
    pub(super) fn resolve(&self, theme: Theme, width: usize) -> StyledLine {
        let mut base = theme.style(Role::Text);
        if let Some(fill) = self.fill {
            base = base.patch(theme.style(fill));
        }
        let mut line = StyledLine {
            link: self.link.clone(),
            ..Default::default()
        };
        let mut column = 0;
        for span in &self.spans {
            let style = span.role.map_or(base, |role| base.patch(theme.style(role)));
            column += span
                .text
                .chars()
                .map(|character| character.width().unwrap_or(0))
                .sum::<usize>();
            line.push(Span::styled(span.text.clone(), style).linked(span.link.clone()));
        }
        if self.fill.is_some() && column < width {
            line.push(Span::styled(" ".repeat(width - column), base));
        }
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_role_resolves_in_every_theme() {
        for name in ThemeName::ALL {
            let theme = Theme::new(name);
            for role in Role::ALL {
                let style = theme.style(role);
                assert!(!style.is_plain(), "{name:?} has no value for {role:?}");
            }
        }
    }

    #[test]
    fn background_roles_paint_the_background_and_others_the_text() {
        let theme = Theme::new(ThemeName::ClaudeCodeDark);
        let body = theme.style(Role::Text);
        assert!(body.foreground.is_some() && body.background.is_none());
        let block = theme.style(Role::UserMessageBg);
        assert!(block.background.is_some() && block.foreground.is_none());
    }

    #[test]
    fn accent_reproduces_the_official_brand_colour_in_both_themes() {
        let expected = Some(Rgb::new(215, 119, 87));
        for name in ThemeName::ALL {
            assert_eq!(Theme::new(name).style(Role::Accent).foreground, expected);
        }
    }

    #[test]
    fn decorations_belong_to_roles() {
        let theme = Theme::new(ThemeName::ClaudeCodeDark);
        assert!(theme.style(Role::Title).bold);
        assert!(theme.style(Role::ToolName).bold);
        assert!(!theme.style(Role::Text).bold);
    }

    #[test]
    fn muted_text_is_claude_codes_dim_colour_not_subtle() {
        let theme = Theme::new(ThemeName::ClaudeCodeDark);
        assert_eq!(
            theme.style(Role::MutedText).foreground,
            Some(Rgb::new(153, 153, 153))
        );
        assert_eq!(
            theme.style(Role::Subtle).foreground,
            Some(Rgb::new(80, 80, 80))
        );
    }

    #[test]
    fn plain_spans_inherit_the_body_text_colour() {
        let theme = Theme::new(ThemeName::ClaudeCodeLight);
        let line = RoleLine::plain("body").resolve(theme, 10);
        assert_eq!(line.spans[0].style.foreground, Some(Rgb::new(0, 0, 0)));
    }

    #[test]
    fn hyperlink_metadata_is_only_added_when_rendered() {
        let address = "https://auth.openai.com/authorize?state=abc";
        let line = RoleLine::linked("  auth.openai.com", Role::Accent, address);
        assert_eq!(line.text(), "  auth.openai.com");
        for depth in [
            ColourDepth::None,
            ColourDepth::Indexed256,
            ColourDepth::TrueColour,
        ] {
            let rendered = line
                .resolve(Theme::new(ThemeName::ClaudeCodeDark), 80)
                .render(depth);
            assert!(rendered.starts_with(&format!("\x1b]8;;{address}\x1b\\")));
            assert!(rendered.ends_with("\x1b]8;;\x1b\\"));
            assert_eq!(rendered.matches("\x1b]8;;").count(), 2);
        }
    }

    #[test]
    fn a_role_keeps_body_colour_for_fields_it_does_not_set() {
        let theme = Theme::new(ThemeName::ClaudeCodeDark);
        let line = RoleLine::role("hello", Role::UserMessageBg).resolve(theme, 10);
        let style = line.spans[0].style;
        assert_eq!(style.foreground, Some(Rgb::new(255, 255, 255)));
        assert_eq!(style.background, Some(Rgb::new(55, 55, 55)));
    }

    #[test]
    fn a_filled_row_pads_its_background_to_the_full_width() {
        let theme = Theme::new(ThemeName::ClaudeCodeDark);
        let line = RoleLine::role("❯ ", Role::Subtle)
            .with("中", Role::Text)
            .filled(Role::UserMessageBg)
            .resolve(theme, 8);
        let background = Some(Rgb::new(55, 55, 55));
        assert!(
            line.spans
                .iter()
                .all(|span| span.style.background == background)
        );
        assert_eq!(line.render(ColourDepth::None), "❯ 中    ");
    }

    #[test]
    fn truncation_keeps_roles_and_never_splits_a_wide_character() {
        let mut line = RoleLine::role("ab", Role::Accent).with("中文", Role::Error);
        line.truncate(3);
        assert_eq!(line.text(), "ab");
        assert_eq!(line.spans.len(), 1);
        let mut line = RoleLine::role("ab", Role::Accent).with("中文", Role::Error);
        line.truncate(4);
        assert_eq!(line.text(), "ab中");
        assert_eq!(line.spans[1].role, Some(Role::Error));
    }

    /// The vector the Claude Code source calls out: the brand orange must not
    /// land on 174, the washed-out salmon the simpler quantiser produces.
    #[test]
    fn quantiser_keeps_the_brand_orange_off_the_washed_out_index() {
        let index = indexed(Rgb::new(215, 119, 87));
        assert_eq!(index, 173);
        assert_ne!(index, 174);
    }

    #[test]
    fn quantiser_matches_reference_vectors() {
        for (colour, expected) in [
            (Rgb::new(255, 255, 255), 231),
            (Rgb::new(0, 0, 0), 16),
            (Rgb::new(80, 80, 80), 239),
            (Rgb::new(255, 107, 128), 204),
            (Rgb::new(177, 185, 249), 147),
            (Rgb::new(136, 136, 136), 102),
            // Grey step (153 - 8) / 10 = 14.5 must round half up, as
            // JavaScript's Math.round does, to reach 247 rather than 246.
            (Rgb::new(153, 153, 153), 247),
            (Rgb::new(78, 186, 101), 71),
        ] {
            assert_eq!(indexed(colour), expected, "{colour:?}");
        }
    }

    #[test]
    fn the_two_grey_collapses_are_overridden() {
        // Without the override these fall onto the grey ramp, which would make
        // a success message look like body text and an added diff line look
        // like context.
        assert_eq!(quantise(Rgb::new(44, 122, 57)), 239);
        assert_eq!(indexed(Rgb::new(44, 122, 57)), 29);
        assert_eq!(quantise(Rgb::new(34, 92, 43)), 237);
        assert_eq!(indexed(Rgb::new(34, 92, 43)), 22);
    }

    #[test]
    fn overrides_only_apply_to_the_two_recorded_colours() {
        let overridden: Vec<Role> = Role::ALL
            .into_iter()
            .filter(|role| {
                ThemeName::ALL.into_iter().any(|name| {
                    let style = Theme::new(name).style(*role);
                    style
                        .foreground
                        .or(style.background)
                        .is_some_and(|colour| manual_override(colour).is_some())
                })
            })
            .collect();
        assert_eq!(overridden, vec![Role::Success, Role::DiffAdded]);
    }

    #[test]
    fn depth_follows_colorterm_and_interactivity() {
        assert_eq!(
            ColourDepth::detect(true, Some("truecolor")),
            ColourDepth::TrueColour
        );
        assert_eq!(
            ColourDepth::detect(true, Some("24bit")),
            ColourDepth::TrueColour
        );
        assert_eq!(
            ColourDepth::detect(true, Some("256color")),
            ColourDepth::Indexed256
        );
        assert_eq!(ColourDepth::detect(true, None), ColourDepth::Indexed256);
        assert_eq!(
            ColourDepth::detect(false, Some("truecolor")),
            ColourDepth::None
        );
    }

    #[test]
    fn non_interactive_output_carries_no_escape_sequences() {
        let style = Theme::new(ThemeName::ClaudeCodeDark).style(Role::Accent);
        assert_eq!(open(&style, ColourDepth::None), "");
        assert_eq!(close(&style, ColourDepth::None), "");
    }

    #[test]
    fn plain_styles_emit_nothing_even_when_colour_is_available() {
        let style = Style::default();
        assert_eq!(open(&style, ColourDepth::TrueColour), "");
        assert_eq!(close(&style, ColourDepth::TrueColour), "");
    }

    #[test]
    fn foreground_and_background_use_the_expected_leads() {
        let colour = Rgb::new(215, 119, 87);
        assert_eq!(
            open(&Style::foreground(colour), ColourDepth::TrueColour),
            "\x1b[38;2;215;119;87m"
        );
        assert_eq!(
            open(&Style::background(colour), ColourDepth::TrueColour),
            "\x1b[48;2;215;119;87m"
        );
        assert_eq!(
            open(&Style::foreground(colour), ColourDepth::Indexed256),
            "\x1b[38;5;173m"
        );
        assert_eq!(
            open(&Style::background(colour), ColourDepth::Indexed256),
            "\x1b[48;5;173m"
        );
    }

    #[test]
    fn the_default_theme_is_dark() {
        assert_eq!(ThemeName::default(), ThemeName::ClaudeCodeDark);
    }

    #[test]
    fn bold_suppresses_dim_and_attributes_emit_their_codes() {
        let base = Style::foreground(Rgb::new(0, 0, 0));
        let both = open(
            &Style {
                bold: true,
                dim: true,
                ..base
            },
            ColourDepth::TrueColour,
        );
        assert!(both.contains("\x1b[1m") && !both.contains("\x1b[2m"));
        let italic = open(
            &Style {
                italic: true,
                ..base
            },
            ColourDepth::TrueColour,
        );
        assert!(italic.contains("\x1b[3m"));
    }

    #[test]
    fn a_plain_line_renders_to_exactly_its_text_at_every_depth() {
        let line = StyledLine::plain("Tick Loop Agent");
        for depth in [
            ColourDepth::None,
            ColourDepth::Indexed256,
            ColourDepth::TrueColour,
        ] {
            assert_eq!(line.render(depth), "Tick Loop Agent");
        }
    }

    #[test]
    fn styled_runs_are_closed_with_a_full_reset() {
        // Dim and bold share the `\x1b[22m` reset, so closing a dim run with it
        // leaves a following bold run dim on some terminals.
        let theme = Theme::new(ThemeName::ClaudeCodeDark);
        let dimmed = Style {
            dim: true,
            ..theme.style(Role::MutedText)
        };
        let bold = Style {
            bold: true,
            ..theme.style(Role::Text)
        };
        let mut line = StyledLine::default();
        line.push(Span::styled("⏺", dimmed));
        line.push(Span::styled("Bash", bold));
        let rendered = line.render(ColourDepth::Indexed256);
        assert!(!rendered.contains("\x1b[22m"));
        assert_eq!(rendered.matches("\x1b[0m").count(), 2);
    }

    #[test]
    fn a_linked_run_is_wrapped_in_osc_8_while_its_text_and_width_stay_plain() {
        let theme = Theme::new(ThemeName::ClaudeCodeDark);
        let link = Hyperlink::file(std::path::Path::new("/cache/1.png"));
        let mut role_line = RoleLine::role("see ", Role::Text);
        role_line.push_linked("[Image #1]", None, link.clone());
        role_line.push_linked("[Image #2]", None, None);
        role_line.truncate(14);
        assert_eq!(role_line.text(), "see [Image #1]");

        let line = role_line.filled(Role::UserMessageBg).resolve(theme, 16);
        assert_eq!(
            line.spans
                .iter()
                .map(|span| span.text.as_str())
                .collect::<String>(),
            "see [Image #1]  "
        );
        let rendered = line.render(ColourDepth::TrueColour);
        let linked = rendered
            .split("\x1b]8;;file:///cache/1.png\x07")
            .nth(1)
            .and_then(|rest| rest.split("\x1b]8;;\x07").next())
            .expect("the image placeholder should be linked");
        assert!(linked.contains("[Image #1]") && !linked.contains("see"));
        assert_eq!(rendered.matches("\x1b]8;;").count(), 2);
    }

    #[test]
    fn adjacent_runs_sharing_a_style_merge() {
        let mut line = StyledLine::default();
        line.push(Span::plain("ab"));
        line.push(Span::plain("cd"));
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.render(ColourDepth::TrueColour), "abcd");
    }

    #[test]
    fn empty_spans_are_dropped() {
        let mut line = StyledLine::default();
        line.push(Span::plain(""));
        assert!(line.spans.is_empty());
        assert_eq!(line.render(ColourDepth::TrueColour), "");
    }

    #[test]
    fn text_reports_visible_characters_without_escapes() {
        let theme = Theme::new(ThemeName::ClaudeCodeDark);
        let mut line = StyledLine::default();
        line.push(Span::styled("❯ ", theme.style(Role::MutedText)));
        line.push(Span::plain("中文"));
        assert_eq!(line.render(ColourDepth::None), "❯ 中文");
    }
}
