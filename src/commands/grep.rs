//! Cross-server tool search command.

use std::{collections::BTreeMap, num::NonZeroUsize, sync::Arc};

use crate::{
    config::ServerDefinition,
    connection::{
        ConnectionError, ConnectionManager, ConnectionResourceRegistry, DirectConnectionManager,
        DirectConnector,
    },
    domain::{CommandOutcome, PerServer, SearchHit},
    error::CliError,
    output::format_grep_hits,
    policy::{search_glob::SearchMatcher, tool_filter::ToolFilter},
    runtime::{CommandContext, RuntimeConfig},
};

use super::execute_bounded_server_batch;

/// Executes a deterministic cross-server search through an injected
/// connection-selection boundary.
pub struct GrepHandler {
    connections: Arc<dyn ConnectionManager>,
    concurrency: NonZeroUsize,
}

impl GrepHandler {
    pub fn new(connections: Arc<dyn ConnectionManager>, concurrency: NonZeroUsize) -> Self {
        Self {
            connections,
            concurrency,
        }
    }

    /// Builds a direct-only grep handler using the command runtime's bounded
    /// concurrency, retry policy, and resource registry.
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

    /// Compiles the search pattern once, then searches every configured server
    /// with the shared bounded batch executor.
    ///
    /// Per-server connect/list/close failures become safe warnings and do not
    /// discard hits from other servers. Each server's visibility policy is
    /// applied before matching tool names; descriptions are presentation-only.
    pub async fn execute(
        &self,
        ctx: &CommandContext,
        servers: &BTreeMap<String, ServerDefinition>,
        pattern: &str,
        with_descriptions: bool,
    ) -> Result<CommandOutcome, CliError> {
        let matcher = Arc::new(compile_pattern(pattern)?);
        let connections = Arc::clone(&self.connections);
        let results = execute_bounded_server_batch(
            ctx,
            servers,
            self.concurrency,
            move |task_context, server| {
                let connections = Arc::clone(&connections);
                let matcher = Arc::clone(&matcher);
                Box::pin(async move {
                    grep_one_server(task_context, server, connections.as_ref(), matcher.as_ref())
                        .await
                })
            },
        )
        .await;

        let mut hits = Vec::new();
        for result in results {
            match result {
                PerServer::Success { value, .. } => hits.extend(value),
                PerServer::Failure { server, error } => {
                    ctx.diagnostics.warning(&format!(
                        "Server \"{}\" could not be searched ({}); continuing",
                        safe_diagnostic_label(&server),
                        error.machine_kind(),
                    ));
                }
            }
        }
        hits.sort_by(|left, right| {
            left.server
                .cmp(&right.server)
                .then_with(|| left.tool.name.cmp(&right.tool.name))
        });

        Ok(CommandOutcome::HumanText(format_grep_hits(
            &hits,
            with_descriptions,
        )))
    }
}

fn compile_pattern(pattern: &str) -> Result<SearchMatcher, CliError> {
    if pattern.is_empty() {
        return Err(CliError::invalid_arguments(
            "Missing required argument for grep: pattern",
            "The grep command requires a non-empty pattern",
        )
        .with_suggestion("Use 'mcp-cli grep <pattern>'"));
    }
    SearchMatcher::compile(pattern)
}

async fn grep_one_server(
    ctx: &CommandContext,
    server: &ServerDefinition,
    connections: &dyn ConnectionManager,
    matcher: &SearchMatcher,
) -> Result<Vec<SearchHit>, CliError> {
    let connection = connections.acquire(ctx, server).await?;
    let tools = match connection.list_tools(ctx).await {
        Ok(tools) => tools,
        Err(error) => {
            let primary = connection_error(server, "listing tools for grep", error);
            if connection.close(ctx).await.is_err() {
                ctx.diagnostics
                    .debug("connection close also failed after grep list tools failure");
            }
            return Err(primary);
        }
    };

    if let Err(error) = connection.close(ctx).await {
        return Err(connection_error(
            server,
            "closing the grep connection",
            error,
        ));
    }

    let allowed_tools = ToolFilter::new(&server.filter).filter(tools);
    Ok(allowed_tools
        .into_iter()
        .filter(|tool| matcher.is_match(&tool.name))
        .map(|tool| SearchHit {
            server: server.name.clone(),
            tool,
        })
        .collect())
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

fn safe_diagnostic_label(value: &str) -> String {
    const MAX_CHARS: usize = 160;
    let mut safe = String::new();
    for (index, character) in value.chars().enumerate() {
        if index == MAX_CHARS {
            safe.push_str("...");
            break;
        }
        safe.push(if character.is_control() {
            ' '
        } else {
            character
        });
    }
    safe
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

    use serde_json::json;

    use super::*;
    use crate::{
        BoxFuture, CancellationFlag, CommandSpec, ConfigHash, ConnectionMode, Deadline,
        DiagnosticSink, ErrorKind, McpConnection, ServerId, ToolFilterConfig, ToolInfo, ToolResult,
        cli::parse_cli, commands::dispatch_grep_command, domain::JsonObject,
    };

    #[derive(Default)]
    struct RecordingDiagnostics {
        warnings: Mutex<Vec<String>>,
        debug: Mutex<Vec<String>>,
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
            transport: crate::TransportConfig::Stdio {
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
            name: name.into(),
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
            Box::pin(async { panic!("grep handler must not call tools") })
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
                .insert(server.into(), AcquirePlan::Connection(connection));
        }

        fn insert_failure(&self, server: &str, error: CliError) {
            self.plans
                .lock()
                .expect("plans lock")
                .insert(server.into(), AcquirePlan::Failure(error));
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

    fn human_text(outcome: CommandOutcome) -> String {
        match outcome {
            CommandOutcome::HumanText(text) => text,
            other => panic!("expected human text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn filters_before_name_only_case_insensitive_glob_search_and_sorts_hits() {
        let manager = Arc::new(FakeManager::default());
        let (beta, beta_trace) = FakeConnection::scripted(
            Ok(vec![
                tool("GROUP/READ.X", Some("beta description")),
                tool("other", Some("group/read.z appears only in description")),
            ]),
            Ok(()),
        );
        let (alpha, alpha_trace) = FakeConnection::scripted(
            Ok(vec![
                tool("z/read.2", Some("second")),
                tool("secret/read.0", Some("must remain hidden")),
                tool("a/read.1", Some("first")),
            ]),
            Ok(()),
        );
        manager.insert_connection("beta", beta);
        manager.insert_connection("alpha", alpha);
        let servers = BTreeMap::from([
            ("beta".into(), server("beta", ToolFilterConfig::default())),
            (
                "alpha".into(),
                server(
                    "alpha",
                    ToolFilterConfig {
                        allowed_tools: vec!["*".into()],
                        disabled_tools: vec!["secret/*".into()],
                    },
                ),
            ),
        ]);
        let handler = GrepHandler::new(manager.clone(), NonZeroUsize::new(2).unwrap());

        let output = human_text(
            handler
                .execute(
                    &context(Arc::new(RecordingDiagnostics::default())),
                    &servers,
                    "**/read.?",
                    true,
                )
                .await
                .unwrap(),
        );

        assert_eq!(
            output,
            "alpha a/read.1 - first\nalpha z/read.2 - second\nbeta GROUP/READ.X - beta description\n"
        );
        assert!(!output.contains("secret"));
        assert!(!output.contains("appears only in description"));
        assert_eq!(manager.acquired(), ["alpha", "beta"]);
        assert_eq!(alpha_trace.listed.load(Ordering::SeqCst), 1);
        assert_eq!(alpha_trace.closed.load(Ordering::SeqCst), 1);
        assert_eq!(beta_trace.closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn description_switch_changes_only_rendered_description() {
        for with_descriptions in [false, true] {
            let manager = Arc::new(FakeManager::default());
            let (connection, trace) =
                FakeConnection::scripted(Ok(vec![tool("read_file", Some("Reads a file"))]), Ok(()));
            manager.insert_connection("files", connection);
            let servers =
                BTreeMap::from([("files".into(), server("files", ToolFilterConfig::default()))]);
            let handler = GrepHandler::new(manager, NonZeroUsize::new(1).unwrap());

            let output = human_text(
                handler
                    .execute(
                        &context(Arc::new(RecordingDiagnostics::default())),
                        &servers,
                        "read_*",
                        with_descriptions,
                    )
                    .await
                    .unwrap(),
            );

            let expected = if with_descriptions {
                "files read_file - Reads a file\n"
            } else {
                "files read_file\n"
            };
            assert_eq!(output, expected);
            assert_eq!(trace.closed.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn connect_list_and_close_failures_warn_safely_and_preserve_successes() {
        let diagnostics = Arc::new(RecordingDiagnostics::default());
        let manager = Arc::new(FakeManager::default());
        manager.insert_failure(
            "connect\nfail",
            CliError::network_error("connect fail", "Authorization: connect-secret"),
        );
        let (list_failure, list_trace) = FakeConnection::scripted(
            Err(ConnectionError::new("Authorization: list-secret")),
            Ok(()),
        );
        let (close_failure, close_trace) = FakeConnection::scripted(
            Ok(vec![tool("discarded", None)]),
            Err(ConnectionError::new("Authorization: close-secret")),
        );
        let (success, success_trace) =
            FakeConnection::scripted(Ok(vec![tool("kept", None)]), Ok(()));
        manager.insert_connection("list-fail", list_failure);
        manager.insert_connection("close-fail", close_failure);
        manager.insert_connection("success", success);
        let servers = ["success", "list-fail", "connect\nfail", "close-fail"]
            .into_iter()
            .map(|name| (name.into(), server(name, ToolFilterConfig::default())))
            .collect();
        let handler = GrepHandler::new(manager, NonZeroUsize::new(4).unwrap());

        let output = human_text(
            handler
                .execute(&context(Arc::clone(&diagnostics)), &servers, "*", false)
                .await
                .unwrap(),
        );

        assert_eq!(output, "success kept\n");
        let warnings = diagnostics.warnings.lock().expect("warnings lock");
        assert_eq!(warnings.len(), 3);
        assert!(warnings.iter().all(|warning| !warning.contains('\n')));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("connect fail"))
        );
        let visible = warnings.join(" ");
        assert!(!visible.contains("Authorization"));
        assert!(!visible.contains("secret"));
        assert_eq!(list_trace.listed.load(Ordering::SeqCst), 1);
        assert_eq!(list_trace.closed.load(Ordering::SeqCst), 1);
        assert_eq!(close_trace.closed.load(Ordering::SeqCst), 1);
        assert_eq!(success_trace.closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn zero_results_succeed_with_explicit_message() {
        let manager = Arc::new(FakeManager::default());
        let (connection, trace) =
            FakeConnection::scripted(Ok(vec![tool("write_file", None)]), Ok(()));
        manager.insert_connection("files", connection);
        let servers =
            BTreeMap::from([("files".into(), server("files", ToolFilterConfig::default()))]);
        let handler = GrepHandler::new(manager, NonZeroUsize::new(1).unwrap());

        let output = human_text(
            handler
                .execute(
                    &context(Arc::new(RecordingDiagnostics::default())),
                    &servers,
                    "read_*",
                    false,
                )
                .await
                .unwrap(),
        );

        assert_eq!(output, "No matching tools found.\n");
        assert_eq!(trace.closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn invalid_pattern_is_typed_and_prevents_all_connections() {
        let manager = Arc::new(FakeManager::default());
        let servers = BTreeMap::from([(
            "unused".into(),
            server("unused", ToolFilterConfig::default()),
        )]);
        let handler = GrepHandler::new(manager.clone(), NonZeroUsize::new(1).unwrap());

        let error = handler
            .execute(
                &context(Arc::new(RecordingDiagnostics::default())),
                &servers,
                "",
                false,
            )
            .await
            .unwrap_err();

        assert_eq!(error.kind, ErrorKind::InvalidArguments);
        assert!(error.suggestion.as_deref().unwrap().contains("grep"));
        assert!(manager.acquired().is_empty());
    }

    #[tokio::test]
    async fn parsed_grep_reaches_only_the_minimal_grep_dispatch_seam() {
        let invocation = parse_cli(["grep", "read_*"].into_iter().map(Into::into)).unwrap();
        let manager = Arc::new(FakeManager::default());
        let handler = GrepHandler::new(manager.clone(), NonZeroUsize::new(1).unwrap());
        let dispatched = dispatch_grep_command(
            &handler,
            &context(Arc::new(RecordingDiagnostics::default())),
            &BTreeMap::new(),
            &invocation.command,
        )
        .await
        .expect("grep is handled")
        .unwrap();

        assert_eq!(human_text(dispatched), "No matching tools found.\n");
        assert!(manager.acquired().is_empty());

        let non_grep = CommandSpec::List {
            with_descriptions: false,
        };
        assert!(
            dispatch_grep_command(
                &handler,
                &context(Arc::new(RecordingDiagnostics::default())),
                &BTreeMap::new(),
                &non_grep,
            )
            .await
            .is_none()
        );
    }
}
