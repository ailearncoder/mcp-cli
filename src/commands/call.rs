//! Tool invocation input handling and single-target execution.

use std::{
    collections::BTreeMap,
    io::{self, Read},
    sync::Arc,
};

use serde_json::Value;

use crate::{
    config::ServerDefinition,
    connection::{
        ConnectionError, ConnectionManager, ConnectionResourceRegistry, DirectConnectionManager,
        DirectConnector,
    },
    domain::{CommandOutcome, JsonObject, ToolResult},
    error::{CliError, ErrorKind},
    policy::{retry::ErrorClass, tool_filter::ToolFilter},
    runtime::{CommandContext, RuntimeConfig},
};

/// Maximum UTF-8 encoded size accepted for inline or stdin call arguments.
pub const CALL_INPUT_MAX_SIZE: usize = 16 * 1024 * 1024;
const CALL_INPUT_PROBE_SIZE: usize = CALL_INPUT_MAX_SIZE + 1;
const READ_CHUNK_SIZE: usize = 8 * 1024;

/// Injectable stdin and TTY boundary for resolving one call's arguments.
///
/// The reader is never touched when inline JSON is present or when `is_tty`
/// is true. Non-TTY input is consumed incrementally, with a hard probe limit
/// of 16 MiB + 1 byte, so malformed or hostile streams cannot cause unbounded
/// buffering before a connection is acquired.
pub struct CallInput<R> {
    stdin: R,
    is_tty: bool,
}

impl<R> CallInput<R> {
    pub const fn new(stdin: R, is_tty: bool) -> Self {
        Self { stdin, is_tty }
    }

    pub fn into_inner(self) -> R {
        self.stdin
    }
}

impl<R: Read> CallInput<R> {
    /// Resolves inline JSON or bounded stdin into the only accepted call shape.
    ///
    /// Inline input has unconditional priority. Without inline input, TTY stdin
    /// resolves to an empty object without any read. Empty EOF and Unicode
    /// whitespace-only input also resolve to an empty object.
    pub fn read(&mut self, inline_json: Option<&str>) -> Result<JsonObject, CliError> {
        if let Some(inline_json) = inline_json {
            return parse_call_input(inline_json.as_bytes());
        }
        if self.is_tty {
            return Ok(JsonObject::new());
        }

        let bytes = read_bounded(&mut self.stdin)?;
        parse_call_input(&bytes)
    }
}

/// Executes an authorized tool call through one injected connection manager.
pub struct CallHandler {
    connections: Arc<dyn ConnectionManager>,
}

impl CallHandler {
    pub fn new(connections: Arc<dyn ConnectionManager>) -> Self {
        Self { connections }
    }

    /// Builds a direct-only handler whose manager enforces one owned server
    /// connection and supplies the shared retry/deadline implementation.
    pub fn direct(
        connector: Arc<dyn DirectConnector>,
        resources: ConnectionResourceRegistry,
        runtime: &RuntimeConfig,
    ) -> Self {
        Self::new(Arc::new(DirectConnectionManager::with_runtime_config(
            connector, resources, runtime,
        )))
    }

    /// Validates input, server lookup, and authorization before connecting.
    ///
    /// An allowed tool is confirmed against the target server's advertised
    /// tools so a missing tool can return deterministic authorized candidates.
    /// The handler invokes `call_tool` exactly once; retry attempts remain
    /// exclusively owned by the connection layer. Every acquired connection is
    /// explicitly closed, while close failures are diagnostic-only and never
    /// replace a successful result or the primary typed error.
    pub async fn execute<R: Read>(
        &self,
        ctx: &CommandContext,
        servers: &BTreeMap<String, ServerDefinition>,
        server_name: &str,
        tool_name: &str,
        inline_json: Option<&str>,
        input: &mut CallInput<R>,
    ) -> Result<CommandOutcome, CliError> {
        // Input has deliberately higher precedence than configuration lookup so
        // every malformed/oversized request is rejected before any connection.
        let arguments = input.read(inline_json)?;
        let server = servers.get(server_name).ok_or_else(|| {
            CliError::server_not_found(server_name, servers.keys().map(String::as_str))
        })?;
        let filter = ToolFilter::new(&server.filter);
        if !filter.is_allowed(tool_name) {
            return Err(CliError::tool_disabled(server_name, tool_name));
        }

        let connection = self.connections.acquire(ctx, server).await?;
        let tools = match connection.list_tools(ctx).await {
            Ok(tools) => tools,
            Err(error) => {
                let primary = connection_error(server, None, "listing tools before call", error);
                close_without_masking(
                    ctx,
                    connection,
                    "connection close also failed after call tool discovery failure",
                )
                .await;
                return Err(primary);
            }
        };

        if !tools.iter().any(|tool| tool.name == tool_name) {
            let candidates = filter.filter(tools);
            let primary = CliError::tool_not_found(
                server_name,
                tool_name,
                candidates.iter().map(|tool| tool.name.as_str()),
            );
            close_without_masking(
                ctx,
                connection,
                "connection close failed after call tool lookup",
            )
            .await;
            return Err(primary);
        }

        // One handler invocation means one call into McpConnection. The direct
        // wrapper may retry transient failures, once per RetryExecutor attempt.
        let outcome = match connection.call_tool(ctx, tool_name, arguments).await {
            Ok(result) if tool_result_is_error(&result) => Err(CliError::tool_execution_failed(
                server_name,
                tool_name,
                "The MCP tool result reported isError=true",
            )),
            Ok(result) => Ok(CommandOutcome::Json(result)),
            Err(error) => Err(connection_error(
                server,
                Some(tool_name),
                "calling tool",
                error,
            )),
        };

        close_without_masking(
            ctx,
            connection,
            "connection close failed after call command",
        )
        .await;
        outcome
    }
}

/// Convenience entry point for callers that keep ownership of stdin.
pub fn read_call_input<R: Read>(
    inline_json: Option<&str>,
    stdin: &mut R,
    stdin_is_tty: bool,
) -> Result<JsonObject, CliError> {
    CallInput::new(stdin, stdin_is_tty).read(inline_json)
}

fn read_bounded(reader: &mut impl Read) -> Result<Vec<u8>, CliError> {
    // Reserving exactly the complete probe prevents growth while retaining a
    // fixed, documented upper bound. Only bytes actually read are initialized.
    let mut bytes = Vec::with_capacity(CALL_INPUT_PROBE_SIZE);
    let mut chunk = [0_u8; READ_CHUNK_SIZE];

    while bytes.len() < CALL_INPUT_PROBE_SIZE {
        let remaining = CALL_INPUT_PROBE_SIZE - bytes.len();
        let requested = remaining.min(chunk.len());
        match reader.read(&mut chunk[..requested]) {
            Ok(0) => break,
            Ok(read) => bytes.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(stdin_read_error(error)),
        }
    }

    if bytes.len() > CALL_INPUT_MAX_SIZE {
        return Err(input_too_large_from_probe());
    }
    Ok(bytes)
}

fn parse_call_input(bytes: &[u8]) -> Result<JsonObject, CliError> {
    if bytes.len() > CALL_INPUT_MAX_SIZE {
        return Err(CliError::input_too_large(bytes.len(), CALL_INPUT_MAX_SIZE));
    }

    let text = std::str::from_utf8(bytes).map_err(|error| invalid_utf8(bytes, error))?;
    if text.trim().is_empty() {
        return Ok(JsonObject::new());
    }

    let value = serde_json::from_str::<Value>(text).map_err(invalid_json_syntax)?;
    match value {
        Value::Object(object) => Ok(object),
        other => Err(non_object_input(&other)),
    }
}

fn invalid_utf8(bytes: &[u8], error: std::str::Utf8Error) -> CliError {
    let byte_offset = error.valid_up_to();
    // `valid_up_to` always ends on a UTF-8 boundary. Count Unicode scalar
    // values since the preceding newline so line/column remain meaningful even
    // when valid multibyte text appears before the malformed byte.
    let valid_prefix = std::str::from_utf8(&bytes[..byte_offset])
        .expect("Utf8Error::valid_up_to ends on a valid UTF-8 boundary");
    let line = valid_prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let line_start = valid_prefix.rfind('\n').map_or(0, |index| index + 1);
    let column = valid_prefix[line_start..].chars().count() + 1;
    CliError::invalid_json(format!(
        "Invalid UTF-8 at line {line}, column {column}, byte offset {byte_offset}"
    ))
    .with_source(error)
}

fn invalid_json_syntax(error: serde_json::Error) -> CliError {
    let details = format!(
        "JSON parser position: line {}, column {}",
        error.line(),
        error.column()
    );
    CliError::invalid_json(details).with_source(error)
}

fn non_object_input(value: &Value) -> CliError {
    let found = match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    CliError::invalid_arguments(
        "Tool arguments must be a JSON object",
        format!("Top-level JSON value is {found}; expected object"),
    )
    .with_suggestion("Wrap tool arguments in a JSON object, for example: {}")
}

fn stdin_read_error(source: io::Error) -> CliError {
    CliError::from_kind(
        ErrorKind::InvalidArguments,
        "Could not read tool arguments from stdin",
    )
    .with_details("An I/O error occurred while reading bounded call input")
    .with_suggestion("Retry with readable stdin or provide inline JSON")
    .with_source(source)
}

fn input_too_large_from_probe() -> CliError {
    CliError::from_kind(
        ErrorKind::InputTooLarge,
        "Tool input exceeds the maximum size",
    )
    .with_details(format!(
        "Observed at least {CALL_INPUT_PROBE_SIZE} bytes; maximum is {CALL_INPUT_MAX_SIZE} bytes"
    ))
    .with_suggestion("Reduce the JSON input size and retry")
}

fn tool_result_is_error(result: &ToolResult) -> bool {
    result
        .as_object()
        .and_then(|object| object.get("isError"))
        .and_then(Value::as_bool)
        == Some(true)
}

fn connection_error(
    server: &ServerDefinition,
    tool_name: Option<&str>,
    operation: &str,
    error: ConnectionError,
) -> CliError {
    let cli_error = if error.is_timeout() {
        CliError::timeout(operation)
    } else if error.is_cancelled() {
        CliError::cancelled(&server.name, operation)
    } else if let Some(status) = error.http_status() {
        CliError::http_status(&server.name, status)
    } else if error.error_class() == ErrorClass::Business {
        match tool_name {
            Some(tool_name) => CliError::tool_execution_failed(
                &server.name,
                tool_name,
                "The MCP server reported a tool business execution failure",
            ),
            None => CliError::network_error_classified(
                &server.name,
                format!("Failed while {operation}"),
                ErrorClass::NonTransient,
            ),
        }
    } else if error.error_class() == ErrorClass::Auth {
        CliError::from_kind(
            ErrorKind::AuthError,
            "Authentication or authorization failed for target server",
        )
        .with_details(format!("Authentication failed while {operation}"))
        .with_suggestion(
            "Check the Authorization header, credentials, and access permissions in config",
        )
    } else {
        CliError::network_error_classified(
            &server.name,
            format!("Failed while {operation}"),
            error.error_class(),
        )
    };
    cli_error.with_source(error)
}

async fn close_without_masking(
    ctx: &CommandContext,
    connection: Box<dyn crate::connection::McpConnection>,
    diagnostic: &str,
) {
    if connection.close(ctx).await.is_err() {
        // Adapter text is intentionally omitted because transport close errors
        // may contain credentials. The primary command outcome remains intact.
        ctx.diagnostics.debug(diagnostic);
    }
}

#[cfg(test)]
mod handler_tests {
    use std::{
        io::Cursor,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use serde_json::json;

    use super::*;
    use crate::{
        BoxFuture, CancellationFlag, ConfigHash, ConnectionMode, Deadline, DiagnosticSink,
        ExitCode, McpConnection, ServerId, ToolFilterConfig, ToolInfo, TransportConfig,
    };

    #[derive(Default)]
    struct RecordingDiagnostics {
        warnings: Mutex<Vec<String>>,
        debug: Mutex<Vec<String>>,
        server_stderr: Mutex<Vec<Vec<u8>>>,
    }

    impl DiagnosticSink for RecordingDiagnostics {
        fn warning(&self, message: &str) {
            self.warnings
                .lock()
                .expect("warning lock")
                .push(message.into());
        }

        fn debug(&self, message: &str) {
            self.debug.lock().expect("debug lock").push(message.into());
        }

        fn server_stderr(&self, _server: &str, bytes: &[u8]) {
            self.server_stderr
                .lock()
                .expect("stderr lock")
                .push(bytes.to_vec());
        }
    }

    fn context(diagnostics: Arc<RecordingDiagnostics>) -> CommandContext {
        CommandContext {
            deadline: Deadline::new(Instant::now() + Duration::from_secs(30)),
            cancellation: Arc::new(CancellationFlag::default()),
            diagnostics,
        }
    }

    fn server(name: &str, filter: ToolFilterConfig) -> ServerDefinition {
        ServerDefinition {
            name: name.into(),
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

    fn tool(name: &str) -> ToolInfo {
        ToolInfo {
            name: name.into(),
            description: None,
            input_schema: json!({"type": "object"}),
        }
    }

    #[derive(Default)]
    struct Trace {
        acquired: Mutex<Vec<String>>,
        listed: AtomicUsize,
        called: AtomicUsize,
        closed: AtomicUsize,
        calls: Mutex<Vec<(String, JsonObject)>>,
    }

    struct FakeConnection {
        trace: Arc<Trace>,
        tools: Mutex<Option<Result<Vec<ToolInfo>, ConnectionError>>>,
        call: Mutex<Option<Result<ToolResult, ConnectionError>>>,
        close: Mutex<Option<Result<(), ConnectionError>>>,
        emit_diagnostics: bool,
    }

    impl McpConnection for FakeConnection {
        fn list_tools<'a>(
            &'a self,
            _ctx: &'a CommandContext,
        ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
            self.trace.listed.fetch_add(1, Ordering::SeqCst);
            let result = self
                .tools
                .lock()
                .expect("tools lock")
                .take()
                .expect("one tools result");
            Box::pin(async move { result })
        }

        fn call_tool<'a>(
            &'a self,
            ctx: &'a CommandContext,
            name: &'a str,
            args: JsonObject,
        ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
            self.trace.called.fetch_add(1, Ordering::SeqCst);
            self.trace
                .calls
                .lock()
                .expect("calls lock")
                .push((name.to_owned(), args));
            if self.emit_diagnostics {
                ctx.diagnostics.warning("transport warning");
                ctx.diagnostics
                    .server_stderr("target", b"server diagnostic only");
            }
            let result = self
                .call
                .lock()
                .expect("call lock")
                .take()
                .expect("one call result");
            Box::pin(async move { result })
        }

        fn instructions(&self) -> Option<&str> {
            None
        }

        fn close<'a>(
            self: Box<Self>,
            _ctx: &'a CommandContext,
        ) -> BoxFuture<'a, Result<(), ConnectionError>> {
            self.trace.closed.fetch_add(1, Ordering::SeqCst);
            let result = self
                .close
                .lock()
                .expect("close lock")
                .take()
                .expect("one close result");
            Box::pin(async move { result })
        }

        fn mode(&self) -> ConnectionMode {
            ConnectionMode::Direct
        }
    }

    #[derive(Default)]
    struct FakeManager {
        trace: Arc<Trace>,
        connection: Mutex<Option<FakeConnection>>,
    }

    impl FakeManager {
        fn scripted(
            tools: Result<Vec<ToolInfo>, ConnectionError>,
            call: Result<ToolResult, ConnectionError>,
            close: Result<(), ConnectionError>,
            emit_diagnostics: bool,
        ) -> Arc<Self> {
            let trace = Arc::new(Trace::default());
            Arc::new(Self {
                trace: Arc::clone(&trace),
                connection: Mutex::new(Some(FakeConnection {
                    trace,
                    tools: Mutex::new(Some(tools)),
                    call: Mutex::new(Some(call)),
                    close: Mutex::new(Some(close)),
                    emit_diagnostics,
                })),
            })
        }
    }

    impl ConnectionManager for FakeManager {
        fn acquire<'a>(
            &'a self,
            _ctx: &'a CommandContext,
            server: &'a ServerDefinition,
        ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, CliError>> {
            self.trace
                .acquired
                .lock()
                .expect("acquired lock")
                .push(server.name.clone());
            let connection = self
                .connection
                .lock()
                .expect("connection lock")
                .take()
                .expect("call handler may acquire only one connection");
            Box::pin(async move { Ok(Box::new(connection) as Box<dyn McpConnection>) })
        }
    }

    fn servers(filter: ToolFilterConfig) -> BTreeMap<String, ServerDefinition> {
        BTreeMap::from([
            ("other".into(), server("other", ToolFilterConfig::default())),
            ("target".into(), server("target", filter)),
        ])
    }

    fn handler(manager: &Arc<FakeManager>) -> CallHandler {
        let connections: Arc<dyn ConnectionManager> = manager.clone();
        CallHandler::new(connections)
    }

    #[tokio::test]
    async fn input_server_and_filter_failures_happen_before_connection() {
        let manager = Arc::new(FakeManager::default());
        let handler = handler(&manager);
        let diagnostics = Arc::new(RecordingDiagnostics::default());
        let ctx = context(diagnostics);
        let configured = servers(ToolFilterConfig {
            allowed_tools: vec!["safe*".into()],
            disabled_tools: vec!["safe-secret".into()],
        });

        let cases = [
            ("target", "safe", Some("{"), ErrorKind::InvalidJson),
            ("target", "safe", Some("[]"), ErrorKind::InvalidArguments),
            ("missing", "safe", Some("{}"), ErrorKind::ServerNotFound),
            ("target", "unsafe", Some("{}"), ErrorKind::ToolDisabled),
            ("target", "safe-secret", Some("{}"), ErrorKind::ToolDisabled),
        ];
        for (server, tool, inline, expected) in cases {
            let mut input = CallInput::new(Cursor::new(Vec::<u8>::new()), false);
            let error = handler
                .execute(&ctx, &configured, server, tool, inline, &mut input)
                .await
                .unwrap_err();
            assert_eq!(error.kind, expected);
        }

        let oversized = " ".repeat(CALL_INPUT_PROBE_SIZE);
        let mut input = CallInput::new(Cursor::new(Vec::<u8>::new()), false);
        let error = handler
            .execute(
                &ctx,
                &configured,
                "target",
                "safe",
                Some(&oversized),
                &mut input,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::InputTooLarge);
        assert!(
            manager
                .trace
                .acquired
                .lock()
                .expect("acquired lock")
                .is_empty()
        );
        assert_eq!(manager.trace.listed.load(Ordering::SeqCst), 0);
        assert_eq!(manager.trace.called.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unknown_tool_lists_authorized_candidates_and_closes_without_calling() {
        let manager = FakeManager::scripted(
            Ok(vec![tool("zeta"), tool("hidden"), tool("alpha")]),
            Ok(json!({"unused": true})),
            Ok(()),
            false,
        );
        let handler = handler(&manager);
        let ctx = context(Arc::new(RecordingDiagnostics::default()));
        let configured = servers(ToolFilterConfig {
            allowed_tools: vec!["*".into()],
            disabled_tools: vec!["hidden".into()],
        });
        let mut input = CallInput::new(Cursor::new(Vec::<u8>::new()), true);

        let error = handler
            .execute(&ctx, &configured, "target", "missing", None, &mut input)
            .await
            .unwrap_err();

        assert_eq!(error.kind, ErrorKind::ToolNotFound);
        assert_eq!(
            error.details.as_deref(),
            Some("Available tools: alpha, zeta")
        );
        assert_eq!(
            manager
                .trace
                .acquired
                .lock()
                .expect("acquired lock")
                .as_slice(),
            ["target"]
        );
        assert_eq!(manager.trace.listed.load(Ordering::SeqCst), 1);
        assert_eq!(manager.trace.called.load(Ordering::SeqCst), 0);
        assert_eq!(manager.trace.closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn successful_call_preserves_arguments_complete_result_and_diagnostics_isolation() {
        let complete = json!({
            "content": [{"type": "text", "text": "ok"}],
            "isError": false,
            "structuredContent": {"nested": [1, null, true]},
            "x-extension": {"kept": true}
        });
        let manager =
            FakeManager::scripted(Ok(vec![tool("echo")]), Ok(complete.clone()), Ok(()), true);
        let handler = handler(&manager);
        let diagnostics = Arc::new(RecordingDiagnostics::default());
        let ctx = context(Arc::clone(&diagnostics));
        let configured = servers(ToolFilterConfig::default());
        let mut input = CallInput::new(Cursor::new(br#"{"source":"stdin"}"#.to_vec()), false);

        let outcome = handler
            .execute(
                &ctx,
                &configured,
                "target",
                "echo",
                Some(r#"{"source":"inline","nested":{"雪":true}}"#),
                &mut input,
            )
            .await
            .unwrap();

        assert_eq!(outcome, CommandOutcome::Json(complete.clone()));
        let calls = manager.trace.calls.lock().expect("calls lock");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "echo");
        assert_eq!(
            calls[0].1,
            json!({"source": "inline", "nested": {"雪": true}})
                .as_object()
                .unwrap()
                .clone()
        );
        drop(calls);
        assert_eq!(
            manager
                .trace
                .acquired
                .lock()
                .expect("acquired lock")
                .as_slice(),
            ["target"]
        );
        assert_eq!(manager.trace.called.load(Ordering::SeqCst), 1);
        assert_eq!(manager.trace.closed.load(Ordering::SeqCst), 1);
        let stdout = crate::output::format_tool_result(&complete).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&stdout).unwrap(), complete);
        assert!(!String::from_utf8_lossy(&stdout).contains("transport warning"));
        assert_eq!(
            diagnostics
                .warnings
                .lock()
                .expect("warning lock")
                .as_slice(),
            ["transport warning"]
        );
        assert_eq!(
            diagnostics
                .server_stderr
                .lock()
                .expect("stderr lock")
                .as_slice(),
            [b"server diagnostic only".to_vec()]
        );
    }

    #[tokio::test]
    async fn business_and_typed_transport_failures_map_without_extra_calls() {
        let scenarios = vec![
            (
                Ok(json!({"content": [{"type": "text", "text": "failed"}], "isError": true})),
                ErrorKind::ToolExecutionFailed,
                ExitCode::Tool,
            ),
            (
                Err(ConnectionError::new("business secret").with_class(ErrorClass::Business)),
                ErrorKind::ToolExecutionFailed,
                ExitCode::Tool,
            ),
            (
                Err(ConnectionError::new("network secret")),
                ErrorKind::NetworkError,
                ExitCode::Network,
            ),
            (
                Err(ConnectionError::timed_out("timeout secret")),
                ErrorKind::Timeout,
                ExitCode::Network,
            ),
            (
                Err(ConnectionError::new("auth secret").with_http_status(403)),
                ErrorKind::AuthError,
                ExitCode::Auth,
            ),
        ];

        for (call_result, expected_kind, expected_exit) in scenarios {
            let manager = FakeManager::scripted(Ok(vec![tool("echo")]), call_result, Ok(()), false);
            let handler = handler(&manager);
            let ctx = context(Arc::new(RecordingDiagnostics::default()));
            let mut input = CallInput::new(Cursor::new(Vec::<u8>::new()), true);
            let error = handler
                .execute(
                    &ctx,
                    &servers(ToolFilterConfig::default()),
                    "target",
                    "echo",
                    None,
                    &mut input,
                )
                .await
                .unwrap_err();

            assert_eq!(error.kind, expected_kind);
            assert_eq!(error.exit_code, expected_exit);
            assert_eq!(manager.trace.called.load(Ordering::SeqCst), 1);
            assert_eq!(manager.trace.closed.load(Ordering::SeqCst), 1);
            assert!(!format!("{error:?}").contains("secret"));
        }
    }

    #[tokio::test]
    async fn list_and_close_failures_preserve_the_primary_outcome() {
        let diagnostics = Arc::new(RecordingDiagnostics::default());
        let manager = FakeManager::scripted(
            Err(ConnectionError::timed_out("list secret")),
            Ok(json!({"unused": true})),
            Err(ConnectionError::new("close secret")),
            false,
        );
        let call_handler = handler(&manager);
        let ctx = context(Arc::clone(&diagnostics));
        let mut input = CallInput::new(Cursor::new(Vec::<u8>::new()), true);
        let error = call_handler
            .execute(
                &ctx,
                &servers(ToolFilterConfig::default()),
                "target",
                "echo",
                None,
                &mut input,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Timeout);
        assert_eq!(manager.trace.called.load(Ordering::SeqCst), 0);
        assert_eq!(manager.trace.closed.load(Ordering::SeqCst), 1);
        let debug = diagnostics.debug.lock().expect("debug lock").join("\n");
        assert!(debug.contains("close also failed"));
        assert!(!debug.contains("close secret"));

        let diagnostics = Arc::new(RecordingDiagnostics::default());
        let manager = FakeManager::scripted(
            Ok(vec![tool("echo")]),
            Ok(json!({"ok": true})),
            Err(ConnectionError::new("close secret")),
            false,
        );
        let outcome = handler(&manager)
            .execute(
                &context(Arc::clone(&diagnostics)),
                &servers(ToolFilterConfig::default()),
                "target",
                "echo",
                Some("{}"),
                &mut CallInput::new(Cursor::new(Vec::<u8>::new()), false),
            )
            .await
            .expect("close error must not mask success");
        assert_eq!(outcome, CommandOutcome::Json(json!({"ok": true})));
        assert_eq!(manager.trace.closed.load(Ordering::SeqCst), 1);
        let debug = diagnostics.debug.lock().expect("debug lock").join("\n");
        assert!(debug.contains("close failed"));
        assert!(!debug.contains("close secret"));
    }

    #[tokio::test]
    async fn normalized_inline_and_stdin_commands_share_the_dispatch_seam() {
        let scenarios = [
            (
                vec!["call", "target/echo", r#"{"source":"inline"}"#],
                br#"{"source":"ignored-stdin"}"#.to_vec(),
                json!({"source": "inline"}),
            ),
            (
                vec!["call", "target", "echo"],
                br#"{"source":"stdin"}"#.to_vec(),
                json!({"source": "stdin"}),
            ),
        ];

        for (args, stdin, expected_args) in scenarios {
            let invocation = crate::cli::parse_cli(args.into_iter().map(Into::into)).unwrap();
            let manager = FakeManager::scripted(
                Ok(vec![tool("echo")]),
                Ok(json!({"ok": true})),
                Ok(()),
                false,
            );
            let handler = handler(&manager);
            let mut input = CallInput::new(Cursor::new(stdin), false);
            let outcome = crate::commands::dispatch_call_command(
                &handler,
                &context(Arc::new(RecordingDiagnostics::default())),
                &servers(ToolFilterConfig::default()),
                &invocation.command,
                &mut input,
            )
            .await
            .expect("call command is claimed")
            .unwrap();

            assert_eq!(outcome, CommandOutcome::Json(json!({"ok": true})));
            let calls = manager.trace.calls.lock().expect("calls lock");
            assert_eq!(calls.len(), 1);
            assert_eq!(Value::Object(calls[0].1.clone()), expected_args);
            assert_eq!(manager.trace.closed.load(Ordering::SeqCst), 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        io::{Cursor, Read},
    };

    use serde_json::json;

    use super::*;
    use crate::error::ErrorKind;

    #[derive(Default)]
    struct ReadProbe {
        reads: usize,
        bytes: Cursor<Vec<u8>>,
    }

    impl ReadProbe {
        fn with_bytes(bytes: impl Into<Vec<u8>>) -> Self {
            Self {
                reads: 0,
                bytes: Cursor::new(bytes.into()),
            }
        }
    }

    impl Read for ReadProbe {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            self.bytes.read(buffer)
        }
    }

    struct ChunkedReader {
        bytes: Cursor<Vec<u8>>,
        chunk_size: usize,
        reads: usize,
    }

    impl ChunkedReader {
        fn new(bytes: Vec<u8>, chunk_size: usize) -> Self {
            Self {
                bytes: Cursor::new(bytes),
                chunk_size,
                reads: 0,
            }
        }
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            let limit = buffer.len().min(self.chunk_size);
            self.bytes.read(&mut buffer[..limit])
        }
    }

    struct SecretIoError;

    impl Read for SecretIoError {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("Authorization: Bearer super-secret"))
        }
    }

    fn object_with_exact_size(size: usize) -> Vec<u8> {
        let prefix = br#"{"data":""#;
        let suffix = br#""}"#;
        assert!(size >= prefix.len() + suffix.len());
        let mut input = Vec::with_capacity(size);
        input.extend_from_slice(prefix);
        input.resize(size - suffix.len(), b'x');
        input.extend_from_slice(suffix);
        assert_eq!(input.len(), size);
        input
    }

    #[test]
    fn inline_input_has_priority_and_never_reads_stdin() {
        let stdin = ReadProbe::with_bytes(br#"{"source":"stdin"}"#.to_vec());
        let mut input = CallInput::new(stdin, false);

        let object = input
            .read(Some(r#"{"source":"inline","nested":{"ok":true}}"#))
            .expect("valid inline object");
        let stdin = input.into_inner();

        assert_eq!(stdin.reads, 0);
        assert_eq!(object["source"], json!("inline"));
        assert_eq!(object["nested"], json!({"ok": true}));
    }

    #[test]
    fn tty_without_inline_returns_empty_object_without_reading() {
        let stdin = ReadProbe::with_bytes(br#"{"ignored":true}"#.to_vec());
        let mut input = CallInput::new(stdin, true);

        assert_eq!(input.read(None).expect("TTY default"), JsonObject::new());
        assert_eq!(input.into_inner().reads, 0);
    }

    #[test]
    fn eof_and_whitespace_only_input_return_empty_objects() {
        for bytes in [
            Vec::new(),
            b" \t\r\n".to_vec(),
            "\u{2003}\n".as_bytes().to_vec(),
        ] {
            let mut stdin = Cursor::new(bytes);
            assert_eq!(
                read_call_input(None, &mut stdin, false).expect("empty object"),
                JsonObject::new()
            );
        }
    }

    #[test]
    fn accepts_exactly_sixteen_mib_and_rejects_the_probe_byte() {
        let exact = object_with_exact_size(CALL_INPUT_MAX_SIZE);
        let mut exact_reader = ChunkedReader::new(exact, 31 * 1024);
        let object = read_call_input(None, &mut exact_reader, false).expect("boundary object");
        assert_eq!(
            object["data"].as_str().expect("string value").len(),
            CALL_INPUT_MAX_SIZE - br#"{"data":""#.len() - br#""}"#.len()
        );
        assert!(exact_reader.reads > 1);

        let mut oversized = object_with_exact_size(CALL_INPUT_MAX_SIZE);
        oversized.push(b' ');
        let mut oversized_reader = ChunkedReader::new(oversized, 4 * 1024);
        let error = read_call_input(None, &mut oversized_reader, false).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InputTooLarge);
        assert!(
            error
                .details
                .as_deref()
                .is_some_and(|details| details.contains("16777217"))
        );
    }

    #[test]
    fn non_tty_stdin_is_read_in_multiple_chunks() {
        let bytes = br#"{"alpha":1,"nested":{"items":[true,false,null]}}"#.to_vec();
        let mut stdin = ChunkedReader::new(bytes, 3);

        let object = read_call_input(None, &mut stdin, false).expect("chunked object");

        assert!(stdin.reads > 2);
        assert_eq!(object["alpha"], json!(1));
        assert_eq!(object["nested"]["items"], json!([true, false, null]));
    }

    #[test]
    fn invalid_utf8_reports_safe_exact_byte_position() {
        let bytes = b"{\n\"x\":\"\xFF\"}".to_vec();
        let mut stdin = Cursor::new(bytes);

        let error = read_call_input(None, &mut stdin, false).unwrap_err();

        assert_eq!(error.kind, ErrorKind::InvalidJson);
        let details = error.details.as_deref().expect("position details");
        assert!(details.contains("byte offset 7"), "{details}");
        assert!(!details.contains("super-secret"));
    }

    #[test]
    fn invalid_json_reports_line_and_column_without_echoing_input() {
        const SECRET: &str = "super-secret-token";
        let text = format!("{{\n  \"token\": \"{SECRET}\",\n  \"broken\": ]\n}}");
        let mut stdin = Cursor::new(text.into_bytes());

        let error = read_call_input(None, &mut stdin, false).unwrap_err();

        assert_eq!(error.kind, ErrorKind::InvalidJson);
        let details = error.details.as_deref().expect("parser position");
        assert!(details.contains("line 3"), "{details}");
        assert!(details.contains("column 13"), "{details}");
        assert!(!details.contains(SECRET));
        assert!(!error.message.contains(SECRET));
    }

    #[test]
    fn rejects_every_non_object_top_level_type() {
        for (input, expected_type) in [
            ("null", "null"),
            ("true", "boolean"),
            ("42", "number"),
            (r#""text""#, "string"),
            ("[]", "array"),
        ] {
            let mut stdin = Cursor::new(input.as_bytes());
            let error = read_call_input(None, &mut stdin, false).unwrap_err();
            assert_eq!(error.kind, ErrorKind::InvalidArguments);
            assert!(
                error
                    .details
                    .as_deref()
                    .is_some_and(|details| details.contains(expected_type)),
                "{input:?} should identify {expected_type}"
            );
        }
    }

    #[test]
    fn preserves_all_object_keys_and_values_semantically() {
        let text = r#"{"z":null,"unicode":"雪","number":1.25,"array":[1,{"x":false}],"empty":{}}"#;
        let mut stdin = Cursor::new(text.as_bytes());

        let object = read_call_input(None, &mut stdin, false).expect("object");

        assert_eq!(
            Value::Object(object),
            json!({
                "z": null,
                "unicode": "雪",
                "number": 1.25,
                "array": [1, {"x": false}],
                "empty": {}
            })
        );
    }

    #[test]
    fn io_errors_are_typed_and_do_not_expose_the_source_text() {
        let mut stdin = SecretIoError;

        let error = read_call_input(None, &mut stdin, false).unwrap_err();

        assert_eq!(error.kind, ErrorKind::InvalidArguments);
        assert!(Error::source(&error).is_some());
        for visible in [
            error.message.clone(),
            error.details.clone().unwrap_or_default(),
            error.suggestion.clone().unwrap_or_default(),
            format!("{error:?}"),
        ] {
            assert!(
                !visible.contains("super-secret"),
                "leaked through {visible:?}"
            );
            assert!(!visible.contains("Bearer"), "leaked through {visible:?}");
        }
    }
}
