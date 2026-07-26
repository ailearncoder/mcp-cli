//! Server and tool inspection command.

use std::{collections::BTreeMap, sync::Arc};

use crate::{
    config::{ServerDefinition, TransportConfig},
    connection::{
        ConnectionError, ConnectionManager, ConnectionResourceRegistry, DirectConnectionManager,
        DirectConnector,
    },
    domain::{CommandOutcome, ServerSnapshot, ToolInfo, TransportSummary},
    error::CliError,
    output::format_server_info,
    policy::tool_filter::ToolFilter,
    runtime::{CommandContext, RuntimeConfig},
};

/// Executes all public info syntaxes through one single-target command path.
pub struct InfoHandler {
    connections: Arc<dyn ConnectionManager>,
}

impl InfoHandler {
    pub fn new(connections: Arc<dyn ConnectionManager>) -> Self {
        Self { connections }
    }

    /// Builds a direct-only handler with a hard limit of one owned connection.
    pub fn direct(
        connector: Arc<dyn DirectConnector>,
        resources: ConnectionResourceRegistry,
        runtime: &RuntimeConfig,
    ) -> Self {
        Self::new(Arc::new(DirectConnectionManager::with_runtime_config(
            connector, resources, runtime,
        )))
    }

    /// Inspects one configured server or one tool exposed by that server.
    ///
    /// Server lookup deliberately precedes connection acquisition. Once a
    /// connection exists, every exit path closes it; a close failure is only a
    /// diagnostic and never replaces the command's business result or primary
    /// error.
    pub async fn execute(
        &self,
        ctx: &CommandContext,
        servers: &BTreeMap<String, ServerDefinition>,
        server_name: &str,
        tool_name: Option<&str>,
        with_descriptions: bool,
    ) -> Result<CommandOutcome, CliError> {
        let server = servers.get(server_name).ok_or_else(|| {
            CliError::server_not_found(server_name, servers.keys().map(String::as_str))
        })?;

        let connection = self.connections.acquire(ctx, server).await?;
        let instructions = connection.instructions().map(str::to_owned);
        let tools = match connection.list_tools(ctx).await {
            Ok(tools) => tools,
            Err(error) => {
                let primary = connection_error(server, "listing tools", error);
                if connection.close(ctx).await.is_err() {
                    ctx.diagnostics
                        .debug("connection close also failed after info list tools failure");
                }
                return Err(primary);
            }
        };

        let outcome = build_outcome(server, instructions, tools, tool_name, with_descriptions);

        if connection.close(ctx).await.is_err() {
            ctx.diagnostics
                .debug("connection close failed after info command completion");
        }

        outcome
    }
}

fn build_outcome(
    server: &ServerDefinition,
    instructions: Option<String>,
    tools: Vec<ToolInfo>,
    tool_name: Option<&str>,
    with_descriptions: bool,
) -> Result<CommandOutcome, CliError> {
    let filter = ToolFilter::new(&server.filter);

    if let Some(tool_name) = tool_name {
        if tools.iter().any(|tool| tool.name == tool_name) && !filter.is_allowed(tool_name) {
            return Err(CliError::tool_disabled(&server.name, tool_name));
        }

        let mut allowed_tools = filter.filter(tools);
        allowed_tools.sort_by(compare_tools);
        let tool = allowed_tools
            .iter()
            .find(|tool| tool.name == tool_name)
            .ok_or_else(|| {
                CliError::tool_not_found(
                    &server.name,
                    tool_name,
                    allowed_tools.iter().map(|tool| tool.name.as_str()),
                )
            })?;

        return Ok(CommandOutcome::Json(tool.input_schema.clone()));
    }

    let mut tools = filter.filter(tools);
    tools.sort_by(compare_tools);
    let snapshot = ServerSnapshot {
        server: server.name.clone(),
        transport_summary: transport_summary(server),
        instructions,
        tools,
    };
    Ok(CommandOutcome::HumanText(format_server_info(
        &snapshot,
        with_descriptions,
    )))
}

fn connection_error(
    server: &ServerDefinition,
    operation: &str,
    error: ConnectionError,
) -> CliError {
    let cli_error = if error.is_timeout() {
        CliError::timeout(operation)
    } else if error.is_cancelled() {
        CliError::cancelled(&server.name, operation)
    } else if let Some(status) = error.http_status() {
        CliError::http_status(&server.name, status)
    } else {
        CliError::network_error_classified(
            &server.name,
            format!("Failed while {operation}"),
            error.error_class(),
        )
    };
    cli_error.with_source(error)
}

fn transport_summary(server: &ServerDefinition) -> TransportSummary {
    match &server.transport {
        TransportConfig::Stdio { command, .. } => TransportSummary::Stdio {
            command: command.clone(),
        },
        TransportConfig::Http { url, .. } => TransportSummary::Http { url: url.clone() },
    }
}

fn compare_tools(left: &ToolInfo, right: &ToolInfo) -> std::cmp::Ordering {
    left.name
        .cmp(&right.name)
        .then_with(|| left.description.cmp(&right.description))
        .then_with(|| {
            left.input_schema
                .to_string()
                .cmp(&right.input_schema.to_string())
        })
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use serde_json::{Value, json};
    use url::Url;

    use super::*;
    use crate::{
        BoxFuture, CancellationFlag, CommandSpec, ConfigHash, ConnectionMode, Deadline,
        DiagnosticSink, ErrorKind, McpConnection, ServerId, ToolFilterConfig, ToolResult,
        cli::parse_cli, commands::dispatch_info_command, domain::JsonObject,
    };

    #[derive(Default)]
    struct RecordingDiagnostics {
        debug: Mutex<Vec<String>>,
    }

    impl DiagnosticSink for RecordingDiagnostics {
        fn warning(&self, _message: &str) {}

        fn debug(&self, message: &str) {
            self.debug.lock().expect("debug lock").push(message.into());
        }

        fn server_stderr(&self, _server: &str, _bytes: &[u8]) {}
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

    fn tool(name: &str, description: Option<&str>, schema: Value) -> ToolInfo {
        ToolInfo {
            name: name.into(),
            description: description.map(str::to_owned),
            input_schema: schema,
        }
    }

    #[derive(Default)]
    struct ConnectionTrace {
        listed: AtomicUsize,
        closed: AtomicUsize,
    }

    struct FakeConnection {
        trace: Arc<ConnectionTrace>,
        instructions: Option<String>,
        tools: Mutex<Option<Result<Vec<ToolInfo>, ConnectionError>>>,
        close: Mutex<Option<Result<(), ConnectionError>>>,
    }

    impl FakeConnection {
        fn scripted(
            instructions: Option<&str>,
            tools: Result<Vec<ToolInfo>, ConnectionError>,
            close: Result<(), ConnectionError>,
        ) -> (Self, Arc<ConnectionTrace>) {
            let trace = Arc::new(ConnectionTrace::default());
            (
                Self {
                    trace: Arc::clone(&trace),
                    instructions: instructions.map(str::to_owned),
                    tools: Mutex::new(Some(tools)),
                    close: Mutex::new(Some(close)),
                },
                trace,
            )
        }
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
                .expect("one list result");
            Box::pin(async move { result })
        }

        fn call_tool<'a>(
            &'a self,
            _ctx: &'a CommandContext,
            _name: &'a str,
            _args: JsonObject,
        ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
            Box::pin(async { panic!("info handler must not call tools") })
        }

        fn instructions(&self) -> Option<&str> {
            self.instructions.as_deref()
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
        connection: Mutex<Option<FakeConnection>>,
        acquired: Mutex<Vec<String>>,
    }

    impl FakeManager {
        fn with_connection(connection: FakeConnection) -> Self {
            Self {
                connection: Mutex::new(Some(connection)),
                acquired: Mutex::new(Vec::new()),
            }
        }

        fn acquired(&self) -> Vec<String> {
            self.acquired.lock().expect("acquired lock").clone()
        }
    }

    impl ConnectionManager for FakeManager {
        fn acquire<'a>(
            &'a self,
            _ctx: &'a CommandContext,
            server: &'a ServerDefinition,
        ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, CliError>> {
            self.acquired
                .lock()
                .expect("acquired lock")
                .push(server.name.clone());
            let connection = self
                .connection
                .lock()
                .expect("connection lock")
                .take()
                .expect("info may acquire only one connection");
            Box::pin(async move { Ok(Box::new(connection) as Box<dyn McpConnection>) })
        }
    }

    fn human_text(outcome: CommandOutcome) -> String {
        match outcome {
            CommandOutcome::HumanText(text) => text,
            other => panic!("expected human text, got {other:?}"),
        }
    }

    fn schema(outcome: CommandOutcome) -> Value {
        match outcome {
            CommandOutcome::Json(value) => value,
            other => panic!("expected JSON schema, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bare_and_explicit_server_syntax_share_overview_path() {
        for args in [vec!["alpha"], vec!["info", "alpha"]] {
            let invocation = parse_cli(args.into_iter().map(Into::into)).unwrap();
            let (connection, trace) = FakeConnection::scripted(
                Some("Use carefully"),
                Ok(vec![tool("read", Some("Read data"), json!({}))]),
                Ok(()),
            );
            let manager = Arc::new(FakeManager::with_connection(connection));
            let handler = InfoHandler::new(manager.clone());
            let servers =
                BTreeMap::from([("alpha".into(), server("alpha", ToolFilterConfig::default()))]);
            let dispatched = dispatch_info_command(
                &handler,
                &context(Arc::new(RecordingDiagnostics::default())),
                &servers,
                &invocation.command,
            )
            .await
            .expect("info is handled")
            .unwrap();

            assert!(human_text(dispatched).contains("Server: alpha"));
            assert_eq!(manager.acquired(), ["alpha"]);
            assert_eq!(trace.closed.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn split_and_slash_tool_syntax_return_the_same_complete_schema() {
        let expected = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"query": {"type": ["string", "null"]}},
            "required": ["query"],
            "x-extension": {"preserved": true}
        });

        for args in [
            vec!["info", "alpha", "search"],
            vec!["info", "alpha/search"],
        ] {
            let invocation = parse_cli(args.into_iter().map(Into::into)).unwrap();
            let (connection, trace) = FakeConnection::scripted(
                None,
                Ok(vec![tool("search", None, expected.clone())]),
                Ok(()),
            );
            let manager = Arc::new(FakeManager::with_connection(connection));
            let handler = InfoHandler::new(manager.clone());
            let servers =
                BTreeMap::from([("alpha".into(), server("alpha", ToolFilterConfig::default()))]);
            let outcome = dispatch_info_command(
                &handler,
                &context(Arc::new(RecordingDiagnostics::default())),
                &servers,
                &invocation.command,
            )
            .await
            .expect("info is handled")
            .unwrap();

            assert_eq!(schema(outcome), expected);
            assert_eq!(manager.acquired(), ["alpha"]);
            assert_eq!(trace.listed.load(Ordering::SeqCst), 1);
            assert_eq!(trace.closed.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn overview_filters_tools_sorts_parameters_and_controls_descriptions() {
        let (connection, trace) = FakeConnection::scripted(
            Some("first line\nsecond line"),
            Ok(vec![
                tool("zeta", Some("last"), json!({})),
                tool("hidden", Some("must not leak"), json!({})),
                tool(
                    "alpha",
                    Some("visible description"),
                    json!({
                        "properties": {
                            "z": {"type": "number", "description": "last parameter"},
                            "a": {"type": "string", "description": "first parameter"}
                        },
                        "required": ["z"]
                    }),
                ),
            ]),
            Ok(()),
        );
        let manager = Arc::new(FakeManager::with_connection(connection));
        let handler = InfoHandler::new(manager.clone());
        let servers = BTreeMap::from([(
            "alpha".into(),
            server(
                "alpha",
                ToolFilterConfig {
                    allowed_tools: vec!["*".into()],
                    disabled_tools: vec!["hidden".into()],
                },
            ),
        )]);

        let output = human_text(
            handler
                .execute(
                    &context(Arc::new(RecordingDiagnostics::default())),
                    &servers,
                    "alpha",
                    None,
                    true,
                )
                .await
                .unwrap(),
        );

        assert_eq!(
            output,
            "Server: alpha\nTransport: stdio\nCommand: run-alpha\n\nInstructions:\n  first line\n  second line\n\nTools (2):\n  alpha\n    visible description\n    Parameters:\n      • a (string, optional) - first parameter\n      • z (number, required) - last parameter\n  zeta\n    last\n"
        );
        assert!(!output.contains("hidden"));
        assert_eq!(manager.acquired(), ["alpha"]);
        assert_eq!(trace.closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unknown_server_is_typed_sorted_and_never_connects() {
        let manager = Arc::new(FakeManager::default());
        let handler = InfoHandler::new(manager.clone());
        let servers = ["zeta", "alpha", "middle"]
            .into_iter()
            .map(|name| (name.into(), server(name, ToolFilterConfig::default())))
            .collect();

        let error = handler
            .execute(
                &context(Arc::new(RecordingDiagnostics::default())),
                &servers,
                "missing",
                None,
                false,
            )
            .await
            .unwrap_err();

        assert_eq!(error.kind, ErrorKind::ServerNotFound);
        assert_eq!(
            error.details.as_deref(),
            Some("Available servers: alpha, middle, zeta")
        );
        assert!(error.suggestion.as_deref().unwrap().contains("info alpha"));
        assert!(manager.acquired().is_empty());
    }

    #[tokio::test]
    async fn unknown_tool_uses_only_allowed_sorted_candidates_and_closes() {
        let (connection, trace) = FakeConnection::scripted(
            None,
            Ok(vec![
                tool("zeta", None, json!({})),
                tool("hidden", None, json!({})),
                tool("alpha", None, json!({})),
            ]),
            Ok(()),
        );
        let manager = Arc::new(FakeManager::with_connection(connection));
        let handler = InfoHandler::new(manager.clone());
        let servers = BTreeMap::from([(
            "target".into(),
            server(
                "target",
                ToolFilterConfig {
                    allowed_tools: vec!["*".into()],
                    disabled_tools: vec!["hidden".into()],
                },
            ),
        )]);

        let error = handler
            .execute(
                &context(Arc::new(RecordingDiagnostics::default())),
                &servers,
                "target",
                Some("missing"),
                false,
            )
            .await
            .unwrap_err();

        assert_eq!(error.kind, ErrorKind::ToolNotFound);
        assert_eq!(
            error.details.as_deref(),
            Some("Available tools: alpha, zeta")
        );
        assert!(error.suggestion.as_deref().unwrap().contains("info target"));
        assert_eq!(manager.acquired(), ["target"]);
        assert_eq!(trace.closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn advertised_but_unapproved_tool_is_typed_disabled_and_closes() {
        let (connection, trace) =
            FakeConnection::scripted(None, Ok(vec![tool("danger", None, json!({}))]), Ok(()));
        let manager = Arc::new(FakeManager::with_connection(connection));
        let handler = InfoHandler::new(manager.clone());
        let servers = BTreeMap::from([(
            "target".into(),
            server(
                "target",
                ToolFilterConfig {
                    allowed_tools: vec!["safe*".into()],
                    disabled_tools: Vec::new(),
                },
            ),
        )]);

        let error = handler
            .execute(
                &context(Arc::new(RecordingDiagnostics::default())),
                &servers,
                "target",
                Some("danger"),
                false,
            )
            .await
            .unwrap_err();

        assert_eq!(error.kind, ErrorKind::ToolDisabled);
        assert!(
            error
                .suggestion
                .as_deref()
                .unwrap()
                .contains("allowedTools")
        );
        assert_eq!(manager.acquired(), ["target"]);
        assert_eq!(trace.closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn list_and_close_failures_close_once_without_masking_primary_results() {
        let diagnostics = Arc::new(RecordingDiagnostics::default());
        let (connection, trace) = FakeConnection::scripted(
            None,
            Err(ConnectionError::new("primary secret")),
            Err(ConnectionError::new("close secret")),
        );
        let handler = InfoHandler::new(Arc::new(FakeManager::with_connection(connection)));
        let servers = BTreeMap::from([(
            "target".into(),
            server("target", ToolFilterConfig::default()),
        )]);

        let error = handler
            .execute(
                &context(Arc::clone(&diagnostics)),
                &servers,
                "target",
                None,
                false,
            )
            .await
            .unwrap_err();

        assert_eq!(error.kind, ErrorKind::NetworkError);
        assert_eq!(error.details.as_deref(), Some("Failed while listing tools"));
        assert!(!format!("{error:?}").contains("close secret"));
        assert_eq!(trace.closed.load(Ordering::SeqCst), 1);
        assert_eq!(diagnostics.debug.lock().expect("debug lock").len(), 1);

        let diagnostics = Arc::new(RecordingDiagnostics::default());
        let (connection, trace) = FakeConnection::scripted(
            Some("kept"),
            Ok(vec![tool("read", None, json!({}))]),
            Err(ConnectionError::new("close secret")),
        );
        let handler = InfoHandler::new(Arc::new(FakeManager::with_connection(connection)));
        let outcome = handler
            .execute(
                &context(Arc::clone(&diagnostics)),
                &servers,
                "target",
                None,
                false,
            )
            .await
            .expect("close failure must not replace successful info");

        assert!(human_text(outcome).contains("Instructions:\n  kept"));
        assert_eq!(trace.closed.load(Ordering::SeqCst), 1);
        let visible = diagnostics.debug.lock().expect("debug lock").join("\n");
        assert!(visible.contains("close failed"));
        assert!(!visible.contains("close secret"));
    }

    #[tokio::test]
    async fn http_transport_instructions_and_single_target_connection_are_preserved() {
        let (connection, trace) = FakeConnection::scripted(
            Some("Remote instructions"),
            Ok(vec![tool("read", None, json!({}))]),
            Ok(()),
        );
        let manager = Arc::new(FakeManager::with_connection(connection));
        let handler = InfoHandler::new(manager.clone());
        let mut target = server("remote", ToolFilterConfig::default());
        target.transport = TransportConfig::Http {
            url: Url::parse("https://example.test/mcp").unwrap(),
            headers: BTreeMap::new(),
        };
        let servers = BTreeMap::from([
            ("other".into(), server("other", ToolFilterConfig::default())),
            ("remote".into(), target),
        ]);

        let output = human_text(
            handler
                .execute(
                    &context(Arc::new(RecordingDiagnostics::default())),
                    &servers,
                    "remote",
                    None,
                    false,
                )
                .await
                .unwrap(),
        );

        assert!(output.contains("Transport: HTTP\nURL: https://example.test/mcp"));
        assert!(output.contains("Instructions:\n  Remote instructions"));
        assert_eq!(manager.acquired(), ["remote"]);
        assert_eq!(trace.listed.load(Ordering::SeqCst), 1);
        assert_eq!(trace.closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dispatcher_does_not_claim_non_info_commands() {
        let manager = Arc::new(FakeManager::default());
        let handler = InfoHandler::new(manager.clone());
        let command = CommandSpec::List {
            with_descriptions: false,
        };

        assert!(
            dispatch_info_command(
                &handler,
                &context(Arc::new(RecordingDiagnostics::default())),
                &BTreeMap::new(),
                &command,
            )
            .await
            .is_none()
        );
        assert!(manager.acquired().is_empty());
    }
}
