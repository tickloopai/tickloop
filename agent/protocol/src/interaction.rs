use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ChatId, LoopId, RunId};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QuestionRequest {
    pub id: Uuid,
    pub chat_id: ChatId,
    pub loop_id: LoopId,
    pub run_id: RunId,
    pub call_id: String,
    pub questions: Vec<Question>,
}

/// One question of an `AskUserQuestion` call, shaped like Claude Code's `questionSchema`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Question {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    #[serde(rename = "multiSelect", default)]
    pub multi_select: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// The user's answer to one question: chosen option labels, in the order they were chosen,
/// and optional free text entered through the automatic "Other" choice.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QuestionAnswer {
    pub options: Vec<String>,
    pub other: Option<String>,
}

impl QuestionAnswer {
    pub fn option(label: impl Into<String>) -> Self {
        Self {
            options: vec![label.into()],
            other: None,
        }
    }

    pub fn options<Label: Into<String>>(labels: impl IntoIterator<Item = Label>) -> Self {
        Self {
            options: labels.into_iter().map(Into::into).collect(),
            other: None,
        }
    }

    pub fn other(text: impl Into<String>) -> Self {
        Self {
            options: Vec::new(),
            other: Some(text.into()),
        }
    }

    pub fn with_other(mut self, text: impl Into<String>) -> Self {
        self.other = Some(text.into());
        self
    }

    /// Claude Code joins a multi-select answer, including the "Other" text, with ", ".
    pub fn text(&self) -> String {
        self.options
            .iter()
            .map(String::as_str)
            .chain(self.other.as_deref())
            .collect::<Vec<_>>()
            .join(", ")
    }
}
