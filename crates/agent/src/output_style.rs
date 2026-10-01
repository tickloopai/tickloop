//! Output styles for Agent replies, following Claude Code's `constants/outputStyles.ts`.
//! Standard adds no prompt; other styles append an `# Output Style: <name>` section.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputStyle {
    #[default]
    Standard,
    Concise,
    Detailed,
}

const CONCISE_PROMPT: &str = "# Concise Style Active
Keep responses short. Lead with the answer or result in the first sentence. Answer simple questions in one to three sentences of plain prose. Use headings, lists and tables only when the content has real structure. Do not restate the request, narrate each step or repeat a summary at the end. Keep caveats that change what the user should do next.";

const DETAILED_PROMPT: &str = "# Detailed Style Active
Give complete explanations. Lead with the answer, then explain the reasoning, the evidence it rests on, and the assumptions or risks that could change it. Define terms the user may not know. Use headings, lists or tables when they make comparisons or steps easier to follow, and put explanations in prose before or after a table rather than inside its cells. These explanations may exceed the usual length constraints, but stay focused and relevant.";

impl OutputStyle {
    pub const ALL: [Self; 3] = [Self::Standard, Self::Concise, Self::Detailed];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Standard => "Standard",
            Self::Concise => "Concise",
            Self::Detailed => "Detailed",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Standard => "Brief, direct answers suited to most tasks",
            Self::Concise => "The shortest answers, leading with the result",
            Self::Detailed => "Fuller explanations with reasoning and assumptions",
        }
    }

    /// Returns the system prompt section, if this style adds one.
    pub fn system_prompt_section(self) -> Option<String> {
        let prompt = match self {
            Self::Standard => return None,
            Self::Concise => CONCISE_PROMPT,
            Self::Detailed => DETAILED_PROMPT,
        };
        Some(format!("# Output Style: {}\n{prompt}", self.label()))
    }
}
