use std::path::PathBuf;

use mcp_cli::{CliError, ErrorKind, ExitCode};
use proptest::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SuggestionType {
    Command,
    Server,
    Tool,
    Config,
    Auth,
    Network,
}

impl SuggestionType {
    const ALL: [Self; 6] = [
        Self::Command,
        Self::Server,
        Self::Tool,
        Self::Config,
        Self::Auth,
        Self::Network,
    ];

    const fn expected_error(self) -> (ErrorKind, ExitCode) {
        match self {
            Self::Command => (ErrorKind::UnknownCommand, ExitCode::Client),
            Self::Server => (ErrorKind::ServerNotFound, ExitCode::Client),
            Self::Tool => (ErrorKind::ToolNotFound, ExitCode::Client),
            Self::Config => (ErrorKind::ConfigNotFound, ExitCode::Client),
            Self::Auth => (ErrorKind::AuthError, ExitCode::Auth),
            Self::Network => (ErrorKind::NetworkError, ExitCode::Network),
        }
    }

    const fn action_keywords(self) -> &'static [&'static str] {
        match self {
            Self::Command => &["--help", " help", "info", "grep", "call"],
            Self::Server => &["mcp-cli info", "add server", "listed server"],
            Self::Tool => &["available tool", "tool schema", "schemas"],
            Self::Config => &["--config", "configuration file", "mcp_servers.json"],
            Self::Auth => &["authorization", "credential", "permission", "access"],
            Self::Network => &["network", "connectivity", "server address", "availability"],
        }
    }
}

fn safe_name() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_-]{0,23}"
}

fn suggestion_types(suggestion: &str) -> Vec<SuggestionType> {
    let normalized = suggestion.to_lowercase();
    SuggestionType::ALL
        .into_iter()
        .filter(|suggestion_type| {
            suggestion_type
                .action_keywords()
                .iter()
                .any(|keyword| normalized.contains(keyword))
        })
        .collect()
}

fn assert_typed_suggestion(
    error: CliError,
    expected_type: SuggestionType,
) -> Result<(), TestCaseError> {
    let (expected_kind, expected_exit_code) = expected_type.expected_error();
    prop_assert_eq!(error.kind, expected_kind);
    prop_assert_eq!(error.exit_code, expected_exit_code);
    prop_assert_eq!(error.canonical_exit_code(), expected_exit_code);

    let suggestion = error.suggestion.as_deref().unwrap_or_default().trim();
    prop_assert!(
        !suggestion.is_empty(),
        "{expected_kind} must provide a non-empty recovery suggestion"
    );
    prop_assert!(
        !suggestion.contains("__daemon"),
        "{expected_kind} exposed a non-public command in {suggestion:?}"
    );

    let classified_types = suggestion_types(suggestion);
    prop_assert!(
        classified_types.contains(&expected_type),
        "{expected_kind} suggestion contained only unrelated actions: {suggestion:?} classified as {classified_types:?}"
    );
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 30: 要求恢复建议的错误具有类型相关建议
    // **Validates: Requirements 12.11**
    #[test]
    fn property_30_recovery_suggestions_match_their_error_type(
        command in safe_name(),
        server in safe_name(),
        tool in safe_name(),
        server_candidates in prop::collection::vec(safe_name(), 1..=6),
        tool_candidates in prop::collection::vec(safe_name(), 1..=6),
        config_file in safe_name(),
        auth_status in prop::sample::select(vec![401_u16, 403_u16]),
        network_detail in prop::sample::select(vec![
            "connection refused",
            "connection reset",
            "temporary DNS failure",
            "host unreachable",
        ]),
    ) {
        let config_path = PathBuf::from(format!("/safe/config/{config_file}.json"));
        let cases = [
            (
                SuggestionType::Command,
                CliError::unknown_command(&command),
            ),
            (
                SuggestionType::Server,
                CliError::server_not_found(&server, &server_candidates),
            ),
            (
                SuggestionType::Tool,
                CliError::tool_not_found(&server, &tool, &tool_candidates),
            ),
            (
                SuggestionType::Config,
                CliError::config_not_found(&config_path),
            ),
            (
                SuggestionType::Auth,
                CliError::auth_error(&server, auth_status),
            ),
            (
                SuggestionType::Network,
                CliError::network_error(&server, network_detail),
            ),
        ];

        for (expected_type, error) in cases {
            assert_typed_suggestion(error, expected_type)?;
        }
    }
}
