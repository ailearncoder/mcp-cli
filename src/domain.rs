//! Transport-independent domain values shared by commands, connections, and output.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use url::Url;

/// Metadata for a tool advertised by an MCP server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

/// The only valid top-level argument shape for an MCP tool call.
pub type JsonObject = Map<String, Value>;

/// A complete MCP tool result.
///
/// This deliberately remains an unmodified JSON value so adapter-specific and
/// future protocol extension fields are preserved.
pub type ToolResult = Value;

/// A presentation-safe summary of a server transport.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum TransportSummary {
    Stdio { command: String },
    Http { url: Url },
}

/// Information collected from one server for list and info presentation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerSnapshot {
    pub server: String,
    pub transport_summary: TransportSummary,
    pub instructions: Option<String>,
    pub tools: Vec<ToolInfo>,
}

/// The isolated result of operating on one configured server.
#[derive(Debug)]
pub enum PerServer<T> {
    Success {
        server: String,
        value: T,
    },
    Failure {
        server: String,
        error: crate::error::CliError,
    },
}

impl<T> PerServer<T> {
    pub fn server(&self) -> &str {
        match self {
            Self::Success { server, .. } | Self::Failure { server, .. } => server,
        }
    }
}

/// One authorized tool matched by a cross-server search.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    pub server: String,
    pub tool: ToolInfo,
}

/// Business output returned by a command handler.
#[derive(Clone, Debug, PartialEq)]
pub enum CommandOutcome {
    HumanText(String),
    Json(Value),
    Empty,
}

/// The connection strategy selected by the connection manager.
///
/// `Daemon` is a logical mode only; it does not expose a Unix socket type and
/// is therefore safe to use in cross-platform command code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConnectionMode {
    Direct,
    Daemon,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_aliases_preserve_objects_and_extension_fields() {
        let mut args = JsonObject::new();
        args.insert("count".into(), json!(3));
        args.insert("nested".into(), json!({"enabled": true}));

        let result: ToolResult = json!({
            "content": [{"type": "text", "text": "ok"}],
            "vendorExtension": {"args": args}
        });

        assert_eq!(result["vendorExtension"]["args"]["count"], json!(3));
        assert_eq!(
            result["vendorExtension"]["args"]["nested"],
            json!({"enabled": true})
        );
    }

    #[test]
    fn transport_summary_has_a_tagged_serde_shape() {
        let summary = TransportSummary::Http {
            url: Url::parse("https://example.test/mcp").expect("valid URL"),
        };

        assert_eq!(
            serde_json::to_value(summary).expect("serializable summary"),
            json!({"kind": "Http", "url": "https://example.test/mcp"})
        );
    }

    #[test]
    fn per_server_exposes_the_server_for_both_variants() {
        let success = PerServer::Success {
            server: "alpha".into(),
            value: 1_u8,
        };
        let failure = PerServer::<u8>::Failure {
            server: "beta".into(),
            error: crate::error::CliError::new(
                crate::error::ErrorKind::NetworkError,
                "unavailable",
                crate::error::ExitCode::Network,
            ),
        };

        assert_eq!(success.server(), "alpha");
        assert_eq!(failure.server(), "beta");
    }
}
