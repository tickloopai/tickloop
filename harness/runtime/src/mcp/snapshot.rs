use super::*;
use sha2::{Digest, Sha256};
use tickloop_harness_protocol::mcp::ToolExposure;

pub(super) struct Tool {
    pub generation: u64,
    pub original: String,
    pub definition: ToolDefinition,
    pub original_schema: Value,
    pub annotations: Value,
    pub exposure: ToolExposure,
    pub validator: jsonschema::Validator,
    pub display_validator: jsonschema::Validator,
    pub fingerprint: [u8; 32],
}
#[derive(Clone, Default)]
pub(super) struct Catalog {
    pub generation: u64,
    pub revision: u64,
    pub secret_generation: u64,
    pub tools: BTreeMap<String, Arc<Tool>>,
    pub instructions: String,
    pub warnings: Vec<String>,
    pub resources: Vec<Value>,
}
impl Entry {
    pub(super) fn current_catalog(&self, connection: &Connection) -> Arc<Catalog> {
        let snapshot = connection.snapshot.read().unwrap().clone();
        if snapshot.secret_generation == self.secret_generation.load(Ordering::SeqCst) {
            snapshot
        } else {
            Arc::new(Catalog::default())
        }
    }
    pub(super) fn publish_metadata(&self, connection: &Connection) {
        let _publication = self.publication.lock().unwrap();
        let mut snapshot = (*self.current_catalog(connection)).clone();
        snapshot.resources = connection
            .resources
            .as_ref()
            .map(|directory| redacted_resources(self, directory.resources.read().unwrap().clone()))
            .unwrap_or_default();
        snapshot.warnings = connection
            .warnings
            .read()
            .unwrap()
            .iter()
            .map(|warning| redact(self, warning.clone()))
            .collect();
        *connection.snapshot.write().unwrap() = Arc::new(snapshot);
    }
    pub(super) fn publish_catalog(&self, connection: &Connection) {
        let _publication = self.publication.lock().unwrap();
        let started = std::time::Instant::now();
        let _timing = crate::timing::Phase::new("mcp_catalog_build");
        let revision = connection.catalog_revision.load(Ordering::SeqCst);
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let raw = connection.tools.read().unwrap().clone();
        let mut tools = BTreeMap::new();
        for (_, tool) in raw {
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
                    definition,
                    original_schema,
                    annotations,
                    exposure,
                    validator,
                    display_validator,
                    fingerprint,
                }),
            );
        }
        let snapshot = Arc::new(Catalog {
            generation,
            revision,
            secret_generation: self.secret_generation.load(Ordering::SeqCst),
            tools,
            instructions: redact_server_text(self, connection.instructions.clone()),
            warnings: connection
                .warnings
                .read()
                .unwrap()
                .iter()
                .map(|warning| redact(self, warning.clone()))
                .collect(),
            resources: connection
                .resources
                .as_ref()
                .map(|directory| {
                    redacted_resources(self, directory.resources.read().unwrap().clone())
                })
                .unwrap_or_default(),
        });
        if connection.catalog_revision.load(Ordering::SeqCst) == revision {
            let previous = std::mem::replace(&mut *connection.snapshot.write().unwrap(), snapshot);
            drop(previous);
            self.catalog_builds.fetch_add(1, Ordering::Relaxed);
            self.catalog_micros
                .store(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        }
    }
}
