use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

use mcp_cli::{DiagnosticSink, ErrorKind, config::substitute::substitute_environment};
use proptest::prelude::*;
use serde_json::{Map, Value, json};

#[derive(Default)]
struct RecordingSink {
    warnings: Mutex<Vec<String>>,
    debug: Mutex<Vec<String>>,
    server_stderr: Mutex<Vec<(String, Vec<u8>)>>,
}

impl RecordingSink {
    fn warnings(&self) -> Vec<String> {
        self.warnings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn debug(&self) -> Vec<String> {
        self.debug
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn server_stderr(&self) -> Vec<(String, Vec<u8>)> {
        self.server_stderr
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl DiagnosticSink for RecordingSink {
    fn warning(&self, message: &str) {
        self.warnings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(message.to_owned());
    }

    fn debug(&self, message: &str) {
        self.debug
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(message.to_owned());
    }

    fn server_stderr(&self, server: &str, bytes: &[u8]) {
        self.server_stderr
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((server.to_owned(), bytes.to_vec()));
    }
}

#[derive(Clone, Debug)]
struct MissingEnvironmentCase {
    input: Value,
    missing: BTreeSet<String>,
    env: BTreeMap<String, String>,
}

fn secret_token() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[a-zA-Z0-9]{8,24}").expect("the secret token regex is valid")
}

fn placeholder(name: &str) -> String {
    format!("${{{name}}}")
}

fn missing_environment_case() -> impl Strategy<Value = MissingEnvironmentCase> {
    (
        prop::collection::btree_set(0_u16..10_000, 1..6),
        prop::collection::btree_set(secret_token(), 2..6),
    )
        .prop_map(|(missing_suffixes, secret_tokens)| {
            let missing = missing_suffixes
                .into_iter()
                .map(|suffix| format!("MISSING_{suffix}"))
                .collect::<BTreeSet<_>>();
            let env = secret_tokens
                .into_iter()
                .enumerate()
                .map(|(index, token)| {
                    (
                        format!("DEFINED_SECRET_{index}"),
                        format!("secret-value-{token}"),
                    )
                })
                .collect::<BTreeMap<_, _>>();

            let first_missing = missing.first().expect("at least one missing variable");
            let repeated_missing = [
                placeholder(first_missing),
                placeholder(first_missing),
                placeholder(first_missing),
            ]
            .join("::");
            let every_missing = missing
                .iter()
                .map(|name| placeholder(name))
                .collect::<Vec<_>>()
                .join("|");
            let every_defined = env
                .keys()
                .map(|name| placeholder(name))
                .collect::<Vec<_>>()
                .join("|");
            let mixed = format!(
                "prefix-{}-middle-{}-suffix",
                placeholder(first_missing),
                every_defined
            );

            let input = json!({
                "level_one": {
                    "level_two": [
                        {"repeated_missing": repeated_missing},
                        {"all_missing": every_missing},
                        {
                            "level_three": {
                                "all_defined": every_defined,
                                "mixed": mixed,
                            }
                        },
                        [null, true, 42],
                    ]
                },
                "plain": "unchanged",
            });

            MissingEnvironmentCase {
                input,
                missing,
                env,
            }
        })
}

fn valid_variable_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn reference_non_strict_string(input: &str, env: &BTreeMap<String, String>) -> String {
    let mut output = String::with_capacity(input.len());
    let mut remaining = input;

    while let Some(start) = remaining.find("${") {
        output.push_str(&remaining[..start]);
        let candidate = &remaining[start + 2..];
        let Some(end) = candidate.find('}') else {
            output.push_str(&remaining[start..]);
            return output;
        };
        let name = &candidate[..end];
        if valid_variable_name(name) {
            output.push_str(env.get(name).map(String::as_str).unwrap_or_default());
            remaining = &candidate[end + 1..];
        } else {
            output.push_str("${");
            remaining = candidate;
        }
    }

    output.push_str(remaining);
    output
}

fn reference_non_strict(value: &Value, env: &BTreeMap<String, String>) -> Value {
    match value {
        Value::String(text) => Value::String(reference_non_strict_string(text, env)),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| reference_non_strict(value, env))
                .collect(),
        ),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), reference_non_strict(value, env)))
                .collect::<Map<_, _>>(),
        ),
        Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 6: 缺失环境变量策略完备且不泄密
    // **Validates: Requirements 3.2, 3.3**
    #[test]
    fn property_06_missing_environment_policy_is_complete_and_secret_safe(
        case in missing_environment_case(),
    ) {
        prop_assert!(!case.missing.is_empty());
        prop_assert!(case.env.len() >= 2);
        prop_assert!(case.env.values().all(|secret| !secret.is_empty()));
        prop_assert_eq!(
            case.env.values().collect::<BTreeSet<_>>().len(),
            case.env.len(),
            "defined secrets must be unique so every registration is observable",
        );

        let strict_sink = RecordingSink::default();
        let error = substitute_environment(
            &case.input,
            true,
            |name| case.env.get(name).cloned(),
            &strict_sink,
        )
        .expect_err("strict mode must reject every case containing a missing reference");

        let sorted_missing = case.missing.iter().cloned().collect::<Vec<_>>();
        let expected_details = format!("Missing variables: {}", sorted_missing.join(", "));
        prop_assert_eq!(error.kind, ErrorKind::MissingEnvVar);
        prop_assert_eq!(error.details.as_deref(), Some(expected_details.as_str()));
        prop_assert!(strict_sink.warnings().is_empty());
        prop_assert!(strict_sink.debug().is_empty());
        prop_assert!(strict_sink.server_stderr().is_empty());

        let visible_error_fields = [
            error.kind.to_string(),
            format!("{:?}", error.kind),
            format!("{:?}", error.exit_code),
            error.message.clone(),
            error.details.clone().unwrap_or_default(),
            error.suggestion.clone().unwrap_or_default(),
            error.to_string(),
            format!("{error:?}"),
        ];
        for secret in case.env.values() {
            for visible in &visible_error_fields {
                prop_assert!(
                    !visible.contains(secret),
                    "strict error channel leaked secret {secret:?} through {visible:?}",
                );
            }
        }

        let non_strict_sink = RecordingSink::default();
        let outcome = substitute_environment(
            &case.input,
            false,
            |name| case.env.get(name).cloned(),
            &non_strict_sink,
        )
        .expect("non-strict mode must replace missing references with empty strings");

        let expected_value = reference_non_strict(&case.input, &case.env);
        prop_assert_eq!(&outcome.value, &expected_value);
        prop_assert_eq!(&outcome.missing, &case.missing);

        let expected_warnings = sorted_missing
            .iter()
            .map(|name| {
                format!(
                    "Environment variable {name} is not set; substituting an empty string"
                )
            })
            .collect::<Vec<_>>();
        let warnings = non_strict_sink.warnings();
        prop_assert_eq!(&warnings, &expected_warnings);
        prop_assert!(non_strict_sink.debug().is_empty());
        prop_assert!(non_strict_sink.server_stderr().is_empty());

        for secret in case.env.values() {
            for warning in &warnings {
                prop_assert!(
                    !warning.contains(secret),
                    "non-strict warning leaked secret {secret:?} through {warning:?}",
                );
            }

            let sample = format!("before-{secret}-after");
            let redacted = outcome.secrets.redact(&sample);
            prop_assert!(!redacted.contains(secret));
            prop_assert_eq!(redacted, "before-[REDACTED]-after");
        }
        prop_assert_eq!(outcome.secrets.len(), case.env.len());
        let secret_set_debug = format!("{:?}", outcome.secrets);
        for secret in case.env.values() {
            prop_assert!(!secret_set_debug.contains(secret));
        }
    }
}
