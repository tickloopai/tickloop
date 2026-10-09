use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::event_chart::EventSession;
use tick_provider::error::{Error, Result};

pub const RESEARCH_CONTEXT_ENV: &str = "TICKLOOP_RESEARCH_CONTEXT";
pub const MAX_CONTEXT_BYTES: usize = 64 * 1024;

pub const EARNINGS_CONTEXT_SCHEMA: &str = "equity.earnings.v1";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResearchContext {
    pub symbol: String,
    pub period_id: String,
    #[serde(default)]
    pub fiscal_period_end: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub release_session: EventSession,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub conference_at: Option<String>,
    #[serde(default)]
    pub status: ResearchStatus,
    #[serde(default)]
    pub documents: Vec<ResearchDocument>,
    #[serde(default)]
    pub evidence: Option<ResearchEvidence>,
}

impl ResearchContext {
    pub fn from_environment() -> Result<Option<Self>> {
        let value = match std::env::var(RESEARCH_CONTEXT_ENV) {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(_) => return Err(Error::usage("research context environment must be UTF-8")),
        };
        if value.len() > MAX_CONTEXT_BYTES {
            return Err(Error::usage("research context exceeds its size limit"));
        }
        let context: Self = serde_json::from_str(&value)
            .map_err(|error| Error::usage(format!("invalid research context: {error}")))?;
        context.validate()?;
        Ok(Some(context))
    }

    pub fn identity(&self) -> String {
        format!("{}:{}", self.symbol, self.period_id)
    }

    pub fn title(&self) -> String {
        format!("{} · {}", self.symbol, self.period_id)
    }

    pub fn validate(&self) -> Result<()> {
        if self.symbol.trim().is_empty() || self.period_id.trim().is_empty() {
            return Err(Error::usage(
                "research context requires symbol and period_id",
            ));
        }
        if self.symbol.len() > 64
            || self.period_id.len() > 128
            || self.documents.len() > 64
            || serde_json::to_vec(self).map_or(true, |value| value.len() > MAX_CONTEXT_BYTES)
        {
            return Err(Error::usage("research context exceeds its size limits"));
        }
        for date in [&self.release_date, &self.fiscal_period_end]
            .into_iter()
            .flatten()
        {
            if tick_provider::dates::parse_iso_date(date).is_none() {
                return Err(Error::usage("research dates must be YYYY-MM-DD"));
            }
        }
        for timestamp in [&self.published_at, &self.conference_at]
            .into_iter()
            .flatten()
        {
            time::OffsetDateTime::parse(timestamp, &time::format_description::well_known::Rfc3339)
                .map_err(|_| {
                    Error::usage("research timestamps require RFC3339 with an explicit offset")
                })?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResearchStatus {
    Upcoming,
    #[default]
    Released,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResearchDocument {
    pub title: String,
    pub location: String,
    pub kind: String,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub verification: String,
    #[serde(default)]
    pub match_basis: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResearchEvidence {
    pub label: String,
    pub text: String,
    #[serde(default)]
    pub metric_key: Option<String>,
    #[serde(default)]
    pub search_terms: Vec<String>,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub document_id: Option<String>,
    #[serde(default)]
    pub revision: Option<String>,
    #[serde(default)]
    pub section: Option<String>,
    #[serde(default)]
    pub offset: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetResearchContext {
    pub context: ResearchContext,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ResearchTarget {
    Reader,
    Chart,
    Options,
    Chat,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResearchNavigation {
    pub target: ResearchTarget,
    pub context: ResearchContext,
}
