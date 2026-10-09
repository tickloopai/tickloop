use serde::Serialize;
use tick_protocol::TickTool;
use tick_provider::{
    Interface,
    error::{Error, Result},
};
use tick_sdk::stream::Describe;

/// Ticks whose content comes from services the shared runtime does not hand to query tools, so
/// they declare no recovery tool. The Agent re-reads their Panel content with `ReadTick`.
const EXEMPT_FROM_RECOVERY: [TickKind; 2] = [TickKind::Positions, TickKind::News];

#[derive(clap::ValueEnum, Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TickKind {
    Quote,
    Watchlist,
    Kline,
    Positions,
    News,
    Earnings,
    Options,
    Reader,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TickRuntimeClass {
    Command,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TickConfigurationRequirement {
    None,
    Optional,
    UserInput,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub struct MinimumPanelSize {
    pub columns: u16,
    pub rows: u16,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct TickDescriptor {
    pub kind: TickKind,
    pub title: &'static str,
    pub description: &'static str,
    pub runtime: TickRuntimeClass,
    pub configuration: TickConfigurationRequirement,
    pub minimum_panel_size: Option<MinimumPanelSize>,
    /// Version of the Tick Instance stream protocol this Tick speaks.
    pub protocol: u32,
    /// The query tool that rebuilds what the latest-only update stream dropped.
    pub recover_with: Option<&'static str>,
    pub tools: Vec<TickTool>,
}

impl TickDescriptor {
    /// Rejects a catalog the Agent could not use: a duplicate tool name, a
    /// recovery tool that is not a declared query tool, and — the ADR 0007 rule —
    /// a Tick that pushes Tick Updates without naming the query tool that
    /// recovers what the latest-only stream drops. Every shipped Tick pushes
    /// updates, so only the kinds in [`EXEMPT_FROM_RECOVERY`] may leave it unset.
    pub fn validate(&self) -> Result<()> {
        if let Some(duplicate) = self.tools.iter().enumerate().find_map(|(index, tool)| {
            self.tools[..index]
                .iter()
                .any(|earlier| earlier.name == tool.name)
                .then(|| tool.name.clone())
        }) {
            return Err(Error::usage(format!(
                "{} declares the Tick Tool {duplicate:?} twice",
                self.kind.name()
            )));
        }
        let queries = || self.tools.iter().filter(|tool| !tool.mutates);
        match self.recover_with {
            Some(recovery) if !queries().any(|tool| tool.name == recovery) => {
                Err(Error::usage(format!(
                    "{} recovers Tick Updates with {recovery:?}, which is not a declared query tool",
                    self.kind.name()
                )))
            }
            None if !EXEMPT_FROM_RECOVERY.contains(&self.kind) => Err(Error::usage(format!(
                "{} pushes Tick Updates but names no recover_with query tool",
                self.kind.name()
            ))),
            _ => Ok(()),
        }
    }
}

impl TickKind {
    pub const ALL: [Self; 8] = [
        Self::Quote,
        Self::Watchlist,
        Self::Kline,
        Self::Positions,
        Self::News,
        Self::Earnings,
        Self::Options,
        Self::Reader,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Quote => "quote",
            Self::Watchlist => "watchlist",
            Self::Kline => "kline",
            Self::Positions => "positions",
            Self::News => "news",
            Self::Earnings => "earnings",
            Self::Options => "options",
            Self::Reader => "reader",
        }
    }

    /// The smallest Panel a Tick renders in. The renderer reads this on every
    /// frame, so it stays independent of the Tick Tool catalog.
    pub fn minimum_panel_size(self) -> MinimumPanelSize {
        let (columns, rows) = match self {
            Self::Quote | Self::Watchlist => (32, 6),
            Self::Kline => (40, 14),
            Self::Positions => (40, 8),
            Self::News => (40, 10),
            Self::Earnings => (100, 14),
            Self::Options => (60, 14),
            Self::Reader => (40, 10),
        };
        MinimumPanelSize { columns, rows }
    }

    /// The provider interfaces this Tick reads its data through.
    pub fn interfaces(self) -> &'static [Interface] {
        match self {
            Self::Quote | Self::Watchlist | Self::Kline | Self::Earnings => &[Interface::Quote],
            Self::Positions => &[Interface::Quote, Interface::Trade],
            Self::News => &[Interface::Quote, Interface::News],
            Self::Options => &[Interface::Options],
            Self::Reader => &[],
        }
    }

    pub fn descriptor(self) -> TickDescriptor {
        let (title, description, configuration) = match self {
            Self::Quote => (
                "Quote",
                "Watch current prices for one or more symbols.",
                TickConfigurationRequirement::UserInput,
            ),
            Self::Watchlist => (
                "Watchlist",
                "Monitor a Longbridge watchlist group.",
                TickConfigurationRequirement::Optional,
            ),
            Self::Kline => (
                "Kline",
                "Inspect candlesticks for a symbol.",
                TickConfigurationRequirement::UserInput,
            ),
            Self::Positions => (
                "Positions",
                "Monitor holdings and profit and loss.",
                TickConfigurationRequirement::None,
            ),
            Self::News => (
                "News",
                "Read market-wide or symbol news.",
                TickConfigurationRequirement::Optional,
            ),
            Self::Earnings => (
                "Earnings",
                "Review past earnings and how the market reacted.",
                TickConfigurationRequirement::UserInput,
            ),
            Self::Options => (
                "Futu Options",
                "Inspect option pricing, IV and saved snapshots from Futu OpenD.",
                TickConfigurationRequirement::UserInput,
            ),
            Self::Reader => (
                "Reader",
                "Read original financial documents and cite stable evidence.",
                TickConfigurationRequirement::UserInput,
            ),
        };
        let catalog = crate::tools::for_kind(self).catalog();
        TickDescriptor {
            kind: self,
            title,
            description,
            runtime: TickRuntimeClass::Command,
            configuration,
            minimum_panel_size: Some(self.minimum_panel_size()),
            protocol: tick_protocol::NATIVE_PROTOCOL_VERSION,
            recover_with: catalog.recover_with,
            tools: catalog.tools,
        }
    }
}

impl Describe for TickKind {
    fn tick_type(&self) -> &str {
        self.name()
    }

    fn descriptor(&self) -> std::result::Result<tick_protocol::TickDescriptor, String> {
        let descriptor = TickKind::descriptor(*self);
        descriptor.validate().map_err(|error| error.message)?;
        Ok(tick_protocol::TickDescriptor {
            protocol: descriptor.protocol,
            tick_type: self.name().to_owned(),
            title: descriptor.title.to_owned(),
            description: descriptor.description.to_owned(),
            recover_with: descriptor.recover_with.map(str::to_owned),
            tools: descriptor.tools,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, mutates: bool) -> TickTool {
        TickTool {
            name: name.to_owned(),
            description: String::new(),
            params: serde_json::Value::Null,
            mutates,
        }
    }

    fn descriptor(recover_with: Option<&'static str>, tools: Vec<TickTool>) -> TickDescriptor {
        TickDescriptor {
            recover_with,
            tools,
            ..TickKind::Watchlist.descriptor()
        }
    }

    #[test]
    fn every_shipped_tick_declares_a_usable_catalog() {
        for kind in TickKind::ALL {
            kind.descriptor().validate().unwrap();
        }
    }

    #[test]
    fn a_tick_that_pushes_updates_must_name_the_tool_that_recovers_them() {
        let error = descriptor(None, vec![tool("rows", false)])
            .validate()
            .unwrap_err();
        assert!(error.message.contains("recover_with"), "{error:?}");

        let without_any_tool = descriptor(None, Vec::new()).validate().unwrap_err();
        assert!(without_any_tool.message.contains("recover_with"));
    }

    #[test]
    fn only_the_exempt_kinds_may_leave_recover_with_unset() {
        for kind in TickKind::ALL {
            let descriptor = TickDescriptor {
                recover_with: None,
                tools: Vec::new(),
                ..kind.descriptor()
            };
            assert_eq!(
                descriptor.validate().is_ok(),
                EXEMPT_FROM_RECOVERY.contains(&kind),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn recover_with_must_name_a_declared_query_tool() {
        let unknown = descriptor(Some("candles"), vec![tool("rows", false)])
            .validate()
            .unwrap_err();
        assert!(unknown.message.contains("candles"), "{unknown:?}");

        let mutating = descriptor(Some("mark"), vec![tool("mark", true)])
            .validate()
            .unwrap_err();
        assert!(mutating.message.contains("mark"), "{mutating:?}");
    }

    #[test]
    fn a_duplicate_tool_name_is_rejected() {
        let error = descriptor(Some("rows"), vec![tool("rows", false), tool("rows", false)])
            .validate()
            .unwrap_err();
        assert!(error.message.contains("twice"), "{error:?}");
    }
}
