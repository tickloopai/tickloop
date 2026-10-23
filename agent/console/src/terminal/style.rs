//! Semantic styling for the terminal.
//!
//! Rendering code never names a colour. It names a [`Role`] — what the text is
//! in the interface — and the active [`Theme`] resolves that into a [`Style`]
//! at the point where bytes reach the terminal. Body text stays free of escape
//! sequences: styling and hyperlinks travel in the span structure, not inside
//! the string.

use tickloop_ui::markdown::Continuation;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

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

/// One of the terminal's own palette colours. The user's terminal theme decides how it looks;
/// Claude Code's `chalk.blue` and similar named colours are these, not fixed RGB values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AnsiColour {
    Red,
    Green,
    Yellow,
    Blue,
    Cyan,
    /// `chalk.grey`, the bright black of the sixteen-colour palette.
    Grey,
}

impl AnsiColour {
    const fn foreground_code(self) -> u8 {
        match self {
            Self::Red => 31,
            Self::Green => 32,
            Self::Yellow => 33,
            Self::Blue => 34,
            Self::Cyan => 36,
            Self::Grey => 90,
        }
    }
}

/// A colour a theme sends: a literal RGB value, quantised to the terminal's depth when emitted,
/// or a palette colour the terminal resolves itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Colour {
    Rgb(Rgb),
    Ansi(AnsiColour),
}

impl From<Rgb> for Colour {
    fn from(colour: Rgb) -> Self {
        Self::Rgb(colour)
    }
}

/// Visual attributes of a span. The eight fields correspond one-to-one with the
/// workbench client's `Property` set so that handing theme ownership to the
/// Layout client later is a mechanical mapping rather than a translation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Style {
    pub(super) foreground: Option<Colour>,
    pub(super) background: Option<Colour>,
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
            foreground: Some(Colour::Rgb(colour)),
            ..Self::PLAIN
        }
    }

    pub(super) const fn background(colour: Rgb) -> Self {
        Self {
            background: Some(Colour::Rgb(colour)),
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
/// never to the call site.
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
    /// The brand's Tick Red, kept to the one lit tick: the working spinner, a delivered Tick
    /// Update and the startup mark. Never a word, never a fill.
    TickMark,
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
    InProgressTodo,
    CompletedTodo,
    /// The detailed transcript's `∴ Thinking…` label: dim and italic.
    ThinkingHeading,
    /// The detailed transcript's `Prompt:` and `Response:` labels of a Subagent call: bold in
    /// the success colour.
    AgentSection,
    /// Agent Colors tell family members apart; they never express state. Values reproduce
    /// Claude Code's `*_FOR_SUBAGENTS_ONLY` keys, the same in both themes.
    AgentRed,
    AgentBlue,
    AgentGreen,
    AgentYellow,
    AgentPurple,
    AgentOrange,
    AgentPink,
    AgentCyan,
    /// State marks: the glyph that leads a Monitor or Subagents row and carries its state in the
    /// accent, success, error or warning colour. A running row's mark blinks between
    /// `RunningMark` and the dimmed `RunningMarkDim`.
    RunningMark,
    RunningMarkDim,
    SuccessMark,
    ErrorMark,
    WarningMark,
    /// A Monitor row's glyph while the row holds an update the Chat has not handled, over its
    /// state colour: the accent's hue, clear on the row's every background, and its dimmed phase,
    /// which each new update flashes once.
    UpdateMark,
    UpdateMarkDim,
    /// A one-off flash alternates these fills: strong, soft, strong. A Monitor row flashes the
    /// warm take-in pair when content arrives or an Agent turn takes its update in, and the cold use pair as the Agent
    /// reads or operates its instance with a tool.
    TakeInFlashStrong,
    TakeInFlashSoft,
    UseFlashStrong,
    UseFlashSoft,
    /// A Monitor row's `↻`, one hue from the brightest for the most frequent subscription to the
    /// dimmest for the least. Marks, so a selected row keeps them.
    SubscriptionHigh,
    SubscriptionMedium,
    SubscriptionLow,
    /// The bar that runs from a Monitor row's first column while a periodic subscription holds
    /// an update, to the part of the period elapsed before its next check hands it to the Agent:
    /// the accent's hue, dimmed, on a selected row too.
    UpdateProgress,
    /// A Markdown link label: the terminal's own blue, as Claude Code's `chalk.blue`.
    Link,
    /// Inline code inside strong text or a heading: the code colour, still bold.
    StrongCode,
    /// Bold secondary text, such as the counts in the task list heading.
    MutedStrong,
    /// Claude Code's colour-diff palette for a change shown in the conversation: whole changed
    /// lines, the changed words inside them, and the `+`/`-` markers.
    DiffLineAdded,
    DiffLineRemoved,
    DiffWordAdded,
    DiffWordRemoved,
    DiffMarkerAdded,
    DiffMarkerRemoved,
    /// Code block highlighting in cli-highlight's default theme, which uses the terminal's own
    /// palette colours.
    CodeKeyword,
    CodeBuiltIn,
    CodeType,
    CodeNumber,
    CodeString,
    CodeFunction,
    CodeComment,
    CodeMeta,
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
        Self::TickMark,
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
        Self::ModeIndicator,
        Self::Title,
        Self::DialogTitle,
        Self::ToolName,
        Self::Strong,
        Self::Emphasis,
        Self::StrongEmphasis,
        Self::Heading,
        Self::QuoteBar,
        Self::InProgressTodo,
        Self::CompletedTodo,
        Self::ThinkingHeading,
        Self::AgentSection,
        Self::AgentRed,
        Self::AgentBlue,
        Self::AgentGreen,
        Self::AgentYellow,
        Self::AgentPurple,
        Self::AgentOrange,
        Self::AgentPink,
        Self::AgentCyan,
        Self::RunningMark,
        Self::RunningMarkDim,
        Self::SuccessMark,
        Self::ErrorMark,
        Self::WarningMark,
        Self::UpdateMark,
        Self::UpdateMarkDim,
        Self::TakeInFlashStrong,
        Self::TakeInFlashSoft,
        Self::UseFlashStrong,
        Self::UseFlashSoft,
        Self::SubscriptionHigh,
        Self::SubscriptionMedium,
        Self::SubscriptionLow,
        Self::UpdateProgress,
        Self::Link,
        Self::StrongCode,
        Self::MutedStrong,
        Self::DiffLineAdded,
        Self::DiffLineRemoved,
        Self::DiffWordAdded,
        Self::DiffWordRemoved,
        Self::DiffMarkerAdded,
        Self::DiffMarkerRemoved,
        Self::CodeKeyword,
        Self::CodeBuiltIn,
        Self::CodeType,
        Self::CodeNumber,
        Self::CodeString,
        Self::CodeFunction,
        Self::CodeComment,
        Self::CodeMeta,
    ];

    /// Agent Colors and marks only paint marks, such as a member dot, a row's glyph or its `↻`,
    /// never the words of a row.
    const fn is_mark(self) -> bool {
        matches!(
            self,
            Self::AgentRed
                | Self::AgentBlue
                | Self::AgentGreen
                | Self::AgentYellow
                | Self::AgentPurple
                | Self::AgentOrange
                | Self::AgentPink
                | Self::AgentCyan
                | Self::RunningMark
                | Self::RunningMarkDim
                | Self::SuccessMark
                | Self::ErrorMark
                | Self::WarningMark
                | Self::UpdateMark
                | Self::UpdateMarkDim
                | Self::UpdateProgress
                | Self::SubscriptionHigh
                | Self::SubscriptionMedium
                | Self::SubscriptionLow
        )
    }

    /// The role of a highlighting class of fenced code.
    pub(super) const fn code(token: tickloop_ui::highlight::CodeToken) -> Self {
        use tickloop_ui::highlight::CodeToken;
        match token {
            CodeToken::Keyword => Self::CodeKeyword,
            CodeToken::BuiltIn => Self::CodeBuiltIn,
            CodeToken::Type => Self::CodeType,
            CodeToken::Number => Self::CodeNumber,
            CodeToken::String => Self::CodeString,
            CodeToken::Function => Self::CodeFunction,
            CodeToken::Comment => Self::CodeComment,
            CodeToken::Meta => Self::CodeMeta,
        }
    }

    pub(super) const fn agent(color: tickloop_agent_protocol::AgentColor) -> Self {
        use tickloop_agent_protocol::AgentColor;
        match color {
            AgentColor::Red => Self::AgentRed,
            AgentColor::Blue => Self::AgentBlue,
            AgentColor::Green => Self::AgentGreen,
            AgentColor::Yellow => Self::AgentYellow,
            AgentColor::Purple => Self::AgentPurple,
            AgentColor::Orange => Self::AgentOrange,
            AgentColor::Pink => Self::AgentPink,
            AgentColor::Cyan => Self::AgentCyan,
        }
    }
}

const ROLE_COUNT: usize = 85;

/// `AgentRed` … `AgentCyan`, reproduced from Claude Code's agent palette (Tailwind 600).
const AGENT_COLORS: [Style; 8] = [
    fg(220, 38, 38),
    fg(37, 99, 235),
    fg(22, 163, 74),
    fg(202, 138, 4),
    fg(147, 51, 234),
    fg(234, 88, 12),
    fg(219, 39, 119),
    fg(8, 145, 178),
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
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

/// Attributes only: the span keeps the terminal's own text colour, as a Claude Code `<Text>`
/// without a colour does.
const fn attributes(bold: bool, italic: bool, underline: bool, dim: bool) -> Style {
    Style {
        bold,
        italic,
        underline,
        dim,
        ..Style::PLAIN
    }
}

const fn ansi(colour: AnsiColour) -> Style {
    Style {
        foreground: Some(Colour::Ansi(colour)),
        ..Style::PLAIN
    }
}

/// The code-highlighting roles, the same in both themes because the terminal palette decides
/// them. Order matches the `Code*` roles in [`Role::ALL`].
const CODE_COLOURS: [Style; 8] = [
    ansi(AnsiColour::Blue),
    ansi(AnsiColour::Cyan),
    Style {
        dim: true,
        ..ansi(AnsiColour::Cyan)
    },
    ansi(AnsiColour::Green),
    ansi(AnsiColour::Red),
    ansi(AnsiColour::Yellow),
    ansi(AnsiColour::Green),
    ansi(AnsiColour::Grey),
];

const fn fg(red: u8, green: u8, blue: u8) -> Style {
    Style::foreground(Rgb::new(red, green, blue))
}

/// `style` drawn on a background.
const fn on(style: Style, background: Rgb) -> Style {
    Style {
        background: Some(Colour::Rgb(background)),
        ..style
    }
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
    fg(236, 48, 19),   // TickMark        <- local: Tick Red
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
        background: Some(Colour::Rgb(Rgb::new(38, 49, 43))),
        ..fg(153, 153, 153)
    }, // StatusBar
    fg(153, 153, 153), // ModeIndicator   <- footer text: dimColor
    attributes(true, false, false, false), // Title <- <Text bold>
    strong(177, 185, 249), // DialogTitle <- Dialog: bold, permission
    attributes(true, false, false, false), // ToolName <- <Text bold>
    attributes(true, false, false, false), // Strong <- chalk.bold
    attributes(false, true, false, false), // Emphasis <- chalk.italic
    attributes(true, true, false, false), // StrongEmphasis <- chalk.bold.italic
    attributes(true, true, true, false), // Heading <- chalk.bold.italic.underline
    attributes(false, false, false, true), // QuoteBar <- chalk.dim
    fg(215, 119, 87),  // InProgressTodo <- claude
    Style {
        crossed_out: true,
        ..fg(153, 153, 153)
    },
    italic(153, 153, 153), // ThinkingHeading <- dimColor italic
    strong(78, 186, 101),  // AgentSection    <- success + bold
    AGENT_COLORS[0],
    AGENT_COLORS[1],
    AGENT_COLORS[2],
    AGENT_COLORS[3],
    AGENT_COLORS[4],
    AGENT_COLORS[5],
    AGENT_COLORS[6],
    AGENT_COLORS[7],
    fg(215, 119, 87), // RunningMark     <- claude
    Style {
        dim: true,
        ..fg(215, 119, 87)
    }, // RunningMarkDim  <- claude, dimmed
    fg(78, 186, 101), // SuccessMark     <- success
    fg(255, 107, 128), // ErrorMark       <- error
    fg(255, 193, 7),  // WarningMark     <- warning
    // The accent itself falls under 3:1 on the selection and the update bar, so the update mark
    // is a lighter tint of its hue.
    fg(255, 160, 120), // UpdateMark      <- local: claude, lightened
    Style {
        dim: true,
        ..fg(255, 160, 120)
    }, // UpdateMarkDim
    bg(95, 61, 50),    // TakeInFlashStrong <- local: claude tinted into the dark background
    bg(58, 43, 39),    // TakeInFlashSoft <- local: claude tinted half as strong
    // A cyan-green this dark falls to #005F5F at 256 colours, where MutedText drops under 3:1, so
    // the cold flash is an indigo, still far from the selection's blue.
    bg(70, 40, 130), // UseFlashStrong  <- local: indigo tinted into the dark background
    bg(36, 24, 92),  // UseFlashSoft    <- local: indigo tinted about half as strong
    fg(175, 255, 255), // SubscriptionHigh <- local: one cyan hue, brightest
    fg(95, 215, 215), // SubscriptionMedium
    fg(0, 175, 175), // SubscriptionLow
    bg(110, 63, 48), // UpdateProgress  <- local: claude dimmed, still clear on black terminals
    ansi(AnsiColour::Blue), // Link            <- chalk.blue
    strong(177, 185, 249), // StrongCode      <- permission + bold
    strong(153, 153, 153), // MutedStrong     <- inactive + bold
    bg(2, 40, 0),    // DiffLineAdded   <- colour-diff addLine
    bg(61, 1, 0),    // DiffLineRemoved <- colour-diff deleteLine
    bg(4, 71, 0),    // DiffWordAdded   <- colour-diff addWord
    bg(92, 2, 0),    // DiffWordRemoved <- colour-diff deleteWord
    on(fg(80, 200, 80), Rgb::new(2, 40, 0)), // DiffMarkerAdded <- addDecoration on addLine
    on(fg(220, 90, 90), Rgb::new(61, 1, 0)), // DiffMarkerRemoved <- deleteDecoration on deleteLine
    CODE_COLOURS[0],
    CODE_COLOURS[1],
    CODE_COLOURS[2],
    CODE_COLOURS[3],
    CODE_COLOURS[4],
    CODE_COLOURS[5],
    CODE_COLOURS[6],
    CODE_COLOURS[7],
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
    fg(236, 48, 19),   // TickMark (same in both themes)
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
        background: Some(Colour::Rgb(Rgb::new(230, 237, 232))),
        ..fg(102, 102, 102)
    }, // StatusBar
    fg(102, 102, 102), // ModeIndicator
    attributes(true, false, false, false), // Title
    strong(87, 105, 247), // DialogTitle
    attributes(true, false, false, false), // ToolName
    attributes(true, false, false, false), // Strong
    attributes(false, true, false, false), // Emphasis
    attributes(true, true, false, false), // StrongEmphasis
    attributes(true, true, true, false), // Heading
    attributes(false, false, false, true), // QuoteBar
    fg(215, 119, 87),  // InProgressTodo
    Style {
        crossed_out: true,
        ..fg(102, 102, 102)
    },
    italic(102, 102, 102), // ThinkingHeading
    strong(44, 122, 57),   // AgentSection
    AGENT_COLORS[0],
    AGENT_COLORS[1],
    AGENT_COLORS[2],
    AGENT_COLORS[3],
    AGENT_COLORS[4],
    AGENT_COLORS[5],
    AGENT_COLORS[6],
    AGENT_COLORS[7],
    fg(215, 119, 87), // RunningMark
    Style {
        dim: true,
        ..fg(215, 119, 87)
    }, // RunningMarkDim
    fg(44, 122, 57),  // SuccessMark
    fg(171, 43, 63),  // ErrorMark
    fg(150, 108, 30), // WarningMark
    // The accent itself falls under 3:1 on white at 256 colours, so the update mark is a darker
    // shade of its hue.
    fg(180, 80, 40), // UpdateMark
    Style {
        dim: true,
        ..fg(180, 80, 40)
    }, // UpdateMarkDim
    bg(243, 214, 205), // TakeInFlashStrong
    bg(250, 239, 235), // TakeInFlashSoft
    // The light theme's only 256-colour blue-violet sits next to the selection's blue, so the cold
    // flash is a cyan-green here.
    bg(180, 220, 215),                            // UseFlashStrong
    bg(222, 246, 244),                            // UseFlashSoft
    fg(0, 95, 95),          // SubscriptionHigh: the strongest on a light background
    fg(0, 135, 135),        // SubscriptionMedium
    fg(95, 135, 135),       // SubscriptionLow
    bg(238, 223, 215),      // UpdateProgress
    ansi(AnsiColour::Blue), // Link
    strong(87, 105, 247),   // StrongCode
    strong(102, 102, 102),  // MutedStrong
    bg(220, 255, 220),      // DiffLineAdded
    bg(255, 220, 220),      // DiffLineRemoved
    bg(178, 255, 178),      // DiffWordAdded
    bg(255, 199, 199),      // DiffWordRemoved
    on(fg(36, 138, 61), Rgb::new(220, 255, 220)), // DiffMarkerAdded
    on(fg(207, 34, 46), Rgb::new(255, 220, 220)), // DiffMarkerRemoved
    CODE_COLOURS[0],
    CODE_COLOURS[1],
    CODE_COLOURS[2],
    CODE_COLOURS[3],
    CODE_COLOURS[4],
    CODE_COLOURS[5],
    CODE_COLOURS[6],
    CODE_COLOURS[7],
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

    /// Restyles text painted with the selection background. Several role
    /// colours fall below 4.5:1 on the selection fill once quantised, so the
    /// body text colour replaces them, as it does for text left in the
    /// terminal's default colour. Dim and reverse are dropped because the
    /// terminal, not the theme, decides how they recolour the text. A theme
    /// without a selection fill falls back to reverse video.
    pub(super) fn selected(self, style: Style) -> Style {
        match self.style(Role::Selection).background {
            Some(fill) => Style {
                foreground: self.style(Role::Text).foreground,
                background: Some(fill),
                dim: false,
                reversed: false,
                ..style
            },
            None => Style {
                foreground: None,
                background: None,
                dim: false,
                reversed: true,
                ..style
            },
        }
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
/// disagree.
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
/// cube instead, and Tick Red onto the cube's pure red rather than its orange.
const fn manual_override(colour: Rgb) -> Option<u8> {
    match (colour.red, colour.green, colour.blue) {
        (44, 122, 57) => Some(29), // Success / light
        (34, 92, 43) => Some(22),  // DiffAdded / dark
        // Claude Code's colour-diff sends these two indices itself below true colour.
        (2, 40, 0) => Some(22), // DiffLineAdded / dark
        (4, 71, 0) => Some(28), // DiffWordAdded / dark
        // Tick Red's nearest cube entry is the orange 202; the brand red must read as red.
        (236, 48, 19) => Some(196), // TickMark
        _ => None,
    }
}

pub(super) const fn indexed(colour: Rgb) -> u8 {
    match manual_override(colour) {
        Some(index) => index,
        None => quantise(colour),
    }
}

fn push_colour(sequence: &mut String, colour: Colour, depth: ColourDepth, background: bool) {
    let lead = if background { 48 } else { 38 };
    match (colour, depth) {
        (_, ColourDepth::None) => {}
        (Colour::Ansi(colour), _) => sequence.push_str(&format!(
            "\x1b[{}m",
            colour.foreground_code() + if background { 10 } else { 0 }
        )),
        (Colour::Rgb(colour), ColourDepth::TrueColour) => sequence.push_str(&format!(
            "\x1b[{lead};2;{};{};{}m",
            colour.red, colour.green, colour.blue
        )),
        (Colour::Rgb(colour), ColourDepth::Indexed256) => {
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
/// background role, as Claude Code's message boxes do, and `band` paints some of
/// its columns over it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct RoleLine {
    pub(super) spans: Vec<RoleSpan>,
    pub(super) fill: Option<Role>,
    /// State-bearing text can keep its semantic colour on the selection background.
    pub(super) preserve_selection_colours: bool,
    /// The column `fill` starts at; the columns before it keep their own background.
    pub(super) fill_start: usize,
    pub(super) band: Option<BandFill>,
    pub(super) link: Option<String>,
    /// Set when the row continues the logical line of the row above it after wrapping, so a
    /// copy joins the two rows instead of breaking the line.
    pub(super) continuation: Option<Continuation>,
}

/// A background role over a row's `columns`, such as a bar that runs from a mark in its lead.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct BandFill {
    pub(super) columns: std::ops::Range<usize>,
    pub(super) role: Role,
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

    pub(super) fn filled(self, role: Role) -> Self {
        self.filled_from(role, 0)
    }

    /// Fills the row from column `start` on, leaving the columns before it their own background.
    pub(super) fn filled_from(mut self, role: Role, start: usize) -> Self {
        self.fill = Some(role);
        self.fill_start = start;
        self
    }

    pub(super) fn banded(mut self, columns: std::ops::Range<usize>, role: Role) -> Self {
        self.band = Some(BandFill { columns, role });
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

    /// Cuts the row to `width` display columns without splitting a grapheme, keeping its role.
    pub(super) fn truncate(&mut self, width: usize) {
        let mut column = 0;
        let mut kept = Vec::new();
        for span in self.spans.drain(..) {
            let mut text = String::new();
            let mut full = false;
            for grapheme in span.text.graphemes(true) {
                let grapheme_width = grapheme.width();
                if column + grapheme_width > width {
                    full = true;
                    break;
                }
                text.push_str(grapheme);
                column += grapheme_width;
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

    /// Resolves roles against `theme`. A span without a role keeps the
    /// terminal's own text colour, as a Claude Code `<Text>` without a colour
    /// does; `Role::Text` is the theme's text colour where Claude Code asks for
    /// it. A filled row is padded to `width` so its background spans the row, and
    /// a row with a band at least to the end of its band. A character belongs
    /// to the background of the column it starts in. Text on the selection
    /// background is restyled to stay readable; an Agent Color or a mark keeps
    /// its hue and dimming there, because it only paints marks such as the
    /// member dot or a row's state glyph, never the words of a row.
    pub(super) fn resolve(&self, theme: Theme, width: usize) -> StyledLine {
        let band = self.band.as_ref().map_or(0..0, |band| {
            band.columns.start.min(width)..band.columns.end.min(width)
        });
        let fill_start = self.fill.map_or(0, |_| self.fill_start.min(width));
        let boundaries = [band.start, band.end, fill_start];
        let mut line = StyledLine {
            link: self.link.clone(),
            ..Default::default()
        };
        let mut column = 0;
        for span in &self.spans {
            let mut rest = span.text.as_str();
            while !rest.is_empty() {
                let boundary = boundaries
                    .into_iter()
                    .filter(|boundary| *boundary > column)
                    .min();
                let (piece, after) = match boundary {
                    Some(boundary) => split_at_column(rest, boundary - column),
                    None => (rest, ""),
                };
                line.push(
                    Span::styled(piece, self.style_at(theme, column, span.role))
                        .linked(span.link.clone()),
                );
                column += piece.width();
                rest = after;
            }
        }
        let padded_width = if self.fill.is_some() { width } else { band.end };
        while column < padded_width {
            let end = boundaries
                .into_iter()
                .filter(|boundary| *boundary > column)
                .min()
                .unwrap_or(padded_width)
                .min(padded_width);
            line.push(Span::styled(
                " ".repeat(end - column),
                self.style_at(theme, column, None),
            ));
            column = end;
        }
        line
    }

    /// The style of text in `role` starting at `column`, on the background there.
    fn style_at(&self, theme: Theme, column: usize, role: Option<Role>) -> Style {
        let band = self
            .band
            .as_ref()
            .filter(|band| band.columns.contains(&column));
        let fill = self.fill.filter(|_| column >= self.fill_start);
        let base = band
            .map(|band| band.role)
            .or(fill)
            .map_or(Style::default(), |fill| theme.style(fill));
        let style = role.map_or(base, |role| base.patch(theme.style(role)));
        let on_selection = fill == Some(Role::Selection) || role == Some(Role::Selection);
        if !on_selection || self.preserve_selection_colours || role.is_some_and(Role::is_mark) {
            return style;
        }
        let selected = theme.selected(style);
        match band {
            // The band keeps its own background under the restyled text.
            Some(_) => Style {
                background: style.background,
                ..selected
            },
            None => selected,
        }
    }
}

/// Splits `text` before the first grapheme that starts at or after `columns`.
fn split_at_column(text: &str, columns: usize) -> (&str, &str) {
    let mut used = 0;
    for (index, grapheme) in text.grapheme_indices(true) {
        if used >= columns {
            return text.split_at(index);
        }
        used += grapheme.width();
    }
    (text, "")
}

/// WCAG 2 contrast ratio of the two colours the terminal is actually sent at
/// `depth`, so tests can check emitted output rather than palette entries.
#[cfg(test)]
pub(super) fn emitted_contrast(foreground: Colour, background: Colour, depth: ColourDepth) -> f64 {
    let sent = |colour: Colour| {
        let Colour::Rgb(colour) = colour else {
            panic!("the terminal decides {colour:?}, so its contrast is unknown");
        };
        match depth {
            ColourDepth::Indexed256 => xterm_colour(indexed(colour)),
            ColourDepth::TrueColour | ColourDepth::None => colour,
        }
    };
    let luminance = |colour: Rgb| {
        0.2126 * linear(colour.red) + 0.7152 * linear(colour.green) + 0.0722 * linear(colour.blue)
    };
    let (first, second) = (luminance(sent(foreground)), luminance(sent(background)));
    (first.max(second) + 0.05) / (first.min(second) + 0.05)
}

/// An sRGB channel as linear light.
#[cfg(test)]
fn linear(value: u8) -> f64 {
    let value = f64::from(value) / 255.0;
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// The standard xterm value of an index the quantiser can emit.
#[cfg(test)]
fn xterm_colour(index: u8) -> Rgb {
    match index {
        16..=231 => {
            let cube = index - 16;
            Rgb::new(
                CUBE_LEVELS[usize::from(cube / 36)],
                CUBE_LEVELS[usize::from(cube / 6 % 6)],
                CUBE_LEVELS[usize::from(cube % 6)],
            )
        }
        232..=255 => {
            let level = 8 + (index - 232) * 10;
            Rgb::new(level, level, level)
        }
        _ => unreachable!("the quantiser never emits the terminal-defined colours"),
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
        let expected = Some(Colour::Rgb(Rgb::new(215, 119, 87)));
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
            Some(Colour::Rgb(Rgb::new(153, 153, 153)))
        );
        assert_eq!(
            theme.style(Role::Subtle).foreground,
            Some(Colour::Rgb(Rgb::new(80, 80, 80)))
        );
    }

    #[test]
    fn plain_spans_keep_the_terminal_text_colour_and_text_names_the_theme_colour() {
        let theme = Theme::new(ThemeName::ClaudeCodeLight);
        let line = RoleLine::plain("body")
            .with(" prompt", Role::Text)
            .resolve(theme, 20);
        assert_eq!(line.spans[0].style.foreground, None);
        assert_eq!(
            line.spans[1].style.foreground,
            Some(Colour::Rgb(Rgb::new(0, 0, 0)))
        );
        assert_eq!(line.render(ColourDepth::TrueColour).find("body"), Some(0));
    }

    #[test]
    fn markdown_decorations_add_attributes_without_a_colour() {
        let theme = Theme::new(ThemeName::ClaudeCodeDark);
        for role in [
            Role::Strong,
            Role::Emphasis,
            Role::StrongEmphasis,
            Role::Heading,
            Role::QuoteBar,
            Role::ToolName,
            Role::Title,
        ] {
            assert_eq!(theme.style(role).foreground, None, "{role:?}");
        }
    }

    #[test]
    fn palette_colours_use_the_terminal_palette_at_every_depth() {
        let link = Theme::new(ThemeName::ClaudeCodeLight).style(Role::Link);
        for depth in [ColourDepth::Indexed256, ColourDepth::TrueColour] {
            assert_eq!(open(&link, depth), "\x1b[34m");
        }
        assert_eq!(open(&link, ColourDepth::None), "");
        let code_type = Theme::new(ThemeName::ClaudeCodeDark).style(Role::CodeType);
        assert_eq!(open(&code_type, ColourDepth::TrueColour), "\x1b[2m\x1b[36m");
        assert_eq!(
            open(
                &Theme::new(ThemeName::ClaudeCodeDark).style(Role::CodeMeta),
                ColourDepth::Indexed256
            ),
            "\x1b[90m"
        );
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
        assert_eq!(style.foreground, None);
        assert_eq!(style.background, Some(Colour::Rgb(Rgb::new(55, 55, 55))));
    }

    #[test]
    fn a_filled_row_pads_its_background_to_the_full_width() {
        let theme = Theme::new(ThemeName::ClaudeCodeDark);
        let line = RoleLine::role("❯ ", Role::Subtle)
            .with("中", Role::Text)
            .filled(Role::UserMessageBg)
            .resolve(theme, 8);
        let background = Some(Colour::Rgb(Rgb::new(55, 55, 55)));
        assert!(
            line.spans
                .iter()
                .all(|span| span.style.background == background)
        );
        assert_eq!(line.render(ColourDepth::None), "❯ 中    ");
    }

    #[test]
    fn partial_flash_colours_name_text_and_padding_without_colouring_icons() {
        for name in ThemeName::ALL {
            let theme = Theme::new(name);
            for flash in [
                Role::UseFlashStrong,
                Role::UseFlashSoft,
                Role::TakeInFlashStrong,
                Role::TakeInFlashSoft,
            ] {
                for text in ["", "↻ > ", "↻ > python3"] {
                    let line = RoleLine::role(text, Role::MutedText)
                        .filled_from(flash, 4)
                        .resolve(theme, 16);
                    let backgrounds = line
                        .spans
                        .iter()
                        .flat_map(|span| span.text.chars().map(move |_| span.style.background))
                        .collect::<Vec<_>>();
                    assert_eq!(backgrounds.len(), 16);
                    for (column, background) in backgrounds.into_iter().enumerate() {
                        assert_eq!(
                            background,
                            if column < 4 {
                                None
                            } else {
                                theme.style(flash).background
                            },
                            "{name:?} {flash:?} {text:?} column {column}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_selected_row_stays_readable_at_every_colour_depth() {
        for name in ThemeName::ALL {
            let theme = Theme::new(name);
            let selection = theme.style(Role::Selection).background;
            let line = RoleLine::role("❯ ", Role::Permission)
                .with("agent", Role::Text)
                .with(" · waiting", Role::MutedText)
                .with(" · failed", Role::Error)
                .with(" │", Role::QuoteBar)
                .filled(Role::Selection)
                .resolve(theme, 40);
            for span in &line.spans {
                assert_eq!(span.style.background, selection);
                assert!(
                    !span.style.dim && !span.style.reversed,
                    "{name:?} {:?}",
                    span.text
                );
                let foreground = span
                    .style
                    .foreground
                    .expect("selected text names its colour");
                for depth in [ColourDepth::Indexed256, ColourDepth::TrueColour] {
                    let ratio = emitted_contrast(foreground, selection.unwrap(), depth);
                    assert!(
                        ratio >= 4.5,
                        "{name:?} {depth:?} {:?}: {ratio:.2}",
                        span.text
                    );
                }
            }
        }
    }

    #[test]
    fn state_marks_keep_their_hue_and_blink_on_a_selected_row() {
        for name in ThemeName::ALL {
            let theme = Theme::new(name);
            let line = RoleLine::role("  ", Role::Text)
                .with("$", Role::RunningMarkDim)
                .with(" tests", Role::Text)
                .filled(Role::Selection)
                .resolve(theme, 20);
            let mark = &line.spans[1].style;
            assert_eq!(
                mark.foreground,
                theme.style(Role::Accent).foreground,
                "{name:?}"
            );
            assert!(mark.dim, "the dimmed blink phase survives the selection");
            assert_eq!(mark.background, theme.style(Role::Selection).background);
            assert_eq!(
                line.spans[2].style.foreground,
                theme.style(Role::Text).foreground
            );
            for (mark, state) in [
                (Role::RunningMark, Role::Accent),
                (Role::SuccessMark, Role::Success),
                (Role::ErrorMark, Role::Error),
                (Role::WarningMark, Role::Warning),
            ] {
                assert_eq!(theme.style(mark), theme.style(state), "{name:?} {mark:?}");
            }
        }
    }

    /// The colour `role` paints, as the terminal is sent it at `depth`.
    fn sent_colour(theme: Theme, role: Role, depth: ColourDepth) -> Rgb {
        let style = theme.style(role);
        let Some(Colour::Rgb(colour)) = style.background.or(style.foreground) else {
            panic!("{role:?} is a theme colour");
        };
        match depth {
            ColourDepth::Indexed256 => xterm_colour(indexed(colour)),
            ColourDepth::TrueColour | ColourDepth::None => colour,
        }
    }

    fn background(theme: Theme, role: Role) -> Colour {
        let style = theme.style(role);
        assert_eq!(
            style.foreground, None,
            "{role:?} paints only the background"
        );
        style.background.expect("a fill role paints the background")
    }

    #[test]
    fn row_fills_stay_readable_and_apart_from_each_other_and_the_selection() {
        for name in ThemeName::ALL {
            let theme = Theme::new(name);
            let foreground = |role: Role| theme.style(role).foreground.unwrap();
            for depth in [ColourDepth::Indexed256, ColourDepth::TrueColour] {
                let fills = [
                    Role::TakeInFlashStrong,
                    Role::TakeInFlashSoft,
                    Role::UseFlashStrong,
                    Role::UseFlashSoft,
                    Role::UpdateProgress,
                    Role::Selection,
                ];
                for (index, left) in fills.iter().enumerate() {
                    for right in &fills[index + 1..] {
                        assert_ne!(
                            sent_colour(theme, *left, depth),
                            sent_colour(theme, *right, depth),
                            "{name:?} {depth:?}: {left:?} and {right:?}"
                        );
                    }
                }
                // A flashing row sits beside the selected one, so the cold flash keeps clear of
                // the selection's blue.
                for fill in [Role::UseFlashStrong, Role::UseFlashSoft] {
                    let distance = colour_distance(
                        sent_colour(theme, fill, depth),
                        sent_colour(theme, Role::Selection, depth),
                    );
                    assert!(
                        distance >= 15.0,
                        "{name:?} {depth:?} {fill:?} apart from the selection: {distance:.1}"
                    );
                }
                for (text, fill, minimum) in [
                    (Role::MutedText, Role::TakeInFlashSoft, 4.5),
                    (Role::Text, Role::TakeInFlashStrong, 4.5),
                    (Role::MutedText, Role::TakeInFlashStrong, 3.0),
                    (Role::MutedText, Role::UseFlashSoft, 4.5),
                    (Role::Text, Role::UseFlashStrong, 4.5),
                    (Role::MutedText, Role::UseFlashStrong, 3.0),
                    (Role::MutedText, Role::UpdateProgress, 3.0),
                    (Role::Text, Role::UpdateProgress, 4.5),
                ] {
                    let ratio = emitted_contrast(foreground(text), background(theme, fill), depth);
                    assert!(
                        ratio >= minimum,
                        "{name:?} {depth:?} {text:?} on {fill:?}: {ratio:.2}"
                    );
                }
            }
        }
    }

    /// The terminal background each theme is designed for.
    fn canvas(name: ThemeName) -> Colour {
        Colour::Rgb(match name {
            ThemeName::ClaudeCodeDark => Rgb::new(0, 0, 0),
            ThemeName::ClaudeCodeLight => Rgb::new(255, 255, 255),
        })
    }

    /// The terminal backgrounds each theme meets, with how far the update bar keeps apart from
    /// each: pure black and a common near-black for the dark theme, white for the light one.
    fn terminal_backgrounds(name: ThemeName) -> Vec<(Rgb, f64)> {
        match name {
            ThemeName::ClaudeCodeDark => {
                vec![(Rgb::new(0, 0, 0), 2.0), (Rgb::new(30, 30, 30), 1.8)]
            }
            ThemeName::ClaudeCodeLight => vec![(Rgb::new(255, 255, 255), 1.25)],
        }
    }

    #[test]
    fn the_update_bar_shows_on_every_terminal_and_keeps_the_row_readable() {
        for name in ThemeName::ALL {
            let theme = Theme::new(name);
            let progress = background(theme, Role::UpdateProgress);
            for depth in [ColourDepth::Indexed256, ColourDepth::TrueColour] {
                for (terminal, minimum) in terminal_backgrounds(name) {
                    let ratio = emitted_contrast(progress, Colour::Rgb(terminal), depth);
                    assert!(
                        ratio >= minimum,
                        "{name:?} {depth:?} bar on {terminal:?}: {ratio:.2}"
                    );
                }
                for (role, minimum) in [(Role::Text, 4.5), (Role::MutedText, 3.0)] {
                    let foreground = theme.style(role).foreground.unwrap();
                    let ratio = emitted_contrast(foreground, progress, depth);
                    assert!(
                        ratio >= minimum,
                        "{name:?} {depth:?} {role:?} on the bar: {ratio:.2}"
                    );
                }
                // The `↻` keeps its frequency's colour on the bar; row flashes cover it too.
                for mark in [
                    Role::SubscriptionHigh,
                    Role::SubscriptionMedium,
                    Role::SubscriptionLow,
                ] {
                    let foreground = theme.style(mark).foreground.unwrap();
                    let ratio = emitted_contrast(foreground, progress, depth);
                    assert!(
                        ratio >= 3.0,
                        "{name:?} {depth:?} {mark:?} on the bar: {ratio:.2}"
                    );
                }
            }
        }
    }

    #[test]
    fn tick_mark_is_tick_red_and_reads_as_red_on_every_terminal() {
        for name in ThemeName::ALL {
            let theme = Theme::new(name);
            let tick_red = Colour::Rgb(Rgb::new(236, 48, 19));
            assert_eq!(theme.style(Role::TickMark).foreground, Some(tick_red));
            assert_eq!(
                sent_colour(theme, Role::TickMark, ColourDepth::Indexed256),
                Rgb::new(255, 0, 0),
                "{name:?}: 256 colours send the cube's red, not its orange"
            );
            for depth in [ColourDepth::Indexed256, ColourDepth::TrueColour] {
                for (terminal, _) in terminal_backgrounds(name) {
                    let ratio = emitted_contrast(tick_red, Colour::Rgb(terminal), depth);
                    assert!(
                        ratio >= 3.0,
                        "{name:?} {depth:?} on {terminal:?}: {ratio:.2}"
                    );
                }
                assert_ne!(
                    sent_colour(theme, Role::TickMark, depth),
                    sent_colour(theme, Role::Accent, depth),
                    "{name:?} {depth:?}: Tick Red stays apart from the accent"
                );
            }
        }
    }

    #[test]
    fn subscription_marks_share_one_hue_and_step_down_from_high_to_low_in_both_themes() {
        const MARKS: [Role; 3] = [
            Role::SubscriptionHigh,
            Role::SubscriptionMedium,
            Role::SubscriptionLow,
        ];
        for name in ThemeName::ALL {
            let theme = Theme::new(name);
            for depth in [ColourDepth::Indexed256, ColourDepth::TrueColour] {
                let colours = MARKS.map(|mark| sent_colour(theme, mark, depth));
                assert!(
                    colours[0] != colours[1]
                        && colours[1] != colours[2]
                        && colours[0] != colours[2],
                    "{name:?} {depth:?}: {colours:?}"
                );
                for colour in colours {
                    assert!(
                        colour.green == colour.blue && colour.red < colour.green,
                        "{name:?} {depth:?}: every mark is the same cyan hue, never a state colour: {colour:?}"
                    );
                }
                let strength = MARKS.map(|mark| {
                    emitted_contrast(theme.style(mark).foreground.unwrap(), canvas(name), depth)
                });
                assert!(
                    strength[0] > strength[1] && strength[1] > strength[2],
                    "{name:?} {depth:?}: high stands out most, low least: {strength:?}"
                );
                // A selected row leaves its first column, where the mark is, off the selection,
                // and on the update bar the mark keeps its colour (checked with the bar).
                for (mark, strength) in MARKS.into_iter().zip(strength) {
                    assert!(
                        strength >= 3.0,
                        "{name:?} {depth:?} {mark:?} on the canvas: {strength:.2}"
                    );
                }
            }
        }
    }

    /// CIE76 colour difference, the distance of two colours in CIELAB: about 2 is the least a
    /// viewer notices, and past 10 two colours read as different ones.
    fn colour_distance(left: Rgb, right: Rgb) -> f64 {
        let lab = |colour: Rgb| {
            let (red, green, blue) = (
                linear(colour.red),
                linear(colour.green),
                linear(colour.blue),
            );
            let scale = |value: f64| {
                if value > 0.008856 {
                    value.cbrt()
                } else {
                    7.787 * value + 16.0 / 116.0
                }
            };
            let x = scale((0.4124 * red + 0.3576 * green + 0.1805 * blue) / 0.95047);
            let y = scale(0.2126 * red + 0.7152 * green + 0.0722 * blue);
            let z = scale((0.0193 * red + 0.1192 * green + 0.9505 * blue) / 1.08883);
            [116.0 * y - 16.0, 500.0 * (x - y), 200.0 * (y - z)]
        };
        lab(left)
            .into_iter()
            .zip(lab(right))
            .map(|(left, right)| (left - right).powi(2))
            .sum::<f64>()
            .sqrt()
    }

    /// A row's glyph in the update colour stays clear wherever the glyph sits: on the terminal,
    /// the selection, the update bar and the flash of a row taken in or used; and it never reads
    /// as one of the state colours it covers.
    #[test]
    fn the_update_mark_stays_clear_on_every_row_background_and_apart_from_the_state_colours() {
        for name in ThemeName::ALL {
            let theme = Theme::new(name);
            assert_eq!(
                theme.style(Role::UpdateMarkDim),
                Style {
                    dim: true,
                    ..theme.style(Role::UpdateMark)
                },
                "{name:?}: a flash dims the mark itself"
            );
            let mark = theme.style(Role::UpdateMark).foreground.unwrap();
            let backdrops = terminal_backgrounds(name)
                .into_iter()
                .map(|(terminal, _)| Colour::Rgb(terminal))
                .chain(
                    [
                        Role::Selection,
                        Role::UpdateProgress,
                        Role::TakeInFlashStrong,
                        Role::TakeInFlashSoft,
                        Role::UseFlashStrong,
                        Role::UseFlashSoft,
                    ]
                    .map(|fill| background(theme, fill)),
                )
                .collect::<Vec<_>>();
            for depth in [ColourDepth::Indexed256, ColourDepth::TrueColour] {
                for backdrop in &backdrops {
                    let ratio = emitted_contrast(mark, *backdrop, depth);
                    assert!(
                        ratio >= 3.0,
                        "{name:?} {depth:?} on {backdrop:?}: {ratio:.2}"
                    );
                }
                for state in [
                    Role::SuccessMark,
                    Role::ErrorMark,
                    Role::WarningMark,
                    Role::MutedText,
                ] {
                    let distance = colour_distance(
                        sent_colour(theme, Role::UpdateMark, depth),
                        sent_colour(theme, state, depth),
                    );
                    assert!(
                        distance >= 15.0,
                        "{name:?} {depth:?} apart from {state:?}: {distance:.1}"
                    );
                }
            }
        }
    }

    #[test]
    fn test_screens_recognise_every_selection_shade_the_themes_send() {
        for name in ThemeName::ALL {
            for role in [Role::Selection] {
                for depth in [ColourDepth::Indexed256, ColourDepth::TrueColour] {
                    let sent = open(&Theme::new(name).style(role), depth);
                    let parameters = sent
                        .strip_prefix("\x1b[")
                        .and_then(|sent| sent.strip_suffix('m'))
                        .unwrap();
                    assert!(
                        tickloop_agent_test_support::screen::SELECTION_BACKGROUNDS
                            .contains(&parameters),
                        "{name:?} {role:?} {depth:?}: {sent:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_band_paints_its_columns_over_the_row_and_keeps_its_colour_under_a_selection() {
        for name in ThemeName::ALL {
            let theme = Theme::new(name);
            let backgrounds = |line: &StyledLine| {
                line.spans
                    .iter()
                    .flat_map(|span| span.text.chars().map(move |_| span.style.background))
                    .collect::<Vec<_>>()
            };
            let progress = theme.style(Role::UpdateProgress).background;
            let row = RoleLine::role("  ", Role::MutedText)
                .with("◉", Role::UpdateMark)
                .with(" 中文", Role::MutedText)
                .with("↻", Role::SubscriptionLow);
            let unselected = row
                .clone()
                .banded(1..5, Role::UpdateProgress)
                .resolve(theme, 12);
            assert_eq!(unselected.render(ColourDepth::None), "  ◉ 中文↻");
            assert_eq!(
                backgrounds(&unselected),
                [vec![None], vec![progress; 4], vec![None; 2]].concat(),
                "{name:?}: the wide character that starts inside the band stays in it"
            );
            let short = RoleLine::role("ab", Role::Text)
                .banded(1..6, Role::UpdateProgress)
                .resolve(theme, 12);
            assert_eq!(
                short.render(ColourDepth::None),
                "ab    ",
                "an unfilled row is padded only to the end of its band"
            );
            assert_eq!(
                backgrounds(&short),
                [vec![None], vec![progress; 5]].concat()
            );

            let selection = theme.style(Role::Selection).background;
            let selected = row
                .banded(1..5, Role::UpdateProgress)
                .filled(Role::Selection)
                .resolve(theme, 12);
            assert_eq!(
                backgrounds(&selected),
                [vec![selection], vec![progress; 4], vec![selection; 5]].concat(),
                "{name:?}: the bar keeps its colour, the rest is the selection"
            );
            for span in &selected.spans {
                if let Some(mark) = [("◉", Role::UpdateMark), ("↻", Role::SubscriptionLow)]
                    .into_iter()
                    .find_map(|(text, mark)| (span.text == text).then_some(mark))
                {
                    assert_eq!(
                        span.style.foreground,
                        theme.style(mark).foreground,
                        "a mark keeps its hue"
                    );
                } else if !span.text.trim().is_empty() {
                    assert_eq!(span.style.foreground, theme.style(Role::Text).foreground);
                }
            }
        }
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

    #[test]
    fn truncation_and_background_boundaries_preserve_graphemes_and_following_details() {
        let theme = Theme::new(ThemeName::ClaudeCodeDark);
        for grapheme in ["👩‍💻", "👍🏽", "🇨🇳", "e\u{301}"] {
            let width = grapheme.width();
            let mut clipped = RoleLine::role(grapheme, Role::Text);
            clipped.truncate(width - 1);
            assert!(clipped.text().is_empty());
            let mut line = RoleLine::role(grapheme, Role::Text)
                .with("  14s", Role::MutedText)
                .banded(0..1, Role::UseFlashStrong);
            line.truncate(width + 5);
            assert_eq!(line.text(), format!("{grapheme}  14s"));
            let resolved = line.resolve(theme, width + 5);
            assert_eq!(
                resolved
                    .spans
                    .iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>(),
                line.text()
            );
            assert_eq!(resolved.spans[0].text, grapheme);
            assert_eq!(
                resolved.spans[0].style.background,
                theme.style(Role::UseFlashStrong).background
            );
            assert_eq!(resolved.spans.last().unwrap().style.background, None);
        }
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
                        .is_some_and(|colour| {
                            matches!(colour, Colour::Rgb(colour) if manual_override(colour).is_some())
                        })
                })
            })
            .collect();
        assert_eq!(
            overridden,
            vec![
                Role::TickMark,
                Role::Success,
                Role::DiffAdded,
                Role::AgentSection,
                Role::SuccessMark,
                Role::DiffLineAdded,
                Role::DiffWordAdded,
            ]
        );
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
        let line = StyledLine::plain("Tickloop");
        for depth in [
            ColourDepth::None,
            ColourDepth::Indexed256,
            ColourDepth::TrueColour,
        ] {
            assert_eq!(line.render(depth), "Tickloop");
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
