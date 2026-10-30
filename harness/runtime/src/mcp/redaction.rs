use super::*;
use crate::secret_redaction::SecretRedactor;

pub(super) struct Redactor {
    secrets: std::collections::BTreeSet<String>,
    matcher: SecretRedactor,
}
impl Redactor {
    pub(super) fn new(entry: &Entry) -> Option<Self> {
        let mut secrets = entry.environment_secrets.clone();
        secrets.extend(entry.credential_secrets.read().unwrap().iter().cloned());
        secrets.extend(entry.config.sensitive_values.iter().map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string())
        }));
        if let Some(token) = entry.token.read().unwrap().as_ref() {
            secrets.push(token.secret().to_owned());
        }
        if let Some(token) = entry
            .credential
            .read()
            .unwrap()
            .as_ref()
            .and_then(|saved| saved.token.as_ref())
        {
            secrets.push(token.access_token.clone());
            secrets.extend(token.refresh_token.clone());
        }
        match &entry.config.transport {
            McpTransport::Http { url, headers } => {
                for value in headers.values() {
                    secrets.push(value.clone());
                    let mut parts = value.split_whitespace();
                    if parts.next().is_some_and(|scheme| {
                        scheme.eq_ignore_ascii_case("bearer")
                            || scheme.eq_ignore_ascii_case("basic")
                    }) && let Some(token) = parts.next()
                    {
                        secrets.push(token.into());
                    }
                }
                if let Ok(url) = reqwest::Url::parse(url) {
                    secrets.extend(url.query_pairs().map(|(_, value)| value.into_owned()));
                }
            }
            McpTransport::Stdio { env, args, .. } => {
                secrets.extend(env.values().cloned());
                for argument in args {
                    if let Some((_, value)) = argument.split_once('=') {
                        secrets.push(value.into());
                    } else if !argument.starts_with('-') {
                        secrets.push(argument.clone());
                    }
                }
            }
            _ => {}
        }
        let secrets: std::collections::BTreeSet<_> = secrets
            .into_iter()
            .filter(|secret| !secret.is_empty())
            .collect();
        if entry
            .redactor
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|old| old.secrets == secrets)
        {
            return None;
        }
        let matcher = SecretRedactor::new(&secrets);
        Some(Self { secrets, matcher })
    }
    pub(super) fn redact(&self, content: String) -> String {
        self.matcher.redact(content)
    }
}
