//! Strongly typed server configuration validation.

use std::{collections::BTreeMap, path::PathBuf};

use serde_json::{Map, Value};
use url::Url;

use crate::error::CliError;

use super::{
    ServerDefinition, ToolFilterConfig, TransportConfig,
    canonical::{config_hash, server_id},
};

/// Raw fields retained as JSON values so validation can distinguish a missing
/// field from a present `null` or a value of the wrong type.
#[derive(Clone, Copy, Debug)]
struct RawServerConfig<'a> {
    command: Option<&'a Value>,
    url: Option<&'a Value>,
    args: Option<&'a Value>,
    env: Option<&'a Value>,
    cwd: Option<&'a Value>,
    headers: Option<&'a Value>,
    allowed_tools: Option<&'a Value>,
    disabled_tools: Option<&'a Value>,
}

impl<'a> RawServerConfig<'a> {
    fn from_object(object: &'a Map<String, Value>) -> Self {
        Self {
            command: object.get("command"),
            url: object.get("url"),
            args: object.get("args"),
            env: object.get("env"),
            cwd: object.get("cwd"),
            headers: object.get("headers"),
            allowed_tools: object.get("allowedTools"),
            disabled_tools: object.get("disabledTools"),
        }
    }
}

/// Validates every entry in an `mcpServers` object and returns definitions in
/// deterministic server-name order.
///
/// Unknown fields are preserved by the source document but ignored here for
/// compatibility with clients that add extensions. Every known field is
/// validated when present, including fields not used by the selected
/// transport, so malformed configuration cannot be silently accepted.
pub fn validate_mcp_servers(
    mcp_servers: &Map<String, Value>,
) -> Result<BTreeMap<String, ServerDefinition>, CliError> {
    mcp_servers
        .iter()
        .map(|(name, value)| validate_server(name, value).map(|server| (name.clone(), server)))
        .collect()
}

fn validate_server(name: &str, value: &Value) -> Result<ServerDefinition, CliError> {
    let root = server_path(name);
    let object = value.as_object().ok_or_else(|| {
        invalid_field(
            name,
            &root,
            "server configuration must be a non-null object",
        )
    })?;
    let raw = RawServerConfig::from_object(object);

    match (raw.command.is_some(), raw.url.is_some()) {
        (true, true) => {
            return Err(invalid_field(
                name,
                &root,
                "command and url are mutually exclusive",
            ));
        }
        (false, false) => {
            return Err(invalid_field(
                name,
                &root,
                "exactly one of command or url is required",
            ));
        }
        _ => {}
    }

    let args = string_array(name, raw.args, &field_path(&root, "args"))?;
    let env = string_map(name, raw.env, &field_path(&root, "env"))?;
    let cwd = optional_string(name, raw.cwd, &field_path(&root, "cwd"))?.map(PathBuf::from);
    let headers = string_map(name, raw.headers, &field_path(&root, "headers"))?;
    let allowed_tools = string_array(name, raw.allowed_tools, &field_path(&root, "allowedTools"))?;
    let disabled_tools = string_array(
        name,
        raw.disabled_tools,
        &field_path(&root, "disabledTools"),
    )?;

    let transport = if let Some(command) = raw.command {
        let path = field_path(&root, "command");
        let command = required_string(name, command, &path)?;
        if command.is_empty() {
            return Err(invalid_field(name, &path, "must be a non-empty string"));
        }
        TransportConfig::Stdio {
            command,
            args,
            env,
            cwd,
        }
    } else {
        let path = field_path(&root, "url");
        let url = required_string(name, raw.url.expect("url presence checked"), &path)?;
        let url = validate_http_url(name, &path, &url)?;
        TransportConfig::Http { url, headers }
    };

    Ok(ServerDefinition {
        name: name.to_owned(),
        id: server_id(name),
        config_hash: config_hash(value),
        transport,
        filter: ToolFilterConfig {
            allowed_tools,
            disabled_tools,
        },
    })
}

fn required_string(name: &str, value: &Value, path: &str) -> Result<String, CliError> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid_field(name, path, "must be a string"))
}

fn optional_string(
    name: &str,
    value: Option<&Value>,
    path: &str,
) -> Result<Option<String>, CliError> {
    value
        .map(|value| required_string(name, value, path))
        .transpose()
}

fn string_array(name: &str, value: Option<&Value>, path: &str) -> Result<Vec<String>, CliError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| invalid_field(name, path, "must be an array of strings"))?;

    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid_field(name, &format!("{path}[{index}]"), "must be a string"))
        })
        .collect()
}

fn string_map(
    name: &str,
    value: Option<&Value>,
    path: &str,
) -> Result<BTreeMap<String, String>, CliError> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let values = value
        .as_object()
        .ok_or_else(|| invalid_field(name, path, "must be an object with string values"))?;

    values
        .iter()
        .map(|(key, value)| {
            value
                .as_str()
                .map(|value| (key.clone(), value.to_owned()))
                .ok_or_else(|| invalid_field(name, &map_value_path(path, key), "must be a string"))
        })
        .collect()
}

fn validate_http_url(name: &str, path: &str, value: &str) -> Result<Url, CliError> {
    let url = Url::parse(value)
        .map_err(|_| invalid_field(name, path, "must be a valid HTTP or HTTPS URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(invalid_field(
            name,
            path,
            "must be a valid HTTP or HTTPS URL",
        ));
    }
    Ok(url)
}

fn invalid_field(server: &str, path: &str, reason: &str) -> CliError {
    CliError::invalid_server_config(server, path, reason)
}

fn server_path(server: &str) -> String {
    format!(
        "mcpServers[{}]",
        serde_json::to_string(server).expect("serializing a string cannot fail")
    )
}

fn field_path(root: &str, field: &str) -> String {
    format!("{root}.{field}")
}

fn map_value_path(root: &str, key: &str) -> String {
    format!(
        "{root}[{}]",
        serde_json::to_string(key).expect("serializing a string cannot fail")
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::error::ErrorKind;

    fn validate(value: Value) -> Result<BTreeMap<String, ServerDefinition>, CliError> {
        validate_mcp_servers(value.as_object().expect("test mcpServers object"))
    }

    fn assert_invalid(value: Value, server: &str, path: &str) {
        let error = validate(value).expect_err("configuration must be rejected");
        assert_eq!(error.kind, ErrorKind::InvalidServerConfig);
        assert!(error.message.contains(server), "{}", error.message);
        let details = error.details.expect("field details");
        assert!(details.contains(server), "{details}");
        assert!(details.contains(path), "{details}");
    }

    #[test]
    fn valid_definitions_are_typed_defaulted_and_sorted() {
        let servers = validate(json!({
            "z-http": {
                "url": "https://example.test/mcp",
                "headers": {"Authorization": "Bearer token"},
                "allowedTools": ["read_*"],
                "disabledTools": []
            },
            "a-stdio": {
                "command": "runner",
                "args": ["--stdio", ""],
                "env": {"MODE": "test"},
                "cwd": "/workspace"
            }
        }))
        .expect("valid server definitions");

        assert_eq!(
            servers.keys().collect::<Vec<_>>(),
            vec!["a-stdio", "z-http"]
        );
        assert_eq!(servers["a-stdio"].name, "a-stdio");
        assert_eq!(servers["a-stdio"].id, server_id("a-stdio"));
        assert_eq!(
            servers["a-stdio"].config_hash,
            config_hash(&json!({
                "command": "runner",
                "args": ["--stdio", ""],
                "env": {"MODE": "test"},
                "cwd": "/workspace"
            }))
        );
        assert_eq!(
            servers["a-stdio"].transport,
            TransportConfig::Stdio {
                command: "runner".into(),
                args: vec!["--stdio".into(), "".into()],
                env: BTreeMap::from([("MODE".into(), "test".into())]),
                cwd: Some(PathBuf::from("/workspace")),
            }
        );
        assert_eq!(
            servers["z-http"].transport,
            TransportConfig::Http {
                url: Url::parse("https://example.test/mcp").expect("URL"),
                headers: BTreeMap::from([("Authorization".into(), "Bearer token".into())]),
            }
        );
        assert_eq!(
            servers["z-http"].filter.allowed_tools,
            vec!["read_*".to_owned()]
        );
    }

    #[test]
    fn server_value_must_be_a_non_null_object() {
        for value in [Value::Null, json!([]), json!("command"), json!(7)] {
            assert_invalid(json!({"bad/server": value}), "bad/server", "mcpServers");
        }
    }

    #[test]
    fn command_and_url_must_be_present_exclusively() {
        assert_invalid(
            json!({"both": {"command": "runner", "url": "https://example.test"}}),
            "both",
            "mcpServers[\"both\"]",
        );
        assert_invalid(
            json!({"neither": {"args": []}}),
            "neither",
            "mcpServers[\"neither\"]",
        );
        assert_invalid(
            json!({"null-command": {"command": null}}),
            "null-command",
            ".command",
        );
    }

    #[test]
    fn command_must_be_a_non_empty_string() {
        for command in [json!(""), Value::Null, json!(false), json!([])] {
            assert_invalid(json!({"stdio": {"command": command}}), "stdio", ".command");
        }
    }

    #[test]
    fn url_must_be_an_absolute_http_or_https_url_with_a_host() {
        for url in [
            json!("relative/path"),
            json!("ftp://example.test/mcp"),
            json!("http:///"),
            Value::Null,
            json!(42),
        ] {
            assert_invalid(json!({"remote": {"url": url}}), "remote", ".url");
        }
    }

    #[test]
    fn array_fields_require_strings_and_report_element_paths() {
        for field in ["args", "allowedTools", "disabledTools"] {
            let mut wrong_element = json!({"array-server": {"command": "runner"}});
            wrong_element["array-server"]
                .as_object_mut()
                .expect("server object")
                .insert(field.to_owned(), json!(["ok", 3]));
            assert_invalid(wrong_element, "array-server", &format!(".{field}[1]"));

            let mut wrong_container = json!({"array-server": {"command": "runner"}});
            wrong_container["array-server"]
                .as_object_mut()
                .expect("server object")
                .insert(field.to_owned(), json!({}));
            assert_invalid(wrong_container, "array-server", &format!(".{field}"));
        }
    }

    #[test]
    fn map_fields_require_string_values_and_report_key_paths() {
        for field in ["env", "headers"] {
            let mut wrong_value = json!({"map-server": {"command": "runner"}});
            wrong_value["map-server"]
                .as_object_mut()
                .expect("server object")
                .insert(field.to_owned(), json!({"TOKEN": 3}));
            assert_invalid(wrong_value, "map-server", &format!(".{field}[\"TOKEN\"]"));

            let mut wrong_container = json!({"map-server": {"command": "runner"}});
            wrong_container["map-server"]
                .as_object_mut()
                .expect("server object")
                .insert(field.to_owned(), json!([]));
            assert_invalid(wrong_container, "map-server", &format!(".{field}"));
        }
    }

    #[test]
    fn cwd_must_be_a_string_when_present() {
        assert_invalid(
            json!({"stdio": {"command": "runner", "cwd": null}}),
            "stdio",
            ".cwd",
        );
    }
}
