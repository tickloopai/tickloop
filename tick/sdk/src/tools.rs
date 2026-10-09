//! The Tick Tool catalog a Tick offers the Agent.
//!
//! A query tool is a stateless closure over the Tick's services: it never
//! borrows the view, so the runtime can run several of them concurrently while
//! the Panel keeps drawing. An action tool declares only its arguments; the
//! runtime hands it to the view's `act` on the UI path and publishes the next
//! Tick Update when the content changed. Argument schemas are derived from the
//! argument type, and the runtime returns the schema with an invalid-parameter
//! error.

use std::{io, sync::Arc};

use futures_util::future::BoxFuture;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tick_protocol::TickTool;

type QueryHandler<S, E> = Arc<
    dyn for<'a> Fn(&'a S, Value) -> Result<BoxFuture<'a, Result<Value, E>>, String> + Send + Sync,
>;

type ArgumentCheck = Arc<dyn Fn(&Value) -> Result<(), String> + Send + Sync>;

/// A Tick's published tools and the query tool that recovers dropped Tick Updates.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TickCatalog {
    pub recover_with: Option<&'static str>,
    pub tools: Vec<TickTool>,
}

/// An action tool call on its way to the view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub tool: String,
    pub args: Value,
}

impl ToolCall {
    /// The runtime checked these arguments against the declared schema before
    /// dispatching, so a failure here means the declaration and the view disagree.
    pub fn arguments<A: DeserializeOwned>(&self) -> io::Result<A> {
        serde_json::from_value(self.args.clone())
            .map_err(|error| io::Error::other(format!("{:?} arguments: {error}", self.tool)))
    }
}

/// What the runtime does with an accepted call.
pub enum ToolDispatch<'a, E> {
    Query(BoxFuture<'a, Result<Value, E>>),
    Action(ToolCall),
}

enum Handler<S: ?Sized, E> {
    Query(QueryHandler<S, E>),
    Action(ArgumentCheck),
}

/// One declared tool: what `describe` publishes, plus how the runtime runs it.
pub struct DeclaredTool<S: ?Sized, E> {
    tool: TickTool,
    handler: Handler<S, E>,
}

impl<S: ?Sized, E> DeclaredTool<S, E> {
    pub fn mutates(&self) -> bool {
        self.tool.mutates
    }

    pub fn params(&self) -> &Value {
        &self.tool.params
    }

    /// Checks the arguments against the declared schema and either starts the
    /// query, or hands the action back for the UI path. The error is the
    /// message explaining why the arguments do not match the schema.
    pub fn prepare<'a>(&self, services: &'a S, args: Value) -> Result<ToolDispatch<'a, E>, String> {
        match &self.handler {
            Handler::Query(query) => query(services, args).map(ToolDispatch::Query),
            Handler::Action(check) => check(&args).map(|()| {
                ToolDispatch::Action(ToolCall {
                    tool: self.tool.name.clone(),
                    args,
                })
            }),
        }
    }
}

/// A Tick's declared tools. `S` is the services its query tools close over and `E` the error
/// they fail with.
pub struct Tools<S: ?Sized, E> {
    tools: Vec<DeclaredTool<S, E>>,
    recovery_tool: Option<&'static str>,
}

impl<S: ?Sized, E> Default for Tools<S, E> {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            recovery_tool: None,
        }
    }
}

impl<S: ?Sized, E> Tools<S, E> {
    pub fn query<A>(
        mut self,
        name: &'static str,
        description: &'static str,
        handler: impl for<'a> Fn(&'a S, A) -> BoxFuture<'a, Result<Value, E>> + Send + Sync + 'static,
    ) -> Self
    where
        A: JsonSchema + DeserializeOwned + Send + 'static,
    {
        self.tools.push(DeclaredTool {
            tool: declared::<A>(name, description, false),
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
            tool: declared::<A>(name, description, true),
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

    pub fn get(&self, name: &str) -> Option<&DeclaredTool<S, E>> {
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

/// A tool as `describe` publishes it, with the JSON Schema derived from its argument type.
fn declared<A: JsonSchema>(name: &str, description: &str, mutates: bool) -> TickTool {
    TickTool {
        name: name.to_owned(),
        description: description.to_owned(),
        params: schemars::schema_for!(A).to_value(),
        mutates,
    }
}
