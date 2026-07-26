use std::collections::BTreeSet;

use mcp_cli::{
    CliError, CommandOutcome, DiagnosticSink, DualStreamWriter, ErrorKind, JsonPresenter,
    PlainTextPresenter, SecretSet, StreamStylePolicies, render_structured_error_with_style,
};
use proptest::prelude::*;
use serde_json::{Number, Value};

const BOOLEAN_VALUES: [bool; 2] = [false, true];

#[derive(Clone, Debug)]
struct GeneratedCase {
    text: String,
    error_kind: ErrorKind,
    error_message: String,
    error_details: Option<String>,
    error_suggestion: Option<String>,
    diagnostic: String,
    json: Value,
}

#[derive(Debug)]
struct Observation {
    text_stdout: Vec<u8>,
    diagnostic_stderr: Vec<u8>,
    error_stderr: Vec<u8>,
    json_stdout: Vec<u8>,
    json_stderr: Vec<u8>,
}

fn semantic_text() -> BoxedStrategy<String> {
    proptest::string::string_regex("[A-Za-z0-9 ._/:-]{1,80}")
        .expect("semantic text regex is valid")
        .boxed()
}

fn json_string() -> BoxedStrategy<String> {
    prop::collection::vec(any::<char>(), 0..=32)
        .prop_map(|characters| characters.into_iter().collect())
        .boxed()
}

fn json_scalar() -> BoxedStrategy<Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|value| Value::Number(Number::from(value))),
        json_string().prop_map(Value::String),
    ]
    .boxed()
}

fn json_value() -> BoxedStrategy<Value> {
    json_scalar()
        .prop_recursive(5, 128, 8, |inner| {
            let arrays = prop::collection::vec(inner.clone(), 0..=6).prop_map(Value::Array);
            let objects = prop::collection::btree_map(json_string(), inner, 0..=6)
                .prop_map(|entries| Value::Object(entries.into_iter().collect()));
            prop_oneof![arrays, objects]
        })
        .boxed()
}

fn generated_case() -> BoxedStrategy<GeneratedCase> {
    (
        semantic_text(),
        prop::sample::select(ErrorKind::ALL.to_vec()),
        semantic_text(),
        prop::option::of(semantic_text()),
        prop::option::of(semantic_text()),
        semantic_text(),
        json_value(),
    )
        .prop_map(
            |(
                text,
                error_kind,
                error_message,
                error_details,
                error_suggestion,
                diagnostic,
                json,
            )| {
                GeneratedCase {
                    text,
                    error_kind,
                    error_message,
                    error_details,
                    error_suggestion,
                    diagnostic,
                    json,
                }
            },
        )
        .boxed()
}

fn make_error(case: &GeneratedCase) -> CliError {
    let mut error = CliError::from_kind(case.error_kind, case.error_message.clone());
    if let Some(details) = &case.error_details {
        error = error.with_details(details.clone());
    }
    if let Some(suggestion) = &case.error_suggestion {
        error = error.with_suggestion(suggestion.clone());
    }
    error
}

fn observe(
    case: &GeneratedCase,
    stdout_is_tty: bool,
    stderr_is_tty: bool,
    no_color_environment: Option<&str>,
) -> Observation {
    // Presence, not truthiness, is passed to production. `Some("")` therefore
    // explicitly exercises the required `NO_COLOR=` empty-value semantics.
    let no_color_present = no_color_environment.is_some();
    let streams = DualStreamWriter::new(
        Vec::new(),
        Vec::new(),
        stdout_is_tty,
        stderr_is_tty,
        no_color_present,
        true,
        SecretSet::new(),
    );
    streams
        .write_outcome(
            &PlainTextPresenter,
            CommandOutcome::HumanText(case.text.clone()),
        )
        .expect("Vec-backed text output cannot fail");
    streams.warning(&case.diagnostic);
    streams.debug(&case.diagnostic);
    streams.server_stderr("generated-server", case.diagnostic.as_bytes());
    streams.server_stderr_flush("generated-server");
    let (text_stdout, diagnostic_stderr) = streams.into_writers();

    let styles = StreamStylePolicies::new(stdout_is_tty, stderr_is_tty, no_color_present);
    let mut error_stderr = Vec::new();
    render_structured_error_with_style(&mut error_stderr, &make_error(case), styles.stderr)
        .expect("Vec-backed error output cannot fail");

    let json_streams = DualStreamWriter::new(
        Vec::new(),
        Vec::new(),
        stdout_is_tty,
        stderr_is_tty,
        no_color_present,
        false,
        SecretSet::new(),
    );
    json_streams
        .write_outcome(&JsonPresenter, CommandOutcome::Json(case.json.clone()))
        .expect("generated JSON output must serialize");
    let (json_stdout, json_stderr) = json_streams.into_writers();

    Observation {
        text_stdout,
        diagnostic_stderr,
        error_stderr,
        json_stdout,
        json_stderr,
    }
}

/// Independent ANSI CSI stripping oracle. It intentionally does not use any
/// production constants or helpers and preserves malformed escape prefixes.
fn strip_ansi(bytes: &[u8]) -> Vec<u8> {
    let mut plain = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] != 0x1b || bytes.get(index + 1) != Some(&b'[') {
            plain.push(bytes[index]);
            index += 1;
            continue;
        }

        let escape_start = index;
        index += 2;
        while index < bytes.len()
            && ((0x30..=0x3f).contains(&bytes[index]) || (0x20..=0x2f).contains(&bytes[index]))
        {
            index += 1;
        }
        if index < bytes.len() && (0x40..=0x7e).contains(&bytes[index]) {
            index += 1;
        } else {
            plain.extend_from_slice(&bytes[escape_start..index]);
        }
    }

    plain
}

fn contains_ansi(bytes: &[u8]) -> bool {
    strip_ansi(bytes) != bytes
}

fn observation_index(stdout_is_tty: bool, stderr_is_tty: bool, no_color_present: bool) -> usize {
    usize::from(stdout_is_tty) * 4 + usize::from(stderr_is_tty) * 2 + usize::from(no_color_present)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 31: 颜色策略 truth table
    // **Validates: Requirements 13.4, 13.5**
    #[test]
    fn property_31_color_policy_obeys_the_complete_truth_table(case in generated_case()) {
        let baseline = observe(&case, false, false, None);
        let mut observations = Vec::with_capacity(8);
        let mut combinations = BTreeSet::new();

        // Every generated semantic case explicitly traverses all 2^3 inputs.
        for stdout_is_tty in BOOLEAN_VALUES {
            for stderr_is_tty in BOOLEAN_VALUES {
                for no_color_present in BOOLEAN_VALUES {
                    combinations.insert((stdout_is_tty, stderr_is_tty, no_color_present));
                    let no_color_environment = no_color_present.then_some("");
                    let observed = observe(
                        &case,
                        stdout_is_tty,
                        stderr_is_tty,
                        no_color_environment,
                    );
                    let stdout_allows_ansi = stdout_is_tty && !no_color_present;
                    let stderr_allows_ansi = stderr_is_tty && !no_color_present;

                    prop_assert_eq!(
                        contains_ansi(&observed.text_stdout),
                        stdout_allows_ansi,
                        "stdout text used the wrong policy for {:?}/{:?}/{:?}",
                        stdout_is_tty,
                        stderr_is_tty,
                        no_color_present,
                    );
                    prop_assert_eq!(
                        contains_ansi(&observed.diagnostic_stderr),
                        stderr_allows_ansi,
                        "stderr diagnostics used the wrong policy for {:?}/{:?}/{:?}",
                        stdout_is_tty,
                        stderr_is_tty,
                        no_color_present,
                    );
                    prop_assert_eq!(
                        contains_ansi(&observed.error_stderr),
                        stderr_allows_ansi,
                        "structured errors used the wrong stderr policy",
                    );
                    prop_assert!(!contains_ansi(&observed.json_stdout), "JSON stdout must never contain ANSI");
                    prop_assert!(observed.json_stderr.is_empty(), "JSON rendering must not write stderr");

                    prop_assert_eq!(strip_ansi(&observed.text_stdout), baseline.text_stdout.clone());
                    prop_assert_eq!(
                        strip_ansi(&observed.diagnostic_stderr),
                        baseline.diagnostic_stderr.clone(),
                    );
                    prop_assert_eq!(strip_ansi(&observed.error_stderr), baseline.error_stderr.clone());
                    prop_assert_eq!(observed.json_stdout.as_slice(), baseline.json_stdout.as_slice());
                    let parsed: Value = serde_json::from_slice(&observed.json_stdout)
                        .map_err(|error| TestCaseError::fail(error.to_string()))?;
                    prop_assert_eq!(parsed, case.json.clone());

                    if no_color_present {
                        prop_assert!(!contains_ansi(&observed.text_stdout));
                        prop_assert!(!contains_ansi(&observed.diagnostic_stderr));
                        prop_assert!(!contains_ansi(&observed.error_stderr));
                        prop_assert_eq!(observed.text_stdout.as_slice(), baseline.text_stdout.as_slice());
                        prop_assert_eq!(
                            observed.diagnostic_stderr.as_slice(),
                            baseline.diagnostic_stderr.as_slice(),
                        );
                        prop_assert_eq!(observed.error_stderr.as_slice(), baseline.error_stderr.as_slice());
                    }

                    observations.push(observed);
                }
            }
        }
        prop_assert_eq!(combinations.len(), 8, "the complete truth table must be traversed per case");

        // Toggling the other stream's TTY bit must not alter this stream.
        for no_color_present in BOOLEAN_VALUES {
            for stdout_is_tty in BOOLEAN_VALUES {
                let stderr_not_tty = &observations[observation_index(
                    stdout_is_tty,
                    false,
                    no_color_present,
                )];
                let stderr_tty = &observations[observation_index(
                    stdout_is_tty,
                    true,
                    no_color_present,
                )];
                prop_assert_eq!(&stderr_not_tty.text_stdout, &stderr_tty.text_stdout);
                prop_assert_eq!(&stderr_not_tty.json_stdout, &stderr_tty.json_stdout);
            }
            for stderr_is_tty in BOOLEAN_VALUES {
                let stdout_not_tty = &observations[observation_index(
                    false,
                    stderr_is_tty,
                    no_color_present,
                )];
                let stdout_tty = &observations[observation_index(
                    true,
                    stderr_is_tty,
                    no_color_present,
                )];
                prop_assert_eq!(
                    &stdout_not_tty.diagnostic_stderr,
                    &stdout_tty.diagnostic_stderr,
                );
                prop_assert_eq!(&stdout_not_tty.error_stderr, &stdout_tty.error_stderr);
            }
        }
    }
}
