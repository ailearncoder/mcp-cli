//! Configuration discovery, substitution, validation, and canonicalization.

use std::{collections::BTreeMap, fmt, path::PathBuf};

use serde::Serialize;
use url::Url;

pub mod canonical;
pub mod discover;
pub mod substitute;
pub mod validate;

pub use canonical::{SHA256_HEX_LENGTH, canonical_json, config_hash, server_id};
pub use discover::{
    ConfigurationLoader, EnvSource, FileConfigurationLoader, LoadRequest, LoadedConfig,
    MAX_CONFIG_BYTES, ProcessEnv,
};
pub use substitute::{SubstitutionOutcome, substitute_environment};
pub use validate::validate_mcp_servers;

/// Full SHA-256 digest of a canonical, validated server configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ConfigHash(pub [u8; 32]);

/// Fixed-length lowercase hexadecimal SHA-256 identifier derived from a
/// server name, safe for use as a filename component.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ServerId(pub String);

/// A validated server definition consumed by command and connection layers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ServerDefinition {
    pub name: String,
    pub id: ServerId,
    pub config_hash: ConfigHash,
    pub transport: TransportConfig,
    pub filter: ToolFilterConfig,
}

/// A transport configuration whose variants make command and URL transports
/// mutually exclusive.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind")]
pub enum TransportConfig {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
    },
    Http {
        url: Url,
        headers: BTreeMap<String, String>,
    },
}

impl fmt::Debug for TransportConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio {
                command,
                args,
                env,
                cwd,
            } => formatter
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", args)
                .field("env_keys", &env.keys().collect::<Vec<_>>())
                .field("cwd", cwd)
                .finish(),
            Self::Http { url, headers } => {
                let header_names = headers
                    .keys()
                    .map(|name| name.to_ascii_lowercase())
                    .collect::<Vec<_>>();
                formatter
                    .debug_struct("Http")
                    .field("scheme", &url.scheme())
                    .field("has_host", &url.host().is_some())
                    .field("header_names", &header_names)
                    .finish()
            }
        }
    }
}

/// Visibility and invocation policy configured for one server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ToolFilterConfig {
    pub allowed_tools: Vec<String>,
    pub disabled_tools: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stdio_transport_uses_the_design_tagged_shape() {
        let transport = TransportConfig::Stdio {
            command: "example-server".into(),
            args: vec!["--stdio".into()],
            env: BTreeMap::from([("MODE".into(), "test".into())]),
            cwd: Some(PathBuf::from("/workspace")),
        };

        assert_eq!(
            serde_json::to_value(transport).expect("serializable transport"),
            json!({
                "kind": "Stdio",
                "command": "example-server",
                "args": ["--stdio"],
                "env": {"MODE": "test"},
                "cwd": "/workspace"
            })
        );
    }

    #[test]
    fn http_transport_cannot_contain_stdio_fields() {
        let transport = TransportConfig::Http {
            url: Url::parse("https://example.test/mcp").expect("valid URL"),
            headers: BTreeMap::from([("X-Test".into(), "value".into())]),
        };
        let value = serde_json::to_value(transport).expect("serializable transport");

        assert_eq!(value["kind"], "Http");
        assert!(value.get("command").is_none());
        assert!(value.get("args").is_none());
    }
}
