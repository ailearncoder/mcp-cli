use mcp_cli::{
    CommandOutcome, DiagnosticSink, DualStreamWriter, ExitCode, PlainTextPresenter, SecretSet,
};
use proptest::prelude::*;
use serde_json::{Number, Value};

#[derive(Clone, Debug)]
enum GeneratedOutcome {
    Json(Value),
    Text(String),
    Empty,
}

impl GeneratedOutcome {
    fn command_outcome(&self) -> CommandOutcome {
        match self {
            Self::Json(value) => CommandOutcome::Json(value.clone()),
            Self::Text(text) => CommandOutcome::HumanText(text.clone()),
            Self::Empty => CommandOutcome::Empty,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum DiagnosticKind {
    Warning,
    Debug,
    ServerStderr,
}

#[derive(Clone, Debug)]
struct GeneratedEvent {
    order: u32,
    kind: DiagnosticKind,
    token: String,
}

#[derive(Debug)]
struct Observation {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: u8,
}

#[derive(Debug, Default)]
struct DiagnosticOracle {
    warning_count: usize,
    debug_count: usize,
    server_count: usize,
    lines: Vec<String>,
    debug_markers: Vec<String>,
}

fn json_string() -> BoxedStrategy<String> {
    prop::collection::vec(any::<char>(), 0..=48)
        .prop_map(|characters| characters.into_iter().collect())
        .boxed()
}

fn json_scalar() -> BoxedStrategy<Value> {
    prop_oneof![
        1 => Just(Value::Null),
        1 => any::<bool>().prop_map(Value::Bool),
        3 => any::<i64>().prop_map(|value| Value::Number(Number::from(value))),
        3 => json_string().prop_map(Value::String),
    ]
    .boxed()
}

fn complete_json_value() -> BoxedStrategy<Value> {
    json_scalar()
        .prop_recursive(6, 256, 10, |inner| {
            let arrays = prop::collection::vec(inner.clone(), 0..=8).prop_map(Value::Array);
            let objects = prop::collection::btree_map(json_string(), inner, 0..=8)
                .prop_map(|entries| Value::Object(entries.into_iter().collect()));
            prop_oneof![3 => arrays, 5 => objects]
        })
        .boxed()
}

fn business_outcome() -> BoxedStrategy<GeneratedOutcome> {
    prop_oneof![
        10 => complete_json_value().prop_map(GeneratedOutcome::Json),
        3 => proptest::string::string_regex("[A-Za-z0-9 ._-]{1,80}")
            .expect("business text regex is valid")
            .prop_map(GeneratedOutcome::Text),
        1 => Just(GeneratedOutcome::Empty),
    ]
    .boxed()
}

fn diagnostic_kind() -> BoxedStrategy<DiagnosticKind> {
    prop_oneof![
        Just(DiagnosticKind::Warning),
        Just(DiagnosticKind::Debug),
        Just(DiagnosticKind::ServerStderr),
    ]
    .boxed()
}

fn diagnostic_events() -> BoxedStrategy<Vec<GeneratedEvent>> {
    prop::collection::vec(
        (
            any::<u32>(),
            diagnostic_kind(),
            proptest::string::string_regex("[a-z][a-z0-9]{0,15}")
                .expect("diagnostic token regex is valid"),
        ),
        0..=12,
    )
    .prop_map(|events| {
        let mut events = events
            .into_iter()
            .map(|(order, kind, token)| GeneratedEvent { order, kind, token })
            .collect::<Vec<_>>();
        // Independent random order keys exercise arbitrary diagnostic permutations.
        events.sort_by_key(|event| event.order);
        events
    })
    .boxed()
}

fn optional_secret() -> BoxedStrategy<Option<String>> {
    prop::option::of(
        proptest::string::string_regex("secret_[A-Za-z0-9]{8,24}").expect("secret regex is valid"),
    )
    .boxed()
}

fn event_marker(index: usize, token: &str) -> String {
    // Generated tokens cannot contain any production prefix punctuation, so
    // event payloads cannot be mistaken for diagnostic prefixes.
    format!("evt-{index}-{token}")
}

fn visible_message(marker: &str, secret: Option<&str>) -> String {
    match secret {
        Some(_) => format!("{marker} credential=[REDACTED]"),
        None => marker.to_owned(),
    }
}

fn emitted_message(marker: &str, secret: Option<&str>) -> String {
    match secret {
        Some(secret) => format!("{marker} credential={secret}"),
        None => marker.to_owned(),
    }
}

fn build_oracle(
    events: &[GeneratedEvent],
    debug_enabled: bool,
    secret: Option<&str>,
) -> DiagnosticOracle {
    let mut oracle = DiagnosticOracle::default();

    for (index, event) in events.iter().enumerate() {
        let marker = event_marker(index, &event.token);
        match event.kind {
            DiagnosticKind::Warning => {
                oracle.warning_count += 1;
                oracle.lines.push(format!(
                    "[mcp-cli] warning: {}",
                    visible_message(&marker, secret)
                ));
            }
            DiagnosticKind::Debug => {
                oracle.debug_markers.push(marker.clone());
                if debug_enabled {
                    oracle.debug_count += 1;
                    oracle.lines.push(format!(
                        "[mcp-cli] debug: {}",
                        visible_message(&marker, secret)
                    ));
                }
            }
            DiagnosticKind::ServerStderr => {
                oracle.server_count += 1;
                let payload = match secret {
                    Some(_) => "[REDACTED]".to_owned(),
                    None => marker,
                };
                oracle
                    .lines
                    .push(format!("[server] srv-{index}: {payload}"));
            }
        }
    }

    oracle
}

#[allow(clippy::too_many_arguments)]
fn execute(
    outcome: &GeneratedOutcome,
    events: &[GeneratedEvent],
    stdout_is_tty: bool,
    stderr_is_tty: bool,
    no_color_present: bool,
    debug_enabled: bool,
    secret: Option<&str>,
) -> Observation {
    let mut secrets = SecretSet::new();
    if let Some(secret) = secret {
        secrets.insert(secret);
    }
    let streams = DualStreamWriter::new(
        Vec::new(),
        Vec::new(),
        stdout_is_tty,
        stderr_is_tty,
        no_color_present,
        debug_enabled,
        secrets,
    );

    for (index, event) in events.iter().enumerate() {
        let marker = event_marker(index, &event.token);
        match event.kind {
            DiagnosticKind::Warning => streams.warning(&emitted_message(&marker, secret)),
            DiagnosticKind::Debug => streams.debug(&emitted_message(&marker, secret)),
            DiagnosticKind::ServerStderr => {
                let payload =
                    secret.map_or_else(|| marker.into_bytes(), |value| value.as_bytes().to_vec());
                streams.server_stderr(&format!("srv-{index}"), &payload);
                streams.server_stderr_flush(&format!("srv-{index}"));
            }
        }
    }

    let exit_code = match streams.write_outcome(&PlainTextPresenter, outcome.command_outcome()) {
        Ok(()) => ExitCode::Success.as_u8(),
        Err(error) => error.canonical_exit_code().as_u8(),
    };
    let (stdout, stderr) = streams.into_writers();

    Observation {
        stdout,
        stderr,
        exit_code,
    }
}

fn strip_ansi(bytes: &[u8]) -> Vec<u8> {
    let mut plain = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'[') {
            index += 2;
            while index < bytes.len() && !bytes[index].is_ascii_alphabetic() {
                index += 1;
            }
            index += usize::from(index < bytes.len());
        } else {
            plain.push(bytes[index]);
            index += 1;
        }
    }
    plain
}

fn parse_exactly_one_json(bytes: &[u8]) -> Result<Value, String> {
    let mut values = serde_json::Deserializer::from_slice(bytes).into_iter::<Value>();
    let value = values
        .next()
        .ok_or_else(|| "stdout did not contain JSON".to_owned())?
        .map_err(|error| error.to_string())?;
    match values.next() {
        None => Ok(value),
        Some(Ok(_)) => Err("stdout contained multiple JSON values".to_owned()),
        Some(Err(error)) => Err(format!("stdout had a non-JSON suffix: {error}")),
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 27: 诊断不污染业务结果
    // **Validates: Requirements 11.12, 13.3, 13.6, 13.7**
    #[test]
    fn property_27_diagnostics_do_not_change_business_results(
        outcome in business_outcome(),
        events in diagnostic_events(),
        debug_enabled in any::<bool>(),
        stdout_is_tty in any::<bool>(),
        stderr_is_tty in any::<bool>(),
        no_color_present in any::<bool>(),
        secret in optional_secret(),
    ) {
        let baseline = execute(
            &outcome,
            &[],
            stdout_is_tty,
            stderr_is_tty,
            no_color_present,
            debug_enabled,
            secret.as_deref(),
        );
        let diagnosed = execute(
            &outcome,
            &events,
            stdout_is_tty,
            stderr_is_tty,
            no_color_present,
            debug_enabled,
            secret.as_deref(),
        );
        let oracle = build_oracle(&events, debug_enabled, secret.as_deref());

        prop_assert_eq!(
            &diagnosed.stdout,
            &baseline.stdout,
            "adding or permuting diagnostics must not change stdout bytes",
        );
        prop_assert_eq!(diagnosed.exit_code, baseline.exit_code);
        prop_assert_eq!(diagnosed.exit_code, ExitCode::Success.as_u8());
        prop_assert!(baseline.stderr.is_empty(), "baseline has no diagnostic events");

        let plain_stderr = strip_ansi(&diagnosed.stderr);
        let stderr_text = String::from_utf8(plain_stderr)
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        let actual_lines = stderr_text.lines().collect::<Vec<_>>();
        let expected_lines = oracle.lines.iter().map(String::as_str).collect::<Vec<_>>();
        prop_assert_eq!(actual_lines, expected_lines, "stderr must follow the independent event oracle");

        let actual_warning_count = stderr_text
            .lines()
            .filter(|line| line.starts_with("[mcp-cli] warning: "))
            .count();
        let actual_debug_count = stderr_text
            .lines()
            .filter(|line| line.starts_with("[mcp-cli] debug: "))
            .count();
        let actual_server_count = stderr_text
            .lines()
            .filter(|line| line.starts_with("[server] srv-"))
            .count();
        prop_assert_eq!(actual_warning_count, oracle.warning_count);
        prop_assert_eq!(actual_debug_count, oracle.debug_count);
        prop_assert_eq!(actual_server_count, oracle.server_count);

        for marker in &oracle.debug_markers {
            prop_assert_eq!(
                stderr_text.contains(marker),
                debug_enabled,
                "debug payload presence must exactly follow the debug flag",
            );
        }
        if let Some(secret) = &secret {
            prop_assert!(!stderr_text.contains(secret), "registered secret leaked to stderr");
        }

        if let GeneratedOutcome::Json(expected) = &outcome {
            prop_assert!(
                !diagnosed.stdout.contains(&0x1b),
                "JSON stdout must never contain ANSI even when stdout is a TTY",
            );
            let parsed = parse_exactly_one_json(&diagnosed.stdout)
                .map_err(TestCaseError::fail)?;
            prop_assert_eq!(&parsed, expected, "JSON business outcome must remain complete and parseable");
        }
    }
}
