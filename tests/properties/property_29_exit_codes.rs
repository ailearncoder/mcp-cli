use mcp_cli::{CliError, ErrorKind, ExitCode};
use proptest::prelude::*;

const EXPECTED_EXIT_CODES: [(ErrorKind, ExitCode); 18] = [
    (ErrorKind::UnknownCommand, ExitCode::Client),
    (ErrorKind::InvalidArguments, ExitCode::Client),
    (ErrorKind::ConfigNotFound, ExitCode::Client),
    (ErrorKind::ConfigReadError, ExitCode::Client),
    (ErrorKind::InvalidConfig, ExitCode::Client),
    (ErrorKind::MissingEnvVar, ExitCode::Client),
    (ErrorKind::InvalidRuntimeConfig, ExitCode::Client),
    (ErrorKind::InvalidServerConfig, ExitCode::Client),
    (ErrorKind::ServerNotFound, ExitCode::Client),
    (ErrorKind::ToolNotFound, ExitCode::Client),
    (ErrorKind::ToolDisabled, ExitCode::Client),
    (ErrorKind::InvalidJson, ExitCode::Client),
    (ErrorKind::InputTooLarge, ExitCode::Client),
    (ErrorKind::SecurityError, ExitCode::Client),
    (ErrorKind::ToolExecutionFailed, ExitCode::Tool),
    (ErrorKind::NetworkError, ExitCode::Network),
    (ErrorKind::Timeout, ExitCode::Network),
    (ErrorKind::AuthError, ExitCode::Auth),
];

fn supplied_exit_code(index: u8) -> ExitCode {
    match index {
        0 => ExitCode::Success,
        1 => ExitCode::Client,
        2 => ExitCode::Tool,
        3 => ExitCode::Network,
        4 => ExitCode::Auth,
        _ => unreachable!("generated exit-code index is bounded"),
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 29: 错误类别到退出码的总映射
    // **Validates: Requirements 12.6, 12.7, 12.8, 12.9**
    #[test]
    fn property_29_error_kinds_have_a_total_stable_exit_code_mapping(
        selected_index in 0_usize..ErrorKind::ALL.len(),
        supplied_index in 0_u8..5,
        first_message in ".{0,64}",
        second_message in ".{0,64}",
    ) {
        prop_assert_eq!(ErrorKind::ALL.len(), EXPECTED_EXIT_CODES.len());

        for (actual_kind, (expected_kind, expected_code)) in ErrorKind::ALL
            .into_iter()
            .zip(EXPECTED_EXIT_CODES)
        {
            prop_assert_eq!(actual_kind, expected_kind);
            prop_assert_eq!(actual_kind.exit_code(), expected_code);
            prop_assert_eq!(actual_kind.exit_code().as_u8(), expected_code.as_u8());
        }

        let (kind, expected_code) = EXPECTED_EXIT_CODES[selected_index];
        let first = CliError::new(
            kind,
            first_message,
            supplied_exit_code(supplied_index),
        );
        let second = CliError::new(kind, second_message, expected_code);
        let canonical = CliError::from_kind(kind, "canonical construction");

        prop_assert_eq!(first.exit_code, expected_code);
        prop_assert_eq!(first.canonical_exit_code(), expected_code);
        prop_assert_eq!(second.exit_code, expected_code);
        prop_assert_eq!(canonical.exit_code, expected_code);
        prop_assert_eq!(first.exit_code, second.exit_code);
    }
}
