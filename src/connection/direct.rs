//! Direct MCP connection lifecycle and command-owned resource tracking.

use std::{
    collections::BTreeMap,
    future::Future,
    num::NonZeroUsize,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::sync::Mutex;

use crate::{
    config::ServerDefinition,
    domain::{ConnectionMode, JsonObject, ToolInfo, ToolResult},
    error::CliError,
    policy::retry::{RetryError, RetryPolicy, retry},
    runtime::{BoxFuture, Clock, CommandContext, JitterSource, RuntimeConfig, SystemClock},
};

use super::{ConnectionError, ConnectionManager, DirectConnector, McpConnection};

/// Independent grace allowed for command-owned direct resource cleanup.
///
/// This exceeds each adapter's internal close bound (currently six seconds at
/// most) while remaining short and deterministic for shutdown paths.
const DIRECT_CLEANUP_GRACE: Duration = Duration::from_secs(8);

/// Builds the complete environment for a stdio server process.
///
/// `configured` is applied after `parent`, so server configuration wins when
/// both maps contain the same key. On Windows, environment names are
/// case-insensitive, so differently-cased parent collisions are removed before
/// configured values are applied. The function performs no process reads or
/// shell expansion; callers must capture the parent environment explicitly at
/// the process-launch boundary. Returning a [`BTreeMap`] gives process startup
/// one deterministic, independently testable environment source.
pub fn merge_stdio_environment(
    parent: &BTreeMap<String, String>,
    configured: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut merged = parent.clone();
    #[cfg(windows)]
    merged.retain(|parent_key, _| {
        !configured
            .keys()
            .any(|configured_key| configured_key.eq_ignore_ascii_case(parent_key))
    });
    merged.extend(
        configured
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    merged
}

/// Command-scoped accounting for connections owned by the current process.
///
/// A registry reservation is created before `connect` starts, so connection
/// limits also cover in-flight connects. A registered connection releases its
/// reservation only after explicit/automatic async close finishes or reaches
/// the bounded cleanup timeout. Drop schedules best-effort async close when a
/// Tokio runtime is available and keeps the reservation active until that task
/// releases ownership, so the count reflects cleanup that is still in flight.
#[derive(Clone, Default)]
pub struct ConnectionResourceRegistry {
    inner: Arc<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
    active_resources: AtomicUsize,
    best_effort_cleanup_triggers: AtomicUsize,
}

impl ConnectionResourceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of direct/IPC handles currently owned by this command.
    pub fn active_resource_count(&self) -> usize {
        self.inner.active_resources.load(Ordering::SeqCst)
    }

    /// Number of unclosed handles for which Drop initiated best-effort cleanup.
    pub fn best_effort_cleanup_count(&self) -> usize {
        self.inner
            .best_effort_cleanup_triggers
            .load(Ordering::SeqCst)
    }

    /// Registers an already-created connection and binds it to `ctx`.
    ///
    /// Direct managers normally reserve before connecting via
    /// [`DirectConnectionManager`]. This entry point supports adapters and
    /// tests that already own a connection.
    pub fn register_connection(
        &self,
        ctx: &CommandContext,
        connection: Box<dyn McpConnection>,
    ) -> Box<dyn McpConnection> {
        self.register_connection_with_retry_components(
            ctx,
            connection,
            RetryPolicy::default(),
            Arc::new(SystemClock),
            Box::new(SystemJitter::default()),
        )
    }

    /// Registers a daemon IPC client without retrying operations on that
    /// stream. The outer managed connection may switch to direct only after
    /// the daemon client has acknowledged cancellation and returned.
    pub fn register_connection_without_retry(
        &self,
        ctx: &CommandContext,
        connection: Box<dyn McpConnection>,
    ) -> Box<dyn McpConnection> {
        self.register_connection_with_retry_components(
            ctx,
            connection,
            RetryPolicy::new(0, Duration::ZERO),
            Arc::new(SystemClock),
            Box::new(SystemJitter::default()),
        )
    }

    /// Registers an already-created connection with deterministic runtime
    /// components. Adapters use this when command-owned handles (including
    /// daemon IPC clients) must share an injected clock with the command.
    pub fn register_connection_with_retry_components(
        &self,
        ctx: &CommandContext,
        connection: Box<dyn McpConnection>,
        policy: RetryPolicy,
        clock: Arc<dyn Clock>,
        jitter: Box<dyn JitterSource>,
    ) -> Box<dyn McpConnection> {
        let registration = self
            .try_reserve(usize::MAX)
            .expect("a process cannot hold usize::MAX command resources");
        Box::new(RegisteredConnection::new(
            connection,
            ctx.clone(),
            registration,
            Arc::new(DirectRetryRuntime::new(policy, clock, jitter)),
        ))
    }

    fn try_reserve(&self, maximum: usize) -> Option<ResourceRegistration> {
        let mut active = self.inner.active_resources.load(Ordering::SeqCst);
        loop {
            if active >= maximum {
                return None;
            }
            match self.inner.active_resources.compare_exchange(
                active,
                active + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    return Some(ResourceRegistration {
                        registry: self.clone(),
                        active: true,
                    });
                }
                Err(observed) => active = observed,
            }
        }
    }

    fn note_best_effort_cleanup(&self) {
        self.inner
            .best_effort_cleanup_triggers
            .fetch_add(1, Ordering::SeqCst);
    }
}

struct ResourceRegistration {
    registry: ConnectionResourceRegistry,
    active: bool,
}

impl ResourceRegistration {
    fn release(&mut self) {
        if self.active {
            self.active = false;
            self.registry
                .inner
                .active_resources
                .fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl Drop for ResourceRegistration {
    fn drop(&mut self) {
        self.release();
    }
}

struct DirectRetryRuntime {
    policy: RetryPolicy,
    clock: Arc<dyn Clock>,
    jitter: Mutex<Box<dyn JitterSource>>,
}

impl DirectRetryRuntime {
    fn new(policy: RetryPolicy, clock: Arc<dyn Clock>, jitter: Box<dyn JitterSource>) -> Self {
        Self {
            policy,
            clock,
            jitter: Mutex::new(jitter),
        }
    }
}

struct SystemJitter {
    state: u64,
}

impl Default for SystemJitter {
    fn default() -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        Self {
            state: seed ^ 0x9e37_79b9_7f4a_7c15,
        }
    }
}

impl JitterSource for SystemJitter {
    fn factor_basis_points(&mut self) -> u16 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        7_500 + (self.state % 5_001) as u16
    }
}

/// Direct-only manager used by single-target commands such as info and call.
///
/// The manager has no daemon branch and enforces its connection limit before
/// invoking the connector. `single_target` fixes that limit at one server.
pub struct DirectConnectionManager {
    connector: Arc<dyn DirectConnector>,
    resources: ConnectionResourceRegistry,
    connection_limit: NonZeroUsize,
    retry: Arc<DirectRetryRuntime>,
}

impl DirectConnectionManager {
    pub fn single_target(
        connector: Arc<dyn DirectConnector>,
        resources: ConnectionResourceRegistry,
    ) -> Self {
        Self::with_runtime_config(connector, resources, &RuntimeConfig::default())
    }

    /// Builds a direct-only manager for list/grep batch commands.
    ///
    /// The command's configured concurrency is also the maximum number of
    /// in-flight or acquired direct connections registered at once. The batch
    /// executor remains responsible for scheduling exactly one task per
    /// server, so both layers enforce the same upper bound.
    pub fn batch(
        connector: Arc<dyn DirectConnector>,
        resources: ConnectionResourceRegistry,
        config: &RuntimeConfig,
    ) -> Self {
        Self::with_runtime_config_and_limit(connector, resources, config, config.concurrency)
    }

    pub fn with_runtime_config(
        connector: Arc<dyn DirectConnector>,
        resources: ConnectionResourceRegistry,
        config: &RuntimeConfig,
    ) -> Self {
        Self::with_runtime_config_and_limit(
            connector,
            resources,
            config,
            NonZeroUsize::new(1).expect("one is non-zero"),
        )
    }

    fn with_runtime_config_and_limit(
        connector: Arc<dyn DirectConnector>,
        resources: ConnectionResourceRegistry,
        config: &RuntimeConfig,
        connection_limit: NonZeroUsize,
    ) -> Self {
        Self::with_retry_components_and_limit(
            connector,
            resources,
            RetryPolicy::from_runtime_config(config),
            Arc::new(SystemClock),
            Box::new(SystemJitter::default()),
            connection_limit,
        )
    }

    pub fn with_retry_components(
        connector: Arc<dyn DirectConnector>,
        resources: ConnectionResourceRegistry,
        policy: RetryPolicy,
        clock: Arc<dyn Clock>,
        jitter: Box<dyn JitterSource>,
    ) -> Self {
        Self::with_retry_components_and_limit(
            connector,
            resources,
            policy,
            clock,
            jitter,
            NonZeroUsize::new(1).expect("one is non-zero"),
        )
    }

    fn with_retry_components_and_limit(
        connector: Arc<dyn DirectConnector>,
        resources: ConnectionResourceRegistry,
        policy: RetryPolicy,
        clock: Arc<dyn Clock>,
        jitter: Box<dyn JitterSource>,
        connection_limit: NonZeroUsize,
    ) -> Self {
        Self {
            connector,
            resources,
            connection_limit,
            retry: Arc::new(DirectRetryRuntime::new(policy, clock, jitter)),
        }
    }

    pub fn resources(&self) -> &ConnectionResourceRegistry {
        &self.resources
    }
}

impl ConnectionManager for DirectConnectionManager {
    fn acquire<'a>(
        &'a self,
        ctx: &'a CommandContext,
        server: &'a ServerDefinition,
    ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, CliError>> {
        Box::pin(async move {
            let registration = self
                .resources
                .try_reserve(self.connection_limit.get())
                .ok_or_else(|| {
                    CliError::invalid_arguments(
                        "Single-target connection limit exceeded",
                        "info and call may own at most one server connection",
                    )
                })?;

            let mut jitter = self.retry.jitter.lock().await;
            let connection = retry(
                ctx,
                &self.retry.policy,
                self.retry.clock.as_ref(),
                jitter.as_mut(),
                |_| {
                    let connector = Arc::clone(&self.connector);
                    let server = server.clone();
                    async move {
                        await_connect_attempt(ctx, self.retry.clock.as_ref(), connector, server)
                            .await
                    }
                },
            )
            .await
            .map_err(|error| retry_error_to_cli(error, server, "connecting"))?;
            drop(jitter);

            if connection.mode() != ConnectionMode::Direct {
                let cleanup_result =
                    close_connection_bounded(connection, ctx, self.retry.clock.as_ref()).await;
                if cleanup_result.is_err() {
                    ctx.diagnostics
                        .debug("non-direct connector result cleanup did not complete cleanly");
                }
                return Err(CliError::invalid_arguments(
                    "Direct connector returned a non-direct connection",
                    "the direct-only path accepts only direct connections",
                ));
            }

            Ok(Box::new(RegisteredConnection::new(
                connection,
                ctx.clone(),
                registration,
                Arc::clone(&self.retry),
            )) as Box<dyn McpConnection>)
        })
    }
}

fn retry_error_to_cli(
    error: RetryError<ConnectionError>,
    server: &ServerDefinition,
    operation: &str,
) -> CliError {
    match error {
        RetryError::Timeout => CliError::timeout(operation),
        RetryError::Cancelled => CliError::cancelled(&server.name, operation),
        RetryError::Operation(error) if error.is_timeout() => CliError::timeout(operation),
        RetryError::Operation(error) if error.is_cancelled() => {
            CliError::cancelled(&server.name, operation)
        }
        RetryError::Operation(error) => {
            let cli_error = match error.http_status() {
                Some(status) => CliError::http_status(&server.name, status),
                None => CliError::network_error_classified(
                    &server.name,
                    error.message().to_owned(),
                    error.error_class(),
                ),
            };
            cli_error.with_source(error)
        }
    }
}

fn retry_error_to_connection(
    error: RetryError<ConnectionError>,
    operation: &str,
) -> ConnectionError {
    match error {
        RetryError::Operation(error) => error,
        RetryError::Timeout => {
            ConnectionError::timed_out(format!("timed out while attempting to {operation}"))
        }
        RetryError::Cancelled => {
            ConnectionError::cancelled(format!("cancelled while attempting to {operation}"))
        }
    }
}

async fn await_connect_attempt(
    ctx: &CommandContext,
    clock: &dyn Clock,
    connector: Arc<dyn DirectConnector>,
    server: ServerDefinition,
) -> Result<Box<dyn McpConnection>, ConnectionError> {
    if ctx.is_cancelled() {
        return Err(ConnectionError::cancelled(
            "cancelled while attempting to connect",
        ));
    }
    if ctx.deadline.is_expired(clock) {
        return Err(ConnectionError::timed_out(
            "timed out while attempting to connect",
        ));
    }

    let owned_context = ctx.clone();
    let mut task = tokio::spawn(async move { connector.connect(&owned_context, &server).await });
    let interrupted = tokio::select! {
        biased;
        result = &mut task => return join_connect_attempt(result),
        _ = wait_for_context_cancellation(ctx) => ConnectionError::cancelled(
            "cancelled while attempting to connect",
        ),
        _ = clock.sleep_until(ctx.deadline.expires_at()) => ConnectionError::timed_out(
            "timed out while attempting to connect",
        ),
    };

    // Keep the reservation alive while the connector observes cancellation and
    // tears down any partially-created child, pipes, HTTP request, or session.
    // A connector that fails to cooperate is aborted after the independent
    // cleanup grace; aborting drops its adapter-owned RAII guards.
    match ctx
        .run_bounded_cleanup(clock, DIRECT_CLEANUP_GRACE, &mut task)
        .await
    {
        Ok(joined) => {
            if let Ok(Ok(connection)) = joined {
                let cleanup = close_connection_bounded(connection, ctx, clock).await;
                if cleanup.is_err() {
                    ctx.diagnostics.debug(
                        "direct connection completed after interruption but cleanup did not complete cleanly",
                    );
                }
            }
        }
        Err(_) => {
            task.abort();
            let _ = task.await;
            ctx.diagnostics
                .debug("direct connection attempt cleanup reached its bounded timeout");
        }
    }
    Err(interrupted)
}

fn join_connect_attempt(
    result: Result<Result<Box<dyn McpConnection>, ConnectionError>, tokio::task::JoinError>,
) -> Result<Box<dyn McpConnection>, ConnectionError> {
    match result {
        Ok(result) => result,
        Err(_) => Err(ConnectionError::new("direct connection task failed")),
    }
}

async fn await_direct_attempt<T>(
    ctx: &CommandContext,
    clock: &dyn Clock,
    operation: &str,
    future: impl Future<Output = Result<T, ConnectionError>>,
) -> Result<T, ConnectionError> {
    if ctx.is_cancelled() {
        return Err(ConnectionError::cancelled(format!(
            "cancelled while attempting to {operation}"
        )));
    }
    if ctx.deadline.is_expired(clock) {
        return Err(ConnectionError::timed_out(format!(
            "timed out while attempting to {operation}"
        )));
    }

    tokio::select! {
        biased;
        _ = wait_for_context_cancellation(ctx) => Err(ConnectionError::cancelled(format!(
            "cancelled while attempting to {operation}"
        ))),
        _ = clock.sleep_until(ctx.deadline.expires_at()) => Err(ConnectionError::timed_out(format!(
            "timed out while attempting to {operation}"
        ))),
        result = future => result,
    }
}

async fn wait_for_context_cancellation(ctx: &CommandContext) {
    if ctx.is_cancelled() {
        return;
    }
    let mut poll = tokio::time::interval(Duration::from_millis(5));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        poll.tick().await;
        if ctx.is_cancelled() {
            return;
        }
    }
}

struct RegisteredConnection {
    inner: Mutex<Option<Box<dyn McpConnection>>>,
    context: CommandContext,
    registration: StdMutex<Option<ResourceRegistration>>,
    retry: Arc<DirectRetryRuntime>,
    instructions: Option<String>,
    mode: ConnectionMode,
}

impl RegisteredConnection {
    fn new(
        inner: Box<dyn McpConnection>,
        context: CommandContext,
        registration: ResourceRegistration,
        retry: Arc<DirectRetryRuntime>,
    ) -> Self {
        let instructions = inner.instructions().map(str::to_owned);
        let mode = inner.mode();
        Self {
            inner: Mutex::new(Some(inner)),
            context,
            registration: StdMutex::new(Some(registration)),
            retry,
            instructions,
            mode,
        }
    }

    fn accepts_context(&self, context: &CommandContext) -> bool {
        same_command_context(&self.context, context)
    }

    fn release_registration(&self) {
        drop(
            self.registration
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take(),
        );
    }

    async fn cleanup_after_primary_error(&self, connection: Box<dyn McpConnection>) {
        let result =
            close_connection_bounded(connection, &self.context, self.retry.clock.as_ref()).await;
        self.release_registration();
        if result.is_err() {
            // Never include adapter error text here. It can originate from a
            // server or transport and may contain credentials. The primary
            // operation error remains authoritative.
            self.context.diagnostics.debug(
                "direct connection cleanup after operation failure did not complete cleanly",
            );
        }
    }
}

async fn close_connection_bounded(
    connection: Box<dyn McpConnection>,
    context: &CommandContext,
    clock: &dyn Clock,
) -> Result<(), ConnectionError> {
    match context
        .run_bounded_cleanup(clock, DIRECT_CLEANUP_GRACE, connection.close(context))
        .await
    {
        Ok(result) => result,
        Err(_) => Err(ConnectionError::timed_out(
            "timed out cleaning up direct connection",
        )),
    }
}

impl McpConnection for RegisteredConnection {
    fn list_tools<'a>(
        &'a self,
        ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
        Box::pin(async move {
            if !self.accepts_context(ctx) {
                let connection = self.inner.lock().await.take();
                if let Some(connection) = connection {
                    self.cleanup_after_primary_error(connection).await;
                }
                return Err(ConnectionError::new(
                    "connection used with a different command context",
                ));
            }

            let mut inner = self.inner.lock().await;
            let Some(connection) = inner.as_deref() else {
                return Err(ConnectionError::new("direct connection is closed"));
            };
            let mut jitter = self.retry.jitter.lock().await;
            let result = retry(
                &self.context,
                &self.retry.policy,
                self.retry.clock.as_ref(),
                jitter.as_mut(),
                |_| async {
                    await_direct_attempt(
                        &self.context,
                        self.retry.clock.as_ref(),
                        "list tools",
                        connection.list_tools(&self.context),
                    )
                    .await
                },
            )
            .await
            .map_err(|error| retry_error_to_connection(error, "list tools"));
            drop(jitter);

            if result.is_err() {
                let connection = inner
                    .take()
                    .expect("a failed operation still owns its direct connection");
                drop(inner);
                self.cleanup_after_primary_error(connection).await;
            }
            result
        })
    }

    fn call_tool<'a>(
        &'a self,
        ctx: &'a CommandContext,
        name: &'a str,
        args: JsonObject,
    ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
        Box::pin(async move {
            if !self.accepts_context(ctx) {
                let connection = self.inner.lock().await.take();
                if let Some(connection) = connection {
                    self.cleanup_after_primary_error(connection).await;
                }
                return Err(ConnectionError::new(
                    "connection used with a different command context",
                ));
            }

            let mut inner = self.inner.lock().await;
            let Some(connection) = inner.as_deref() else {
                return Err(ConnectionError::new("direct connection is closed"));
            };
            let mut jitter = self.retry.jitter.lock().await;
            let result = retry(
                &self.context,
                &self.retry.policy,
                self.retry.clock.as_ref(),
                jitter.as_mut(),
                |_| {
                    let attempt_args = args.clone();
                    async move {
                        await_direct_attempt(
                            &self.context,
                            self.retry.clock.as_ref(),
                            "call tool",
                            connection.call_tool(&self.context, name, attempt_args),
                        )
                        .await
                    }
                },
            )
            .await
            .map_err(|error| retry_error_to_connection(error, "call tool"));
            drop(jitter);

            if result.is_err() {
                let connection = inner
                    .take()
                    .expect("a failed operation still owns its direct connection");
                drop(inner);
                self.cleanup_after_primary_error(connection).await;
            }
            result
        })
    }

    fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    fn close<'a>(
        self: Box<Self>,
        ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<(), ConnectionError>> {
        Box::pin(async move {
            let context_matches = self.accepts_context(ctx);
            let connection = self.inner.lock().await.take();
            let result = match connection {
                Some(connection) => {
                    close_connection_bounded(connection, &self.context, self.retry.clock.as_ref())
                        .await
                }
                None => Ok(()),
            };
            // Release even when adapter close reports an error or cleanup times out.
            self.release_registration();
            if !context_matches && result.is_ok() {
                return Err(ConnectionError::new(
                    "connection closed with a different command context",
                ));
            }
            result
        })
    }

    fn mode(&self) -> ConnectionMode {
        self.mode
    }
}

impl Drop for RegisteredConnection {
    fn drop(&mut self) {
        let Some(connection) = self.inner.get_mut().take() else {
            return;
        };

        let registration = self
            .registration
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(registration) = &registration {
            registration.registry.note_best_effort_cleanup();
        }

        let context = self.context.clone();
        let diagnostics = Arc::clone(&context.diagnostics);
        let retry = Arc::clone(&self.retry);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let result =
                        close_connection_bounded(connection, &context, retry.clock.as_ref()).await;
                    if result.is_err() {
                        diagnostics.debug("best-effort direct connection cleanup failed");
                    }
                    // Keep registry accounting active until cleanup has really
                    // completed or reached its bound.
                    drop(registration);
                });
            }
            Err(_) => {
                diagnostics.debug(
                    "best-effort direct connection cleanup could not be scheduled outside a runtime",
                );
                drop(connection);
                drop(registration);
            }
        }
    }
}

fn same_command_context(left: &CommandContext, right: &CommandContext) -> bool {
    left.deadline == right.deadline
        && Arc::ptr_eq(&left.cancellation, &right.cancellation)
        && Arc::ptr_eq(&left.diagnostics, &right.diagnostics)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        future::pending,
        path::PathBuf,
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicUsize},
        },
        time::{Duration, Instant},
    };

    use serde_json::json;
    use tokio::sync::watch;

    use crate::{
        config::{ConfigHash, ServerId, ToolFilterConfig, TransportConfig},
        output::DiagnosticSink,
        runtime::{CancellationFlag, Deadline},
    };

    fn environment(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[derive(Default)]
    struct NullDiagnostics;

    impl DiagnosticSink for NullDiagnostics {
        fn warning(&self, _message: &str) {}
        fn debug(&self, _message: &str) {}
        fn server_stderr(&self, _server: &str, _bytes: &[u8]) {}
    }

    #[derive(Default)]
    struct RecordingDiagnostics {
        debug: Mutex<Vec<String>>,
    }

    impl RecordingDiagnostics {
        fn messages(&self) -> Vec<String> {
            self.debug.lock().expect("diagnostics lock").clone()
        }
    }

    impl DiagnosticSink for RecordingDiagnostics {
        fn warning(&self, _message: &str) {}

        fn debug(&self, message: &str) {
            self.debug
                .lock()
                .expect("diagnostics lock")
                .push(message.to_owned());
        }

        fn server_stderr(&self, _server: &str, _bytes: &[u8]) {}
    }

    #[derive(Clone)]
    struct ManualClock {
        now: Arc<watch::Sender<Instant>>,
    }

    impl ManualClock {
        fn new(now: Instant) -> Self {
            let (now, _) = watch::channel(now);
            Self { now: Arc::new(now) }
        }

        fn advance(&self, duration: Duration) {
            let next = (*self.now.borrow()) + duration;
            self.now.send_replace(next);
        }
    }

    impl Clock for ManualClock {
        fn now(&self) -> Instant {
            *self.now.borrow()
        }

        fn sleep_until(&self, deadline: Instant) -> BoxFuture<'_, ()> {
            let mut now = self.now.subscribe();
            Box::pin(async move {
                loop {
                    if *now.borrow_and_update() >= deadline {
                        return;
                    }
                    if now.changed().await.is_err() {
                        return;
                    }
                }
            })
        }
    }

    struct FixedJitter;

    impl JitterSource for FixedJitter {
        fn factor_basis_points(&mut self) -> u16 {
            10_000
        }
    }

    fn command_context() -> CommandContext {
        CommandContext {
            deadline: Deadline::new(Instant::now() + Duration::from_secs(60)),
            cancellation: Arc::new(CancellationFlag::default()),
            diagnostics: Arc::new(NullDiagnostics),
        }
    }

    fn manual_context(
        clock: &ManualClock,
        cancellation: Arc<CancellationFlag>,
        diagnostics: Arc<dyn DiagnosticSink>,
        budget: Duration,
    ) -> CommandContext {
        CommandContext {
            deadline: Deadline::after(clock, budget),
            cancellation,
            diagnostics,
        }
    }

    fn server(name: &str) -> ServerDefinition {
        ServerDefinition {
            name: name.to_owned(),
            id: ServerId(format!("{name:0<64}")),
            config_hash: ConfigHash([0; 32]),
            transport: TransportConfig::Stdio {
                command: "mock-server".to_owned(),
                args: Vec::new(),
                env: BTreeMap::new(),
                cwd: Some(PathBuf::from("/tmp")),
            },
            filter: ToolFilterConfig::default(),
        }
    }

    #[derive(Default)]
    struct Trace {
        connect: AtomicUsize,
        connect_started: AtomicBool,
        pending_connect: AtomicBool,
        list: AtomicUsize,
        instructions: AtomicUsize,
        call: AtomicUsize,
        close: AtomicUsize,
        close_started: AtomicBool,
        context_mismatch: AtomicBool,
        fail_list: AtomicBool,
        pending_list: AtomicBool,
        list_started: AtomicBool,
        fail_close: AtomicBool,
        pending_close: AtomicBool,
        connected_servers: Mutex<Vec<String>>,
    }

    struct RecordingConnector {
        trace: Arc<Trace>,
        expected_context: CommandContext,
        mode: ConnectionMode,
    }

    impl DirectConnector for RecordingConnector {
        fn connect<'a>(
            &'a self,
            ctx: &'a CommandContext,
            server: &'a ServerDefinition,
        ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, ConnectionError>> {
            self.trace.connect.fetch_add(1, Ordering::SeqCst);
            self.trace.connect_started.store(true, Ordering::SeqCst);
            self.trace
                .connected_servers
                .lock()
                .expect("connected servers lock")
                .push(server.name.clone());
            if !same_command_context(&self.expected_context, ctx) {
                self.trace.context_mismatch.store(true, Ordering::SeqCst);
            }
            let pending_connect = self.trace.pending_connect.load(Ordering::SeqCst);
            let connection = RecordingConnection {
                trace: Arc::clone(&self.trace),
                bound_context: ctx.clone(),
                mode: self.mode,
            };
            Box::pin(async move {
                if pending_connect {
                    pending::<()>().await;
                }
                Ok(Box::new(connection) as Box<dyn McpConnection>)
            })
        }
    }

    struct RecordingConnection {
        trace: Arc<Trace>,
        bound_context: CommandContext,
        mode: ConnectionMode,
    }

    impl RecordingConnection {
        fn observe_context(&self, ctx: &CommandContext) {
            if !same_command_context(&self.bound_context, ctx) {
                self.trace.context_mismatch.store(true, Ordering::SeqCst);
            }
        }
    }

    impl McpConnection for RecordingConnection {
        fn list_tools<'a>(
            &'a self,
            ctx: &'a CommandContext,
        ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
            self.observe_context(ctx);
            self.trace.list.fetch_add(1, Ordering::SeqCst);
            self.trace.list_started.store(true, Ordering::SeqCst);
            let fails = self.trace.fail_list.load(Ordering::SeqCst);
            let pending_list = self.trace.pending_list.load(Ordering::SeqCst);
            Box::pin(async move {
                if pending_list {
                    pending::<()>().await;
                }
                if fails {
                    Err(ConnectionError::new("scripted list failure"))
                } else {
                    Ok(vec![ToolInfo {
                        name: "echo".to_owned(),
                        description: None,
                        input_schema: json!({"type": "object"}),
                    }])
                }
            })
        }

        fn call_tool<'a>(
            &'a self,
            ctx: &'a CommandContext,
            name: &'a str,
            args: JsonObject,
        ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
            self.observe_context(ctx);
            self.trace.call.fetch_add(1, Ordering::SeqCst);
            let name = name.to_owned();
            Box::pin(async move { Ok(json!({"tool": name, "args": args})) })
        }

        fn instructions(&self) -> Option<&str> {
            self.trace.instructions.fetch_add(1, Ordering::SeqCst);
            Some("mock instructions")
        }

        fn close<'a>(
            self: Box<Self>,
            ctx: &'a CommandContext,
        ) -> BoxFuture<'a, Result<(), ConnectionError>> {
            self.observe_context(ctx);
            self.trace.close.fetch_add(1, Ordering::SeqCst);
            self.trace.close_started.store(true, Ordering::SeqCst);
            let fails = self.trace.fail_close.load(Ordering::SeqCst);
            let pending_close = self.trace.pending_close.load(Ordering::SeqCst);
            Box::pin(async move {
                let _connection = self;
                if pending_close {
                    pending::<()>().await;
                }
                if fails {
                    Err(ConnectionError::new("scripted-close-secret-must-not-leak"))
                } else {
                    Ok(())
                }
            })
        }

        fn mode(&self) -> ConnectionMode {
            self.mode
        }
    }

    fn manager(
        context: &CommandContext,
        trace: Arc<Trace>,
        mode: ConnectionMode,
    ) -> (DirectConnectionManager, ConnectionResourceRegistry) {
        let registry = ConnectionResourceRegistry::new();
        let connector = Arc::new(RecordingConnector {
            trace,
            expected_context: context.clone(),
            mode,
        });
        (
            DirectConnectionManager::single_target(connector, registry.clone()),
            registry,
        )
    }

    fn manager_with_clock(
        context: &CommandContext,
        trace: Arc<Trace>,
        clock: Arc<ManualClock>,
    ) -> (DirectConnectionManager, ConnectionResourceRegistry) {
        let registry = ConnectionResourceRegistry::new();
        let connector = Arc::new(RecordingConnector {
            trace,
            expected_context: context.clone(),
            mode: ConnectionMode::Direct,
        });
        (
            DirectConnectionManager::with_retry_components(
                connector,
                registry.clone(),
                RetryPolicy::new(0, Duration::ZERO),
                clock,
                Box::new(FixedJitter),
            ),
            registry,
        )
    }

    async fn wait_for_flag(flag: &AtomicBool) {
        for _ in 0..100 {
            if flag.load(Ordering::SeqCst) {
                return;
            }
            tokio::task::yield_now().await;
        }
        assert!(flag.load(Ordering::SeqCst));
    }

    async fn wait_for_registry(registry: &ConnectionResourceRegistry, expected: usize) {
        for _ in 0..100 {
            if registry.active_resource_count() == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(registry.active_resource_count(), expected);
    }

    #[test]
    fn merges_disjoint_parent_and_configured_keys() {
        let parent = environment(&[("HOME", "/home/test"), ("PATH", "/usr/bin")]);
        let configured = environment(&[("MODE", "stdio"), ("TOKEN", "literal-${SECRET}")]);

        assert_eq!(
            merge_stdio_environment(&parent, &configured),
            environment(&[
                ("HOME", "/home/test"),
                ("MODE", "stdio"),
                ("PATH", "/usr/bin"),
                ("TOKEN", "literal-${SECRET}"),
            ])
        );
    }

    #[test]
    fn configured_values_override_overlapping_parent_values() {
        let parent = environment(&[("PATH", "/usr/bin"), ("SHARED", "parent")]);
        let configured = environment(&[("SHARED", "configured"), ("ONLY_CONFIG", "value")]);

        assert_eq!(
            merge_stdio_environment(&parent, &configured),
            environment(&[
                ("ONLY_CONFIG", "value"),
                ("PATH", "/usr/bin"),
                ("SHARED", "configured"),
            ])
        );
    }

    #[test]
    fn empty_maps_preserve_the_other_side_or_produce_empty_output() {
        let empty = BTreeMap::new();
        let parent = environment(&[("PARENT", "value")]);
        let configured = environment(&[("CONFIGURED", "value")]);

        assert_eq!(merge_stdio_environment(&parent, &empty), parent);
        assert_eq!(merge_stdio_environment(&empty, &configured), configured);
        assert!(merge_stdio_environment(&empty, &empty).is_empty());
    }

    #[test]
    fn merged_environment_iterates_in_deterministic_key_order() {
        let parent = environment(&[("zeta", "1"), ("alpha", "2")]);
        let configured = environment(&[("middle", "3"), ("beta", "4")]);

        let keys = merge_stdio_environment(&parent, &configured)
            .into_keys()
            .collect::<Vec<_>>();

        assert_eq!(keys, ["alpha", "beta", "middle", "zeta"]);
    }

    #[tokio::test]
    async fn direct_only_manager_acquires_direct_connections() {
        let context = command_context();
        let trace = Arc::new(Trace::default());
        let (manager, registry) = manager(&context, Arc::clone(&trace), ConnectionMode::Direct);

        let connection = manager
            .acquire(&context, &server("alpha"))
            .await
            .expect("direct connection");

        assert_eq!(connection.mode(), ConnectionMode::Direct);
        assert_eq!(trace.connect.load(Ordering::SeqCst), 1);
        assert_eq!(registry.active_resource_count(), 1);
        connection.close(&context).await.expect("close succeeds");
        assert_eq!(registry.active_resource_count(), 0);
    }

    #[tokio::test]
    async fn single_target_manager_never_opens_a_second_server_connection() {
        let context = command_context();
        let trace = Arc::new(Trace::default());
        let (manager, registry) = manager(&context, Arc::clone(&trace), ConnectionMode::Direct);
        let first = manager
            .acquire(&context, &server("alpha"))
            .await
            .expect("first connection");

        let second = manager.acquire(&context, &server("beta")).await;

        assert!(second.is_err());
        assert_eq!(trace.connect.load(Ordering::SeqCst), 1);
        assert_eq!(registry.active_resource_count(), 1);
        first.close(&context).await.expect("first close");
        assert_eq!(registry.active_resource_count(), 0);
    }

    #[tokio::test]
    async fn explicit_close_runs_exactly_once_and_unregisters_even_on_error() {
        let context = command_context();
        let trace = Arc::new(Trace::default());
        trace.fail_close.store(true, Ordering::SeqCst);
        let (manager, registry) = manager(&context, Arc::clone(&trace), ConnectionMode::Direct);
        let connection = manager
            .acquire(&context, &server("alpha"))
            .await
            .expect("connection");

        let error = connection
            .close(&context)
            .await
            .expect_err("explicit close failure");

        assert_eq!(error.message(), "scripted-close-secret-must-not-leak");

        assert_eq!(trace.close.load(Ordering::SeqCst), 1);
        assert_eq!(registry.active_resource_count(), 0);
        assert_eq!(registry.best_effort_cleanup_count(), 0);
    }

    #[tokio::test]
    async fn operation_error_closes_and_unregisters_before_returning_primary_error() {
        let context = command_context();
        let trace = Arc::new(Trace::default());
        trace.fail_list.store(true, Ordering::SeqCst);
        let (manager, registry) = manager(&context, Arc::clone(&trace), ConnectionMode::Direct);
        let connection = manager
            .acquire(&context, &server("alpha"))
            .await
            .expect("connection");

        let error = connection
            .list_tools(&context)
            .await
            .expect_err("scripted operation failure");

        assert_eq!(error.message(), "scripted list failure");
        assert_eq!(trace.close.load(Ordering::SeqCst), 1);
        assert_eq!(registry.active_resource_count(), 0);
        assert_eq!(registry.best_effort_cleanup_count(), 0);
        connection
            .close(&context)
            .await
            .expect("already cleaned close");
    }

    #[tokio::test]
    async fn deadline_failure_still_closes_with_the_original_command_context() {
        let clock = Arc::new(ManualClock::new(Instant::now()));
        let cancellation = Arc::new(CancellationFlag::default());
        let context = manual_context(
            clock.as_ref(),
            cancellation,
            Arc::new(NullDiagnostics),
            Duration::from_secs(1),
        );
        let trace = Arc::new(Trace::default());
        trace.pending_list.store(true, Ordering::SeqCst);
        let (manager, registry) =
            manager_with_clock(&context, Arc::clone(&trace), Arc::clone(&clock));
        let connection = manager
            .acquire(&context, &server("deadline"))
            .await
            .expect("connection");

        let (result, ()) = tokio::join!(connection.list_tools(&context), async {
            wait_for_flag(&trace.list_started).await;
            clock.advance(Duration::from_secs(1));
        });

        assert!(result.expect_err("deadline").is_timeout());
        assert_eq!(trace.close.load(Ordering::SeqCst), 1);
        assert!(!trace.context_mismatch.load(Ordering::SeqCst));
        assert_eq!(registry.active_resource_count(), 0);
    }

    #[tokio::test]
    async fn cancellation_failure_still_runs_cleanup_and_releases_registry() {
        let clock = Arc::new(ManualClock::new(Instant::now()));
        let cancellation = Arc::new(CancellationFlag::default());
        let context = manual_context(
            clock.as_ref(),
            cancellation.clone(),
            Arc::new(NullDiagnostics),
            Duration::from_secs(30),
        );
        let trace = Arc::new(Trace::default());
        trace.pending_list.store(true, Ordering::SeqCst);
        let (manager, registry) =
            manager_with_clock(&context, Arc::clone(&trace), Arc::clone(&clock));
        let connection = manager
            .acquire(&context, &server("cancelled"))
            .await
            .expect("connection");

        let (result, ()) = tokio::join!(connection.list_tools(&context), async {
            wait_for_flag(&trace.list_started).await;
            cancellation.cancel();
        });

        assert!(result.expect_err("cancellation").is_cancelled());
        assert_eq!(trace.close.load(Ordering::SeqCst), 1);
        assert!(!trace.context_mismatch.load(Ordering::SeqCst));
        assert_eq!(registry.active_resource_count(), 0);
    }

    #[tokio::test]
    async fn cleanup_timeout_and_error_do_not_replace_the_primary_operation_error() {
        let clock = Arc::new(ManualClock::new(Instant::now()));
        let diagnostics = Arc::new(RecordingDiagnostics::default());
        let sink: Arc<dyn DiagnosticSink> = diagnostics.clone();
        let context = manual_context(
            clock.as_ref(),
            Arc::new(CancellationFlag::default()),
            sink,
            Duration::from_secs(30),
        );
        let trace = Arc::new(Trace::default());
        trace.fail_list.store(true, Ordering::SeqCst);
        trace.pending_close.store(true, Ordering::SeqCst);
        let (manager, registry) =
            manager_with_clock(&context, Arc::clone(&trace), Arc::clone(&clock));
        let connection = manager
            .acquire(&context, &server("cleanup-timeout"))
            .await
            .expect("connection");

        let (result, ()) = tokio::join!(connection.list_tools(&context), async {
            wait_for_flag(&trace.close_started).await;
            assert_eq!(registry.active_resource_count(), 1);
            clock.advance(DIRECT_CLEANUP_GRACE);
        });
        let error = result.expect_err("primary operation failure");

        assert_eq!(error.message(), "scripted list failure");
        assert_eq!(registry.active_resource_count(), 0);
        let visible = diagnostics.messages().join("\n");
        assert!(visible.contains("cleanup after operation failure"));
        assert!(!visible.contains("scripted-close-secret-must-not-leak"));
    }

    #[tokio::test]
    async fn cleanup_error_is_diagnostic_only_when_an_operation_already_failed() {
        let clock = Arc::new(ManualClock::new(Instant::now()));
        let diagnostics = Arc::new(RecordingDiagnostics::default());
        let sink: Arc<dyn DiagnosticSink> = diagnostics.clone();
        let context = manual_context(
            clock.as_ref(),
            Arc::new(CancellationFlag::default()),
            sink,
            Duration::from_secs(30),
        );
        let trace = Arc::new(Trace::default());
        trace.fail_list.store(true, Ordering::SeqCst);
        trace.fail_close.store(true, Ordering::SeqCst);
        let (manager, registry) =
            manager_with_clock(&context, Arc::clone(&trace), Arc::clone(&clock));
        let connection = manager
            .acquire(&context, &server("cleanup-error"))
            .await
            .expect("connection");

        let error = connection
            .list_tools(&context)
            .await
            .expect_err("primary operation failure");

        assert_eq!(error.message(), "scripted list failure");
        assert_eq!(registry.active_resource_count(), 0);
        let visible = diagnostics.messages().join("\n");
        assert!(visible.contains("cleanup after operation failure"));
        assert!(!visible.contains("scripted-close-secret-must-not-leak"));
    }

    #[tokio::test]
    async fn standalone_explicit_close_returns_its_bounded_timeout() {
        let clock = Arc::new(ManualClock::new(Instant::now()));
        let context = manual_context(
            clock.as_ref(),
            Arc::new(CancellationFlag::default()),
            Arc::new(NullDiagnostics),
            Duration::from_secs(30),
        );
        let trace = Arc::new(Trace::default());
        trace.pending_close.store(true, Ordering::SeqCst);
        let (manager, registry) =
            manager_with_clock(&context, Arc::clone(&trace), Arc::clone(&clock));
        let connection = manager
            .acquire(&context, &server("explicit-timeout"))
            .await
            .expect("connection");

        let (result, ()) = tokio::join!(connection.close(&context), async {
            wait_for_flag(&trace.close_started).await;
            assert_eq!(registry.active_resource_count(), 1);
            clock.advance(DIRECT_CLEANUP_GRACE);
        });

        assert!(result.expect_err("bounded close timeout").is_timeout());
        assert_eq!(registry.active_resource_count(), 0);
    }

    #[tokio::test]
    async fn drop_keeps_registry_active_until_best_effort_cleanup_finishes() {
        let clock = Arc::new(ManualClock::new(Instant::now()));
        let context = manual_context(
            clock.as_ref(),
            Arc::new(CancellationFlag::default()),
            Arc::new(NullDiagnostics),
            Duration::from_secs(30),
        );
        let trace = Arc::new(Trace::default());
        trace.pending_close.store(true, Ordering::SeqCst);
        let (manager, registry) =
            manager_with_clock(&context, Arc::clone(&trace), Arc::clone(&clock));
        let connection = manager
            .acquire(&context, &server("drop-cleanup"))
            .await
            .expect("connection");

        drop(connection);
        wait_for_flag(&trace.close_started).await;
        assert_eq!(registry.active_resource_count(), 1);
        assert_eq!(registry.best_effort_cleanup_count(), 1);

        clock.advance(DIRECT_CLEANUP_GRACE);
        wait_for_registry(&registry, 0).await;
    }

    #[tokio::test]
    async fn connect_list_instructions_call_and_close_share_one_command_context() {
        let context = command_context();
        let trace = Arc::new(Trace::default());
        let (manager, registry) = manager(&context, Arc::clone(&trace), ConnectionMode::Direct);
        let connection = manager
            .acquire(&context, &server("alpha"))
            .await
            .expect("connection");

        connection.list_tools(&context).await.expect("list");
        assert_eq!(connection.instructions(), Some("mock instructions"));
        connection
            .call_tool(&context, "echo", JsonObject::new())
            .await
            .expect("call");
        connection.close(&context).await.expect("close");

        assert!(!trace.context_mismatch.load(Ordering::SeqCst));
        assert_eq!(trace.connect.load(Ordering::SeqCst), 1);
        assert_eq!(trace.list.load(Ordering::SeqCst), 1);
        assert_eq!(trace.instructions.load(Ordering::SeqCst), 1);
        assert_eq!(trace.call.load(Ordering::SeqCst), 1);
        assert_eq!(trace.close.load(Ordering::SeqCst), 1);
        assert_eq!(registry.active_resource_count(), 0);
    }

    #[tokio::test]
    async fn direct_only_manager_rejects_non_direct_connector_results() {
        let context = command_context();
        let trace = Arc::new(Trace::default());
        let (manager, registry) = manager(&context, Arc::clone(&trace), ConnectionMode::Daemon);

        let result = manager.acquire(&context, &server("alpha")).await;

        assert!(result.is_err());
        assert_eq!(trace.connect.load(Ordering::SeqCst), 1);
        assert_eq!(trace.close.load(Ordering::SeqCst), 1);
        assert_eq!(registry.active_resource_count(), 0);
    }
}
