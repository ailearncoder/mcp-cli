use std::ffi::OsString;

use mcp_cli::{CliError, ErrorKind, ExitCode, parse_cli};
use proptest::prelude::*;

const PUBLIC_COMMANDS: &[&str] = &["info", "grep", "call"];
const PUBLIC_ENTRY_POINTS: &[&str] = &["info", "grep", "call", "--help"];

fn server_name() -> impl Strategy<Value = String> {
    "srv[a-z0-9_]{1,16}"
}

fn tool_name() -> impl Strategy<Value = String> {
    "tool[a-z0-9_]{1,16}"
}

fn unknown_option_tokens() -> impl Strategy<Value = Vec<String>> {
    (
        "--[a-z][a-z0-9-]{0,20}".prop_filter("option must not be public", |option| {
            !matches!(
                option.as_str(),
                "--config" | "--with-descriptions" | "--help" | "--version"
            )
        }),
        server_name(),
    )
        .prop_flat_map(|(option, server)| {
            prop_oneof![
                Just(vec![option.clone()]),
                Just(vec!["info".into(), server, option]),
            ]
        })
}

fn rejected_alias_or_internal_tokens() -> impl Strategy<Value = Vec<String>> {
    prop::sample::select(vec![
        "run", "execute", "exec", "invoke", "list", "ls", "get", "show", "describe", "search",
        "find", "query", "__daemon",
    ])
    .prop_map(|command| vec![command.to_owned()])
}

fn empty_target_component_tokens() -> impl Strategy<Value = Vec<String>> {
    (server_name(), tool_name(), 0_u8..8).prop_map(|(server, tool, shape)| match shape {
        0 => vec!["info".into(), format!("/{tool}")],
        1 => vec!["info".into(), format!("{server}/")],
        2 => vec!["info".into(), server, String::new()],
        3 => vec!["call".into(), format!("/{tool}")],
        4 => vec!["call".into(), format!("{server}/")],
        5 => vec!["call".into(), server, String::new()],
        6 => vec!["info".into(), String::new()],
        _ => vec!["call".into(), String::new(), tool],
    })
}

fn missing_argument_tokens() -> impl Strategy<Value = Vec<String>> {
    server_name().prop_flat_map(|server| {
        prop_oneof![
            Just(vec!["info".into()]),
            Just(vec!["grep".into()]),
            Just(vec!["call".into()]),
            Just(vec!["call".into(), server]),
        ]
    })
}

fn too_many_argument_tokens() -> impl Strategy<Value = Vec<String>> {
    (server_name(), tool_name(), "[a-z*?]{1,12}", 0_u8..4).prop_map(
        |(server, tool, pattern, shape)| match shape {
            0 => vec!["info".into(), server, tool, "extra".into()],
            1 => vec!["grep".into(), pattern, "extra".into()],
            2 => vec!["call".into(), server, tool, "{}".into(), "extra".into()],
            _ => vec![
                "call".into(),
                format!("{server}/{tool}"),
                "{}".into(),
                "extra".into(),
            ],
        },
    )
}

fn commandless_ambiguous_target_tokens() -> impl Strategy<Value = Vec<String>> {
    (server_name(), tool_name(), any::<bool>(), any::<bool>()).prop_map(
        |(server, tool, slash_form, with_json)| {
            let mut tokens = if slash_form {
                vec![format!("{server}/{tool}")]
            } else {
                vec![server, tool]
            };
            if with_json {
                tokens.push("{}".into());
            }
            tokens
        },
    )
}

fn assert_public_recoverable_error(
    tokens: &[String],
    expected_kind: ErrorKind,
) -> Result<(), TestCaseError> {
    let error = parse_cli(tokens.iter().map(OsString::from))
        .expect_err("the generated token sequence must be invalid");

    prop_assert_eq!(error.kind, expected_kind, "tokens: {:?}", tokens);
    prop_assert_eq!(error.exit_code, ExitCode::Client, "tokens: {:?}", tokens);
    prop_assert_eq!(
        error.canonical_exit_code(),
        ExitCode::Client,
        "tokens: {:?}",
        tokens
    );

    let suggestion = error.suggestion.as_deref().unwrap_or_default().trim();
    prop_assert!(
        !suggestion.is_empty(),
        "invalid syntax must have a recovery suggestion; tokens: {tokens:?}"
    );

    assert_only_public_commands(&error, tokens)?;
    Ok(())
}

fn assert_only_public_commands(error: &CliError, tokens: &[String]) -> Result<(), TestCaseError> {
    let fields = [
        ("message", error.message.as_str()),
        ("details", error.details.as_deref().unwrap_or_default()),
        (
            "suggestion",
            error.suggestion.as_deref().unwrap_or_default(),
        ),
    ];

    for (field_name, field) in fields {
        prop_assert!(
            !field.contains("__daemon"),
            "{field_name} leaked the internal command for tokens {tokens:?}: {field:?}"
        );

        for invocation in field.split("mcp-cli ").skip(1) {
            let entry_point = invocation
                .trim_start_matches(['\'', '"'])
                .split(|character: char| {
                    character.is_whitespace() || matches!(character, '\'' | '"')
                })
                .next()
                .unwrap_or_default();
            prop_assert!(
                PUBLIC_ENTRY_POINTS.contains(&entry_point),
                "{field_name} referenced non-public entry point {entry_point:?} for tokens {tokens:?}: {field:?}"
            );
        }
    }

    let details = error.details.as_deref().unwrap_or_default();
    if details.contains("Valid commands:") {
        for command in PUBLIC_COMMANDS {
            prop_assert!(
                details.contains(command),
                "public command list omitted {command:?}: {details:?}"
            );
        }
    }

    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 2: 非法 CLI 语法总是产生可恢复错误
    // **Validates: Requirements 1.12**
    #[test]
    fn property_02_invalid_cli_syntax_always_returns_a_recoverable_error(
        unknown_option in unknown_option_tokens(),
        rejected_alias_or_internal in rejected_alias_or_internal_tokens(),
        empty_target_component in empty_target_component_tokens(),
        missing_argument in missing_argument_tokens(),
        too_many_arguments in too_many_argument_tokens(),
        commandless_ambiguous_target in commandless_ambiguous_target_tokens(),
    ) {
        assert_public_recoverable_error(&unknown_option, ErrorKind::InvalidArguments)?;
        assert_public_recoverable_error(
            &rejected_alias_or_internal,
            ErrorKind::UnknownCommand,
        )?;
        assert_public_recoverable_error(
            &empty_target_component,
            ErrorKind::InvalidArguments,
        )?;
        assert_public_recoverable_error(&missing_argument, ErrorKind::InvalidArguments)?;
        assert_public_recoverable_error(&too_many_arguments, ErrorKind::InvalidArguments)?;
        assert_public_recoverable_error(
            &commandless_ambiguous_target,
            ErrorKind::InvalidArguments,
        )?;
    }
}
