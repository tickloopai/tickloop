use std::collections::HashSet;

use serde_json::Value;
use sha2::{Digest, Sha256};
use tickloop_agent_artifacts::CredentialSnapshot;
use tickloop_agent_protocol::{RemoteSkillDefinition, RemoteSkillResource, SkillSource};

pub(crate) const EXTENSION: &str = "io.modelcontextprotocol/skills";
pub(crate) const MAX_FILE_BYTES: usize = 256 * 1024;
const MAX_RESOURCES: usize = 64;
const PRODUCT_NAMES: [&str; 5] = [
    "artifact-design",
    "artifact-diagramming",
    "artifact-dataviz",
    "artifact-draft",
    "artifact-slides",
];

pub(crate) struct RemoteSkillCatalog {
    pub definitions: Vec<RemoteSkillDefinition>,
    pub errors: Vec<String>,
}

pub(crate) fn parse_directory(
    result: &Value,
    credential: &CredentialSnapshot,
) -> Result<RemoteSkillCatalog, String> {
    if result["resultType"] != "complete"
        || result
            .get("nextCursor")
            .is_some_and(|value| !value.is_null())
    {
        return Err("incomplete Artifact Skill directory".into());
    }
    let entries = result["skills"]
        .as_array()
        .ok_or("invalid Artifact Skill directory")?;
    if entries.len() != PRODUCT_NAMES.len() {
        return Err("Artifact Skill directory must contain the five product Skills".into());
    }
    let mut found = HashSet::new();
    let mut definitions = Vec::new();
    let mut errors = Vec::new();
    for entry in entries {
        let name = entry["frontmatter"]["name"]
            .as_str()
            .ok_or("missing Skill name")?;
        if !PRODUCT_NAMES.contains(&name) || !found.insert(name) {
            return Err("unknown or duplicate Artifact Skill".into());
        }
        match parse_definition(entry, name, credential) {
            Ok(definition) => definitions.push(definition),
            Err(error) => errors.push(format!("{name}: {error}")),
        }
    }
    definitions.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(RemoteSkillCatalog {
        definitions,
        errors,
    })
}

fn parse_definition(
    entry: &Value,
    name: &str,
    credential: &CredentialSnapshot,
) -> Result<RemoteSkillDefinition, String> {
    let description = entry["frontmatter"]["description"]
        .as_str()
        .filter(|description| !description.trim().is_empty() && description.len() <= 4096)
        .ok_or("invalid Skill description")?;
    let fields = entry["frontmatter"]
        .as_object()
        .ok_or("invalid Skill frontmatter")?;
    if fields
        .keys()
        .any(|field| !matches!(field.as_str(), "name" | "description"))
    {
        return Err("unsupported remote Skill frontmatter".into());
    }
    let uri = format!("skill://tickloop-artifacts/{name}/SKILL.md");
    if entry["uri"] != uri {
        return Err("invalid Artifact Skill URI".into());
    }
    let mut resources: Vec<RemoteSkillResource> =
        serde_json::from_value(entry["resources"].clone())
            .map_err(|_| "invalid static Skill manifest")?;
    if resources.is_empty()
        || resources.len() > MAX_RESOURCES
        || !resources.iter().any(|resource| resource.uri == uri)
    {
        return Err("incomplete Skill manifest".into());
    }
    let prefix = format!("skill://tickloop-artifacts/{name}/");
    let mut seen = HashSet::new();
    for resource in &resources {
        let path = resource
            .uri
            .strip_prefix(&prefix)
            .ok_or("cross-source Skill resource")?;
        if !valid_resource(path)
            || !seen.insert(&resource.uri)
            || resource.size > MAX_FILE_BYTES as u64
            || !valid_digest(&resource.digest)
        {
            return Err("invalid Skill resource manifest".into());
        }
    }
    resources.sort_by(|left, right| left.uri.cmp(&right.uri));
    Ok(RemoteSkillDefinition {
        name: format!("tickloop-artifacts:{name}"),
        description: description.to_owned(),
        source: SkillSource::Remote {
            server_id: credential.origin.clone(),
            account_id: credential.account_id.clone(),
            uri,
            resources,
        },
    })
}

pub(crate) fn valid_resource(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 512
        && path.ends_with(".md")
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
}

fn valid_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hash| {
        hash.len() == 64
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

pub(crate) fn resource_entry<'a>(
    definition: &'a RemoteSkillDefinition,
    relative: Option<&str>,
) -> Result<&'a RemoteSkillResource, String> {
    let SkillSource::Remote { uri, resources, .. } = &definition.source else {
        return Err("Skill is not remote".into());
    };
    let target = if let Some(path) = relative {
        if !valid_resource(path) || path == "SKILL.md" {
            return Err("invalid Skill reference path".into());
        }
        format!(
            "{}{path}",
            uri.strip_suffix("SKILL.md").ok_or("invalid Skill URI")?
        )
    } else {
        uri.clone()
    };
    resources
        .iter()
        .find(|resource| resource.uri == target)
        .ok_or_else(|| "resource is not in the held Skill manifest".into())
}

pub(crate) fn verify_text(
    resource: &RemoteSkillResource,
    result: &Value,
) -> Result<String, String> {
    let contents = result["contents"]
        .as_array()
        .filter(|contents| contents.len() == 1)
        .ok_or("invalid Skill resource response")?;
    let content = &contents[0];
    if content["uri"] != resource.uri || content["mimeType"] != "text/markdown" {
        return Err("Skill resource identity or encoding changed".into());
    }
    let text = content["text"]
        .as_str()
        .ok_or("Skill resource is not UTF-8 text")?;
    if text.len() != resource.size as usize
        || format!("sha256:{:x}", Sha256::digest(text.as_bytes())) != resource.digest
    {
        return Err(
            "Skill resource size or digest mismatch; refresh the directory before retrying".into(),
        );
    }
    Ok(text.to_owned())
}
