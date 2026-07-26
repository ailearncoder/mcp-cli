#![forbid(unsafe_code)]

#[path = "../support/mod.rs"]
mod support;

use std::{
    collections::BTreeMap,
    future::pending,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use mcp_cli::{
    BoxFuture, CancellationFlag, ClassifyError, CommandContext, ConfigHash, ConnectionError,
    ConnectionManager, ConnectionMode, ConnectionResourceRegistry, DirectConnectionManager,
    ErrorClass, JsonObject, McpConnection, RetryPolicy, ServerDefinition, ServerId,
    ToolFilterConfig, ToolInfo, ToolResult, TransportConfig,
};
use proptest::prelude::*;
use serde_json::json;
use support::{FakeClock, FixedJitter, MockConnector, RecordingDiagnosticSink};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandResourceShape {
    DirectHttp,
    DirectStdio,
    DaemonIpcClient,
}

impl CommandResourceShape {
    const ALL: [Self; 3] = [Self::DirectHttp, Self::DirectStdio, Self::DaemonIpcClient];

    const fn mode(self) -> ConnectionMode {
        match self {
            Self::DirectHttp | Self::DirectStdio => ConnectionMode::Direct,
            Self::DaemonIpcClient => ConnectionMode::Daemon,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminationPath {
    Success,
    TypedFailure,
    Deadline,
    Cancellation,
}

impl TerminationPath {
    const ALL: [Self; 4] = [
        Self::Success,
        Self::TypedFailure,
        Self::Deadline,
        Self::Cancellation,
    ];
}

#[derive(Clone, Copy, Debug)]
enum Operation {
    ListTools,
    CallTool,
}

#[derive(Clone, Debug)]
struct ScenarioParameters {
    operation: Operation,
    close_fails: bool,
    deadline_in_flight: bool,
    budget_millis: u64,
    argument_value: i32,
    resource_rotation: usize,
    termination_rotation: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ResourceOracle {
    direct_sessions: usize,
    stdio_children: usize,
    ipc_clients: usize,
    daemon_worker_backends: usize,
}

impl ResourceOracle {
    // This oracle is intentionally independent of ConnectionResourceRegistry:
    // it derives ownership solely from the generated command shape.
    const fn for_shape(shape: CommandResourceShape) -> Self {
        match shape {
            CommandResourceShape::DirectHttp => Self {
                direct_sessions: 1,
                stdio_children: 0,
                ipc_clients: 0,
                daemon_worker_backends: 0,
            },
            CommandResourceShape::DirectStdio => Self {
                direct_sessions: 1,
                stdio_children: 1,
                ipc_clients: 0,
                daemon_worker_backends: 0,
            },
            CommandResourceShape::DaemonIpcClient => Self {
                direct_sessions: 0,
                stdio_children: 0,
                ipc_clients: 1,
                // The reusable worker connection exists, but is not owned by
                // this command and must never enter the command registry.
                daemon_worker_backends: 1,
            },
        }
    }

    const fn command_owned(self) -> usize {
        self.direct_sessions + self.stdio_children + self.ipc_clients
    }
}

#[derive(Default)]
struct ResourceTrace {
    direct_sessions: AtomicUsize,
    stdio_children: AtomicUsize,
    ipc_clients: AtomicUsize,
    daemon_worker_backends: AtomicUsize,
    operation_started: AtomicUsize,
    close_calls: AtomicUsize,
    dropped_without_close: AtomicUsize,
    operation_started_signal: tokio::sync::Notify,
}

impl ResourceTrace {
    fn from_oracle(oracle: ResourceOracle) -> Self {
        Self {
            direct_sessions: AtomicUsize::new(oracle.direct_sessions),
            stdio_children: AtomicUsize::new(oracle.stdio_children),
            ipc_clients: AtomicUsize::new(oracle.ipc_clients),
            daemon_worker_backends: AtomicUsize::new(oracle.daemon_worker_backends),
            ..Self::default()
        }
    }

    fn command_owned_count(&self) -> usize {
        self.direct_sessions.load(Ordering::SeqCst)
            + self.stdio_children.load(Ordering::SeqCst)
            + self.ipc_clients.load(Ordering::SeqCst)
    }

    fn start_operation(&self) {
        self.operation_started.fetch_add(1, Ordering::SeqCst);
        self.operation_started_signal.notify_one();
    }

    fn finish_command_cleanup(&self) {
        self.direct_sessions.store(0, Ordering::SeqCst);
        self.stdio_children.store(0, Ordering::SeqCst);
        self.ipc_clients.store(0, Ordering::SeqCst);
    }
}

struct InstrumentedConnection {
    shape: CommandResourceShape,
    termination: TerminationPath,
    close_fails: bool,
    trace: Arc<ResourceTrace>,
    closed: AtomicBool,
}

impl InstrumentedConnection {
    fn operation_result<T: Send + 'static>(
        &self,
        success: T,
    ) -> BoxFuture<'static, Result<T, ConnectionError>> {
        self.trace.start_operation();
        match self.termination {
            TerminationPath::Success => Box::pin(async move { Ok(success) }),
            TerminationPath::TypedFailure => Box::pin(async {
                Err(ConnectionError::new("typed operation failure")
                    .with_class(ErrorClass::Business))
            }),
            TerminationPath::Deadline | TerminationPath::Cancellation => Box::pin(pending()),
        }
    }
}

impl McpConnection for InstrumentedConnection {
    fn list_tools<'a>(
        &'a self,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
        self.operation_result(vec![ToolInfo {
            name: "resource-check".to_owned(),
            description: None,
            input_schema: json!({"type": "object"}),
        }])
    }

    fn call_tool<'a>(
        &'a self,
        _ctx: &'a CommandContext,
        _name: &'a str,
        args: JsonObject,
    ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
        self.operation_result(json!({"args": args}))
    }

    fn instructions(&self) -> Option<&str> {
        None
    }

    fn close<'a>(
        self: Box<Self>,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<(), ConnectionError>> {
        self.trace.close_calls.fetch_add(1, Ordering::SeqCst);
        self.closed.store(true, Ordering::SeqCst);
        self.trace.finish_command_cleanup();
        let close_fails = self.close_fails;
        Box::pin(async move {
            if close_fails {
                Err(ConnectionError::new("typed cleanup failure"))
            } else {
                Ok(())
            }
        })
    }

    fn mode(&self) -> ConnectionMode {
        self.shape.mode()
    }
}

impl Drop for InstrumentedConnection {
    fn drop(&mut self) {
        if !self.closed.load(Ordering::SeqCst) {
            self.trace
                .dropped_without_close
                .fetch_add(1, Ordering::SeqCst);
        }
    }
}

fn test_epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn server() -> ServerDefinition {
    ServerDefinition {
        name: "resource-property".to_owned(),
        id: ServerId("d".repeat(64)),
        config_hash: ConfigHash([4; 32]),
        transport: TransportConfig::Stdio {
            command: "unused-in-memory-connector".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        },
        filter: ToolFilterConfig::default(),
    }
}

async fn invoke_operation(
    connection: &dyn McpConnection,
    context: &CommandContext,
    operation: Operation,
    argument_value: i32,
) -> Result<(), ConnectionError> {
    match operation {
        Operation::ListTools => connection.list_tools(context).await.map(|_| ()),
        Operation::CallTool => {
            let mut args = JsonObject::new();
            args.insert("value".to_owned(), json!(argument_value));
            connection
                .call_tool(context, "resource-check", args)
                .await
                .map(|_| ())
        }
    }
}

fn rotated<T: Copy, const N: usize>(values: [T; N], rotation: usize) -> [T; N] {
    std::array::from_fn(|index| values[(index + rotation) % N])
}

async fn run_scenario(
    parameters: &ScenarioParameters,
    shape: CommandResourceShape,
    termination: TerminationPath,
) -> Result<(), TestCaseError> {
    let oracle = ResourceOracle::for_shape(shape);
    let clock = Arc::new(FakeClock::new(test_epoch()));
    let cancellation = Arc::new(CancellationFlag::default());
    let context = CommandContext {
        deadline: mcp_cli::Deadline::after(
            clock.as_ref(),
            Duration::from_millis(parameters.budget_millis),
        ),
        cancellation: cancellation.clone(),
        diagnostics: Arc::new(RecordingDiagnosticSink::default()),
    };
    let trace = Arc::new(ResourceTrace::from_oracle(oracle));
    let raw_connection = InstrumentedConnection {
        shape,
        termination,
        close_fails: parameters.close_fails,
        trace: Arc::clone(&trace),
        closed: AtomicBool::new(false),
    };
    let registry = ConnectionResourceRegistry::new();

    let connection: Box<dyn McpConnection> = match shape {
        CommandResourceShape::DirectHttp | CommandResourceShape::DirectStdio => {
            let connector = Arc::new(MockConnector::new());
            connector.queue_connection(raw_connection);
            let manager = DirectConnectionManager::with_retry_components(
                connector,
                registry.clone(),
                RetryPolicy::new(0, Duration::ZERO),
                clock.clone(),
                Box::new(FixedJitter::new(10_000)),
            );
            manager
                .acquire(&context, &server())
                .await
                .map_err(|error| TestCaseError::fail(format!("direct acquire failed: {error:?}")))?
        }
        CommandResourceShape::DaemonIpcClient => registry
            .register_connection_with_retry_components(
                &context,
                Box::new(raw_connection),
                RetryPolicy::new(0, Duration::ZERO),
                clock.clone(),
                Box::new(FixedJitter::new(10_000)),
            ),
    };

    prop_assert_eq!(registry.active_resource_count(), 1);
    prop_assert_eq!(trace.command_owned_count(), oracle.command_owned());
    prop_assert_eq!(
        trace.daemon_worker_backends.load(Ordering::SeqCst),
        oracle.daemon_worker_backends
    );
    if shape == CommandResourceShape::DaemonIpcClient {
        prop_assert_eq!(registry.active_resource_count(), oracle.ipc_clients);
        prop_assert_eq!(oracle.daemon_worker_backends, 1);
    }

    if termination == TerminationPath::Deadline && !parameters.deadline_in_flight {
        clock.advance(Duration::from_millis(parameters.budget_millis));
    }
    if termination == TerminationPath::Cancellation {
        // Pre-cancellation avoids scheduler- or wall-clock-based polling. The
        // property still verifies cancellation-triggered automatic cleanup.
        cancellation.cancel();
    }

    let operation_result =
        if termination == TerminationPath::Deadline && parameters.deadline_in_flight {
            let (result, ()) = tokio::join!(
                invoke_operation(
                    connection.as_ref(),
                    &context,
                    parameters.operation,
                    parameters.argument_value,
                ),
                async {
                    trace.operation_started_signal.notified().await;
                    clock.advance(Duration::from_millis(parameters.budget_millis));
                }
            );
            result
        } else {
            invoke_operation(
                connection.as_ref(),
                &context,
                parameters.operation,
                parameters.argument_value,
            )
            .await
        };

    match termination {
        TerminationPath::Success => prop_assert!(operation_result.is_ok()),
        TerminationPath::TypedFailure => {
            let error = operation_result.expect_err("typed failure path must fail");
            prop_assert_eq!(error.class(), ErrorClass::Business);
            prop_assert_eq!(error.message(), "typed operation failure");
        }
        TerminationPath::Deadline => {
            prop_assert!(
                operation_result
                    .expect_err("deadline path must fail")
                    .is_timeout()
            );
        }
        TerminationPath::Cancellation => {
            prop_assert!(
                operation_result
                    .expect_err("cancellation path must fail")
                    .is_cancelled()
            );
        }
    }

    let close_result = connection.close(&context).await;
    if termination == TerminationPath::Success && parameters.close_fails {
        prop_assert!(close_result.is_err());
    } else {
        prop_assert!(close_result.is_ok());
    }

    let expected_operation_calls = usize::from(
        (parameters.deadline_in_flight || termination != TerminationPath::Deadline)
            && termination != TerminationPath::Cancellation,
    );
    prop_assert_eq!(
        trace.operation_started.load(Ordering::SeqCst),
        expected_operation_calls
    );
    prop_assert_eq!(trace.close_calls.load(Ordering::SeqCst), 1);
    prop_assert_eq!(trace.dropped_without_close.load(Ordering::SeqCst), 0);
    prop_assert_eq!(registry.best_effort_cleanup_count(), 0);
    prop_assert_eq!(registry.active_resource_count(), 0);
    prop_assert_eq!(trace.direct_sessions.load(Ordering::SeqCst), 0);
    prop_assert_eq!(trace.stdio_children.load(Ordering::SeqCst), 0);
    prop_assert_eq!(trace.ipc_clients.load(Ordering::SeqCst), 0);
    prop_assert_eq!(trace.command_owned_count(), 0);
    prop_assert_eq!(
        trace.daemon_worker_backends.load(Ordering::SeqCst),
        oracle.daemon_worker_backends,
        "closing a command IPC client must not close the daemon worker backend"
    );

    Ok(())
}

fn scenario_parameters() -> impl Strategy<Value = ScenarioParameters> {
    (
        any::<bool>(),
        any::<bool>(),
        1_u64..=10_000,
        any::<i32>(),
        0_usize..CommandResourceShape::ALL.len(),
        0_usize..TerminationPath::ALL.len(),
    )
        .prop_map(
            |(
                call_tool,
                close_fails,
                budget_millis,
                argument_value,
                resource_rotation,
                termination_rotation,
            )| ScenarioParameters {
                operation: if call_tool {
                    Operation::CallTool
                } else {
                    Operation::ListTools
                },
                close_fails,
                deadline_in_flight: argument_value & 1 == 0,
                budget_millis,
                argument_value,
                resource_rotation,
                termination_rotation,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 34: 命令拥有资源最终关闭
    // **Validates: Requirements 14.6**
    #[test]
    fn property_34_command_owned_resources_are_eventually_closed(
        parameters in scenario_parameters()
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("property runtime");

        // Every generated case executes the complete 3 resource x 4
        // termination matrix. Generated rotations vary ordering while keeping
        // all required paths present in every one of the 128 proptest cases.
        for shape in rotated(CommandResourceShape::ALL, parameters.resource_rotation) {
            for termination in rotated(TerminationPath::ALL, parameters.termination_rotation) {
                runtime.block_on(run_scenario(&parameters, shape, termination))?;
            }
        }
    }
}
