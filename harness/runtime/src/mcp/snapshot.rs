use super::*;
use sha2::{Digest, Sha256};
use tickloop_harness_protocol::mcp::ToolExposure;

pub(super) struct Tool {
    pub generation: u64,
    pub original: String,
    pub definition: Arc<ToolDefinition>,
    pub original_schema: Arc<Value>,
    pub annotations: Arc<Value>,
    pub exposure: ToolExposure,
    pub validator: jsonschema::Validator,
    pub display_validator: jsonschema::Validator,
    pub fingerprint: [u8; 32],
}
#[derive(Clone, Default)]
pub(super) struct Catalog {
    pub generation: u64,
    pub secret_generation: u64,
    pub tools: BTreeMap<String, Arc<Tool>>,
    pub instructions: String,
    pub warnings: Vec<String>,
    pub resources: Vec<Value>,
}
impl Entry {
    pub(super) fn current_catalog(&self) -> Arc<Catalog> {
        let snapshot = self.snapshot.read().unwrap().clone();
        if snapshot.secret_generation == self.secret_generation.load(Ordering::SeqCst) {
            snapshot
        } else {
            Arc::new(Catalog::default())
        }
    }
    /// Republishes what the connection says about itself, keeping the tools and their
    /// generation.
    pub(super) fn publish_metadata(&self) {
        let _publication = self.publication.lock().unwrap();
        let mut snapshot = (*self.current_catalog()).clone();
        let connection = self.connection.read().unwrap().clone();
        snapshot.instructions = connection
            .as_ref()
            .map(|connection| redact_server_text(self, connection.instructions.clone()))
            .unwrap_or_default();
        snapshot.resources = self.published_resources(connection.as_deref());
        snapshot.warnings = self.published_warnings(connection.as_deref());
        *self.snapshot.write().unwrap() = Arc::new(snapshot);
    }
    fn published_resources(&self, connection: Option<&Connection>) -> Vec<Value> {
        connection
            .and_then(|connection| connection.resources.as_ref())
            .map(|directory| redacted_resources(self, directory.resources.read().unwrap().clone()))
            .unwrap_or_default()
    }
    fn published_warnings(&self, connection: Option<&Connection>) -> Vec<String> {
        let listed = self.tools.read().unwrap().clone();
        listed
            .iter()
            .flat_map(|list| list.warnings.iter())
            .chain(
                connection
                    .map(|connection| connection.warnings.read().unwrap().clone())
                    .unwrap_or_default()
                    .iter(),
            )
            .map(|warning| redact(self, warning.clone()))
            .collect()
    }
    /// Builds the model-facing catalog from the last listed tools; a server that has not listed
    /// any yet has nothing to publish.
    pub(super) fn publish_catalog(&self) {
        let Some(listed) = self.tools.read().unwrap().clone() else {
            return;
        };
        let _publication = self.publication.lock().unwrap();
        let started = std::time::Instant::now();
        let _timing = crate::timing::Phase::new("mcp_catalog_build");
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let mut tools = BTreeMap::new();
        for tool in listed.tools.values().cloned() {
            let mut definition = tool.definition;
            if redact(self, definition.name.clone()) != definition.name {
                continue;
            }
            definition.description = redact_server_text(self, definition.description);
            redact_schema(self, &mut definition.input_schema);
            if definition.input_schema["type"] != "object" {
                continue;
            }
            let Ok(display_validator) = jsonschema::validator_for(&definition.input_schema) else {
                continue;
            };
            let Ok(validator) = jsonschema::validator_for(&tool.schema) else {
                continue;
            };
            let fingerprint = Sha256::digest(
                serde_json::to_vec(&(&tool.original, &tool.schema, &definition))
                    .expect("tool serializes"),
            )
            .into();
            let mut original_schema = tool.schema;
            redact_schema(self, &mut original_schema);
            let mut annotations = tool.annotations;
            redact_schema(self, &mut annotations);
            let exposure = crate::tool_exposure::effective_exposure(
                self.config.exposure,
                &self.config.tool_exposure,
                &tool.original,
            );
            tools.insert(
                definition.name.clone(),
                Arc::new(Tool {
                    generation,
                    original: tool.original,
                    definition: Arc::new(definition),
                    original_schema: Arc::new(original_schema),
                    annotations: Arc::new(annotations),
                    exposure,
                    validator,
                    display_validator,
                    fingerprint,
                }),
            );
        }
        let connection = self.connection.read().unwrap().clone();
        let snapshot = Arc::new(Catalog {
            generation,
            secret_generation: self.secret_generation.load(Ordering::SeqCst),
            tools,
            instructions: connection
                .as_ref()
                .map(|connection| redact_server_text(self, connection.instructions.clone()))
                .unwrap_or_default(),
            warnings: self.published_warnings(connection.as_deref()),
            resources: self.published_resources(connection.as_deref()),
        });
        let previous = std::mem::replace(&mut *self.snapshot.write().unwrap(), snapshot);
        drop(previous);
        self.catalog_builds.fetch_add(1, Ordering::Relaxed);
        self.catalog_micros
            .store(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    }
}
