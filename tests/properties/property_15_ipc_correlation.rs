#![cfg(unix)]
#![forbid(unsafe_code)]

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use mcp_cli::{
    BoxFuture, CancellationFlag, CommandContext, ConnectionError, ConnectionMode, Deadline,
    DiagnosticSink, JsonObject, McpConnection, ToolInfo, ToolResult,
    daemon::worker::serve_test_client,
};
use proptest::{prelude::*, test_runner::TestCaseError};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::timeout,
};

const CASES: u32 = 128;
const IO_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Default)]
struct SilentDiagnostics;

impl DiagnosticSink for SilentDiagnostics {
    fn warning(&self, _message: &str) {}
    fn debug(&self, _message: &str) {}
    fn server_stderr(&self, _server: &str, _bytes: &[u8]) {}
}

struct PropertyConnection;

impl McpConnection for PropertyConnection {
    fn list_tools<'a>(
        &'a self,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
        Box::pin(async {
            Ok(vec![ToolInfo {
                name: "echo".to_owned(),
                description: Some("property fixture".to_owned()),
                input_schema: json!({"type":"object"}),
            }])
        })
    }

    fn call_tool<'a>(
        &'a self,
        _ctx: &'a CommandContext,
        name: &'a str,
        args: JsonObject,
    ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
        Box::pin(async move { Ok(json!({"tool":name,"args":args})) })
    }

    fn instructions(&self) -> Option<&str> {
        Some("property instructions")
    }

    fn close<'a>(
        self: Box<Self>,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<(), ConnectionError>> {
        Box::pin(async { Ok(()) })
    }

    fn mode(&self) -> ConnectionMode {
        ConnectionMode::Daemon
    }
}

fn context() -> CommandContext {
    CommandContext {
        deadline: Deadline::new(Instant::now() + Duration::from_secs(10)),
        cancellation: Arc::new(CancellationFlag::default()),
        diagnostics: Arc::new(SilentDiagnostics),
    }
}

fn request_id() -> impl Strategy<Value = String> {
    let generated = prop::collection::vec(
        prop::sample::select(
            "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_é界🦀"
                .chars()
                .collect::<Vec<_>>(),
        ),
        1..=20,
    )
    .prop_map(|characters| characters.into_iter().collect::<String>());

    prop_oneof![
        8 => generated,
        1 => Just("x".repeat(128)),
        1 => Just("🦀".repeat(32)),
    ]
}

fn safe_text(prefix: &'static str) -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(
            "abcdefghijklmnopqrstuvwxyz0123456789"
                .chars()
                .collect::<Vec<_>>(),
        ),
        1..=12,
    )
    .prop_map(move |characters| format!("{prefix}{}", characters.into_iter().collect::<String>()))
}

fn tool_name() -> impl Strategy<Value = String> {
    safe_text("tool-")
}

fn arguments() -> impl Strategy<Value = Map<String, Value>> {
    prop::collection::btree_map(
        safe_text("key-"),
        prop_oneof![
            any::<bool>().prop_map(Value::Bool),
            any::<i32>().prop_map(|value| json!(value)),
            safe_text("value-").prop_map(Value::String),
        ],
        0..=4,
    )
    .prop_map(|entries| entries.into_iter().collect())
}

#[derive(Clone, Debug)]
enum ValidOperation {
    Ping,
    ListTools,
    CallTool { tool_name: String, args: JsonObject },
    GetInstructions,
}

fn valid_operation() -> impl Strategy<Value = ValidOperation> {
    prop_oneof![
        Just(ValidOperation::Ping),
        Just(ValidOperation::ListTools),
        (tool_name(), arguments())
            .prop_map(|(tool_name, args)| ValidOperation::CallTool { tool_name, args }),
        Just(ValidOperation::GetInstructions),
    ]
}

#[derive(Clone, Copy, Debug)]
enum InvalidKind {
    InvalidJson,
    MissingId,
    UnknownType,
    InvalidArguments,
    InvalidId,
}

#[derive(Clone, Debug)]
enum ExpectedOutcome {
    Success(Value),
    Failure {
        code: &'static str,
        message: &'static str,
    },
}

#[derive(Clone, Debug)]
struct ExpectedResponse {
    id: String,
    outcome: ExpectedOutcome,
}

fn append_json_frame(wire: &mut Vec<u8>, value: &Value) {
    wire.extend(serde_json::to_vec(value).expect("generated JSON serializes"));
    wire.push(b'\n');
}

fn canonical_error(kind: InvalidKind) -> (&'static str, &'static str) {
    match kind {
        InvalidKind::InvalidJson => ("INVALID_JSON", "Invalid JSON request"),
        InvalidKind::MissingId => ("MISSING_ID", "Request ID is required"),
        InvalidKind::UnknownType => ("UNKNOWN_TYPE", "Unknown request type"),
        InvalidKind::InvalidArguments | InvalidKind::InvalidId => {
            ("INVALID_ARGUMENTS", "Invalid request arguments")
        }
    }
}

fn append_invalid_frame(
    wire: &mut Vec<u8>,
    kind: InvalidKind,
    id: &str,
    secret: &str,
    invalid_argument_shape: u8,
) -> ExpectedResponse {
    match kind {
        InvalidKind::InvalidJson => {
            wire.extend(format!("{{\"payload\":\"{secret}\"").as_bytes());
            wire.push(b'\n');
        }
        InvalidKind::MissingId => {
            append_json_frame(wire, &json!({"type":"ping","payload":secret}));
        }
        InvalidKind::UnknownType => {
            append_json_frame(
                wire,
                &json!({"id":id,"type":"unknown-operation","payload":secret}),
            );
        }
        InvalidKind::InvalidArguments => {
            let value = match invalid_argument_shape % 3 {
                0 => {
                    json!({"id":id,"type":"callTool","toolName":"echo","args":[],"payload":secret})
                }
                1 => json!({"id":id,"type":"callTool","args":{},"payload":secret}),
                _ => json!({"id":id,"type":"ping","unexpected":secret}),
            };
            append_json_frame(wire, &value);
        }
        InvalidKind::InvalidId => {
            append_json_frame(wire, &json!({"id":"","type":"ping","payload":secret}));
        }
    }

    let (code, message) = canonical_error(kind);
    ExpectedResponse {
        id: match kind {
            InvalidKind::UnknownType | InvalidKind::InvalidArguments => id.to_owned(),
            InvalidKind::InvalidJson | InvalidKind::MissingId | InvalidKind::InvalidId => {
                String::new()
            }
        },
        outcome: ExpectedOutcome::Failure { code, message },
    }
}

fn append_valid_frame(
    wire: &mut Vec<u8>,
    id: &str,
    operation: &ValidOperation,
) -> ExpectedResponse {
    let (request, data) = match operation {
        ValidOperation::Ping => (json!({"id":id,"type":"ping"}), json!("pong")),
        ValidOperation::ListTools => (
            json!({"id":id,"type":"listTools"}),
            json!([{
                "name":"echo",
                "description":"property fixture",
                "input_schema":{"type":"object"}
            }]),
        ),
        ValidOperation::CallTool { tool_name, args } => (
            json!({"id":id,"type":"callTool","toolName":tool_name,"args":args}),
            json!({"tool":tool_name,"args":args}),
        ),
        ValidOperation::GetInstructions => (
            json!({"id":id,"type":"getInstructions"}),
            json!("property instructions"),
        ),
    };
    append_json_frame(wire, &request);
    ExpectedResponse {
        id: id.to_owned(),
        outcome: ExpectedOutcome::Success(data),
    }
}

fn assert_response_shape(value: &Value, expected: &ExpectedResponse) -> Result<(), TestCaseError> {
    let object = value
        .as_object()
        .ok_or_else(|| TestCaseError::fail("worker response was not an object"))?;
    prop_assert_eq!(object.get("id"), Some(&Value::String(expected.id.clone())));

    match &expected.outcome {
        ExpectedOutcome::Success(data) => {
            prop_assert_eq!(object.len(), 3);
            prop_assert_eq!(object.get("success"), Some(&Value::Bool(true)));
            prop_assert_eq!(object.get("data"), Some(data));
            prop_assert!(!object.contains_key("error"));
        }
        ExpectedOutcome::Failure { code, message } => {
            prop_assert_eq!(object.len(), 3);
            prop_assert_eq!(object.get("success"), Some(&Value::Bool(false)));
            prop_assert!(!object.contains_key("data"));
            let error = object
                .get("error")
                .and_then(Value::as_object)
                .ok_or_else(|| TestCaseError::fail("failure response lacked an error object"))?;
            prop_assert_eq!(error.len(), 2);
            prop_assert_eq!(error.get("code"), Some(&Value::String((*code).to_owned())));
            prop_assert_eq!(
                error.get("message"),
                Some(&Value::String((*message).to_owned()))
            );
        }
    }
    Ok(())
}

async fn run_scenario(
    generated: Vec<(String, ValidOperation)>,
    duplicate_id: String,
    secret_seed: String,
    order_seed: u8,
    invalid_argument_shape: u8,
) -> Result<(), TestCaseError> {
    let (mut client, worker_stream) = UnixStream::pair()
        .map_err(|error| TestCaseError::fail(format!("UnixStream pair failed: {error}")))?;
    let worker = tokio::spawn(serve_test_client(
        worker_stream,
        Box::new(PropertyConnection),
        context(),
    ));

    let mut wire = Vec::new();
    let mut expected = Vec::new();
    let mut secrets = Vec::new();
    let mut invalid = vec![
        InvalidKind::InvalidJson,
        InvalidKind::MissingId,
        InvalidKind::UnknownType,
        InvalidKind::InvalidArguments,
        InvalidKind::InvalidId,
    ];
    let rotation = usize::from(order_seed) % invalid.len();
    invalid.rotate_left(rotation);
    if order_seed & 1 == 1 {
        invalid.reverse();
    }

    for (index, kind) in invalid.into_iter().enumerate() {
        let id = format!("bad-{index}-{order_seed}");
        let secret = format!("payload-secret-{index}-{secret_seed}");
        expected.push(append_invalid_frame(
            &mut wire,
            kind,
            &id,
            &secret,
            invalid_argument_shape.wrapping_add(index as u8),
        ));
        secrets.push(secret);

        let recovery_id = format!("recover-{index}-{order_seed}");
        expected.push(append_valid_frame(
            &mut wire,
            &recovery_id,
            &ValidOperation::Ping,
        ));
    }

    for (id, operation) in &generated {
        expected.push(append_valid_frame(&mut wire, id, operation));
    }

    for (id, operation) in [
        ("all-ping", ValidOperation::Ping),
        ("all-list", ValidOperation::ListTools),
        (
            "all-call",
            ValidOperation::CallTool {
                tool_name: "echo".to_owned(),
                args: json!({"sequence":1})
                    .as_object()
                    .expect("literal object")
                    .clone(),
            },
        ),
        ("all-instructions", ValidOperation::GetInstructions),
    ] {
        expected.push(append_valid_frame(&mut wire, id, &operation));
    }

    expected.push(append_valid_frame(
        &mut wire,
        &duplicate_id,
        &ValidOperation::Ping,
    ));
    expected.push(append_valid_frame(
        &mut wire,
        &duplicate_id,
        &ValidOperation::Ping,
    ));

    append_json_frame(&mut wire, &json!({"id":"final-close","type":"close"}));
    expected.push(ExpectedResponse {
        id: "final-close".to_owned(),
        outcome: ExpectedOutcome::Success(json!("closing")),
    });

    client
        .write_all(&wire)
        .await
        .map_err(|error| TestCaseError::fail(format!("request write failed: {error}")))?;

    let mut reader = BufReader::new(client);
    let mut response_wire = Vec::new();
    for expected_response in &expected {
        let mut line = Vec::new();
        let read = timeout(IO_TIMEOUT, reader.read_until(b'\n', &mut line))
            .await
            .map_err(|_| TestCaseError::fail("timed out waiting for worker response"))?
            .map_err(|error| TestCaseError::fail(format!("response read failed: {error}")))?;
        prop_assert!(read > 0, "worker closed before all responses arrived");
        prop_assert_eq!(line.last(), Some(&b'\n'));
        response_wire.extend_from_slice(&line);
        let value: Value = serde_json::from_slice(&line)
            .map_err(|error| TestCaseError::fail(format!("invalid response JSON: {error}")))?;
        assert_response_shape(&value, expected_response)?;
    }

    let response_text = String::from_utf8(response_wire)
        .map_err(|error| TestCaseError::fail(format!("response was not UTF-8: {error}")))?;
    for secret in secrets {
        prop_assert!(
            !response_text.contains(&secret),
            "worker reflected an invalid request payload"
        );
    }

    timeout(IO_TIMEOUT, worker)
        .await
        .map_err(|_| TestCaseError::fail("worker did not stop after close"))?
        .map_err(|error| TestCaseError::fail(format!("worker task failed: {error}")))?;
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: CASES,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 15: IPC 关联与错误后可服务性
    // **Validates: Requirements 7.5, 7.7**
    #[test]
    fn property_15_ipc_correlation_and_recovery(
        generated in prop::collection::vec((request_id(), valid_operation()), 0..=6),
        duplicate_id in request_id(),
        secret_seed in safe_text("seed-"),
        order_seed in any::<u8>(),
        invalid_argument_shape in any::<u8>(),
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("property runtime");
        runtime.block_on(async {
            timeout(
                Duration::from_secs(5),
                run_scenario(
                    generated,
                    duplicate_id,
                    secret_seed,
                    order_seed,
                    invalid_argument_shape,
                ),
            )
            .await
            .map_err(|_| TestCaseError::fail("property scenario exceeded its total timeout"))?
        })?;
    }
}
