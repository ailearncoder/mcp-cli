//! Server and tool listing command.

use std::{collections::BTreeMap, num::NonZeroUsize, sync::Arc};

use crate::{
    config::{ServerDefinition, TransportConfig},
    connection::{
        ConnectionError, ConnectionManager, ConnectionResourceRegistry, DirectConnectionManager,
        DirectConnector,
    },
    domain::{CommandOutcome, ServerSnapshot, ToolInfo, TransportSummary},
    error::CliError,
    output::format_server_list,
    policy::tool_filter::ToolFilter,
    runtime::{CommandContext, RuntimeConfig},
};

use super::execute_bounded_server_batch;

/// Executes the list command against an injected connection-selection boundary.
///
/// Production direct mode should use [`ListHandler::direct`]. Keeping the
/// manager injectable lets tests validate command behavior without starting
/// processes, opening sockets, or reading user configuration.
pub struct ListHandler {
    connections: Arc<dyn ConnectionManager>,
    concurrency: NonZeroUsize,
}

impl ListHandler {
    pub fn new(connections: Arc<dyn ConnectionManager>, concurrency: NonZeroUsize) -> Self {
        Self {
            connections,
            concurrency,
        }
    }

    /// Builds a direct-only list handler whose connection ownership and retry
    /// policy use the same runtime configuration as its bounded batch.
    pub fn direct(
        connector: Arc<dyn DirectConnector>,
        resources: ConnectionResourceRegistry,
        runtime: &RuntimeConfig,
    ) -> Self {
        Self::new(
            Arc::new(DirectConnectionManager::batch(
                connector, resources, runtime,
            )),
            runtime.concurrency,
        )
    }

    /// Connects every configured server, lists and filters its tools, closes
    /// the acquired connection, and formats all isolated per-server outcomes.
    pub async fn execute(
        &self,
        ctx: &CommandContext,
        servers: &BTreeMap<String, ServerDefinition>,
        with_descriptions: bool,
    ) -> Result<CommandOutcome, CliError> {
        let connections = Arc::clone(&self.connections);
        let results = execute_bounded_server_batch(
            ctx,
            servers,
            self.concurrency,
            move |task_context, server| {
                let connections = Arc::clone(&connections);
                Box::pin(async move {
                    list_one_server(task_context, server, connections.as_ref()).await
                })
            },
        )
        .await;

        Ok(CommandOutcome::HumanText(format_server_list(
            &results,
            with_descriptions,
        )))
    }
}

async fn list_one_server(
    ctx: &CommandContext,
    server: &ServerDefinition,
    connections: &dyn ConnectionManager,
) -> Result<ServerSnapshot, CliError> {
    let connection = connections.acquire(ctx, server).await?;
    let instructions = connection.instructions().map(str::to_owned);

    let tools = match connection.list_tools(ctx).await {
        Ok(tools) => tools,
        Err(error) => {
            let primary = connection_error(server, "listing tools", error);
            if connection.close(ctx).await.is_err() {
                ctx.diagnostics
                    .debug("connection close also failed after list tools failure");
            }
            return Err(primary);
        }
    };

    if let Err(error) = connection.close(ctx).await {
        return Err(connection_error(server, "closing the connection", error));
    }

    let mut tools = ToolFilter::new(&server.filter).filter(tools);
    tools.sort_by(compare_tools);

    Ok(ServerSnapshot {
        server: server.name.clone(),
        transport_summary: transport_summary(server),
        instructions,
        tools,
    })
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
        collections::BTreeMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use serde_json::json;

    use super::*;
    use crate::{
        BoxFuture, CancellationFlag, CommandSpec, ConfigHash, ConnectionMode, Deadline,
        DiagnosticSink, ErrorKind, ExitCode, McpConnection, ServerId, ToolFilterConfig, ToolResult,
        cli::parse_cli, commands::dispatch_list_command, domain::JsonObject,
    };

    #[derive(Default)]
    struct NullDiagnostics;

    impl DiagnosticSink for NullDiagnostics {
        fn warning(&self, _message: &str) {}
        fn debug(&self, _message: &str) {}
        fn server_stderr(&self, _server: &str, _bytes: &[u8]) {}
    }

    fn context() -> CommandContext {
        CommandContext {
            deadline: Deadline::new(Instant::now() + Duration::from_secs(30)),
            cancellation: Arc::new(CancellationFlag::default()),
            diagnostics: Arc::new(NullDiagnostics),
        }
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
            input_schema: json!({"type": "object"}),
        }
    }

    #[derive(Default)]
    struct ConnectionTrace {
        listed: AtomicUsize,
        closed: AtomicUsize,
    }

    struct FakeConnection {
        trace: Arc<ConnectionTrace>,
        tools: Mutex<Option<Result<Vec<ToolInfo>, ConnectionError>>>,
        close: Mutex<Option<Result<(), ConnectionError>>>,
    }

    impl FakeConnection {
        fn scripted(
            tools: Result<Vec<ToolInfo>, ConnectionError>,
            close: Result<(), ConnectionError>,
        ) -> (Self, Arc<ConnectionTrace>) {
            let trace = Arc::new(ConnectionTrace::default());
            (
                Self {
                    trace: Arc::clone(&trace),
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
            Box::pin(async { panic!("list handler must not call tools") })
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

    enum AcquirePlan {
        Connection(FakeConnection),
        Failure(CliError),
    }

    #[derive(Default)]
    struct FakeManager {
        plans: Mutex<BTreeMap<String, AcquirePlan>>,
        acquired: Mutex<Vec<String>>,
    }

    impl FakeManager {
        fn insert_connection(&self, server: &str, connection: FakeConnection) {
            self.plans
                .lock()
                .expect("plans lock")
                .insert(server.to_owned(), AcquirePlan::Connection(connection));
        }

        fn insert_failure(&self, server: &str, error: CliError) {
            self.plans
                .lock()
                .expect("plans lock")
                .insert(server.to_owned(), AcquirePlan::Failure(error));
        }

        fn acquired(&self) -> Vec<String> {
            let mut acquired = self.acquired.lock().expect("acquired lock").clone();
            acquired.sort();
            acquired
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
            let plan = self
                .plans
                .lock()
                .expect("plans lock")
                .remove(&server.name)
                .expect("scripted acquisition");
            Box::pin(async move {
                match plan {
                    AcquirePlan::Connection(connection) => {
                        Ok(Box::new(connection) as Box<dyn McpConnection>)
                    }
                    AcquirePlan::Failure(error) => Err(error),
                }
            })
        }
    }

    #[derive(Default)]
    struct FakeConnector {
        connections: Mutex<BTreeMap<String, FakeConnection>>,
        connected: Mutex<Vec<String>>,
    }

    impl FakeConnector {
        fn insert(&self, server: &str, connection: FakeConnection) {
            self.connections
                .lock()
                .expect("connections lock")
                .insert(server.to_owned(), connection);
        }

        fn connected(&self) -> Vec<String> {
            let mut connected = self.connected.lock().expect("connected lock").clone();
            connected.sort();
            connected
        }
    }

    impl DirectConnector for FakeConnector {
        fn connect<'a>(
            &'a self,
            _ctx: &'a CommandContext,
            server: &'a ServerDefinition,
        ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, ConnectionError>> {
            self.connected
                .lock()
                .expect("connected lock")
                .push(server.name.clone());
            let connection = self
                .connections
                .lock()
                .expect("connections lock")
                .remove(&server.name)
                .expect("scripted direct connection");
            Box::pin(async move { Ok(Box::new(connection) as Box<dyn McpConnection>) })
        }
    }

    fn human_text(outcome: CommandOutcome) -> String {
        match outcome {
            CommandOutcome::HumanText(text) => text,
            other => panic!("expected human text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn filters_and_sorts_servers_and_tools_then_closes_every_success() {
        let manager = Arc::new(FakeManager::default());
        let (alpha, alpha_trace) = FakeConnection::scripted(
            Ok(vec![
                tool("zeta", Some("last")),
                tool("hidden_secret", Some("hidden")),
                tool("alpha", Some("first")),
            ]),
            Ok(()),
        );
        let (beta, beta_trace) = FakeConnection::scripted(Ok(vec![tool("middle", None)]), Ok(()));
        manager.insert_connection("alpha", alpha);
        manager.insert_connection("beta", beta);
        let servers = BTreeMap::from([
            (
                "alpha".to_owned(),
                server(
                    "alpha",
                    ToolFilterConfig {
                        allowed_tools: vec!["*".to_owned()],
                        disabled_tools: vec!["hidden*".to_owned()],
                    },
                ),
            ),
            (
                "beta".to_owned(),
                server("beta", ToolFilterConfig::default()),
            ),
        ]);
        let handler = ListHandler::new(manager.clone(), NonZeroUsize::new(2).unwrap());

        let output = human_text(handler.execute(&context(), &servers, true).await.unwrap());

        assert_eq!(
            output,
            "alpha\n  • alpha - first\n  • zeta - last\n\nbeta\n  • middle\n"
        );
        assert_eq!(manager.acquired(), ["alpha", "beta"]);
        assert_eq!(alpha_trace.listed.load(Ordering::SeqCst), 1);
        assert_eq!(alpha_trace.closed.load(Ordering::SeqCst), 1);
        assert_eq!(beta_trace.closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn direct_handler_connects_each_server_and_releases_all_resources() {
        let connector = Arc::new(FakeConnector::default());
        let (alpha, alpha_trace) = FakeConnection::scripted(Ok(vec![tool("read", None)]), Ok(()));
        let (beta, beta_trace) = FakeConnection::scripted(Ok(vec![tool("write", None)]), Ok(()));
        connector.insert("alpha", alpha);
        connector.insert("beta", beta);
        let resources = ConnectionResourceRegistry::new();
        let runtime = RuntimeConfig {
            concurrency: NonZeroUsize::new(2).unwrap(),
            ..RuntimeConfig::default()
        };
        let handler = ListHandler::direct(connector.clone(), resources.clone(), &runtime);
        let servers = ["beta", "alpha"]
            .into_iter()
            .map(|name| (name.to_owned(), server(name, ToolFilterConfig::default())))
            .collect();

        let output = human_text(handler.execute(&context(), &servers, false).await.unwrap());

        assert_eq!(output, "alpha\n  • read\n\nbeta\n  • write\n");
        assert_eq!(connector.connected(), ["alpha", "beta"]);
        assert_eq!(alpha_trace.closed.load(Ordering::SeqCst), 1);
        assert_eq!(beta_trace.closed.load(Ordering::SeqCst), 1);
        assert_eq!(resources.active_resource_count(), 0);
    }

    #[tokio::test]
    async fn partial_connect_list_and_close_failures_preserve_successful_servers() {
        let manager = Arc::new(FakeManager::default());
        manager.insert_failure(
            "connect-fail",
            CliError::network_error("connect-fail", "scripted connection failure"),
        );
        let (list_failure, list_trace) =
            FakeConnection::scripted(Err(ConnectionError::new("scripted list failure")), Ok(()));
        let (close_failure, close_trace) = FakeConnection::scripted(
            Ok(vec![tool("discarded", None)]),
            Err(ConnectionError::new("scripted close failure")),
        );
        let (success, success_trace) =
            FakeConnection::scripted(Ok(vec![tool("kept", None)]), Ok(()));
        manager.insert_connection("list-fail", list_failure);
        manager.insert_connection("close-fail", close_failure);
        manager.insert_connection("success", success);
        let servers = ["success", "list-fail", "connect-fail", "close-fail"]
            .into_iter()
            .map(|name| (name.to_owned(), server(name, ToolFilterConfig::default())))
            .collect();
        let handler = ListHandler::new(manager, NonZeroUsize::new(4).unwrap());

        let output = human_text(handler.execute(&context(), &servers, false).await.unwrap());

        assert_eq!(
            output,
            "close-fail\n  <error: Failed to communicate with server \"close-fail\">\n\nconnect-fail\n  <error: Failed to communicate with server \"connect-fail\">\n\nlist-fail\n  <error: Failed to communicate with server \"list-fail\">\n\nsuccess\n  • kept\n"
        );
        assert_eq!(list_trace.listed.load(Ordering::SeqCst), 1);
        assert_eq!(list_trace.closed.load(Ordering::SeqCst), 1);
        assert_eq!(close_trace.closed.load(Ordering::SeqCst), 1);
        assert_eq!(success_trace.closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn empty_server_map_is_successful_and_acquires_nothing() {
        let manager = Arc::new(FakeManager::default());
        let handler = ListHandler::new(manager.clone(), NonZeroUsize::new(3).unwrap());

        let output = human_text(
            handler
                .execute(&context(), &BTreeMap::new(), false)
                .await
                .unwrap(),
        );

        assert_eq!(output, "No servers configured.\n");
        assert!(manager.acquired().is_empty());
    }

    #[tokio::test]
    async fn commandless_parse_dispatches_to_list_without_claiming_other_commands() {
        let invocation = parse_cli(Vec::<std::ffi::OsString>::new()).unwrap();
        assert!(matches!(invocation.command, CommandSpec::List { .. }));

        let manager = Arc::new(FakeManager::default());
        let handler = ListHandler::new(manager.clone(), NonZeroUsize::new(1).unwrap());
        let dispatched =
            dispatch_list_command(&handler, &context(), &BTreeMap::new(), &invocation.command)
                .await
                .expect("list is handled")
                .unwrap();
        assert_eq!(human_text(dispatched), "No servers configured.\n");

        let info = CommandSpec::Info {
            server: "later".to_owned(),
            tool: None,
            with_descriptions: false,
        };
        assert!(
            dispatch_list_command(&handler, &context(), &BTreeMap::new(), &info)
                .await
                .is_none()
        );
        assert!(manager.acquired().is_empty());
    }

    #[test]
    fn connection_failures_remain_typed_without_exposing_adapter_messages() {
        let definition = server("remote", ToolFilterConfig::default());
        let error = connection_error(
            &definition,
            "listing tools",
            ConnectionError::new("Authorization: secret"),
        );

        assert_eq!(error.kind, ErrorKind::NetworkError);
        assert_eq!(error.exit_code, ExitCode::Network);
        let visible = format!(
            "{} {} {} {error:?}",
            error.message,
            error.details.clone().unwrap_or_default(),
            error.suggestion.clone().unwrap_or_default(),
        );
        assert!(!visible.contains("Authorization: secret"));
    }

    #[test]
    fn transport_summary_preserves_stdio_and_http_identity() {
        let stdio = server("local", ToolFilterConfig::default());
        assert_eq!(
            transport_summary(&stdio),
            TransportSummary::Stdio {
                command: "run-local".to_owned()
            }
        );

        let mut http = server("remote", ToolFilterConfig::default());
        http.transport = TransportConfig::Http {
            url: url::Url::parse("https://example.test/mcp").unwrap(),
            headers: BTreeMap::new(),
        };
        assert_eq!(
            transport_summary(&http),
            TransportSummary::Http {
                url: url::Url::parse("https://example.test/mcp").unwrap()
            }
        );
    }
}
