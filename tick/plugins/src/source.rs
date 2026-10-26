//! Structured origins for marketplaces and their Git plugins.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::PluginError;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitSource {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdirectory: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "source", rename_all = "snake_case")]
pub enum MarketplaceSource {
    Git(GitSource),
    Directory(PathBuf),
    File(PathBuf),
    Url(String),
}

impl MarketplaceSource {
    /// Parses a local path, Git URL, HTTPS marketplace document, or `owner/repo#ref`.
    pub fn parse(text: &str) -> Result<Self, PluginError> {
        let path = PathBuf::from(text);
        if path.exists() {
            let path = path
                .canonicalize()
                .map_err(PluginError::io("cannot open marketplace"))?;
            return Ok(if path.is_dir() {
                Self::Directory(path)
            } else {
                Self::File(path)
            });
        }
        let (location, reference) = text
            .split_once('#')
            .map_or((text, None), |(url, reference)| {
                (url, Some(reference.to_owned()))
            });
        if location.starts_with("https://")
            && reqwest::Url::parse(location).is_ok_and(|url| url.path().ends_with(".json"))
            && reference.is_none()
        {
            return Ok(Self::Url(location.to_owned()));
        }
        let url = if location.starts_with("https://")
            || location.starts_with("ssh://")
            || location.starts_with("file://")
            || location.starts_with("git@")
        {
            location.to_owned()
        } else {
            let parts: Vec<_> = location.split('/').collect();
            if parts.len() != 2
                || parts.iter().any(|part| {
                    part.is_empty()
                        || *part == "."
                        || *part == ".."
                        || !part
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
                })
            {
                return Err(PluginError::Invalid(format!(
                    "{text:?} is not a marketplace path, URL or GitHub repository"
                )));
            }
            format!("https://github.com/{location}.git")
        };
        Ok(Self::Git(GitSource {
            url,
            reference,
            sha: None,
            subdirectory: None,
        }))
    }
}
