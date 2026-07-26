#![forbid(unsafe_code)]

#[path = "../support/mod.rs"]
mod support;

use std::{
    collections::BTreeMap,
    io::Cursor,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use mcp_cli::{
    BoxFuture, CALL_INPUT_MAX_SIZE, CallHandler, CallInput, CancellationFlag, CliError,
    CommandContext, CommandOutcome, ConfigHash, ConnectionError, ConnectionManager, ConnectionMode,
    Deadline, ErrorKind, ExitCode, GrepHandler, InfoHandler, JsonObject, ListHandler,
    McpConnection, ServerDefinition, ServerId, ToolFilterConfig, ToolInfo, TransportConfig,
};
use serde_json::{Value, json};
use support::{
    ConnectionCall, DiagnosticEvent, MockConnectionHandle, MockMcpConnection,
    RecordingDiagnosticSink,
};

enum AcquirePlan {
    Connection(Box<dyn McpConnection>),
    Failure(CliError),
}

#[derive(Default)]
struct NamedManager {
    plans: Mutex<BTreeMap<String, AcquirePlan>>,
    acquired: Mutex<Vec<String>>,
}

impl NamedManager {
    fn connection(&self, server: &str, connection: impl McpConnection + 'static) {
        self.plans.lock().expect("plans lock").insert(
            server.to_owned(),
            AcquirePlan::Connection(Box::new(connection)),
        );
    }

    fn failure(&self, server: &str, error: CliError) {
        self.plans
            .lock()
            .expect("plans lock")
            .insert(server.to_owned(), AcquirePlan::Failure(error));
    }

    fn acquired(&self) -> Vec<String> {
        self.acquired.lock().expect("acquired lock").clone()
    }
}

impl ConnectionManager for NamedManager {
    fn acquire<'a>(
        &'a self,
        _ctx: &'a CommandContext,
        server: &'a ServerDefinition,
    ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, CliError>> {
        self.acquired
            .lock()
            .expect("acquired lock")
            .push(server.name.clone());
        let plan = self
            .plans
            .lock()
            .expect("plans lock")
            .remove(&server.name)
            .expect("scripted acquisition");
        Box::pin(async move {
            match plan {
                AcquirePlan::Connection(connection) => Ok(connection),
                AcquirePlan::Failure(error) => Err(error),
            }
        })
    }
}

fn context() -> (CommandContext, Arc<RecordingDiagnosticSink>) {
    let diagnostics = Arc::new(RecordingDiagnosticSink::default());
    (
        CommandContext {
            deadline: Deadline::new(Instant::now() + Duration::from_secs(30)),
            cancellation: Arc::new(CancellationFlag::default()),
            diagnostics: diagnostics.clone(),
        },
        diagnostics,
    )
}

fn server(name: &str, filter: ToolFilterConfig) -> ServerDefinition {
    ServerDefinition {
        name: name.to_owned(),
        id: ServerId("0".repeat(64)),
        config_hash: ConfigHash([0; 32]),
        transport: TransportConfig::Stdio {
            command: format!("run-{name}"),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        },
        filter,
    }
}

fn tool(name: &str, description: Option<&str>) -> ToolInfo {
    ToolInfo {
        name: name.to_owned(),
        description: description.map(str::to_owned),
        input_schema: json!({
            "type": "object",
            "properties": {
                "value": {"type": "integer", "description": "input value"}
            }
        }),
    }
}

fn scripted_connection(
    instructions: Option<&str>,
    tools: Result<Vec<ToolInfo>, ConnectionError>,
    call: Option<Result<Value, ConnectionError>>,
    close: Result<(), ConnectionError>,
) -> (MockMcpConnection, MockConnectionHandle) {
    let (connection, handle) = MockMcpConnection::new(ConnectionMode::Direct);
    handle.queue_list_result(tools);
    if let Some(call) = call {
        handle.queue_call_result(call);
    }
    handle.queue_close_result(close);
    let connection = match instructions {
        Some(instructions) => connection.with_instructions(instructions),
        None => connection,
    };
    (connection, handle)
}

fn human(outcome: CommandOutcome) -> String {
    match outcome {
        CommandOutcome::HumanText(text) => text,
        other => panic!("expected human text, got {other:?}"),
    }
}

#[tokio::test]
async fn list_keeps_sorted_successes_across_connect_list_and_close_failures() {
    let manager = Arc::new(NamedManager::default());
    let (alpha, alpha_handle) = scripted_connection(
        None,
        Ok(vec![
            tool("zeta", Some("last")),
            tool("alpha", Some("first")),
        ]),
        None,
        Ok(()),
    );
    let (beta, beta_handle) = scripted_connection(
        None,
        Ok(vec![tool("middle", Some("middle description"))]),
        None,
        Ok(()),
    );
    let (list_failure, list_failure_handle) = scripted_connection(
        None,
        Err(ConnectionError::new("private list failure")),
        None,
        Ok(()),
    );
    let (close_failure, close_failure_handle) = scripted_connection(
        None,
        Ok(vec![tool("discarded", None)]),
        None,
        Err(ConnectionError::new("private close failure")),
    );
    manager.connection("zeta-server", beta);
    manager.connection("alpha-server", alpha);
    manager.failure(
        "connect-fail",
        CliError::network_error("connect-fail", "safe connect failure"),
    );
    manager.connection("list-fail", list_failure);
    manager.connection("close-fail", close_failure);
    let servers = [
        "zeta-server",
        "alpha-server",
        "connect-fail",
        "list-fail",
        "close-fail",
    ]
    .into_iter()
    .map(|name| (name.to_owned(), server(name, ToolFilterConfig::default())))
    .collect();
    let handler = ListHandler::new(manager, NonZeroUsize::new(5).unwrap());
    let (ctx, _) = context();

    let output = human(handler.execute(&ctx, &servers, true).await.unwrap());

    assert_eq!(
        output,
        "alpha-server\n  • alpha - first\n  • zeta - last\n\nclose-fail\n  <error: Failed to communicate with server \"close-fail\">\n\nconnect-fail\n  <error: Failed to communicate with server \"connect-fail\">\n\nlist-fail\n  <error: Failed to communicate with server \"list-fail\">\n\nzeta-server\n  • middle - middle description\n"
    );
    assert_eq!(
        alpha_handle.calls(),
        [ConnectionCall::ListTools, ConnectionCall::Close]
    );
    assert_eq!(
        beta_handle.calls(),
        [ConnectionCall::ListTools, ConnectionCall::Close]
    );
    assert_eq!(
        list_failure_handle.calls(),
        [ConnectionCall::ListTools, ConnectionCall::Close]
    );
    assert_eq!(
        close_failure_handle.calls(),
        [ConnectionCall::ListTools, ConnectionCall::Close]
    );
}

#[tokio::test]
async fn info_and_grep_apply_description_filter_pattern_case_glob_and_stable_sorting() {
    let manager = Arc::new(NamedManager::default());
    let (info_connection, _) = scripted_connection(
        Some("Use carefully"),
        Ok(vec![
            tool("zeta", Some("last")),
            tool("alpha", Some("first")),
            tool("hidden", Some("must not appear")),
        ]),
        None,
        Ok(()),
    );
    manager.connection("target", info_connection);
    let info = InfoHandler::new(manager);
    let configured = BTreeMap::from([(
        "target".to_owned(),
        server(
            "target",
            ToolFilterConfig {
                allowed_tools: vec!["*".to_owned()],
                disabled_tools: vec!["hidden".to_owned()],
            },
        ),
    )]);
    let (ctx, _) = context();
    let output = human(
        info.execute(&ctx, &configured, "target", None, true)
            .await
            .unwrap(),
    );
    assert!(output.contains("Instructions:\n  Use carefully"));
    assert!(output.find("  alpha\n").unwrap() < output.find("  zeta\n").unwrap());
    assert!(output.contains("    first"));
    assert!(!output.contains("hidden"));

    let manager = Arc::new(NamedManager::default());
    let (beta, _) = scripted_connection(
        None,
        Ok(vec![tool("GROUP/READ.X", Some("beta"))]),
        None,
        Ok(()),
    );
    let (alpha, _) = scripted_connection(
        None,
        Ok(vec![
            tool("z/read.2", Some("second")),
            tool("secret/read.0", Some("hidden")),
            tool("a/read.1", Some("first")),
        ]),
        None,
        Ok(()),
    );
    manager.connection("beta", beta);
    manager.connection("alpha", alpha);
    let configured = BTreeMap::from([
        (
            "alpha".to_owned(),
            server(
                "alpha",
                ToolFilterConfig {
                    allowed_tools: vec!["*".to_owned()],
                    disabled_tools: vec!["secret/*".to_owned()],
                },
            ),
        ),
        (
            "beta".to_owned(),
            server("beta", ToolFilterConfig::default()),
        ),
    ]);
    let grep = GrepHandler::new(manager, NonZeroUsize::new(2).unwrap());
    let output = human(
        grep.execute(&ctx, &configured, "**/READ.?", true)
            .await
            .unwrap(),
    );
    assert_eq!(
        output,
        "alpha a/read.1 - first\nalpha z/read.2 - second\nbeta GROUP/READ.X - beta\n"
    );

    let manager = Arc::new(NamedManager::default());
    let (none, _) = scripted_connection(None, Ok(vec![tool("write", None)]), None, Ok(()));
    manager.connection("only", none);
    let configured = BTreeMap::from([(
        "only".to_owned(),
        server("only", ToolFilterConfig::default()),
    )]);
    let output = human(
        GrepHandler::new(manager, NonZeroUsize::new(1).unwrap())
            .execute(&ctx, &configured, "read_*", false)
            .await
            .unwrap(),
    );
    assert_eq!(output, "No matching tools found.\n");
}

#[tokio::test]
async fn call_preflight_blocks_unknown_or_disabled_targets_and_unknown_tool_is_never_called() {
    let manager = Arc::new(NamedManager::default());
    let connections: Arc<dyn ConnectionManager> = manager.clone();
    let handler = CallHandler::new(connections);
    let configured = BTreeMap::from([(
        "target".to_owned(),
        server(
            "target",
            ToolFilterConfig {
                allowed_tools: vec!["safe*".to_owned()],
                disabled_tools: vec!["safe-secret".to_owned()],
            },
        ),
    )]);
    let (ctx, _) = context();

    for (server_name, tool_name, expected) in [
        ("missing", "safe", ErrorKind::ServerNotFound),
        ("target", "unsafe", ErrorKind::ToolDisabled),
        ("target", "safe-secret", ErrorKind::ToolDisabled),
    ] {
        let error = handler
            .execute(
                &ctx,
                &configured,
                server_name,
                tool_name,
                Some("{}"),
                &mut CallInput::new(Cursor::new(Vec::<u8>::new()), false),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, expected);
    }
    assert!(manager.acquired().is_empty());

    let manager = Arc::new(NamedManager::default());
    let (connection, handle) = scripted_connection(
        None,
        Ok(vec![tool("safe-alpha", None), tool("safe-zeta", None)]),
        None,
        Ok(()),
    );
    manager.connection("target", connection);
    let connections: Arc<dyn ConnectionManager> = manager.clone();
    let error = CallHandler::new(connections)
        .execute(
            &ctx,
            &configured,
            "target",
            "safe-missing",
            Some("{}"),
            &mut CallInput::new(Cursor::new(Vec::<u8>::new()), false),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::ToolNotFound);
    assert_eq!(manager.acquired(), ["target"]);
    assert_eq!(
        handle.calls(),
        [ConnectionCall::ListTools, ConnectionCall::Close]
    );
}

#[tokio::test]
async fn call_preserves_inline_priority_complete_results_and_stable_failure_kinds() {
    let complete = json!({
        "content": [{"type": "text", "text": "ok"}],
        "isError": false,
        "structuredContent": {"nested": [1, null, true]},
        "x-extension": {"preserved": true}
    });
    let manager = Arc::new(NamedManager::default());
    let (connection, handle) = scripted_connection(
        None,
        Ok(vec![tool("echo", None)]),
        Some(Ok(complete.clone())),
        Ok(()),
    );
    manager.connection("target", connection);
    let connections: Arc<dyn ConnectionManager> = manager;
    let (ctx, _) = context();
    let outcome = CallHandler::new(connections)
        .execute(
            &ctx,
            &BTreeMap::from([(
                "target".to_owned(),
                server("target", ToolFilterConfig::default()),
            )]),
            "target",
            "echo",
            Some(r#"{"source":"inline"}"#),
            &mut CallInput::new(Cursor::new(br#"{"source":"stdin"}"#.to_vec()), false),
        )
        .await
        .unwrap();
    assert_eq!(outcome, CommandOutcome::Json(complete));
    assert_eq!(
        handle.calls(),
        [
            ConnectionCall::ListTools,
            ConnectionCall::CallTool {
                name: "echo".to_owned(),
                args: json!({"source": "inline"}).as_object().unwrap().clone(),
            },
            ConnectionCall::Close,
        ]
    );

    let scenarios = [
        (
            Ok(json!({"content": [], "isError": true})),
            ErrorKind::ToolExecutionFailed,
            ExitCode::Tool,
            None,
        ),
        (
            Err(ConnectionError::new("network private")),
            ErrorKind::NetworkError,
            ExitCode::Network,
            None,
        ),
        (
            Err(ConnectionError::timed_out("timeout private")),
            ErrorKind::Timeout,
            ExitCode::Network,
            None,
        ),
        (
            Err(ConnectionError::new("auth private").with_http_status(401)),
            ErrorKind::AuthError,
            ExitCode::Auth,
            Some("401"),
        ),
    ];
    for (call_result, kind, exit, detail) in scenarios {
        let manager = Arc::new(NamedManager::default());
        let (connection, handle) = scripted_connection(
            None,
            Ok(vec![tool("echo", None)]),
            Some(call_result),
            Ok(()),
        );
        manager.connection("target", connection);
        let connections: Arc<dyn ConnectionManager> = manager;
        let error = CallHandler::new(connections)
            .execute(
                &ctx,
                &BTreeMap::from([(
                    "target".to_owned(),
                    server("target", ToolFilterConfig::default()),
                )]),
                "target",
                "echo",
                Some("{}"),
                &mut CallInput::new(Cursor::new(Vec::<u8>::new()), false),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, kind);
        assert_eq!(error.exit_code, exit);
        if let Some(detail) = detail {
            assert!(error.details.as_deref().unwrap().contains(detail));
        }
        assert_eq!(
            handle
                .calls()
                .iter()
                .filter(|call| matches!(call, ConnectionCall::CallTool { .. }))
                .count(),
            1
        );
    }
}

fn object_with_exact_size(size: usize) -> String {
    let prefix = r#"{"data":""#;
    let suffix = r#""}"#;
    assert!(size >= prefix.len() + suffix.len());
    let mut value = String::with_capacity(size);
    value.push_str(prefix);
    value.extend(std::iter::repeat_n('x', size - prefix.len() - suffix.len()));
    value.push_str(suffix);
    assert_eq!(value.len(), size);
    value
}

#[test]
fn call_input_covers_tty_whitespace_size_boundary_and_json_shape_diagnostics() {
    let mut tty = CallInput::new(Cursor::new(br#"{"ignored":true}"#.to_vec()), true);
    assert_eq!(tty.read(None).unwrap(), JsonObject::new());
    assert_eq!(tty.into_inner().position(), 0);

    for whitespace in ["", " \t\r\n", "\u{2003}\n"] {
        let mut input = CallInput::new(Cursor::new(whitespace.as_bytes()), false);
        assert_eq!(input.read(None).unwrap(), JsonObject::new());
    }

    let exact = object_with_exact_size(CALL_INPUT_MAX_SIZE);
    let object = CallInput::new(Cursor::new(Vec::<u8>::new()), false)
        .read(Some(&exact))
        .expect("exact 16 MiB object is accepted");
    assert_eq!(
        object["data"].as_str().unwrap().len(),
        CALL_INPUT_MAX_SIZE - r#"{"data":""#.len() - r#""}"#.len()
    );

    let oversized = format!("{exact} ");
    let error = CallInput::new(Cursor::new(Vec::<u8>::new()), false)
        .read(Some(&oversized))
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::InputTooLarge);

    let error = CallInput::new(Cursor::new(b"{\n  \"value\": ]".as_slice()), false)
        .read(None)
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidJson);
    let details = error.details.as_deref().unwrap();
    assert!(details.contains("line 2"), "{details}");
    assert!(details.contains("column"), "{details}");

    for value in ["null", "true", "1", r#""text""#, "[]"] {
        let error = CallInput::new(Cursor::new(value.as_bytes()), false)
            .read(None)
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidArguments, "{value}");
    }
}

#[tokio::test]
async fn grep_failure_diagnostics_remain_out_of_business_output() {
    let manager = Arc::new(NamedManager::default());
    manager.failure(
        "broken",
        CliError::network_error("broken", "Authorization: private"),
    );
    let handler = GrepHandler::new(manager, NonZeroUsize::new(1).unwrap());
    let (ctx, diagnostics) = context();
    let output = human(
        handler
            .execute(
                &ctx,
                &BTreeMap::from([(
                    "broken".to_owned(),
                    server("broken", ToolFilterConfig::default()),
                )]),
                "*",
                false,
            )
            .await
            .unwrap(),
    );
    assert_eq!(output, "No matching tools found.\n");
    assert_eq!(diagnostics.events().len(), 1);
    match &diagnostics.events()[0] {
        DiagnosticEvent::Warning(message) => {
            assert!(message.contains("NETWORK_ERROR"));
            assert!(!message.contains("Authorization"));
            assert!(!message.contains("private"));
        }
        other => panic!("expected warning, got {other:?}"),
    }
}
