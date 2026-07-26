//! Command handlers and bounded batch execution.

use std::{collections::BTreeMap, num::NonZeroUsize, sync::Arc};

use futures::{StreamExt, stream::FuturesUnordered};
use tokio::sync::Semaphore;

use crate::{
    cli::CommandSpec,
    config::ServerDefinition,
    connection::{ConnectionResourceRegistry, DirectConnector},
    domain::{CommandOutcome, PerServer},
    error::CliError,
    runtime::{BoxFuture, CommandContext, RuntimeConfig},
};

pub mod call;
pub mod grep;
pub mod info;
pub mod list;

/// One scheduled server operation, including its server-associated result.
pub type ServerBatchTask<'a, T> = BoxFuture<'a, PerServer<T>>;

/// Injectable scheduling boundary for deterministic batch tests.
///
/// Production scheduling uses [`FuturesUnordered`] through
/// [`InlineBatchScheduler`]. Tests may wrap each task to record creation order
/// or hold its first poll behind a controllable gate without changing batch
/// execution semantics.
pub trait ServerBatchScheduler<T>: Send + Sync {
    fn schedule<'a>(
        &'a self,
        server: &'a str,
        task: ServerBatchTask<'a, T>,
    ) -> ServerBatchTask<'a, T>;
}

/// Default scheduler. Tasks are driven concurrently by the batch executor.
#[derive(Clone, Copy, Debug, Default)]
pub struct InlineBatchScheduler;

impl<T: Send> ServerBatchScheduler<T> for InlineBatchScheduler {
    fn schedule<'a>(
        &'a self,
        _server: &'a str,
        task: ServerBatchTask<'a, T>,
    ) -> ServerBatchTask<'a, T> {
        task
    }
}

/// Executes one operation for every configured server with bounded concurrency.
///
/// Servers are scheduled in `BTreeMap` name order. Every operation receives
/// the exact same [`CommandContext`] reference, and therefore the same absolute
/// command deadline, cancellation token, and diagnostic sink. A per-server
/// error is retained as [`PerServer::Failure`] and never cancels or discards
/// another server's operation. Returned results are sorted by server name so
/// callers remain independent of asynchronous completion order.
pub async fn execute_bounded_server_batch<T, Execute>(
    ctx: &CommandContext,
    servers: &BTreeMap<String, ServerDefinition>,
    concurrency: NonZeroUsize,
    execute: Execute,
) -> Vec<PerServer<T>>
where
    T: Send,
    Execute: for<'a> Fn(&'a CommandContext, &'a ServerDefinition) -> BoxFuture<'a, Result<T, CliError>>
        + Send
        + Sync,
{
    execute_bounded_server_batch_with(ctx, servers, concurrency, &InlineBatchScheduler, execute)
        .await
}

/// Variant of [`execute_bounded_server_batch`] with an injectable scheduler.
pub async fn execute_bounded_server_batch_with<T, Execute, Scheduler>(
    ctx: &CommandContext,
    servers: &BTreeMap<String, ServerDefinition>,
    concurrency: NonZeroUsize,
    scheduler: &Scheduler,
    execute: Execute,
) -> Vec<PerServer<T>>
where
    T: Send,
    Execute: for<'a> Fn(&'a CommandContext, &'a ServerDefinition) -> BoxFuture<'a, Result<T, CliError>>
        + Send
        + Sync,
    Scheduler: ServerBatchScheduler<T>,
{
    if servers.is_empty() {
        return Vec::new();
    }

    // A configured limit may legally exceed both the number of servers and
    // Tokio's representable permit count. Capping it to useful work preserves
    // the requested upper-bound semantics and avoids a constructor panic.
    let effective_limit = concurrency
        .get()
        .min(servers.len())
        .min(Semaphore::MAX_PERMITS);
    let semaphore = Arc::new(Semaphore::new(effective_limit));
    let mut tasks = FuturesUnordered::new();

    for (server_name, server) in servers {
        let semaphore = Arc::clone(&semaphore);
        let execute = &execute;
        let associated_server = server_name.clone();
        let task: ServerBatchTask<'_, T> = Box::pin(async move {
            let permit = semaphore
                .acquire_owned()
                .await
                .expect("the batch-owned semaphore is never closed");
            let result = execute(ctx, server).await;
            drop(permit);

            match result {
                Ok(value) => PerServer::Success {
                    server: associated_server,
                    value,
                },
                Err(error) => PerServer::Failure {
                    server: associated_server,
                    error,
                },
            }
        });
        tasks.push(scheduler.schedule(server_name, task));
    }

    let mut results = Vec::with_capacity(servers.len());
    while let Some(result) = tasks.next().await {
        results.push(result);
    }
    results.sort_by(|left, right| left.server().cmp(right.server()));
    results
}

/// Complete public-command dispatcher used by the process boundary.
///
/// It owns one handler for each command while preserving one connector,
/// resource registry, runtime policy, and [`CommandContext`] across the
/// selected route. Help and version are intentionally excluded: `main`
/// resolves them before runtime environment parsing or configuration I/O.
pub struct CommandDispatcher {
    list: list::ListHandler,
    info: info::InfoHandler,
    grep: grep::GrepHandler,
    call: call::CallHandler,
}

impl CommandDispatcher {
    pub fn new(
        list: list::ListHandler,
        info: info::InfoHandler,
        grep: grep::GrepHandler,
        call: call::CallHandler,
    ) -> Self {
        Self {
            list,
            info,
            grep,
            call,
        }
    }

    /// Builds all four handlers around one shared connection manager. Mode
    /// selection remains entirely below the command layer.
    pub fn managed(
        connections: Arc<dyn crate::connection::ConnectionManager>,
        runtime: &RuntimeConfig,
    ) -> Self {
        Self::new(
            list::ListHandler::new(Arc::clone(&connections), runtime.concurrency),
            info::InfoHandler::new(Arc::clone(&connections)),
            grep::GrepHandler::new(Arc::clone(&connections), runtime.concurrency),
            call::CallHandler::new(connections),
        )
    }

    /// Retained direct-only constructor for focused compatibility tests.
    pub fn direct(
        connector: Arc<dyn DirectConnector>,
        resources: ConnectionResourceRegistry,
        runtime: &RuntimeConfig,
    ) -> Self {
        Self::new(
            list::ListHandler::direct(Arc::clone(&connector), resources.clone(), runtime),
            info::InfoHandler::direct(Arc::clone(&connector), resources.clone(), runtime),
            grep::GrepHandler::direct(Arc::clone(&connector), resources.clone(), runtime),
            call::CallHandler::direct(connector, resources, runtime),
        )
    }

    pub async fn dispatch<R: std::io::Read>(
        &self,
        ctx: &CommandContext,
        servers: &BTreeMap<String, ServerDefinition>,
        command: &CommandSpec,
        input: &mut call::CallInput<R>,
    ) -> Result<CommandOutcome, CliError> {
        match command {
            CommandSpec::List { with_descriptions } => {
                self.list.execute(ctx, servers, *with_descriptions).await
            }
            CommandSpec::Info {
                server,
                tool,
                with_descriptions,
            } => {
                self.info
                    .execute(ctx, servers, server, tool.as_deref(), *with_descriptions)
                    .await
            }
            CommandSpec::Grep {
                pattern,
                with_descriptions,
            } => {
                self.grep
                    .execute(ctx, servers, pattern, *with_descriptions)
                    .await
            }
            CommandSpec::Call {
                server,
                tool,
                inline_json,
            } => {
                self.call
                    .execute(ctx, servers, server, tool, inline_json.as_deref(), input)
                    .await
            }
            CommandSpec::Help | CommandSpec::Version => Err(CliError::invalid_arguments(
                "Help and version are not business commands",
                "Help and version must be resolved before configuration loading",
            )),
        }
    }
}

/// Compatibility seam retained for focused list-handler tests.
pub async fn dispatch_list_command(
    handler: &list::ListHandler,
    ctx: &CommandContext,
    servers: &BTreeMap<String, ServerDefinition>,
    command: &CommandSpec,
) -> Option<Result<CommandOutcome, CliError>> {
    match command {
        CommandSpec::List { with_descriptions } => {
            Some(handler.execute(ctx, servers, *with_descriptions).await)
        }
        CommandSpec::Info { .. }
        | CommandSpec::Grep { .. }
        | CommandSpec::Call { .. }
        | CommandSpec::Help
        | CommandSpec::Version => None,
    }
}

/// Minimal info dispatch seam used until the full four-command dispatcher is
/// introduced by task 6.14. All accepted info target syntaxes have already
/// normalized to one [`CommandSpec::Info`] value before reaching this function.
pub async fn dispatch_info_command(
    handler: &info::InfoHandler,
    ctx: &CommandContext,
    servers: &BTreeMap<String, ServerDefinition>,
    command: &CommandSpec,
) -> Option<Result<CommandOutcome, CliError>> {
    match command {
        CommandSpec::Info {
            server,
            tool,
            with_descriptions,
        } => Some(
            handler
                .execute(ctx, servers, server, tool.as_deref(), *with_descriptions)
                .await,
        ),
        CommandSpec::List { .. }
        | CommandSpec::Grep { .. }
        | CommandSpec::Call { .. }
        | CommandSpec::Help
        | CommandSpec::Version => None,
    }
}

/// Minimal grep dispatch seam used until task 6.14 introduces the complete
/// process dispatcher. It claims only normalized [`CommandSpec::Grep`] values.
pub async fn dispatch_grep_command(
    handler: &grep::GrepHandler,
    ctx: &CommandContext,
    servers: &BTreeMap<String, ServerDefinition>,
    command: &CommandSpec,
) -> Option<Result<CommandOutcome, CliError>> {
    match command {
        CommandSpec::Grep {
            pattern,
            with_descriptions,
        } => Some(
            handler
                .execute(ctx, servers, pattern, *with_descriptions)
                .await,
        ),
        CommandSpec::List { .. }
        | CommandSpec::Info { .. }
        | CommandSpec::Call { .. }
        | CommandSpec::Help
        | CommandSpec::Version => None,
    }
}

/// Minimal call dispatch seam used until task 6.14 introduces the complete
/// process dispatcher. Both split and slash CLI forms have already normalized
/// to one [`CommandSpec::Call`], and this seam owns only stdin selection plus
/// delegation to the call handler.
pub async fn dispatch_call_command<R: std::io::Read>(
    handler: &call::CallHandler,
    ctx: &CommandContext,
    servers: &BTreeMap<String, ServerDefinition>,
    command: &CommandSpec,
    input: &mut call::CallInput<R>,
) -> Option<Result<CommandOutcome, CliError>> {
    match command {
        CommandSpec::Call {
            server,
            tool,
            inline_json,
        } => Some(
            handler
                .execute(ctx, servers, server, tool, inline_json.as_deref(), input)
                .await,
        ),
        CommandSpec::List { .. }
        | CommandSpec::Info { .. }
        | CommandSpec::Grep { .. }
        | CommandSpec::Help
        | CommandSpec::Version => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        num::NonZeroUsize,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use tokio::sync::{Semaphore, mpsc};

    use super::*;
    use crate::{
        config::{ConfigHash, ServerId, ToolFilterConfig, TransportConfig},
        connection::{ConnectionError, ConnectionManager, McpConnection},
        domain::{ConnectionMode, JsonObject, ToolInfo, ToolResult},
        error::{ErrorKind, ExitCode},
        output::DiagnosticSink,
        runtime::{CancellationFlag, Deadline},
    };

    #[derive(Debug, Default)]
    struct NullDiagnostics;

    impl DiagnosticSink for NullDiagnostics {
        fn warning(&self, _message: &str) {}
        fn debug(&self, _message: &str) {}
        fn server_stderr(&self, _server: &str, _bytes: &[u8]) {}
    }

    #[derive(Default)]
    struct RecordingScheduler {
        servers: Mutex<Vec<String>>,
    }

    impl<T: Send> ServerBatchScheduler<T> for RecordingScheduler {
        fn schedule<'a>(
            &'a self,
            server: &'a str,
            task: ServerBatchTask<'a, T>,
        ) -> ServerBatchTask<'a, T> {
            self.servers
                .lock()
                .expect("scheduler mutex")
                .push(server.to_owned());
            task
        }
    }

    fn context(deadline: Deadline) -> CommandContext {
        CommandContext {
            deadline,
            cancellation: Arc::new(CancellationFlag::default()),
            diagnostics: Arc::new(NullDiagnostics),
        }
    }

    fn servers(names: &[&str]) -> BTreeMap<String, ServerDefinition> {
        names
            .iter()
            .map(|name| {
                let name = (*name).to_owned();
                (
                    name.clone(),
                    ServerDefinition {
                        name,
                        id: ServerId("0".repeat(64)),
                        config_hash: ConfigHash([0; 32]),
                        transport: TransportConfig::Stdio {
                            command: "test-server".into(),
                            args: Vec::new(),
                            env: BTreeMap::new(),
                            cwd: None,
                        },
                        filter: ToolFilterConfig::default(),
                    },
                )
            })
            .collect()
    }

    fn update_peak(peak: &AtomicUsize, active: usize) {
        let mut observed = peak.load(Ordering::SeqCst);
        while active > observed {
            match peak.compare_exchange(observed, active, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => break,
                Err(actual) => observed = actual,
            }
        }
    }

    #[derive(Default)]
    struct RoutingTrace {
        acquired: Mutex<Vec<String>>,
        operations: Mutex<Vec<&'static str>>,
        context_mismatch: std::sync::atomic::AtomicBool,
    }

    struct RoutingManager {
        trace: Arc<RoutingTrace>,
    }

    impl ConnectionManager for RoutingManager {
        fn acquire<'a>(
            &'a self,
            ctx: &'a CommandContext,
            server: &'a ServerDefinition,
        ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, CliError>> {
            self.trace
                .acquired
                .lock()
                .expect("acquired lock")
                .push(server.name.clone());
            let connection = RoutingConnection {
                trace: Arc::clone(&self.trace),
                context: ctx.clone(),
            };
            Box::pin(async move { Ok(Box::new(connection) as Box<dyn McpConnection>) })
        }
    }

    struct RoutingConnection {
        trace: Arc<RoutingTrace>,
        context: CommandContext,
    }

    impl RoutingConnection {
        fn observe(&self, ctx: &CommandContext) {
            if self.context.deadline != ctx.deadline
                || !Arc::ptr_eq(&self.context.cancellation, &ctx.cancellation)
                || !Arc::ptr_eq(&self.context.diagnostics, &ctx.diagnostics)
            {
                self.trace.context_mismatch.store(true, Ordering::SeqCst);
            }
        }
    }

    impl McpConnection for RoutingConnection {
        fn list_tools<'a>(
            &'a self,
            ctx: &'a CommandContext,
        ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
            self.observe(ctx);
            self.trace
                .operations
                .lock()
                .expect("operations lock")
                .push("list_tools");
            Box::pin(async {
                Ok(vec![ToolInfo {
                    name: "echo".to_owned(),
                    description: Some("Echo arguments".to_owned()),
                    input_schema: serde_json::json!({"type": "object"}),
                }])
            })
        }

        fn call_tool<'a>(
            &'a self,
            ctx: &'a CommandContext,
            name: &'a str,
            args: JsonObject,
        ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
            self.observe(ctx);
            self.trace
                .operations
                .lock()
                .expect("operations lock")
                .push("call_tool");
            let name = name.to_owned();
            Box::pin(async move { Ok(serde_json::json!({"tool": name, "args": args})) })
        }

        fn instructions(&self) -> Option<&str> {
            Some("routing instructions")
        }

        fn close<'a>(
            self: Box<Self>,
            ctx: &'a CommandContext,
        ) -> BoxFuture<'a, Result<(), ConnectionError>> {
            self.observe(ctx);
            self.trace
                .operations
                .lock()
                .expect("operations lock")
                .push("close");
            Box::pin(async { Ok(()) })
        }

        fn mode(&self) -> ConnectionMode {
            ConnectionMode::Direct
        }
    }

    #[tokio::test]
    async fn batch_bounds_peak_runs_each_server_once_and_isolates_failures() {
        let ctx = context(Deadline::new(Instant::now() + Duration::from_secs(60)));
        let servers = servers(&["delta", "alpha", "charlie", "beta"]);
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let starts = Arc::new(Mutex::new(BTreeMap::<String, usize>::new()));
        let gate = Arc::new(Semaphore::new(0));
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();

        let active_for_batch = Arc::clone(&active);
        let peak_for_batch = Arc::clone(&peak);
        let starts_for_batch = Arc::clone(&starts);
        let gate_for_batch = Arc::clone(&gate);
        let batch = tokio::spawn(async move {
            execute_bounded_server_batch(
                &ctx,
                &servers,
                NonZeroUsize::new(2).expect("non-zero"),
                move |_ctx, server| {
                    let active = Arc::clone(&active_for_batch);
                    let peak = Arc::clone(&peak_for_batch);
                    let starts = Arc::clone(&starts_for_batch);
                    let gate = Arc::clone(&gate_for_batch);
                    let started_tx = started_tx.clone();
                    let server_name = server.name.clone();
                    Box::pin(async move {
                        *starts
                            .lock()
                            .expect("starts mutex")
                            .entry(server_name.clone())
                            .or_default() += 1;
                        let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
                        update_peak(&peak, now_active);
                        started_tx.send(server_name.clone()).expect("test receiver");
                        let permit = gate.acquire().await.expect("test gate remains open");
                        permit.forget();
                        active.fetch_sub(1, Ordering::SeqCst);

                        if server_name == "beta" {
                            Err(CliError::new(
                                ErrorKind::NetworkError,
                                "expected isolated failure",
                                ExitCode::Network,
                            ))
                        } else {
                            Ok(server_name)
                        }
                    })
                },
            )
            .await
        });

        started_rx.recv().await.expect("first task starts");
        started_rx.recv().await.expect("second task starts");
        assert!(
            started_rx.try_recv().is_err(),
            "a third task exceeded limit"
        );

        gate.add_permits(1);
        started_rx.recv().await.expect("third task starts");
        gate.add_permits(1);
        started_rx.recv().await.expect("fourth task starts");
        gate.add_permits(2);

        let results = batch.await.expect("batch task does not panic");
        assert!(peak.load(Ordering::SeqCst) <= 2);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(
            *starts.lock().expect("starts mutex"),
            BTreeMap::from([
                ("alpha".into(), 1),
                ("beta".into(), 1),
                ("charlie".into(), 1),
                ("delta".into(), 1),
            ])
        );
        assert_eq!(results.len(), 4);
        assert_eq!(
            results.iter().map(PerServer::server).collect::<Vec<_>>(),
            vec!["alpha", "beta", "charlie", "delta"]
        );
        assert!(matches!(
            &results[1],
            PerServer::Failure { server, error }
                if server == "beta" && error.kind == ErrorKind::NetworkError
        ));
        assert!(
            results.iter().enumerate().all(|(index, result)| {
                index == 1 || matches!(result, PerServer::Success { .. })
            })
        );
    }

    #[tokio::test]
    async fn batch_shares_one_context_and_deadline_and_schedules_by_server_name() {
        let deadline = Deadline::new(Instant::now() + Duration::from_secs(30));
        let ctx = context(deadline);
        let expected_context = (&ctx as *const CommandContext) as usize;
        let observed = Arc::new(Mutex::new(Vec::new()));
        let scheduler = RecordingScheduler::default();
        let servers = servers(&["zeta", "alpha", "middle"]);

        let observed_by_tasks = Arc::clone(&observed);
        let results = execute_bounded_server_batch_with(
            &ctx,
            &servers,
            NonZeroUsize::new(3).expect("non-zero"),
            &scheduler,
            move |task_context, server| {
                let observed = Arc::clone(&observed_by_tasks);
                let server_name = server.name.clone();
                Box::pin(async move {
                    observed.lock().expect("observations mutex").push((
                        server_name.clone(),
                        (task_context as *const CommandContext) as usize,
                        task_context.deadline,
                    ));
                    Ok(server_name)
                })
            },
        )
        .await;

        assert_eq!(results.len(), 3);
        assert_eq!(
            *scheduler.servers.lock().expect("scheduler mutex"),
            vec!["alpha", "middle", "zeta"]
        );
        let observations = observed.lock().expect("observations mutex");
        assert_eq!(observations.len(), 3);
        assert!(observations.iter().all(|(_, address, task_deadline)| {
            *address == expected_context && *task_deadline == deadline
        }));
    }

    #[tokio::test]
    async fn complete_dispatcher_routes_all_commands_with_one_shared_context() {
        use std::io::Cursor;

        let ctx = context(Deadline::new(Instant::now() + Duration::from_secs(30)));
        let configured = servers(&["fixture"]);
        let trace = Arc::new(RoutingTrace::default());
        let manager = Arc::new(RoutingManager {
            trace: Arc::clone(&trace),
        });
        let connections: Arc<dyn ConnectionManager> = manager;
        let runtime = RuntimeConfig {
            concurrency: NonZeroUsize::new(1).unwrap(),
            ..RuntimeConfig::default()
        };
        let dispatcher = CommandDispatcher::managed(connections, &runtime);
        let mut input = call::CallInput::new(Cursor::new(Vec::<u8>::new()), true);

        let list = dispatcher
            .dispatch(
                &ctx,
                &configured,
                &CommandSpec::List {
                    with_descriptions: false,
                },
                &mut input,
            )
            .await
            .unwrap();
        assert!(matches!(list, CommandOutcome::HumanText(text) if text.contains("fixture")));

        for command in [
            CommandSpec::Info {
                server: "fixture".to_owned(),
                tool: None,
                with_descriptions: false,
            },
            CommandSpec::Info {
                server: "fixture".to_owned(),
                tool: Some("echo".to_owned()),
                with_descriptions: false,
            },
        ] {
            dispatcher
                .dispatch(&ctx, &configured, &command, &mut input)
                .await
                .unwrap();
        }

        let grep = dispatcher
            .dispatch(
                &ctx,
                &configured,
                &CommandSpec::Grep {
                    pattern: "e*".to_owned(),
                    with_descriptions: false,
                },
                &mut input,
            )
            .await
            .unwrap();
        assert!(matches!(grep, CommandOutcome::HumanText(text) if text == "fixture echo\n"));

        let call = dispatcher
            .dispatch(
                &ctx,
                &configured,
                &CommandSpec::Call {
                    server: "fixture".to_owned(),
                    tool: "echo".to_owned(),
                    inline_json: Some("{\"value\":1}".to_owned()),
                },
                &mut input,
            )
            .await
            .unwrap();
        assert_eq!(
            call,
            CommandOutcome::Json(serde_json::json!({
                "tool": "echo",
                "args": {"value": 1}
            }))
        );

        assert_eq!(
            trace.acquired.lock().expect("acquired lock").as_slice(),
            ["fixture", "fixture", "fixture", "fixture", "fixture"]
        );
        let operations = trace.operations.lock().expect("operations lock");
        assert_eq!(
            operations
                .iter()
                .filter(|operation| **operation == "list_tools")
                .count(),
            5
        );
        assert_eq!(
            operations
                .iter()
                .filter(|operation| **operation == "call_tool")
                .count(),
            1
        );
        assert_eq!(
            operations
                .iter()
                .filter(|operation| **operation == "close")
                .count(),
            5
        );
        assert!(!trace.context_mismatch.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn batch_handles_empty_single_and_oversized_legal_limits() {
        let ctx = context(Deadline::new(Instant::now() + Duration::from_secs(30)));
        let empty = BTreeMap::new();
        let empty_results = execute_bounded_server_batch(
            &ctx,
            &empty,
            NonZeroUsize::new(1).expect("non-zero"),
            |_ctx, _server| -> BoxFuture<'_, Result<String, CliError>> {
                panic!("empty batch must not create a task")
            },
        )
        .await;
        assert!(empty_results.is_empty());

        let servers = servers(&["only"]);
        let results = execute_bounded_server_batch(
            &ctx,
            &servers,
            NonZeroUsize::new(usize::MAX).expect("non-zero"),
            |_ctx, server| {
                let name = server.name.clone();
                Box::pin(async move { Ok(name) })
            },
        )
        .await;

        assert!(matches!(
            results.as_slice(),
            [PerServer::Success { server, value }] if server == "only" && value == "only"
        ));
    }
}
