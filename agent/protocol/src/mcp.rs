//! Private, session-scoped MCP stdio launch descriptors. Never place these in history or events.
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::PathBuf,
};

pub const MAX_SESSION_MCP_SERVERS: usize = 8;
pub const MAX_SESSION_MCP_DESCRIPTOR_BYTES: usize = 128 * 1024;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMcpServer {
    pub name: String,
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
}

// Even names and paths may contain user data. Debug never emits descriptor-derived values.
impl fmt::Debug for SessionMcpServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionMcpServer([redacted])")
    }
}

/// Pure validation: does not resolve programs, read credentials, or start a process.
pub fn validate_session_mcp_servers(servers: &[SessionMcpServer]) -> Result<(), String> {
    if servers.len() > MAX_SESSION_MCP_SERVERS {
        return Err("too many session MCP servers".into());
    }
    let mut names = BTreeSet::new();
    for server in servers {
        if server.name.is_empty()
            || server.name.len() > 64
            || server.name.contains("__")
            || !server
                .name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
            || server.name == "tickloop-artifacts"
            || !names.insert(&server.name)
        {
            return Err("invalid, reserved or duplicate session MCP server name".into());
        }
        if !server.program.is_absolute()
            || !server.cwd.is_absolute()
            || server.program.as_os_str().len() > 4096
            || server.cwd.as_os_str().len() > 4096
            || server.program.as_os_str().as_encoded_bytes().contains(&0)
            || server.cwd.as_os_str().as_encoded_bytes().contains(&0)
        {
            return Err("session MCP program and cwd must be absolute paths".into());
        }
        if server.args.len() > 128
            || server
                .args
                .iter()
                .any(|v| v.len() > 16 * 1024 || v.contains('\0'))
            || server.env.len() > 64
            || server.env.iter().any(|(key, value)| {
                !valid_session_mcp_env_key(key) || value.len() > 16 * 1024 || value.contains('\0')
            })
        {
            return Err("invalid or oversized session MCP arguments or environment".into());
        }
    }
    if serde_json::to_vec(servers)
        .map_err(|_| "invalid session MCP descriptors")?
        .len()
        > MAX_SESSION_MCP_DESCRIPTOR_BYTES
    {
        return Err("session MCP descriptors exceed the size limit".into());
    }
    Ok(())
}

pub fn valid_session_mcp_env_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    !key.is_empty()
        && key.len() <= 128
        && key
            .bytes()
            .enumerate()
            .all(|(i, c)| c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
        && !["LD_", "DYLD_", "TICKLOOP_", "TICKHOST_"]
            .iter()
            .any(|prefix| upper.starts_with(prefix))
        && !matches!(
            upper.as_str(),
            "BASH_ENV"
                | "ENV"
                | "SHELLOPTS"
                | "BASHOPTS"
                | "NODE_OPTIONS"
                | "NODE_PATH"
                | "PYTHONPATH"
                | "PYTHONHOME"
                | "RUBYOPT"
                | "PERL5OPT"
                | "JAVA_TOOL_OPTIONS"
                | "JDK_JAVA_OPTIONS"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn descriptor() -> SessionMcpServer {
        SessionMcpServer {
            name: "demo".into(),
            program: "/bin/demo".into(),
            args: vec!["private-arg".into()],
            env: BTreeMap::from([("KEY".into(), "private-value".into())]),
            cwd: "/tmp".into(),
        }
    }
    #[test]
    fn descriptors_debug_is_fully_redacted() {
        assert_eq!(
            format!("{:?}", descriptor()),
            "SessionMcpServer([redacted])"
        );
    }
    #[test]
    fn descriptors_are_bounded_and_environment_is_not_a_control_channel() {
        let server = descriptor();
        assert!(validate_session_mcp_servers(std::slice::from_ref(&server)).is_ok());
        assert!(validate_session_mcp_servers(&[server.clone(), server]).is_err());
        for key in [
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "TICKLOOP_CONTROL_FD",
            "NODE_OPTIONS",
            "BASH_ENV",
        ] {
            assert!(!valid_session_mcp_env_key(key));
        }
        assert!(valid_session_mcp_env_key("EXPLICIT_API_KEY"));
        let mut ambiguous = descriptor();
        ambiguous.name = "a__b".into();
        assert!(validate_session_mcp_servers(&[ambiguous]).is_err());
    }
}
