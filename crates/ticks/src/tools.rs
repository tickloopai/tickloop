//! The Tick Tool catalog a Tick offers the Agent.
//!
//! A query tool is a stateless closure over the Tick's services: it never
//! borrows the view, so the runtime can run several of them concurrently while
//! the Panel keeps drawing. An action tool declares only its arguments; the
//! runtime hands it to the view's `act` on the UI path and publishes the next
//! Tick Update when the content changed. Argument schemas are derived from the
//! argument type, and the runtime returns the schema with an invalid-parameter
//! error.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    api::QuoteApi,
    error::{Error, Result},
    metadata::{TickCatalog, TickKind, TickTool},
};

type QueryHandler<S> = Arc<
    dyn for<'a> Fn(&'a S, Value) -> std::result::Result<BoxFuture<'a, Result<Value>>, String>
        + Send
        + Sync,
>;

type ArgumentCheck = Arc<dyn Fn(&Value) -> std::result::Result<(), String> + Send + Sync>;

/// An action tool call on its way to the view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub tool: String,
    pub args: Value,
}

impl ToolCall {
    /// The runtime checked these arguments against the declared schema before
    /// dispatching, so a failure here means the declaration and the view disagree.
    pub fn arguments<A: DeserializeOwned>(&self) -> Result<A> {
        serde_json::from_value(self.args.clone())
            .map_err(|error| Error::usage(format!("{:?} arguments: {error}", self.tool)))
    }
}

/// What the runtime does with an accepted call.
pub enum ToolDispatch<'a> {
    Query(BoxFuture<'a, Result<Value>>),
    Action(ToolCall),
}

enum Handler<S: ?Sized> {
    Query(QueryHandler<S>),
    Action(ArgumentCheck),
}

/// One declared tool: what `describe` publishes, plus how the runtime runs it.
pub struct DeclaredTool<S: ?Sized> {
    tool: TickTool,
    handler: Handler<S>,
}

impl<S: ?Sized> DeclaredTool<S> {
    pub fn mutates(&self) -> bool {
        self.tool.mutates
    }

    pub fn params(&self) -> &Value {
        &self.tool.params
    }

    /// Checks the arguments against the declared schema and either starts the
    /// query, or hands the action back for the UI path. The error is the
    /// message explaining why the arguments do not match the schema.
    pub fn prepare<'a>(
        &self,
        services: &'a S,
        args: Value,
    ) -> std::result::Result<ToolDispatch<'a>, String> {
        match &self.handler {
            Handler::Query(query) => query(services, args).map(ToolDispatch::Query),
            Handler::Action(check) => check(&args).map(|()| {
                ToolDispatch::Action(ToolCall {
                    tool: self.tool.name.clone().into_owned(),
                    args,
                })
            }),
        }
    }
}

/// A Tick's declared tools. `S` is the services its query tools close over; the
/// shipped Ticks use the Longbridge quote trait, a third-party Tick brings its own.
pub struct Tools<S: ?Sized = dyn QuoteApi> {
    tools: Vec<DeclaredTool<S>>,
    recovery_tool: Option<&'static str>,
}

impl<S: ?Sized> Default for Tools<S> {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            recovery_tool: None,
        }
    }
}

impl<S: ?Sized> Tools<S> {
    pub fn query<A>(
        mut self,
        name: &'static str,
        description: &'static str,
        handler: impl for<'a> Fn(&'a S, A) -> BoxFuture<'a, Result<Value>> + Send + Sync + 'static,
    ) -> Self
    where
        A: JsonSchema + DeserializeOwned + Send + 'static,
    {
        self.tools.push(DeclaredTool {
            tool: TickTool::declared::<A>(name, description, false),
            handler: Handler::Query(Arc::new(
                move |services, args| match serde_json::from_value::<A>(args) {
                    Ok(args) => Ok(handler(services, args)),
                    Err(error) => Err(error.to_string()),
                },
            )),
        });
        self
    }

    /// Declares a tool that changes what the running Tick Instance shows. The
    /// view receives it through `act`, so no handler is registered here.
    pub fn action<A>(mut self, name: &'static str, description: &'static str) -> Self
    where
        A: JsonSchema + DeserializeOwned + 'static,
    {
        self.tools.push(DeclaredTool {
            tool: TickTool::declared::<A>(name, description, true),
            handler: Handler::Action(Arc::new(|args| {
                serde_json::from_value::<A>(args.clone())
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            })),
        });
        self
    }

    /// Names the query tool that rebuilds what the latest-only update stream dropped.
    pub fn recover_with(mut self, tool: &'static str) -> Self {
        self.recovery_tool = Some(tool);
        self
    }

    pub fn get(&self, name: &str) -> Option<&DeclaredTool<S>> {
        self.tools
            .iter()
            .find(|declared| declared.tool.name == name)
    }

    /// What `describe` publishes: the tools without their handlers, so a catalog
    /// can be read without naming the services its queries close over.
    pub fn catalog(&self) -> TickCatalog {
        TickCatalog {
            recover_with: self.recovery_tool,
            tools: self
                .tools
                .iter()
                .map(|declared| declared.tool.clone())
                .collect(),
        }
    }
}

pub fn for_kind(kind: TickKind) -> Tools {
    match kind {
        TickKind::Quote => crate::commands::quote::tools(),
        TickKind::Watchlist => crate::commands::watchlist::tools(),
        TickKind::Kline => crate::commands::kline::tools(),
        TickKind::Earnings => crate::commands::earnings::tools(),
        TickKind::Options => crate::commands::options::tools(),
        TickKind::Reader => crate::commands::reader::tools(),
        TickKind::Positions | TickKind::News => Tools::default(),
    }
}
