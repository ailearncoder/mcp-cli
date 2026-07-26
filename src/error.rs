//! Stable application error model.

use std::{error::Error, fmt, path::Path, sync::Arc};

use serde::{Deserialize, Serialize};

use crate::policy::retry::{ClassifyError, ErrorClass, classify_http_status};

/// Stable machine-readable categories used at the application boundary.
///
/// Variant names are deliberately decoupled from their wire representation;
/// use [`ErrorKind::as_str`] when rendering or serializing machine output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ErrorKind {
    #[serde(rename = "UNKNOWN_COMMAND")]
    UnknownCommand,
    #[serde(rename = "INVALID_ARGUMENTS")]
    InvalidArguments,
    #[serde(rename = "CONFIG_NOT_FOUND")]
    ConfigNotFound,
    #[serde(rename = "CONFIG_READ_ERROR")]
    ConfigReadError,
    #[serde(rename = "INVALID_CONFIG")]
    InvalidConfig,
    #[serde(rename = "MISSING_ENV_VAR")]
    MissingEnvVar,
    #[serde(rename = "INVALID_RUNTIME_CONFIG")]
    InvalidRuntimeConfig,
    #[serde(rename = "INVALID_SERVER_CONFIG")]
    InvalidServerConfig,
    #[serde(rename = "SERVER_NOT_FOUND")]
    ServerNotFound,
    #[serde(rename = "TOOL_NOT_FOUND")]
    ToolNotFound,
    #[serde(rename = "TOOL_DISABLED")]
    ToolDisabled,
    #[serde(rename = "INVALID_JSON")]
    InvalidJson,
    #[serde(rename = "INPUT_TOO_LARGE")]
    InputTooLarge,
    #[serde(rename = "SECURITY_ERROR")]
    SecurityError,
    #[serde(rename = "TOOL_EXECUTION_FAILED")]
    ToolExecutionFailed,
    #[serde(rename = "NETWORK_ERROR")]
    NetworkError,
    #[serde(rename = "TIMEOUT")]
    Timeout,
    #[serde(rename = "AUTH_ERROR")]
    AuthError,
}

impl ErrorKind {
    /// Every public kind, in stable declaration order.
    pub const ALL: [Self; 18] = [
        Self::UnknownCommand,
        Self::InvalidArguments,
        Self::ConfigNotFound,
        Self::ConfigReadError,
        Self::InvalidConfig,
        Self::MissingEnvVar,
        Self::InvalidRuntimeConfig,
        Self::InvalidServerConfig,
        Self::ServerNotFound,
        Self::ToolNotFound,
        Self::ToolDisabled,
        Self::InvalidJson,
        Self::InputTooLarge,
        Self::SecurityError,
        Self::ToolExecutionFailed,
        Self::NetworkError,
        Self::Timeout,
        Self::AuthError,
    ];

    /// Stable machine string used in structured errors and serialized values.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownCommand => "UNKNOWN_COMMAND",
            Self::InvalidArguments => "INVALID_ARGUMENTS",
            Self::ConfigNotFound => "CONFIG_NOT_FOUND",
            Self::ConfigReadError => "CONFIG_READ_ERROR",
            Self::InvalidConfig => "INVALID_CONFIG",
            Self::MissingEnvVar => "MISSING_ENV_VAR",
            Self::InvalidRuntimeConfig => "INVALID_RUNTIME_CONFIG",
            Self::InvalidServerConfig => "INVALID_SERVER_CONFIG",
            Self::ServerNotFound => "SERVER_NOT_FOUND",
            Self::ToolNotFound => "TOOL_NOT_FOUND",
            Self::ToolDisabled => "TOOL_DISABLED",
            Self::InvalidJson => "INVALID_JSON",
            Self::InputTooLarge => "INPUT_TOO_LARGE",
            Self::SecurityError => "SECURITY_ERROR",
            Self::ToolExecutionFailed => "TOOL_EXECUTION_FAILED",
            Self::NetworkError => "NETWORK_ERROR",
            Self::Timeout => "TIMEOUT",
            Self::AuthError => "AUTH_ERROR",
        }
    }

    /// Total mapping from every public error kind to its process exit category.
    pub const fn exit_code(self) -> ExitCode {
        match self {
            Self::ToolExecutionFailed => ExitCode::Tool,
            Self::NetworkError | Self::Timeout => ExitCode::Network,
            Self::AuthError => ExitCode::Auth,
            Self::UnknownCommand
            | Self::InvalidArguments
            | Self::ConfigNotFound
            | Self::ConfigReadError
            | Self::InvalidConfig
            | Self::MissingEnvVar
            | Self::InvalidRuntimeConfig
            | Self::InvalidServerConfig
            | Self::ServerNotFound
            | Self::ToolNotFound
            | Self::ToolDisabled
            | Self::InvalidJson
            | Self::InputTooLarge
            | Self::SecurityError => ExitCode::Client,
        }
    }

    /// Default retry classification for errors without more specific adapter
    /// metadata. HTTP constructors override this using the structured status.
    pub const fn error_class(self) -> ErrorClass {
        match self {
            Self::ToolExecutionFailed => ErrorClass::Business,
            Self::NetworkError => ErrorClass::Transient,
            Self::AuthError => ErrorClass::Auth,
            Self::UnknownCommand
            | Self::InvalidArguments
            | Self::ConfigNotFound
            | Self::ConfigReadError
            | Self::InvalidConfig
            | Self::MissingEnvVar
            | Self::InvalidRuntimeConfig
            | Self::InvalidServerConfig
            | Self::ServerNotFound
            | Self::ToolNotFound
            | Self::ToolDisabled
            | Self::InvalidJson
            | Self::InputTooLarge
            | Self::SecurityError
            | Self::Timeout => ErrorClass::NonTransient,
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Public process exit categories.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum ExitCode {
    Success = 0,
    Client = 1,
    Tool = 2,
    Network = 3,
    Auth = 4,
}

impl ExitCode {
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// A user-facing error value with an optional retained internal cause.
///
/// The source is intentionally absent from `Debug`, `Display`, equality, and
/// all public text fields. Adapters can retain it for error chaining with
/// [`CliError::with_source`] without accidentally rendering credentials or
/// implementation details to users.
#[derive(Clone)]
pub struct CliError {
    pub kind: ErrorKind,
    pub message: String,
    pub details: Option<String>,
    pub suggestion: Option<String>,
    pub exit_code: ExitCode,
    class: ErrorClass,
    details_redacted: bool,
    source: Option<Arc<dyn Error + Send + Sync>>,
}

impl CliError {
    /// Compatibility constructor. The supplied exit code is accepted so older
    /// call sites continue to compile, but the canonical kind mapping always
    /// wins and cannot be changed by an inconsistent caller argument.
    pub fn new(kind: ErrorKind, message: impl Into<String>, _exit_code: ExitCode) -> Self {
        Self::from_kind(kind, message)
    }

    pub fn from_kind(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            details: None,
            suggestion: None,
            exit_code: kind.exit_code(),
            class: kind.error_class(),
            details_redacted: false,
            source: None,
        }
    }

    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self.details_redacted = false;
        self
    }

    /// Marks details that were redacted before a lossy transformation. The
    /// process boundary must not redact this field again because replacement
    /// markers are not guaranteed to be stable under a second pass for every
    /// possible configured secret.
    pub(crate) fn mark_details_redacted(mut self) -> Self {
        self.details_redacted = true;
        self
    }

    pub(crate) const fn details_are_redacted(&self) -> bool {
        self.details_redacted
    }

    pub(crate) fn set_details_redacted(&mut self) {
        self.details_redacted = true;
    }

    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = Some(suggestion.into());
        self
    }

    /// Retains an internal cause for diagnostics and error chaining. The cause
    /// is never copied into message, details, suggestion, Display, or Debug.
    pub fn with_source(mut self, source: impl Error + Send + Sync + 'static) -> Self {
        self.source = Some(Arc::new(source));
        self
    }

    pub const fn machine_kind(&self) -> &'static str {
        self.kind.as_str()
    }

    pub const fn canonical_exit_code(&self) -> ExitCode {
        self.kind.exit_code()
    }

    pub const fn error_class(&self) -> ErrorClass {
        self.class
    }

    pub fn unknown_command(command: &str) -> Self {
        let command = safe_label(command);
        Self::from_kind(
            ErrorKind::UnknownCommand,
            format!("Unknown command: \"{command}\""),
        )
        .with_details("Valid commands: info, grep, call")
        .with_suggestion("Run 'mcp-cli --help' to see valid commands and usage")
    }

    pub fn invalid_arguments(message: impl Into<String>, details: impl Into<String>) -> Self {
        Self::from_kind(ErrorKind::InvalidArguments, message).with_details(details)
    }

    pub fn config_not_found(path: &Path) -> Self {
        Self::from_kind(
            ErrorKind::ConfigNotFound,
            format!("Config file not found: {}", path.display()),
        )
        .with_suggestion(
            "Create mcp_servers.json or use -c/--config to specify the configuration file",
        )
    }

    pub fn config_not_found_in<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Self {
        let searched = paths
            .into_iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        Self::from_kind(
            ErrorKind::ConfigNotFound,
            "No mcp_servers.json found in search paths",
        )
        .with_details(format!("Searched: {}", searched.join(", ")))
        .with_suggestion(
            "Create mcp_servers.json in a search path or use -c/--config to specify one",
        )
    }

    pub fn config_read_error(path: &Path, source: impl Error + Send + Sync + 'static) -> Self {
        Self::from_kind(
            ErrorKind::ConfigReadError,
            format!("Could not read config file: {}", path.display()),
        )
        .with_details(format!("File: {}", path.display()))
        .with_suggestion("Check that the configuration path exists and is readable")
        .with_source(source)
    }

    pub fn invalid_config(path: &Path, safe_details: impl Into<String>) -> Self {
        Self::from_kind(
            ErrorKind::InvalidConfig,
            format!("Invalid configuration: {}", path.display()),
        )
        .with_details(safe_details)
        .with_suggestion("Fix the JSON syntax or configuration structure and try again")
    }

    pub fn missing_env_vars<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let names = safe_candidates(names);
        let displayed = if names.is_empty() {
            "(none)".to_owned()
        } else {
            names.join(", ")
        };
        Self::from_kind(
            ErrorKind::MissingEnvVar,
            "Configuration references missing environment variables",
        )
        .with_details(format!("Missing variables: {displayed}"))
        .with_suggestion("Define the listed environment variables and retry")
    }

    pub fn invalid_runtime_config(name: &str, expected: &str) -> Self {
        Self::from_kind(
            ErrorKind::InvalidRuntimeConfig,
            "Invalid runtime configuration",
        )
        .with_details(format!(
            "{} must be {}",
            safe_label(name),
            safe_label(expected)
        ))
        .with_suggestion("Set the environment variable to a supported value and retry")
    }

    pub fn invalid_server_config(server: &str, field: &str, reason: &str) -> Self {
        Self::from_kind(
            ErrorKind::InvalidServerConfig,
            format!(
                "Invalid configuration for server \"{}\"",
                safe_label(server)
            ),
        )
        .with_details(format!(
            "Field {}: {}",
            safe_label(field),
            safe_label(reason)
        ))
        .with_suggestion("Fix the named server field in mcp_servers.json")
    }

    pub fn server_not_found<I, S>(server: &str, available: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let server = safe_label(server);
        let candidates = safe_candidates(available);
        let suggestion = candidates.first().map_or_else(
            || format!("Add server \"{server}\" to mcp_servers.json"),
            |candidate| format!("Use 'mcp-cli info {candidate}' or choose another listed server"),
        );
        Self::from_kind(
            ErrorKind::ServerNotFound,
            format!("Server \"{server}\" not found in config"),
        )
        .with_details(format!(
            "Available servers: {}",
            display_candidates(&candidates)
        ))
        .with_suggestion(suggestion)
    }

    pub fn tool_not_found<I, S>(server: &str, tool: &str, available: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let server = safe_label(server);
        let tool = safe_label(tool);
        let candidates = safe_candidates(available);
        Self::from_kind(
            ErrorKind::ToolNotFound,
            format!("Tool \"{tool}\" not found in server \"{server}\""),
        )
        .with_details(format!(
            "Available tools: {}",
            display_candidates(&candidates)
        ))
        .with_suggestion(format!(
            "Run 'mcp-cli info {server}' to see available tools and schemas"
        ))
    }

    pub fn tool_disabled(server: &str, tool: &str) -> Self {
        Self::from_kind(
            ErrorKind::ToolDisabled,
            format!(
                "Tool \"{}\" is disabled for server \"{}\"",
                safe_label(tool),
                safe_label(server)
            ),
        )
        .with_suggestion("Check allowedTools and disabledTools in mcp_servers.json")
    }

    pub fn invalid_json(safe_details: impl Into<String>) -> Self {
        Self::from_kind(ErrorKind::InvalidJson, "Invalid JSON in tool arguments")
            .with_details(safe_details)
            .with_suggestion("Provide a valid JSON object; use info to inspect the tool schema")
    }

    pub fn input_too_large(actual_bytes: usize, maximum_bytes: usize) -> Self {
        Self::from_kind(
            ErrorKind::InputTooLarge,
            "Tool input exceeds the maximum size",
        )
        .with_details(format!(
            "Input is {actual_bytes} bytes; maximum is {maximum_bytes} bytes"
        ))
        .with_suggestion("Reduce the JSON input size and retry")
    }

    pub fn security_error(message: impl Into<String>, safe_details: impl Into<String>) -> Self {
        Self::from_kind(ErrorKind::SecurityError, message)
            .with_details(safe_details)
            .with_suggestion("Correct the unsafe path, ownership, or process state before retrying")
    }

    pub fn tool_execution_failed(server: &str, tool: &str, safe_details: &str) -> Self {
        let server = safe_label(server);
        let tool = safe_label(tool);
        Self::from_kind(
            ErrorKind::ToolExecutionFailed,
            format!("Tool \"{tool}\" execution failed on server \"{server}\""),
        )
        .with_details(safe_details)
        .with_suggestion(format!(
            "Run 'mcp-cli info {server} {tool}' and verify the arguments match the input schema"
        ))
    }

    pub fn network_error(server: &str, safe_details: impl Into<String>) -> Self {
        Self::from_kind(
            ErrorKind::NetworkError,
            format!(
                "Failed to communicate with server \"{}\"",
                safe_label(server)
            ),
        )
        .with_details(safe_details)
        .with_suggestion("Check network connectivity, the server address, and server availability")
    }

    pub fn network_error_classified(
        server: &str,
        safe_details: impl Into<String>,
        class: ErrorClass,
    ) -> Self {
        let mut error = Self::network_error(server, safe_details);
        error.class = class;
        error
    }

    pub fn timeout(operation: &str) -> Self {
        Self::from_kind(ErrorKind::Timeout, "Operation timed out")
            .with_details(format!("Timed out while {}", safe_label(operation)))
            .with_suggestion("Check network connectivity or increase MCP_TIMEOUT")
    }

    pub fn cancelled(server: &str, operation: &str) -> Self {
        let mut error =
            Self::network_error(server, format!("Cancelled while {}", safe_label(operation)));
        error.class = ErrorClass::Cancelled;
        error
    }

    pub fn auth_error(server: &str, status: u16) -> Self {
        Self::from_kind(
            ErrorKind::AuthError,
            format!(
                "Authentication or authorization failed for server \"{}\"",
                safe_label(server)
            ),
        )
        .with_details(format!("HTTP status: {status}"))
        .with_suggestion(
            "Check the Authorization header, credentials, and access permissions in config",
        )
    }

    /// Maps a structured HTTP status without parsing user-visible text. Server
    /// context and status are retained; credential/header values are never
    /// accepted by this constructor.
    pub fn http_status(server: &str, status: u16) -> Self {
        let class = classify_http_status(status);
        if class == ErrorClass::Auth {
            return Self::auth_error(server, status);
        }

        let mut error = Self::network_error(server, format!("HTTP status: {status}"));
        error.class = class;
        error
    }
}

impl fmt::Debug for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CliError")
            .field("kind", &self.kind)
            .field("message", &self.message)
            .field("details", &self.details)
            .field("suggestion", &self.suggestion)
            .field("exit_code", &self.exit_code)
            .field("class", &self.class)
            .field("has_source", &self.source.is_some())
            .finish()
    }
}

impl PartialEq for CliError {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.message == other.message
            && self.details == other.details
            && self.suggestion == other.suggestion
            && self.exit_code == other.exit_code
            && self.class == other.class
    }
}

impl Eq for CliError {}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for CliError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

impl ClassifyError for CliError {
    fn class(&self) -> ErrorClass {
        self.error_class()
    }
}

fn safe_label(value: &str) -> String {
    const MAX_CHARS: usize = 160;
    let mut result = String::new();
    let mut truncated = false;
    for character in value.chars() {
        if result.chars().count() == MAX_CHARS {
            truncated = true;
            break;
        }
        if character.is_control() {
            result.push(' ');
        } else {
            result.push(character);
        }
    }
    if truncated {
        result.push_str("...");
    }
    result
}

fn safe_candidates<I, S>(values: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut values = values
        .into_iter()
        .map(|value| safe_label(value.as_ref()))
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

fn display_candidates(candidates: &[String]) -> String {
    const DISPLAY_LIMIT: usize = 5;
    if candidates.is_empty() {
        return "(none)".to_owned();
    }
    let shown = candidates
        .iter()
        .take(DISPLAY_LIMIT)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let remaining = candidates.len().saturating_sub(DISPLAY_LIMIT);
    if remaining == 0 {
        shown
    } else {
        format!("{shown} (+{remaining} more)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct SecretSource(&'static str);

    impl fmt::Display for SecretSource {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for SecretSource {}

    #[test]
    fn every_kind_has_a_stable_machine_string_and_round_trips_through_serde() {
        let expected = [
            "UNKNOWN_COMMAND",
            "INVALID_ARGUMENTS",
            "CONFIG_NOT_FOUND",
            "CONFIG_READ_ERROR",
            "INVALID_CONFIG",
            "MISSING_ENV_VAR",
            "INVALID_RUNTIME_CONFIG",
            "INVALID_SERVER_CONFIG",
            "SERVER_NOT_FOUND",
            "TOOL_NOT_FOUND",
            "TOOL_DISABLED",
            "INVALID_JSON",
            "INPUT_TOO_LARGE",
            "SECURITY_ERROR",
            "TOOL_EXECUTION_FAILED",
            "NETWORK_ERROR",
            "TIMEOUT",
            "AUTH_ERROR",
        ];

        for (kind, expected) in ErrorKind::ALL.into_iter().zip(expected) {
            assert_eq!(kind.as_str(), expected);
            assert_eq!(kind.to_string(), expected);
            let json = serde_json::to_string(&kind).expect("serialize error kind");
            assert_eq!(json, format!("\"{expected}\""));
            assert_eq!(
                serde_json::from_str::<ErrorKind>(&json).expect("deserialize error kind"),
                kind
            );
        }
    }

    #[test]
    fn exit_code_mapping_is_total_and_stable() {
        for kind in ErrorKind::ALL {
            let expected = match kind {
                ErrorKind::ToolExecutionFailed => ExitCode::Tool,
                ErrorKind::NetworkError | ErrorKind::Timeout => ExitCode::Network,
                ErrorKind::AuthError => ExitCode::Auth,
                _ => ExitCode::Client,
            };
            assert_eq!(kind.exit_code(), expected, "{kind}");
            assert_eq!(
                CliError::new(kind, "message", ExitCode::Success).exit_code,
                expected,
                "compatibility constructor must enforce canonical mapping for {kind}"
            );
        }
        assert_eq!(ExitCode::Client.as_u8(), 1);
        assert_eq!(ExitCode::Tool.as_u8(), 2);
        assert_eq!(ExitCode::Network.as_u8(), 3);
        assert_eq!(ExitCode::Auth.as_u8(), 4);
    }

    #[test]
    fn constructors_assign_expected_retry_classes_and_http_exit_codes() {
        assert_eq!(
            CliError::network_error("remote", "connection refused").error_class(),
            ErrorClass::Transient
        );
        assert_eq!(
            CliError::tool_execution_failed("server", "tool", "failed").error_class(),
            ErrorClass::Business
        );
        assert_eq!(
            CliError::timeout("calling tool").error_class(),
            ErrorClass::NonTransient
        );

        for status in [401, 403] {
            let error = CliError::http_status("remote", status);
            assert_eq!(error.kind, ErrorKind::AuthError);
            assert_eq!(error.exit_code, ExitCode::Auth);
            assert_eq!(error.error_class(), ErrorClass::Auth);
        }
        for status in [429, 502, 503, 504] {
            let error = CliError::http_status("remote", status);
            assert_eq!(error.kind, ErrorKind::NetworkError);
            assert_eq!(error.exit_code, ExitCode::Network);
            assert_eq!(error.error_class(), ErrorClass::Transient);
        }
        let bad_request = CliError::http_status("remote", 400);
        assert_eq!(bad_request.exit_code, ExitCode::Network);
        assert_eq!(bad_request.error_class(), ErrorClass::NonTransient);
    }

    #[test]
    fn required_recovery_suggestions_match_the_error_type() {
        let cases = [
            (CliError::unknown_command("run"), "--help"),
            (
                CliError::config_not_found(Path::new("missing.json")),
                "--config",
            ),
            (
                CliError::server_not_found("missing", ["alpha", "beta"]),
                "info alpha",
            ),
            (
                CliError::tool_not_found("alpha", "missing", ["read", "write"]),
                "info alpha",
            ),
            (CliError::auth_error("remote", 401), "Authorization"),
            (
                CliError::network_error("remote", "connection refused"),
                "network",
            ),
        ];

        for (error, expected_fragment) in cases {
            let suggestion = error.suggestion.expect("required suggestion");
            assert!(
                suggestion.contains(expected_fragment),
                "{} suggestion was {suggestion:?}",
                error.kind
            );
        }
    }

    #[test]
    fn candidates_are_sorted_limited_and_control_characters_are_neutralized() {
        let error = CliError::server_not_found(
            "missing\nname",
            ["zeta", "beta\nforged", "alpha", "delta", "gamma", "epsilon"],
        );
        assert!(!error.message.contains('\n'));
        let details = error.details.expect("candidate details");
        assert_eq!(
            details,
            "Available servers: alpha, beta forged, delta, epsilon, gamma (+1 more)"
        );
    }

    #[test]
    fn retained_source_never_enters_user_visible_or_debug_text() {
        const SECRET: &str = "Authorization: Bearer super-secret";
        let error = CliError::network_error("remote", "connection failed")
            .with_source(SecretSource(SECRET));

        assert!(Error::source(&error).is_some());
        for visible in [
            error.to_string(),
            error.message.clone(),
            error.details.clone().unwrap_or_default(),
            error.suggestion.clone().unwrap_or_default(),
            format!("{error:?}"),
        ] {
            assert!(
                !visible.contains(SECRET),
                "source leaked through {visible:?}"
            );
            assert!(!visible.contains("super-secret"));
        }
    }
}
