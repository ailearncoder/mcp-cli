#![cfg(unix)]
//! Per-server Unix daemon worker IPC service and shutdown coordination.
//!
//! Request serving, idle expiry, explicit close, and Unix signals all converge
//! on one consuming `shutdown_once` path. That path stops acceptance, cancels
//! and joins clients, closes the backend, and runs identity-bound artifact
//! cleanup exactly once.

use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    fs::{self, OpenOptions},
    future::{Future, pending},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    process::{Child, Command},
    signal::unix::{Signal, SignalKind, signal},
    sync::{Notify, mpsc, watch},
    task::JoinSet,
};

use crate::{
    config::{
        ConfigHash, SHA256_HEX_LENGTH, ServerDefinition, ServerId, ToolFilterConfig,
        TransportConfig, server_id,
    },
    connection::{ConnectionError, McpConnection, rmcp_adapter::RmcpDirectConnector},
    daemon::{
        DaemonPathError, DaemonPaths, FrameError, IpcErrorCode, IpcOperation, IpcRequest,
        IpcResponse, MetadataStore, NdjsonCodec, PidMetadata, encode_message,
        paths::{ArtifactIdentity, ArtifactKind, private_file_mode},
        validate_request_id,
    },
    policy::redact::{SecretSet, WriterDiagnosticSink},
    runtime::{BoxFuture, CancellationFlag, Clock, CommandContext, Deadline, SystemClock},
};

/// The single trigger selected for a completed worker shutdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerStop {
    /// A valid `close` request was acknowledged.
    CloseRequested,
    /// No valid operation completed before the active idle deadline.
    IdleTimeout,
    /// The worker received SIGINT.
    SignalInterrupt,
    /// The worker received SIGTERM.
    SignalTerminate,
    /// Accept failed; shutdown still drained and cleaned owned resources.
    AcceptFailure,
}

/// Injectable signal event. Tests send these through an in-process channel and
/// never signal another process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerSignal {
    Interrupt,
    Terminate,
}

impl WorkerSignal {
    const fn stop(self) -> WorkerStop {
        match self {
            Self::Interrupt => WorkerStop::SignalInterrupt,
            Self::Terminate => WorkerStop::SignalTerminate,
        }
    }
}

/// Observable shutdown transitions used by deterministic tests and optional
/// diagnostics. Cleanup continues through every phase even when one fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerShutdownPhase {
    Draining(WorkerStop),
    AcceptStopped,
    ClientsCancelled,
    ClientsJoined,
    BackendClosed,
    SocketCleaned,
    PidCleaned,
    LockReleased,
    Closed(WorkerStop),
}

/// Stable names for independently aggregated shutdown failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerShutdownStep {
    ClientJoin,
    BackendClose,
    SocketCleanup,
    PidCleanup,
    LockRelease,
}

impl WorkerShutdownStep {
    const fn description(self) -> &'static str {
        match self {
            Self::ClientJoin => "client task join",
            Self::BackendClose => "MCP backend close",
            Self::SocketCleanup => "socket cleanup",
            Self::PidCleanup => "PID cleanup",
            Self::LockRelease => "lock release",
        }
    }
}

/// A safely rendered cleanup-hook failure. The detailed source is retained for
/// internal inspection without being interpolated into user-facing text.
#[derive(Debug)]
pub struct WorkerCleanupError {
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl WorkerCleanupError {
    pub fn new(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(source),
        }
    }
}

impl fmt::Display for WorkerCleanupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("worker artifact cleanup failed")
    }
}

impl std::error::Error for WorkerCleanupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

impl From<DaemonPathError> for WorkerCleanupError {
    fn from(source: DaemonPathError) -> Self {
        Self::new(source)
    }
}

/// Injected signal and artifact side-effect boundary. Production uses Unix
/// signal streams plus identity-bound daemon paths; tests use channels and
/// counters. Default phase observation is intentionally side-effect free.
pub trait WorkerShutdownHooks: Send {
    fn wait_for_signal(&mut self) -> BoxFuture<'_, WorkerSignal>;
    fn remove_socket(&mut self) -> Result<(), WorkerCleanupError>;
    fn remove_pid(&mut self) -> Result<(), WorkerCleanupError>;
    fn release_lock(&mut self) -> Result<(), WorkerCleanupError>;
    fn observe_phase(&mut self, _phase: WorkerShutdownPhase) {}
}

/// Hooks used by existing in-memory/loopback callers that do not yet publish
/// daemon artifacts. Signals remain pending and cleanup is a no-op.
#[derive(Default)]
pub struct NoopWorkerShutdownHooks;

impl WorkerShutdownHooks for NoopWorkerShutdownHooks {
    fn wait_for_signal(&mut self) -> BoxFuture<'_, WorkerSignal> {
        Box::pin(pending())
    }

    fn remove_socket(&mut self) -> Result<(), WorkerCleanupError> {
        Ok(())
    }

    fn remove_pid(&mut self) -> Result<(), WorkerCleanupError> {
        Ok(())
    }

    fn release_lock(&mut self) -> Result<(), WorkerCleanupError> {
        Ok(())
    }
}

/// Real Unix shutdown hooks. Artifact identities are captured once after the
/// worker has published them; absent artifacts stay unowned and are never
/// removed if another process creates them later.
pub struct UnixWorkerShutdownHooks {
    interrupt: Signal,
    terminate: Signal,
    interrupt_closed: bool,
    terminate_closed: bool,
    paths: DaemonPaths,
    socket_identity: Option<ArtifactIdentity>,
    pid_identity: Option<ArtifactIdentity>,
    lock_identity: Option<ArtifactIdentity>,
}

impl UnixWorkerShutdownHooks {
    pub fn new(paths: DaemonPaths) -> Result<Self, WorkerCleanupError> {
        let socket_identity = paths.capture_socket_identity()?;
        let pid_identity = paths.capture_pid_identity()?;
        let lock_identity = paths.capture_lock_identity()?;
        let interrupt = signal(SignalKind::interrupt()).map_err(WorkerCleanupError::new)?;
        let terminate = signal(SignalKind::terminate()).map_err(WorkerCleanupError::new)?;
        Ok(Self {
            interrupt,
            terminate,
            interrupt_closed: false,
            terminate_closed: false,
            paths,
            socket_identity,
            pid_identity,
            lock_identity,
        })
    }

    async fn next_signal(&mut self) -> WorkerSignal {
        loop {
            tokio::select! {
                biased;
                received = self.interrupt.recv(), if !self.interrupt_closed => {
                    if received.is_some() {
                        return WorkerSignal::Interrupt;
                    }
                    self.interrupt_closed = true;
                }
                received = self.terminate.recv(), if !self.terminate_closed => {
                    if received.is_some() {
                        return WorkerSignal::Terminate;
                    }
                    self.terminate_closed = true;
                }
                else => pending::<()>().await,
            }
        }
    }
}

impl WorkerShutdownHooks for UnixWorkerShutdownHooks {
    fn wait_for_signal(&mut self) -> BoxFuture<'_, WorkerSignal> {
        Box::pin(self.next_signal())
    }

    fn remove_socket(&mut self) -> Result<(), WorkerCleanupError> {
        if let Some(identity) = self.socket_identity.take() {
            self.paths.remove_socket_if_owned(identity)?;
        }
        Ok(())
    }

    fn remove_pid(&mut self) -> Result<(), WorkerCleanupError> {
        if let Some(identity) = self.pid_identity.take() {
            self.paths.remove_pid_if_owned(identity)?;
        }
        Ok(())
    }

    fn release_lock(&mut self) -> Result<(), WorkerCleanupError> {
        if let Some(identity) = self.lock_identity.take() {
            self.paths.remove_lock_if_owned(identity)?;
        }
        Ok(())
    }
}

/// One failure recorded during the ordered shutdown sequence.
#[derive(Debug)]
pub enum WorkerShutdownFailure {
    ClientJoin,
    BackendOwnership,
    Backend(ConnectionError),
    Cleanup {
        step: WorkerShutdownStep,
        error: WorkerCleanupError,
    },
}

impl WorkerShutdownFailure {
    pub const fn step(&self) -> WorkerShutdownStep {
        match self {
            Self::ClientJoin => WorkerShutdownStep::ClientJoin,
            Self::BackendOwnership | Self::Backend(_) => WorkerShutdownStep::BackendClose,
            Self::Cleanup { step, .. } => *step,
        }
    }
}

/// Aggregated shutdown error. Display is deliberately stable and omits
/// arbitrary backend/hook messages while preserving detailed sources in the
/// individual failures.
#[derive(Debug)]
pub struct WorkerShutdownError {
    stop: WorkerStop,
    failures: Vec<WorkerShutdownFailure>,
}

impl WorkerShutdownError {
    pub const fn stop(&self) -> WorkerStop {
        self.stop
    }

    pub fn failures(&self) -> &[WorkerShutdownFailure] {
        &self.failures
    }
}

impl fmt::Display for WorkerShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "daemon shutdown completed with failures in ")?;
        for (index, failure) in self.failures.iter().enumerate() {
            if index != 0 {
                formatter.write_str(", ")?;
            }
            formatter.write_str(failure.step().description())?;
        }
        Ok(())
    }
}

impl std::error::Error for WorkerShutdownError {}

/// Accept-loop and aggregated shutdown failures. Per-client framing and I/O
/// failures remain isolated and never promote themselves to worker failures.
#[derive(Debug)]
pub enum WorkerError {
    Accept(io::Error),
    Shutdown(WorkerShutdownError),
    AcceptAndShutdown {
        accept: io::Error,
        shutdown: WorkerShutdownError,
    },
}

impl fmt::Display for WorkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Accept(_) => "daemon IPC accept failed",
            Self::Shutdown(_) => "daemon shutdown completed with cleanup failures",
            Self::AcceptAndShutdown { .. } => {
                "daemon IPC accept and subsequent shutdown cleanup failed"
            }
        })
    }
}

impl std::error::Error for WorkerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Accept(source) => Some(source),
            Self::Shutdown(source) => Some(source),
            Self::AcceptAndShutdown { accept, .. } => Some(accept),
        }
    }
}

struct SharedConnection {
    connection: Box<dyn McpConnection>,
    #[cfg(feature = "test-fixtures")]
    fixture_delays: WorkerFixtureDelays,
}

#[cfg(feature = "test-fixtures")]
#[derive(Clone, Copy, Default)]
struct WorkerFixtureDelays {
    ping: Duration,
    call: Duration,
}

impl SharedConnection {
    fn new(connection: Box<dyn McpConnection>) -> Self {
        Self {
            connection,
            #[cfg(feature = "test-fixtures")]
            fixture_delays: WorkerFixtureDelays::default(),
        }
    }

    #[cfg(feature = "test-fixtures")]
    fn with_fixture_delays(mut self, fixture_delays: WorkerFixtureDelays) -> Self {
        self.fixture_delays = fixture_delays;
        self
    }

    async fn before_request(&self, operation: &IpcOperation) {
        #[cfg(feature = "test-fixtures")]
        {
            let delay = match operation {
                IpcOperation::Ping => self.fixture_delays.ping,
                IpcOperation::CallTool { .. } => self.fixture_delays.call,
                _ => Duration::ZERO,
            };
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
        }
        #[cfg(not(feature = "test-fixtures"))]
        let _ = operation;
    }

    fn as_connection(&self) -> &dyn McpConnection {
        &*self.connection
    }

    async fn close(self, context: &CommandContext) -> Result<(), ConnectionError> {
        self.connection.close(context).await
    }
}

/// Completion timestamps are queued rather than reduced to one maximum. This
/// preserves a chain of valid extensions if the accept loop is briefly
/// unscheduled while several clients complete requests.
struct IdleActivity {
    completions: Mutex<Vec<Instant>>,
    changed: Notify,
}

impl IdleActivity {
    fn new() -> Self {
        Self {
            completions: Mutex::new(Vec::new()),
            changed: Notify::new(),
        }
    }

    fn record_completion(&self, completed_at: Instant) {
        self.completions
            .lock()
            .expect("idle activity mutex poisoned")
            .push(completed_at);
        self.changed.notify_one();
    }

    fn drain_completions(&self) -> Vec<Instant> {
        let mut completions = self
            .completions
            .lock()
            .expect("idle activity mutex poisoned");
        let mut drained = std::mem::take(&mut *completions);
        drained.sort_unstable();
        drained
    }
}

/// Concurrent Unix IPC service backed by one transport-independent MCP
/// connection. Requests are awaited serially inside each client task, while
/// different client tasks can call the shared connection concurrently.
pub struct WorkerIpcService {
    connection: Box<dyn McpConnection>,
    context: CommandContext,
    idle_timeout: Duration,
    clock: Arc<dyn Clock>,
    idle_deadline_observer: Option<mpsc::UnboundedSender<Instant>>,
    #[cfg(feature = "test-fixtures")]
    fixture_delays: WorkerFixtureDelays,
}

impl WorkerIpcService {
    pub fn new(
        connection: Box<dyn McpConnection>,
        context: CommandContext,
        idle_timeout: Duration,
    ) -> Self {
        Self::with_clock(connection, context, idle_timeout, Arc::new(SystemClock))
    }

    pub fn with_clock(
        connection: Box<dyn McpConnection>,
        context: CommandContext,
        idle_timeout: Duration,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            connection,
            context,
            idle_timeout,
            clock,
            idle_deadline_observer: None,
            #[cfg(feature = "test-fixtures")]
            fixture_delays: WorkerFixtureDelays::default(),
        }
    }

    /// Creates a worker with a non-blocking observer for each active idle
    /// deadline. This fixture-only boundary lets deterministic tests compare
    /// production deadline transitions with an independent model without
    /// changing worker scheduling or lifecycle behavior.
    #[cfg(feature = "test-fixtures")]
    pub fn with_clock_and_idle_deadline_observer(
        connection: Box<dyn McpConnection>,
        context: CommandContext,
        idle_timeout: Duration,
        clock: Arc<dyn Clock>,
    ) -> (Self, mpsc::UnboundedReceiver<Instant>) {
        let (observer, deadlines) = mpsc::unbounded_channel();
        (
            Self {
                connection,
                context,
                idle_timeout,
                clock,
                idle_deadline_observer: Some(observer),
                fixture_delays: WorkerFixtureDelays::default(),
            },
            deadlines,
        )
    }

    #[cfg(feature = "test-fixtures")]
    fn with_fixture_delays(mut self, fixture_delays: WorkerFixtureDelays) -> Self {
        self.fixture_delays = fixture_delays;
        self
    }

    /// Serves clients until one shutdown trigger wins. Existing callers use
    /// no-op hooks; the worker bootstrap layer can install real Unix hooks once
    /// its artifacts have been published.
    pub async fn serve(self, listener: UnixListener) -> Result<WorkerStop, WorkerError> {
        self.serve_with_shutdown_hooks(listener, NoopWorkerShutdownHooks)
            .await
    }

    /// Serves clients with an injectable signal/artifact boundary. A biased
    /// select gives already-completed close requests deterministic precedence,
    /// then signals, activity, idle expiry, and new accepts. Regardless of the
    /// winner, all resources are consumed by exactly one `shutdown_once` call.
    pub async fn serve_with_shutdown_hooks<H>(
        self,
        listener: UnixListener,
        mut hooks: H,
    ) -> Result<WorkerStop, WorkerError>
    where
        H: WorkerShutdownHooks,
    {
        let connection = Arc::new({
            let shared = SharedConnection::new(self.connection);
            #[cfg(feature = "test-fixtures")]
            let shared = shared.with_fixture_delays(self.fixture_delays);
            shared
        });
        let context = self.context;
        let clock = self.clock;
        let idle_timeout = self.idle_timeout;
        let idle_deadline_observer = self.idle_deadline_observer;
        let activity = Arc::new(IdleActivity::new());
        let mut idle_deadline = idle_deadline(&*clock, idle_timeout);
        if let Some(observer) = &idle_deadline_observer {
            let _ = observer.send(idle_deadline);
        }
        let (cancel_clients, _) = watch::channel(false);
        let mut clients = JoinSet::new();

        let (stop, accept_error) = loop {
            for completed_at in activity.drain_completions() {
                // Reaching the deadline is already idle expiry. A request that
                // crosses, or completes exactly on, the old deadline cannot
                // revive the worker. Earlier completions extend in order.
                if completed_at < idle_deadline {
                    idle_deadline = saturating_deadline(completed_at, idle_timeout);
                    if let Some(observer) = &idle_deadline_observer {
                        let _ = observer.send(idle_deadline);
                    }
                }
            }

            if clock.now() >= idle_deadline {
                break (WorkerStop::IdleTimeout, None);
            }

            tokio::select! {
                biased;
                completed = clients.join_next(), if !clients.is_empty() => {
                    if matches!(completed, Some(Ok(ClientStop::CloseRequested))) {
                        break (WorkerStop::CloseRequested, None);
                    }
                    // A disconnected, malformed, oversized, or panicked client
                    // is isolated while the worker remains in Serving.
                }
                received = hooks.wait_for_signal() => {
                    break (received.stop(), None);
                }
                _ = activity.changed.notified() => {}
                _ = clock.sleep_until(idle_deadline) => {}
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _)) => {
                            let connection = Arc::clone(&connection);
                            let context = context.clone();
                            let activity = Arc::clone(&activity);
                            let clock = Arc::clone(&clock);
                            let cancellation = cancel_clients.subscribe();
                            clients.spawn(async move {
                                serve_client(
                                    stream,
                                    connection,
                                    context,
                                    Some((activity, clock)),
                                    Some(cancellation),
                                )
                                .await
                            });
                        }
                        Err(source) => break (WorkerStop::AcceptFailure, Some(source)),
                    }
                }
            }
        };

        let shutdown = shutdown_once(
            stop,
            listener,
            cancel_clients,
            clients,
            connection,
            &context,
            &mut hooks,
        )
        .await;

        match (accept_error, shutdown) {
            (None, Ok(())) => Ok(stop),
            (None, Err(shutdown)) => Err(WorkerError::Shutdown(shutdown)),
            (Some(accept), Ok(())) => Err(WorkerError::Accept(accept)),
            (Some(accept), Err(shutdown)) => {
                Err(WorkerError::AcceptAndShutdown { accept, shutdown })
            }
        }
    }
}

/// Consumes every resource needed for shutdown, making a second invocation
/// impossible by construction. All cleanup steps are attempted and failures
/// are aggregated; no earlier failure can cause a later side effect to repeat
/// or be skipped.
async fn shutdown_once<H>(
    stop: WorkerStop,
    listener: UnixListener,
    cancel_clients: watch::Sender<bool>,
    mut clients: JoinSet<ClientStop>,
    connection: Arc<SharedConnection>,
    context: &CommandContext,
    hooks: &mut H,
) -> Result<(), WorkerShutdownError>
where
    H: WorkerShutdownHooks,
{
    let mut failures = Vec::new();
    hooks.observe_phase(WorkerShutdownPhase::Draining(stop));

    drop(listener);
    hooks.observe_phase(WorkerShutdownPhase::AcceptStopped);

    let _ = cancel_clients.send(true);
    hooks.observe_phase(WorkerShutdownPhase::ClientsCancelled);
    while let Some(result) = clients.join_next().await {
        if result.is_err() {
            failures.push(WorkerShutdownFailure::ClientJoin);
        }
    }
    hooks.observe_phase(WorkerShutdownPhase::ClientsJoined);

    match Arc::try_unwrap(connection) {
        Ok(connection) => {
            if let Err(error) = connection.close(context).await {
                failures.push(WorkerShutdownFailure::Backend(error));
            }
        }
        Err(_) => failures.push(WorkerShutdownFailure::BackendOwnership),
    }
    hooks.observe_phase(WorkerShutdownPhase::BackendClosed);

    run_cleanup_step(
        hooks.remove_socket(),
        WorkerShutdownStep::SocketCleanup,
        &mut failures,
    );
    hooks.observe_phase(WorkerShutdownPhase::SocketCleaned);
    run_cleanup_step(
        hooks.remove_pid(),
        WorkerShutdownStep::PidCleanup,
        &mut failures,
    );
    hooks.observe_phase(WorkerShutdownPhase::PidCleaned);
    run_cleanup_step(
        hooks.release_lock(),
        WorkerShutdownStep::LockRelease,
        &mut failures,
    );
    hooks.observe_phase(WorkerShutdownPhase::LockReleased);
    hooks.observe_phase(WorkerShutdownPhase::Closed(stop));

    if failures.is_empty() {
        Ok(())
    } else {
        Err(WorkerShutdownError { stop, failures })
    }
}

fn run_cleanup_step(
    result: Result<(), WorkerCleanupError>,
    step: WorkerShutdownStep,
    failures: &mut Vec<WorkerShutdownFailure>,
) {
    if let Err(error) = result {
        failures.push(WorkerShutdownFailure::Cleanup { step, error });
    }
}

fn idle_deadline(clock: &dyn Clock, idle_timeout: Duration) -> Instant {
    Deadline::after(clock, idle_timeout).expires_at()
}

fn saturating_deadline(start: Instant, idle_timeout: Duration) -> Instant {
    start.checked_add(idle_timeout).unwrap_or_else(|| {
        let fixed = FixedInstantClock(start);
        Deadline::after(&fixed, idle_timeout).expires_at()
    })
}

struct FixedInstantClock(Instant);

impl Clock for FixedInstantClock {
    fn now(&self) -> Instant {
        self.0
    }

    fn sleep_until(&self, _deadline: Instant) -> crate::runtime::BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

/// Runs the real per-client worker RPC loop on a caller-provided Unix stream.
///
/// This narrow fixture boundary avoids binding a daemon path or reading user
/// configuration while property tests exercise the production framing,
/// parsing, dispatch, and response-writing path end to end.
#[cfg(feature = "test-fixtures")]
pub async fn serve_test_client(
    stream: UnixStream,
    connection: Box<dyn McpConnection>,
    context: CommandContext,
) {
    let _ = serve_client(
        stream,
        Arc::new(SharedConnection::new(connection)),
        context,
        None,
        None,
    )
    .await;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClientStop {
    Disconnected,
    CloseRequested,
}

async fn serve_client(
    stream: UnixStream,
    connection: Arc<SharedConnection>,
    context: CommandContext,
    idle: Option<(Arc<IdleActivity>, Arc<dyn Clock>)>,
    mut cancellation: Option<watch::Receiver<bool>>,
) -> ClientStop {
    let (mut reader, mut writer) = stream.into_split();
    let mut codec = NdjsonCodec::new();
    let mut input = [0_u8; 8192];
    let mut pending_frames = VecDeque::new();

    loop {
        if pending_frames.is_empty() {
            let Some(read) = cancellable(&mut cancellation, reader.read(&mut input)).await else {
                return ClientStop::Disconnected;
            };
            let read = match read {
                Ok(0) => {
                    let _ = codec.finish();
                    return ClientStop::Disconnected;
                }
                Ok(read) => read,
                Err(_) => return ClientStop::Disconnected,
            };
            let frames = match codec.push_request_frames(&input[..read]) {
                Ok(frames) => frames,
                Err(_) => return ClientStop::Disconnected,
            };
            pending_frames.extend(frames);
            continue;
        }

        let frame = pending_frames
            .pop_front()
            .expect("non-empty pending frame queue");
        let parsed = parse_request_frame(&frame);
        let (response, close_requested) = match parsed {
            Ok(request) => {
                let executed = ConnectedClientInput {
                    reader: &mut reader,
                    codec: &mut codec,
                    pending_frames: &mut pending_frames,
                    input: &mut input,
                    cancellation: &mut cancellation,
                }
                .execute(connection.as_ref(), &context, request)
                .await;
                let Some(executed) = executed else {
                    return ClientStop::Disconnected;
                };
                if let Some((activity, clock)) = &idle {
                    activity.record_completion(clock.now());
                }
                executed
            }
            Err(error) => (safe_failure(error.id, error.code), false),
        };

        let Some(written) =
            cancellable(&mut cancellation, write_response(&mut writer, response)).await
        else {
            return ClientStop::Disconnected;
        };
        match written {
            WriteResult::Written => {}
            WriteResult::WrittenThenClose | WriteResult::Closed => {
                return ClientStop::Disconnected;
            }
        }

        if close_requested {
            return ClientStop::CloseRequested;
        }
    }
}

/// Executes one backend request while continuing to observe the client read
/// half. EOF, framing failure, or worker cancellation drops the backend future
/// before the client write half is dropped. A fail-closed DaemonClient can
/// therefore half-close its stream and wait for EOF as cancellation
/// acknowledgement before a manager starts a direct fallback attempt.
struct ConnectedClientInput<'a> {
    reader: &'a mut tokio::net::unix::OwnedReadHalf,
    codec: &'a mut NdjsonCodec,
    pending_frames: &'a mut VecDeque<Vec<u8>>,
    input: &'a mut [u8],
    cancellation: &'a mut Option<watch::Receiver<bool>>,
}

impl ConnectedClientInput<'_> {
    async fn execute(
        &mut self,
        connection: &SharedConnection,
        context: &CommandContext,
        request: IpcRequest,
    ) -> Option<(IpcResponse, bool)> {
        let mut execution = Box::pin(execute_request(connection, context, request));
        loop {
            if let Some(cancellation) = self.cancellation.as_mut() {
                tokio::select! {
                    biased;
                    _ = wait_for_cancellation(cancellation) => return None,
                    read = self.reader.read(self.input) => {
                        if !queue_client_input(
                            read,
                            self.codec,
                            self.pending_frames,
                            self.input,
                        ) {
                            return None;
                        }
                    }
                    result = &mut execution => return Some(result),
                }
            } else {
                tokio::select! {
                    biased;
                    read = self.reader.read(self.input) => {
                        if !queue_client_input(
                            read,
                            self.codec,
                            self.pending_frames,
                            self.input,
                        ) {
                            return None;
                        }
                    }
                    result = &mut execution => return Some(result),
                }
            }
        }
    }
}

fn queue_client_input(
    read: io::Result<usize>,
    codec: &mut NdjsonCodec,
    pending_frames: &mut VecDeque<Vec<u8>>,
    input: &[u8],
) -> bool {
    let read = match read {
        Ok(0) => {
            let _ = codec.finish();
            return false;
        }
        Ok(read) => read,
        Err(_) => return false,
    };
    match codec.push_request_frames(&input[..read]) {
        Ok(frames) => {
            pending_frames.extend(frames);
            true
        }
        Err(_) => false,
    }
}

async fn cancellable<T>(
    cancellation: &mut Option<watch::Receiver<bool>>,
    future: impl Future<Output = T>,
) -> Option<T> {
    match cancellation {
        Some(cancellation) => {
            tokio::select! {
                biased;
                _ = wait_for_cancellation(cancellation) => None,
                result = future => Some(result),
            }
        }
        None => Some(future.await),
    }
}

async fn wait_for_cancellation(cancellation: &mut watch::Receiver<bool>) {
    loop {
        if *cancellation.borrow_and_update() {
            return;
        }
        if cancellation.changed().await.is_err() {
            return;
        }
    }
}

struct RequestError {
    id: String,
    code: IpcErrorCode,
}

fn parse_request_frame(frame: &[u8]) -> Result<IpcRequest, RequestError> {
    let value = serde_json::from_slice::<Value>(frame).map_err(|_| RequestError {
        id: String::new(),
        code: IpcErrorCode::InvalidJson,
    })?;
    let object = value.as_object().ok_or_else(|| RequestError {
        id: String::new(),
        code: IpcErrorCode::MissingId,
    })?;

    let id = match object.get("id") {
        Some(Value::String(id)) if validate_request_id(id).is_ok() => id.clone(),
        None => {
            return Err(RequestError {
                id: String::new(),
                code: IpcErrorCode::MissingId,
            });
        }
        _ => {
            return Err(RequestError {
                id: String::new(),
                code: IpcErrorCode::InvalidArguments,
            });
        }
    };

    let operation_type = match object.get("type") {
        Some(Value::String(operation_type)) => operation_type.as_str(),
        _ => {
            return Err(RequestError {
                id,
                code: IpcErrorCode::UnknownType,
            });
        }
    };
    if !matches!(
        operation_type,
        "ping" | "listTools" | "callTool" | "getInstructions" | "close"
    ) {
        return Err(RequestError {
            id,
            code: IpcErrorCode::UnknownType,
        });
    }

    serde_json::from_value(value).map_err(|_| RequestError {
        id,
        code: IpcErrorCode::InvalidArguments,
    })
}

async fn execute_request(
    shared: &SharedConnection,
    startup_context: &CommandContext,
    request: IpcRequest,
) -> (IpcResponse, bool) {
    // The bootstrap deadline only bounds backend initialization. Reusing it
    // would make every request fail once a healthy daemon has lived past the
    // startup cap, so each IPC operation gets a fresh request-local budget
    // matching the client-side IPC cap.
    let context = CommandContext {
        deadline: Deadline::after(&SystemClock, crate::daemon::client::DAEMON_IPC_CAP),
        cancellation: Arc::clone(&startup_context.cancellation),
        diagnostics: Arc::clone(&startup_context.diagnostics),
    };
    let (id, operation) = request.into_parts();
    shared.before_request(&operation).await;
    let connection = shared.as_connection();
    match operation {
        IpcOperation::Ping => (safe_success(id, json!("pong")), false),
        IpcOperation::ListTools => match connection.list_tools(&context).await {
            Ok(tools) => match serde_json::to_value(tools) {
                Ok(tools) => (safe_success(id, tools), false),
                Err(_) => (safe_failure(id, IpcErrorCode::Internal), false),
            },
            Err(_) => (safe_failure(id, IpcErrorCode::ExecutionError), false),
        },
        IpcOperation::CallTool { tool_name, args } => {
            match connection.call_tool(&context, &tool_name, args).await {
                Ok(result) => (safe_success(id, result), false),
                Err(_) => (safe_failure(id, IpcErrorCode::ExecutionError), false),
            }
        }
        IpcOperation::GetInstructions => (
            safe_success(
                id,
                connection
                    .instructions()
                    .map_or(Value::Null, |value| Value::String(value.to_owned())),
            ),
            false,
        ),
        IpcOperation::Close => (safe_success(id, json!("closing")), true),
    }
}

fn safe_success(id: String, data: Value) -> IpcResponse {
    IpcResponse::success(id, data).expect("validated request IDs produce valid responses")
}

fn safe_failure(id: String, code: IpcErrorCode) -> IpcResponse {
    IpcResponse::failure(id, code).expect("failure responses accept valid or unavailable IDs")
}

enum WriteResult {
    Written,
    WrittenThenClose,
    Closed,
}

async fn write_response<W>(stream: &mut W, response: IpcResponse) -> WriteResult
where
    W: AsyncWrite + Unpin,
{
    match encode_message(&response) {
        Ok(frame) => match stream.write_all(&frame).await {
            Ok(()) => WriteResult::Written,
            Err(_) => WriteResult::Closed,
        },
        Err(FrameError::FrameTooLarge) => {
            let oversized = safe_failure(response.id().to_owned(), IpcErrorCode::FrameTooLarge);
            let Ok(frame) = encode_message(&oversized) else {
                return WriteResult::Closed;
            };
            match stream.write_all(&frame).await {
                Ok(()) => WriteResult::WrittenThenClose,
                Err(_) => WriteResult::Closed,
            }
        }
        Err(_) => {
            let internal = safe_failure(response.id().to_owned(), IpcErrorCode::Internal);
            let Ok(frame) = encode_message(&internal) else {
                return WriteResult::Closed;
            };
            match stream.write_all(&frame).await {
                Ok(()) => WriteResult::Written,
                Err(_) => WriteResult::Closed,
            }
        }
    }
}

const WORKER_BOOTSTRAP_VERSION: u8 = 1;
const WORKER_BOOTSTRAP_MAX_BYTES: usize = 16 * 1024 * 1024;
const WORKER_STARTUP_CAP: Duration = Duration::from_secs(5);
/// The sole byte sent over the inherited anonymous ready pipe.
pub const WORKER_READY_BYTE: u8 = 1;

/// Test-fixture-only daemon-side delay keys. They are carried in the private
/// bootstrap envelope and never in worker argv or environment.
#[cfg(feature = "test-fixtures")]
pub const TEST_DAEMON_PING_DELAY_ENV: &str = "MCP_CLI_TEST_DAEMON_PING_DELAY_MS";
#[cfg(feature = "test-fixtures")]
pub const TEST_DAEMON_CALL_DELAY_ENV: &str = "MCP_CLI_TEST_DAEMON_CALL_DELAY_MS";

/// Process-test-only startup checkpoints used to prove that ready publication
/// is atomic and that every partially-created artifact is reclaimed.
#[cfg(feature = "test-fixtures")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkerStartupFault {
    BeforeBackend,
    BeforeSocket,
    BeforePid,
    BeforeReady,
}

/// Stable, payload-free failures from worker bootstrap. Display deliberately
/// excludes paths, configuration values, environment entries, and adapter
/// errors so a failed hidden invocation cannot disclose bootstrap data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerBootstrapError {
    InvalidInput,
    UnsafeRuntime,
    LockHeld,
    BackendInitialization,
    SocketPublication,
    MetadataPublication,
    ReadyPublication,
    SignalBeforeReady,
    Service,
}

impl fmt::Display for WorkerBootstrapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidInput => "invalid daemon worker bootstrap",
            Self::UnsafeRuntime => "unsafe daemon worker runtime",
            Self::LockHeld => "daemon worker already starting or running",
            Self::BackendInitialization => "daemon MCP initialization failed",
            Self::SocketPublication => "daemon socket publication failed",
            Self::MetadataPublication => "daemon metadata publication failed",
            Self::ReadyPublication => "daemon ready publication failed",
            Self::SignalBeforeReady => "daemon startup interrupted",
            Self::Service => "daemon worker service failed",
        })
    }
}

impl std::error::Error for WorkerBootstrapError {}

/// Object-safe process spawning boundary used by the Unix connection manager.
pub trait DaemonSpawner: Send + Sync {
    fn spawn<'a>(
        &'a self,
        context: &'a CommandContext,
        server: &'a ServerDefinition,
        paths: &'a DaemonPaths,
    ) -> BoxFuture<'a, Result<DaemonReady, DaemonSpawnError>>;
}

/// Current-executable daemon spawner. Tests may inject the built `mcp-cli`
/// path while production resolves the current executable at call time.
#[derive(Clone, Debug)]
pub struct CurrentExecutableDaemonSpawner {
    executable: Option<PathBuf>,
    idle_timeout: Duration,
    #[cfg(feature = "test-fixtures")]
    startup_fault: Option<WorkerStartupFault>,
}

impl CurrentExecutableDaemonSpawner {
    pub fn new(idle_timeout: Duration) -> Self {
        Self {
            executable: None,
            idle_timeout,
            #[cfg(feature = "test-fixtures")]
            startup_fault: None,
        }
    }

    pub fn with_executable(executable: PathBuf, idle_timeout: Duration) -> Self {
        Self {
            executable: Some(executable),
            idle_timeout,
            #[cfg(feature = "test-fixtures")]
            startup_fault: None,
        }
    }

    /// Injects a payload-free startup failure into the hidden worker process.
    /// This is available only to process tests and is transported inside the
    /// same private stdin envelope as the rest of the bootstrap data.
    #[cfg(feature = "test-fixtures")]
    pub fn with_startup_fault(mut self, fault: WorkerStartupFault) -> Self {
        self.startup_fault = Some(fault);
        self
    }
}

/// A child that emitted the one-byte ready token only after all publication
/// prerequisites completed. Keeping the handle lets callers reap it in tests
/// and lets the future manager retain explicit process ownership.
pub struct DaemonReady {
    pid: u32,
    child: Child,
}

impl DaemonReady {
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    pub fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    pub fn into_child(self) -> Child {
        self.child
    }
}

/// Stable parent-side daemon startup failures. Sources are intentionally not
/// retained because process and serialization errors can include sensitive
/// bootstrap fragments on some platforms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonSpawnError {
    InvalidEnvironment,
    InvalidPaths,
    SerializeBootstrap,
    SpawnWorker,
    TransferBootstrap,
    ReadyTimeout,
    WorkerExitedBeforeReady,
    InvalidReady,
    Cancelled,
}

impl fmt::Display for DaemonSpawnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidEnvironment => "daemon environment snapshot is invalid",
            Self::InvalidPaths => "daemon runtime paths are invalid",
            Self::SerializeBootstrap => "daemon bootstrap serialization failed",
            Self::SpawnWorker => "daemon worker process could not be started",
            Self::TransferBootstrap => "daemon bootstrap transfer failed",
            Self::ReadyTimeout => "daemon worker did not become ready in time",
            Self::WorkerExitedBeforeReady => "daemon worker exited before ready",
            Self::InvalidReady => "daemon worker returned an invalid ready token",
            Self::Cancelled => "daemon startup was cancelled",
        })
    }
}

impl std::error::Error for DaemonSpawnError {}

impl DaemonSpawner for CurrentExecutableDaemonSpawner {
    fn spawn<'a>(
        &'a self,
        context: &'a CommandContext,
        server: &'a ServerDefinition,
        paths: &'a DaemonPaths,
    ) -> BoxFuture<'a, Result<DaemonReady, DaemonSpawnError>> {
        Box::pin(async move {
            paths
                .validate_runtime_dir()
                .map_err(|_| DaemonSpawnError::InvalidPaths)?;
            let clock = SystemClock;
            let startup_budget = context.remaining_capped(&clock, WORKER_STARTUP_CAP);
            if startup_budget.is_zero() {
                return Err(DaemonSpawnError::ReadyTimeout);
            }
            let environment = capture_worker_parent_environment()?;
            let mut envelope = WorkerBootstrapEnvelope::new(
                server,
                paths,
                &environment,
                self.idle_timeout,
                startup_budget,
            )?;
            #[cfg(feature = "test-fixtures")]
            {
                envelope.startup_fault = self.startup_fault;
            }
            let encoded =
                serde_json::to_vec(&envelope).map_err(|_| DaemonSpawnError::SerializeBootstrap)?;
            if encoded.len() > WORKER_BOOTSTRAP_MAX_BYTES {
                return Err(DaemonSpawnError::SerializeBootstrap);
            }

            let executable = match &self.executable {
                Some(executable) => executable.clone(),
                None => std::env::current_exe().map_err(|_| DaemonSpawnError::SpawnWorker)?,
            };
            let mut child = Command::new(executable)
                .arg("__daemon")
                // The worker receives no inherited environment. Even parent
                // variables used for substitution travel only inside stdin.
                .env_clear()
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|_| DaemonSpawnError::SpawnWorker)?;
            let pid = child.id().ok_or(DaemonSpawnError::SpawnWorker)?;
            let mut child_stdin = child.stdin.take().ok_or(DaemonSpawnError::SpawnWorker)?;
            let mut ready_pipe = child.stdout.take().ok_or(DaemonSpawnError::SpawnWorker)?;

            let transfer = async {
                child_stdin
                    .write_all(&encoded)
                    .await
                    .map_err(|_| DaemonSpawnError::TransferBootstrap)?;
                child_stdin
                    .shutdown()
                    .await
                    .map_err(|_| DaemonSpawnError::TransferBootstrap)?;
                drop(child_stdin);
                let mut ready = [0_u8; 1];
                match ready_pipe.read_exact(&mut ready).await {
                    Ok(_) if ready[0] == WORKER_READY_BYTE => Ok(()),
                    Ok(_) => Err(DaemonSpawnError::InvalidReady),
                    Err(source) if source.kind() == io::ErrorKind::UnexpectedEof => {
                        Err(DaemonSpawnError::WorkerExitedBeforeReady)
                    }
                    Err(_) => Err(DaemonSpawnError::TransferBootstrap),
                }
            };

            let outcome = tokio::select! {
                biased;
                _ = wait_for_command_cancellation(context) => Err(DaemonSpawnError::Cancelled),
                _ = tokio::time::sleep(startup_budget) => Err(DaemonSpawnError::ReadyTimeout),
                result = transfer => result,
            };
            if let Err(error) = outcome {
                terminate_failed_child(&mut child, paths, pid).await;
                return Err(error);
            }

            Ok(DaemonReady { pid, child })
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerBootstrapEnvelope {
    version: u8,
    runtime_parent: PathBuf,
    server: WorkerServer,
    parent_environment: BTreeMap<String, String>,
    idle_timeout_millis: u64,
    startup_timeout_millis: u64,
    #[cfg(feature = "test-fixtures")]
    #[serde(default)]
    startup_fault: Option<WorkerStartupFault>,
}

impl WorkerBootstrapEnvelope {
    fn new(
        server: &ServerDefinition,
        paths: &DaemonPaths,
        parent_environment: &BTreeMap<String, String>,
        idle_timeout: Duration,
        startup_timeout: Duration,
    ) -> Result<Self, DaemonSpawnError> {
        let runtime_parent = paths
            .runtime_dir
            .parent()
            .ok_or(DaemonSpawnError::InvalidPaths)?
            .to_path_buf();
        Ok(Self {
            version: WORKER_BOOTSTRAP_VERSION,
            runtime_parent,
            server: WorkerServer::from(server),
            parent_environment: parent_environment.clone(),
            idle_timeout_millis: duration_millis(idle_timeout),
            startup_timeout_millis: duration_millis(startup_timeout),
            #[cfg(feature = "test-fixtures")]
            startup_fault: None,
        })
    }

    fn validate(self) -> Result<ValidatedWorkerBootstrap, WorkerBootstrapError> {
        if self.version != WORKER_BOOTSTRAP_VERSION
            || self.idle_timeout_millis == 0
            || self.startup_timeout_millis == 0
            || self.startup_timeout_millis > duration_millis(WORKER_STARTUP_CAP)
        {
            return Err(WorkerBootstrapError::InvalidInput);
        }
        let server = self.server.into_server_definition()?;
        let paths = DaemonPaths::from_runtime_parent(&self.runtime_parent, &server.id)
            .map_err(|_| WorkerBootstrapError::UnsafeRuntime)?;
        Ok(ValidatedWorkerBootstrap {
            server,
            paths,
            parent_environment: self.parent_environment,
            idle_timeout: Duration::from_millis(self.idle_timeout_millis),
            startup_timeout: Duration::from_millis(self.startup_timeout_millis),
            #[cfg(feature = "test-fixtures")]
            startup_fault: self.startup_fault,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerServer {
    name: String,
    id: String,
    config_hash: String,
    transport: WorkerTransport,
    filter: WorkerFilter,
}

impl From<&ServerDefinition> for WorkerServer {
    fn from(server: &ServerDefinition) -> Self {
        Self {
            name: server.name.clone(),
            id: server.id.0.clone(),
            config_hash: server.config_hash.to_hex(),
            transport: WorkerTransport::from(&server.transport),
            filter: WorkerFilter::from(&server.filter),
        }
    }
}

impl WorkerServer {
    fn into_server_definition(self) -> Result<ServerDefinition, WorkerBootstrapError> {
        if server_id(&self.name).0 != self.id {
            return Err(WorkerBootstrapError::InvalidInput);
        }
        let config_hash = decode_config_hash(&self.config_hash)?;
        let transport = match self.transport {
            WorkerTransport::Stdio {
                command,
                args,
                env,
                cwd,
            } if !command.is_empty() => TransportConfig::Stdio {
                command,
                args,
                env,
                cwd,
            },
            WorkerTransport::Http { url, headers } => {
                let url = url::Url::parse(&url).map_err(|_| WorkerBootstrapError::InvalidInput)?;
                if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
                    return Err(WorkerBootstrapError::InvalidInput);
                }
                TransportConfig::Http { url, headers }
            }
            WorkerTransport::Stdio { .. } => return Err(WorkerBootstrapError::InvalidInput),
        };
        Ok(ServerDefinition {
            name: self.name,
            id: ServerId(self.id),
            config_hash,
            transport,
            filter: ToolFilterConfig {
                allowed_tools: self.filter.allowed_tools,
                disabled_tools: self.filter.disabled_tools,
            },
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind")]
enum WorkerTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

impl From<&TransportConfig> for WorkerTransport {
    fn from(transport: &TransportConfig) -> Self {
        match transport {
            TransportConfig::Stdio {
                command,
                args,
                env,
                cwd,
            } => Self::Stdio {
                command: command.clone(),
                args: args.clone(),
                env: env.clone(),
                cwd: cwd.clone(),
            },
            TransportConfig::Http { url, headers } => Self::Http {
                url: url.as_str().to_owned(),
                headers: headers.clone(),
            },
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerFilter {
    allowed_tools: Vec<String>,
    disabled_tools: Vec<String>,
}

impl From<&ToolFilterConfig> for WorkerFilter {
    fn from(filter: &ToolFilterConfig) -> Self {
        Self {
            allowed_tools: filter.allowed_tools.clone(),
            disabled_tools: filter.disabled_tools.clone(),
        }
    }
}

struct ValidatedWorkerBootstrap {
    server: ServerDefinition,
    paths: DaemonPaths,
    parent_environment: BTreeMap<String, String>,
    idle_timeout: Duration,
    startup_timeout: Duration,
    #[cfg(feature = "test-fixtures")]
    startup_fault: Option<WorkerStartupFault>,
}

/// Reads the sensitive bootstrap exactly once from stdin and runs a worker.
/// No fallback file is needed on supported Unix targets, so no configuration
/// is ever persisted by this implementation.
pub async fn run_worker<R, W>(
    reader: &mut R,
    ready: &mut W,
) -> Result<WorkerStop, WorkerBootstrapError>
where
    R: Read,
    W: Write,
{
    let mut encoded = Vec::new();
    reader
        .take((WORKER_BOOTSTRAP_MAX_BYTES + 1) as u64)
        .read_to_end(&mut encoded)
        .map_err(|_| WorkerBootstrapError::InvalidInput)?;
    if encoded.is_empty() || encoded.len() > WORKER_BOOTSTRAP_MAX_BYTES {
        return Err(WorkerBootstrapError::InvalidInput);
    }
    let envelope: WorkerBootstrapEnvelope =
        serde_json::from_slice(&encoded).map_err(|_| WorkerBootstrapError::InvalidInput)?;
    run_validated_worker(envelope.validate()?, ready).await
}

async fn run_validated_worker<W: Write>(
    bootstrap: ValidatedWorkerBootstrap,
    ready: &mut W,
) -> Result<WorkerStop, WorkerBootstrapError> {
    let mut artifacts = BootstrapArtifacts::acquire(bootstrap.paths.clone())?;
    let secrets = configured_worker_secrets(&bootstrap.server);
    let diagnostics = Arc::new(WriterDiagnosticSink::new(io::stderr(), false, secrets));
    let context = CommandContext {
        deadline: Deadline::after(&SystemClock, bootstrap.startup_timeout),
        cancellation: Arc::new(CancellationFlag::default()),
        diagnostics,
    };

    let mut interrupt =
        signal(SignalKind::interrupt()).map_err(|_| WorkerBootstrapError::BackendInitialization)?;
    let mut terminate =
        signal(SignalKind::terminate()).map_err(|_| WorkerBootstrapError::BackendInitialization)?;
    #[cfg(feature = "test-fixtures")]
    if bootstrap.startup_fault == Some(WorkerStartupFault::BeforeBackend) {
        return Err(WorkerBootstrapError::BackendInitialization);
    }
    let connector = RmcpDirectConnector;
    let connection = tokio::select! {
        biased;
        _ = interrupt.recv() => return Err(WorkerBootstrapError::SignalBeforeReady),
        _ = terminate.recv() => return Err(WorkerBootstrapError::SignalBeforeReady),
        result = connector.connect_with_parent_environment(
            &context,
            &bootstrap.server,
            &bootstrap.parent_environment,
        ) => result.map_err(|_| WorkerBootstrapError::BackendInitialization)?,
    };
    drop(interrupt);
    drop(terminate);

    #[cfg(feature = "test-fixtures")]
    if bootstrap.startup_fault == Some(WorkerStartupFault::BeforeSocket) {
        close_bootstrap_connection(connection, &context).await;
        return Err(WorkerBootstrapError::SocketPublication);
    }
    let listener = match bind_worker_socket(&bootstrap.paths) {
        Ok(listener) => listener,
        Err(error) => {
            close_bootstrap_connection(connection, &context).await;
            return Err(error);
        }
    };
    if let Err(error) = artifacts.capture_socket() {
        close_bootstrap_connection(connection, &context).await;
        return Err(error);
    }

    #[cfg(feature = "test-fixtures")]
    if bootstrap.startup_fault == Some(WorkerStartupFault::BeforePid) {
        close_bootstrap_connection(connection, &context).await;
        return Err(WorkerBootstrapError::MetadataPublication);
    }
    if fs::symlink_metadata(&bootstrap.paths.pid).is_ok() {
        close_bootstrap_connection(connection, &context).await;
        return Err(WorkerBootstrapError::MetadataPublication);
    }
    let store = MetadataStore::new(bootstrap.paths.clone());
    let process_metadata = PidMetadata::for_current_worker(bootstrap.server.config_hash)
        .map_err(|_| WorkerBootstrapError::MetadataPublication)?;
    if store.write(&process_metadata).is_err() {
        close_bootstrap_connection(connection, &context).await;
        return Err(WorkerBootstrapError::MetadataPublication);
    }
    if let Err(error) = artifacts.capture_pid() {
        close_bootstrap_connection(connection, &context).await;
        return Err(error);
    }

    #[cfg(feature = "test-fixtures")]
    if bootstrap.startup_fault == Some(WorkerStartupFault::BeforeReady) {
        close_bootstrap_connection(connection, &context).await;
        return Err(WorkerBootstrapError::ReadyPublication);
    }
    let mut hooks = match UnixWorkerShutdownHooks::new(bootstrap.paths.clone()) {
        Ok(hooks) => hooks,
        Err(_) => {
            close_bootstrap_connection(connection, &context).await;
            return Err(WorkerBootstrapError::MetadataPublication);
        }
    };
    if ready
        .write_all(&[WORKER_READY_BYTE])
        .and_then(|_| ready.flush())
        .is_err()
    {
        close_bootstrap_connection(connection, &context).await;
        let _ = hooks.remove_socket();
        let _ = hooks.remove_pid();
        let _ = hooks.release_lock();
        artifacts.disarm();
        return Err(WorkerBootstrapError::ReadyPublication);
    }

    artifacts.disarm();
    let service = WorkerIpcService::new(connection, context, bootstrap.idle_timeout);
    #[cfg(feature = "test-fixtures")]
    let service = service.with_fixture_delays(worker_fixture_delays(&bootstrap.server));
    service
        .serve_with_shutdown_hooks(listener, hooks)
        .await
        .map_err(|_| WorkerBootstrapError::Service)
}

async fn close_bootstrap_connection(connection: Box<dyn McpConnection>, context: &CommandContext) {
    let _ = connection.close(context).await;
}

fn bind_worker_socket(paths: &DaemonPaths) -> Result<UnixListener, WorkerBootstrapError> {
    paths
        .validate_runtime_dir()
        .map_err(|_| WorkerBootstrapError::UnsafeRuntime)?;
    match fs::symlink_metadata(&paths.socket) {
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        _ => return Err(WorkerBootstrapError::SocketPublication),
    }
    let listener =
        UnixListener::bind(&paths.socket).map_err(|_| WorkerBootstrapError::SocketPublication)?;
    fs::set_permissions(
        &paths.socket,
        fs::Permissions::from_mode(private_file_mode()),
    )
    .map_err(|_| WorkerBootstrapError::SocketPublication)?;
    Ok(listener)
}

struct BootstrapArtifacts {
    paths: DaemonPaths,
    _lock_file: fs::File,
    lock: Option<ArtifactIdentity>,
    socket: Option<ArtifactIdentity>,
    pid: Option<ArtifactIdentity>,
    armed: bool,
}

impl BootstrapArtifacts {
    fn acquire(paths: DaemonPaths) -> Result<Self, WorkerBootstrapError> {
        paths
            .validate_runtime_dir()
            .map_err(|_| WorkerBootstrapError::UnsafeRuntime)?;
        let mut lock_file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(private_file_mode())
            .open(&paths.lock)
        {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                return Err(WorkerBootstrapError::LockHeld);
            }
            Err(_) => return Err(WorkerBootstrapError::UnsafeRuntime),
        };
        let pid = std::process::id().to_string();
        if lock_file
            .set_permissions(fs::Permissions::from_mode(private_file_mode()))
            .and_then(|_| lock_file.write_all(pid.as_bytes()))
            .and_then(|_| lock_file.sync_all())
            .is_err()
        {
            let _ = fs::remove_file(&paths.lock);
            return Err(WorkerBootstrapError::UnsafeRuntime);
        }
        let lock = paths
            .capture_lock_identity()
            .map_err(|_| WorkerBootstrapError::UnsafeRuntime)?
            .ok_or(WorkerBootstrapError::UnsafeRuntime)?;
        Ok(Self {
            paths,
            _lock_file: lock_file,
            lock: Some(lock),
            socket: None,
            pid: None,
            armed: true,
        })
    }

    fn capture_socket(&mut self) -> Result<(), WorkerBootstrapError> {
        self.socket = self
            .paths
            .capture_socket_identity()
            .map_err(|_| WorkerBootstrapError::SocketPublication)?;
        if self.socket.is_none() {
            return Err(WorkerBootstrapError::SocketPublication);
        }
        Ok(())
    }

    fn capture_pid(&mut self) -> Result<(), WorkerBootstrapError> {
        self.pid = self
            .paths
            .capture_pid_identity()
            .map_err(|_| WorkerBootstrapError::MetadataPublication)?;
        if self.pid.is_none() {
            return Err(WorkerBootstrapError::MetadataPublication);
        }
        Ok(())
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for BootstrapArtifacts {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(identity) = self.socket.take() {
            let _ = self.paths.remove_socket_if_owned(identity);
        }
        if let Some(identity) = self.pid.take() {
            let _ = self.paths.remove_pid_if_owned(identity);
        }
        if let Some(identity) = self.lock.take() {
            let _ = self.paths.remove_lock_if_owned(identity);
        }
    }
}

#[cfg(feature = "test-fixtures")]
fn worker_fixture_delays(server: &ServerDefinition) -> WorkerFixtureDelays {
    let TransportConfig::Stdio { env, .. } = &server.transport else {
        return WorkerFixtureDelays::default();
    };
    let milliseconds = |name: &str| {
        env.get(name)
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or_default()
    };
    WorkerFixtureDelays {
        ping: milliseconds(TEST_DAEMON_PING_DELAY_ENV),
        call: milliseconds(TEST_DAEMON_CALL_DELAY_ENV),
    }
}

fn configured_worker_secrets(server: &ServerDefinition) -> SecretSet {
    let mut secrets = SecretSet::new();
    match &server.transport {
        TransportConfig::Stdio { env, .. } => {
            for (name, value) in env {
                secrets.register_env(name, value);
            }
        }
        TransportConfig::Http { headers, .. } => {
            for (name, value) in headers {
                secrets.register_header(name, value);
            }
        }
    }
    secrets
}

fn capture_worker_parent_environment() -> Result<BTreeMap<String, String>, DaemonSpawnError> {
    std::env::vars_os()
        .map(|(key, value)| {
            let key = key
                .into_string()
                .map_err(|_| DaemonSpawnError::InvalidEnvironment)?;
            let value = value
                .into_string()
                .map_err(|_| DaemonSpawnError::InvalidEnvironment)?;
            Ok((key, value))
        })
        .collect()
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().clamp(1, u64::MAX as u128) as u64
}

async fn wait_for_command_cancellation(context: &CommandContext) {
    while !context.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn decode_config_hash(encoded: &str) -> Result<ConfigHash, WorkerBootstrapError> {
    if encoded.len() != SHA256_HEX_LENGTH {
        return Err(WorkerBootstrapError::InvalidInput);
    }
    let mut bytes = [0_u8; 32];
    for (index, pair) in encoded.as_bytes().chunks_exact(2).enumerate() {
        let high = decode_hex(pair[0]).ok_or(WorkerBootstrapError::InvalidInput)?;
        let low = decode_hex(pair[1]).ok_or(WorkerBootstrapError::InvalidInput)?;
        bytes[index] = high << 4 | low;
    }
    Ok(ConfigHash(bytes))
}

fn decode_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

async fn terminate_failed_child(child: &mut Child, paths: &DaemonPaths, pid: u32) {
    let _ = child.start_kill();
    let _ = child.wait().await;
    cleanup_failed_child_artifacts(paths, pid);
}

fn cleanup_failed_child_artifacts(paths: &DaemonPaths, pid: u32) {
    let Some(lock_identity) = owned_lock_for_pid(paths, pid) else {
        return;
    };
    let socket = paths.capture_socket_identity().ok().flatten();
    let pid_identity = MetadataStore::new(paths.clone())
        .read()
        .ok()
        .filter(|metadata| metadata.pid == pid)
        .and_then(|_| paths.capture_pid_identity().ok().flatten());
    if let Some(identity) = socket {
        let _ = paths.remove_socket_if_owned(identity);
    }
    if let Some(identity) = pid_identity {
        let _ = paths.remove_pid_if_owned(identity);
    }
    let _ = paths.remove_lock_if_owned(lock_identity);
}

fn owned_lock_for_pid(paths: &DaemonPaths, pid: u32) -> Option<ArtifactIdentity> {
    paths.validate_runtime_dir().ok()?;
    let before = fs::symlink_metadata(&paths.lock).ok()?;
    paths
        .validate_artifact_metadata(&paths.lock, &before, ArtifactKind::RegularFile, true)
        .ok()?;
    let file = fs::File::open(&paths.lock).ok()?;
    let opened = file.metadata().ok()?;
    if before.dev() != opened.dev() || before.ino() != opened.ino() {
        return None;
    }
    let mut contents = String::new();
    file.take(32).read_to_string(&mut contents).ok()?;
    let after = fs::symlink_metadata(&paths.lock).ok()?;
    if opened.dev() != after.dev() || opened.ino() != after.ino() {
        return None;
    }
    if contents != pid.to_string() {
        return None;
    }
    paths.capture_lock_identity().ok().flatten()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        path::Path,
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixStream,
        sync::{Notify, watch},
        time::timeout,
    };

    use super::*;
    use crate::{
        config::server_id,
        connection::ConnectionError,
        domain::{ConnectionMode, JsonObject, ToolInfo, ToolResult},
        output::DiagnosticSink,
        runtime::{BoxFuture, CancellationFlag, Deadline},
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

    #[derive(Clone)]
    struct TestClock {
        now: Arc<watch::Sender<Instant>>,
    }

    impl TestClock {
        fn new(start: Instant) -> Self {
            let (now, _) = watch::channel(start);
            Self { now: Arc::new(now) }
        }

        fn advance(&self, duration: Duration) -> Instant {
            let next = self
                .now()
                .checked_add(duration)
                .expect("test clock overflow");
            self.now.send_replace(next);
            next
        }
    }

    impl Clock for TestClock {
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

    fn context_at(start: Instant) -> CommandContext {
        CommandContext {
            deadline: Deadline::new(start + Duration::from_secs(300)),
            cancellation: Arc::new(CancellationFlag::default()),
            diagnostics: Arc::new(NullDiagnostics),
        }
    }

    #[derive(Default)]
    struct HookState {
        phases: Mutex<Vec<WorkerShutdownPhase>>,
        socket_count: AtomicUsize,
        pid_count: AtomicUsize,
        lock_count: AtomicUsize,
    }

    struct ChannelHooks {
        signals: mpsc::UnboundedReceiver<WorkerSignal>,
        state: Arc<HookState>,
        fail_cleanup: bool,
    }

    impl ChannelHooks {
        fn new(fail_cleanup: bool) -> (Self, mpsc::UnboundedSender<WorkerSignal>, Arc<HookState>) {
            let (sender, signals) = mpsc::unbounded_channel();
            let state = Arc::new(HookState::default());
            (
                Self {
                    signals,
                    state: Arc::clone(&state),
                    fail_cleanup,
                },
                sender,
                state,
            )
        }

        fn cleanup_result(&self) -> Result<(), WorkerCleanupError> {
            if self.fail_cleanup {
                Err(WorkerCleanupError::new(io::Error::other(
                    "injected cleanup detail must stay internal",
                )))
            } else {
                Ok(())
            }
        }
    }

    impl WorkerShutdownHooks for ChannelHooks {
        fn wait_for_signal(&mut self) -> BoxFuture<'_, WorkerSignal> {
            Box::pin(async move {
                match self.signals.recv().await {
                    Some(signal) => signal,
                    None => pending().await,
                }
            })
        }

        fn remove_socket(&mut self) -> Result<(), WorkerCleanupError> {
            self.state.socket_count.fetch_add(1, Ordering::SeqCst);
            self.cleanup_result()
        }

        fn remove_pid(&mut self) -> Result<(), WorkerCleanupError> {
            self.state.pid_count.fetch_add(1, Ordering::SeqCst);
            self.cleanup_result()
        }

        fn release_lock(&mut self) -> Result<(), WorkerCleanupError> {
            self.state.lock_count.fetch_add(1, Ordering::SeqCst);
            self.cleanup_result()
        }

        fn observe_phase(&mut self, phase: WorkerShutdownPhase) {
            self.state.phases.lock().expect("phase lock").push(phase);
        }
    }

    fn assert_once_cleanup(state: &HookState) {
        assert_eq!(state.socket_count.load(Ordering::SeqCst), 1);
        assert_eq!(state.pid_count.load(Ordering::SeqCst), 1);
        assert_eq!(state.lock_count.load(Ordering::SeqCst), 1);
    }

    fn assert_complete_phase_order(state: &HookState, stop: WorkerStop) {
        assert_eq!(
            *state.phases.lock().expect("phase lock"),
            vec![
                WorkerShutdownPhase::Draining(stop),
                WorkerShutdownPhase::AcceptStopped,
                WorkerShutdownPhase::ClientsCancelled,
                WorkerShutdownPhase::ClientsJoined,
                WorkerShutdownPhase::BackendClosed,
                WorkerShutdownPhase::SocketCleaned,
                WorkerShutdownPhase::PidCleaned,
                WorkerShutdownPhase::LockReleased,
                WorkerShutdownPhase::Closed(stop),
            ]
        );
    }

    #[derive(Default)]
    struct TestState {
        calls: Mutex<Vec<String>>,
        list_results: Mutex<VecDeque<Result<Vec<ToolInfo>, ConnectionError>>>,
        call_results: Mutex<VecDeque<Result<ToolResult, ConnectionError>>>,
        block_list: AtomicBool,
        list_cancelled: AtomicBool,
        close_saw_cancelled_client: AtomicBool,
        close_fails: AtomicBool,
        close_count: AtomicUsize,
        list_started: Notify,
        release_list: Notify,
    }

    struct ListCancellationMarker(Arc<TestState>);

    impl Drop for ListCancellationMarker {
        fn drop(&mut self) {
            self.0.list_cancelled.store(true, Ordering::SeqCst);
        }
    }

    struct TestConnection {
        state: Arc<TestState>,
        instructions: Option<String>,
    }

    impl TestConnection {
        fn new(instructions: Option<&str>) -> (Self, Arc<TestState>) {
            let state = Arc::new(TestState::default());
            (
                Self {
                    state: Arc::clone(&state),
                    instructions: instructions.map(str::to_owned),
                },
                state,
            )
        }
    }

    impl McpConnection for TestConnection {
        fn list_tools<'a>(
            &'a self,
            _ctx: &'a CommandContext,
        ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
            self.state
                .calls
                .lock()
                .expect("calls lock")
                .push("listTools".into());
            let blocked = self.state.block_list.load(Ordering::SeqCst);
            let state = Arc::clone(&self.state);
            Box::pin(async move {
                if blocked {
                    let _cancellation_marker = ListCancellationMarker(Arc::clone(&state));
                    state.list_started.notify_one();
                    state.release_list.notified().await;
                }
                state
                    .list_results
                    .lock()
                    .expect("list results lock")
                    .pop_front()
                    .unwrap_or_else(|| Ok(Vec::new()))
            })
        }

        fn call_tool<'a>(
            &'a self,
            _ctx: &'a CommandContext,
            name: &'a str,
            args: JsonObject,
        ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
            self.state
                .calls
                .lock()
                .expect("calls lock")
                .push(format!("callTool:{name}:{args:?}"));
            let result = self
                .state
                .call_results
                .lock()
                .expect("call results lock")
                .pop_front()
                .unwrap_or_else(|| Ok(json!({"tool": name, "args": args})));
            Box::pin(async move { result })
        }

        fn instructions(&self) -> Option<&str> {
            self.instructions.as_deref()
        }

        fn close<'a>(
            self: Box<Self>,
            _ctx: &'a CommandContext,
        ) -> BoxFuture<'a, Result<(), ConnectionError>> {
            self.state.close_count.fetch_add(1, Ordering::SeqCst);
            self.state.close_saw_cancelled_client.store(
                self.state.list_cancelled.load(Ordering::SeqCst),
                Ordering::SeqCst,
            );
            let fails = self.state.close_fails.load(Ordering::SeqCst);
            Box::pin(async move {
                if fails {
                    Err(ConnectionError::new(
                        "backend-secret-detail-must-stay-internal",
                    ))
                } else {
                    Ok(())
                }
            })
        }

        fn mode(&self) -> ConnectionMode {
            ConnectionMode::Daemon
        }
    }

    async fn start_client(
        connection: TestConnection,
    ) -> (UnixStream, tokio::task::JoinHandle<ClientStop>) {
        let (client, server) = UnixStream::pair().expect("Unix stream pair");
        let task = tokio::spawn(serve_client(
            server,
            Arc::new(SharedConnection::new(Box::new(connection))),
            context(),
            None,
            None,
        ));
        (client, task)
    }

    async fn read_response(reader: &mut BufReader<UnixStream>) -> IpcResponse {
        let mut line = String::new();
        timeout(Duration::from_secs(2), reader.read_line(&mut line))
            .await
            .expect("response timeout")
            .expect("response read");
        serde_json::from_str(line.trim_end()).expect("valid IPC response")
    }

    fn assert_failure(response: IpcResponse, id: &str, code: IpcErrorCode) {
        assert_eq!(response.id(), id);
        match response.outcome() {
            crate::daemon::IpcOutcome::Failure(error) => assert_eq!(error.code(), code),
            outcome => panic!("expected failure, got {outcome:?}"),
        }
    }

    #[tokio::test]
    async fn serves_all_operations_in_client_order_and_preserves_ids() {
        let (connection, state) = TestConnection::new(Some("use safely"));
        state
            .list_results
            .lock()
            .expect("list results lock")
            .push_back(Ok(vec![ToolInfo {
                name: "echo".into(),
                description: Some("Echo input".into()),
                input_schema: json!({"type":"object"}),
            }]));
        state
            .call_results
            .lock()
            .expect("call results lock")
            .push_back(Ok(json!({"content":[{"type":"text","text":"ok"}]})));
        let (mut client, task) = start_client(connection).await;

        client
            .write_all(concat!(
                "{\"id\":\"p\",\"type\":\"ping\"}\n",
                "{\"id\":\"l\",\"type\":\"listTools\"}\n",
                "{\"id\":\"c\",\"type\":\"callTool\",\"toolName\":\"echo\",\"args\":{\"x\":1}}\n",
                "{\"id\":\"i\",\"type\":\"getInstructions\"}\n",
                "{\"id\":\"z\",\"type\":\"close\"}\n",
            ).as_bytes())
            .await
            .expect("write requests");
        let mut reader = BufReader::new(client);

        let responses = [
            read_response(&mut reader).await,
            read_response(&mut reader).await,
            read_response(&mut reader).await,
            read_response(&mut reader).await,
            read_response(&mut reader).await,
        ];
        assert_eq!(
            responses.each_ref().map(|response| response.id()),
            ["p", "l", "c", "i", "z"]
        );
        assert_eq!(
            responses[0].outcome(),
            &crate::daemon::IpcOutcome::Success(json!("pong"))
        );
        assert_eq!(
            responses[2].outcome(),
            &crate::daemon::IpcOutcome::Success(json!({"content":[{"type":"text","text":"ok"}]}))
        );
        assert_eq!(
            responses[3].outcome(),
            &crate::daemon::IpcOutcome::Success(json!("use safely"))
        );
        assert_eq!(
            responses[4].outcome(),
            &crate::daemon::IpcOutcome::Success(json!("closing"))
        );
        assert_eq!(task.await.expect("client task"), ClientStop::CloseRequested);
        assert_eq!(state.calls.lock().expect("calls lock").len(), 2);
    }

    #[tokio::test]
    async fn request_errors_are_stable_secret_free_and_same_client_recovers() {
        const SECRET: &str = "Bearer-do-not-reflect";
        let (connection, _) = TestConnection::new(None);
        let (mut client, task) = start_client(connection).await;
        let wire = format!(
            "{{bad:{SECRET}}}\n{{\"type\":\"ping\"}}\n{{\"id\":\"u\",\"type\":\"mystery\",\"payload\":\"{SECRET}\"}}\n{{\"id\":\"a\",\"type\":\"callTool\",\"toolName\":\"echo\",\"args\":[],\"payload\":\"{SECRET}\"}}\n{{\"id\":\"ok\",\"type\":\"ping\"}}\n"
        );
        client
            .write_all(wire.as_bytes())
            .await
            .expect("write errors");
        let mut reader = BufReader::new(client);

        let invalid_json = read_response(&mut reader).await;
        let missing_id = read_response(&mut reader).await;
        let unknown_type = read_response(&mut reader).await;
        let invalid_args = read_response(&mut reader).await;
        let ping = read_response(&mut reader).await;
        assert_failure(invalid_json, "", IpcErrorCode::InvalidJson);
        assert_failure(missing_id, "", IpcErrorCode::MissingId);
        assert_failure(unknown_type, "u", IpcErrorCode::UnknownType);
        assert_failure(invalid_args, "a", IpcErrorCode::InvalidArguments);
        assert_eq!(ping.id(), "ok");
        assert_eq!(
            ping.outcome(),
            &crate::daemon::IpcOutcome::Success(json!("pong"))
        );

        let visible = serde_json::to_string(&[
            IpcErrorCode::InvalidJson.message(),
            IpcErrorCode::MissingId.message(),
            IpcErrorCode::UnknownType.message(),
            IpcErrorCode::InvalidArguments.message(),
        ])
        .expect("serialize canonical messages");
        assert!(!visible.contains(SECRET));
        drop(reader);
        assert_eq!(task.await.expect("client task"), ClientStop::Disconnected);
    }

    #[tokio::test]
    async fn connection_failures_return_canonical_error_without_payload() {
        const SECRET: &str = "backend-secret";
        let (connection, state) = TestConnection::new(None);
        state
            .call_results
            .lock()
            .expect("call results lock")
            .push_back(Err(ConnectionError::new(SECRET)));
        let (mut client, task) = start_client(connection).await;
        client
            .write_all(
                b"{\"id\":\"call\",\"type\":\"callTool\",\"toolName\":\"echo\",\"args\":{}}\n",
            )
            .await
            .expect("write call");
        let mut reader = BufReader::new(client);
        let response = read_response(&mut reader).await;
        assert_failure(response.clone(), "call", IpcErrorCode::ExecutionError);
        let wire = serde_json::to_string(&response).expect("serialize response");
        assert!(!wire.contains(SECRET));
        drop(reader);
        assert_eq!(task.await.expect("client task"), ClientStop::Disconnected);
    }

    #[tokio::test]
    async fn oversized_response_returns_small_error_and_closes_only_that_client() {
        let (connection, state) = TestConnection::new(None);
        state
            .call_results
            .lock()
            .expect("call results lock")
            .push_back(Ok(
                json!({"padding":"x".repeat(crate::daemon::IPC_MAX_FRAME_SIZE)}),
            ));
        let shared = Arc::new(SharedConnection::new(Box::new(connection)));
        let (mut large_client, large_server) = UnixStream::pair().expect("large pair");
        let (mut good_client, good_server) = UnixStream::pair().expect("good pair");
        let large_task = tokio::spawn(serve_client(
            large_server,
            Arc::clone(&shared),
            context(),
            None,
            None,
        ));
        let good_task = tokio::spawn(serve_client(good_server, shared, context(), None, None));

        large_client
            .write_all(
                b"{\"id\":\"large\",\"type\":\"callTool\",\"toolName\":\"big\",\"args\":{}}\n",
            )
            .await
            .expect("large request");
        let mut large_reader = BufReader::new(large_client);
        assert_failure(
            read_response(&mut large_reader).await,
            "large",
            IpcErrorCode::FrameTooLarge,
        );
        assert_eq!(
            large_task.await.expect("large task"),
            ClientStop::Disconnected
        );

        good_client
            .write_all(b"{\"id\":\"good\",\"type\":\"ping\"}\n")
            .await
            .expect("good ping");
        let mut good_reader = BufReader::new(good_client);
        assert_eq!(read_response(&mut good_reader).await.id(), "good");
        drop(good_reader);
        assert_eq!(
            good_task.await.expect("good task"),
            ClientStop::Disconnected
        );
    }

    #[tokio::test]
    async fn oversized_request_closes_only_that_client() {
        let (connection, _) = TestConnection::new(None);
        let shared = Arc::new(SharedConnection::new(Box::new(connection)));
        let (mut large_client, large_server) = UnixStream::pair().expect("large pair");
        let (mut good_client, good_server) = UnixStream::pair().expect("good pair");
        let large_task = tokio::spawn(serve_client(
            large_server,
            Arc::clone(&shared),
            context(),
            None,
            None,
        ));
        let good_task = tokio::spawn(serve_client(good_server, shared, context(), None, None));

        large_client
            .write_all(&vec![b'x'; crate::daemon::IPC_MAX_FRAME_SIZE + 1])
            .await
            .expect("oversized request");
        assert_eq!(
            large_task.await.expect("large task"),
            ClientStop::Disconnected
        );

        good_client
            .write_all(b"{\"id\":\"good\",\"type\":\"ping\"}\n")
            .await
            .expect("good ping");
        let mut reader = BufReader::new(good_client);
        assert_eq!(read_response(&mut reader).await.id(), "good");
        drop(reader);
        assert_eq!(
            good_task.await.expect("good task"),
            ClientStop::Disconnected
        );
    }

    #[tokio::test]
    async fn listener_handles_other_clients_while_one_client_is_blocked() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let socket = directory.path().join("worker.sock");
        let listener = UnixListener::bind(&socket).expect("bind loopback socket");
        let (connection, state) = TestConnection::new(None);
        state.block_list.store(true, Ordering::SeqCst);
        let service =
            WorkerIpcService::new(Box::new(connection), context(), Duration::from_secs(30));
        let worker = tokio::spawn(service.serve(listener));

        let mut first = UnixStream::connect(Path::new(&socket))
            .await
            .expect("first client");
        first
            .write_all(
                b"{\"id\":\"slow\",\"type\":\"listTools\"}\n{\"id\":\"after\",\"type\":\"ping\"}\n",
            )
            .await
            .expect("slow requests");
        timeout(Duration::from_secs(2), state.list_started.notified())
            .await
            .expect("list started");

        let mut second = UnixStream::connect(&socket).await.expect("second client");
        second
            .write_all(b"{\"id\":\"fast\",\"type\":\"ping\"}\n")
            .await
            .expect("fast ping");
        let mut second_reader = BufReader::new(second);
        assert_eq!(read_response(&mut second_reader).await.id(), "fast");

        let mut first_reader = BufReader::new(first);
        let mut no_response = String::new();
        assert!(
            timeout(
                Duration::from_millis(50),
                first_reader.read_line(&mut no_response)
            )
            .await
            .is_err()
        );
        state.release_list.notify_waiters();
        assert_eq!(read_response(&mut first_reader).await.id(), "slow");
        assert_eq!(read_response(&mut first_reader).await.id(), "after");

        second_reader
            .get_mut()
            .write_all(b"{\"id\":\"stop\",\"type\":\"close\"}\n")
            .await
            .expect("close request");
        assert_eq!(read_response(&mut second_reader).await.id(), "stop");
        assert_eq!(
            worker.await.expect("worker task").expect("worker result"),
            WorkerStop::CloseRequested
        );
    }

    async fn start_idle_service(
        connection: TestConnection,
        clock: TestClock,
        idle_timeout: Duration,
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        tokio::task::JoinHandle<Result<WorkerStop, WorkerError>>,
    ) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let socket = directory.path().join("idle-worker.sock");
        let listener = UnixListener::bind(&socket).expect("bind idle loopback socket");
        let context = context_at(clock.now());
        let service = WorkerIpcService::with_clock(
            Box::new(connection),
            context,
            idle_timeout,
            Arc::new(clock),
        );
        let worker = tokio::spawn(service.serve(listener));
        tokio::task::yield_now().await;
        (directory, socket, worker)
    }

    async fn expect_idle_stop(
        worker: tokio::task::JoinHandle<Result<WorkerStop, WorkerError>>,
    ) -> WorkerStop {
        timeout(Duration::from_secs(2), worker)
            .await
            .expect("idle worker did not stop after fake-clock expiry")
            .expect("idle worker task")
            .expect("idle worker result")
    }

    #[tokio::test]
    async fn connection_and_invalid_or_partial_frames_do_not_extend_idle_deadline() {
        let start = Instant::now();
        let clock = TestClock::new(start);
        let (connection, state) = TestConnection::new(None);
        let (_directory, socket, worker) =
            start_idle_service(connection, clock.clone(), Duration::from_secs(10)).await;
        let client = UnixStream::connect(&socket).await.expect("idle client");
        let mut reader = BufReader::new(client);

        clock.advance(Duration::from_secs(4));
        reader
            .get_mut()
            .write_all(
                concat!(
                    "{bad}\n",
                    "{\"type\":\"ping\"}\n",
                    "{\"id\":\"unknown\",\"type\":\"wat\"}\n",
                    "{\"id\":\"args\",\"type\":\"callTool\",\"toolName\":\"x\",\"args\":[]}\n",
                )
                .as_bytes(),
            )
            .await
            .expect("invalid requests");
        for _ in 0..4 {
            let response = read_response(&mut reader).await;
            assert!(matches!(
                response.outcome(),
                crate::daemon::IpcOutcome::Failure(_)
            ));
        }
        reader
            .get_mut()
            .write_all(b"framing-noise-without-newline")
            .await
            .expect("partial framing noise");

        clock.advance(Duration::from_secs(6));
        assert_eq!(expect_idle_stop(worker).await, WorkerStop::IdleTimeout);
        assert_eq!(state.close_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn completed_valid_requests_from_multiple_clients_extend_from_completion_time() {
        let start = Instant::now();
        let clock = TestClock::new(start);
        let (connection, state) = TestConnection::new(None);
        let (_directory, socket, worker) =
            start_idle_service(connection, clock.clone(), Duration::from_secs(10)).await;
        let first = UnixStream::connect(&socket)
            .await
            .expect("first idle client");
        let second = UnixStream::connect(&socket)
            .await
            .expect("second idle client");
        let mut first = BufReader::new(first);
        let mut second = BufReader::new(second);

        clock.advance(Duration::from_secs(2));
        first
            .get_mut()
            .write_all(b"{\"id\":\"first\",\"type\":\"ping\"}\n")
            .await
            .expect("first ping");
        assert_eq!(read_response(&mut first).await.id(), "first");

        clock.advance(Duration::from_secs(5));
        second
            .get_mut()
            .write_all(b"{\"id\":\"second\",\"type\":\"ping\"}\n")
            .await
            .expect("second ping");
        assert_eq!(read_response(&mut second).await.id(), "second");

        clock.advance(Duration::from_secs(5));
        tokio::task::yield_now().await;
        assert!(
            !worker.is_finished(),
            "older deadline must have been replaced"
        );
        clock.advance(Duration::from_secs(5));
        assert_eq!(expect_idle_stop(worker).await, WorkerStop::IdleTimeout);
        assert_eq!(state.close_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_valid_request_extends_idle_even_when_the_backend_operation_fails() {
        let start = Instant::now();
        let clock = TestClock::new(start);
        let (connection, state) = TestConnection::new(None);
        state
            .call_results
            .lock()
            .expect("call results lock")
            .push_back(Err(ConnectionError::new("fixture failure")));
        let (_directory, socket, worker) =
            start_idle_service(connection, clock.clone(), Duration::from_secs(10)).await;
        let client = UnixStream::connect(&socket).await.expect("failure client");
        let mut reader = BufReader::new(client);

        clock.advance(Duration::from_secs(6));
        reader
            .get_mut()
            .write_all(
                b"{\"id\":\"failed\",\"type\":\"callTool\",\"toolName\":\"x\",\"args\":{}}\n",
            )
            .await
            .expect("valid failing call");
        assert_failure(
            read_response(&mut reader).await,
            "failed",
            IpcErrorCode::ExecutionError,
        );

        clock.advance(Duration::from_secs(4));
        tokio::task::yield_now().await;
        assert!(
            !worker.is_finished(),
            "completed valid failure must extend idle"
        );
        clock.advance(Duration::from_secs(6));
        assert_eq!(expect_idle_stop(worker).await, WorkerStop::IdleTimeout);
        assert_eq!(state.close_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn request_crossing_or_completing_at_old_deadline_cannot_revive_worker() {
        let start = Instant::now();
        let clock = TestClock::new(start);
        let (connection, state) = TestConnection::new(None);
        state.block_list.store(true, Ordering::SeqCst);
        let (_directory, socket, worker) =
            start_idle_service(connection, clock.clone(), Duration::from_secs(10)).await;
        let mut client = UnixStream::connect(&socket).await.expect("crossing client");

        clock.advance(Duration::from_secs(9));
        client
            .write_all(b"{\"id\":\"crossing\",\"type\":\"listTools\"}\n")
            .await
            .expect("crossing request");
        state.list_started.notified().await;

        clock.advance(Duration::from_secs(1));
        state.release_list.notify_waiters();
        assert_eq!(expect_idle_stop(worker).await, WorkerStop::IdleTimeout);
        assert_eq!(state.close_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn injected_sigint_and_sigterm_each_reach_one_ordered_closed_state() {
        for (signal, expected) in [
            (WorkerSignal::Interrupt, WorkerStop::SignalInterrupt),
            (WorkerSignal::Terminate, WorkerStop::SignalTerminate),
        ] {
            let directory = tempfile::tempdir().expect("signal tempdir");
            let socket = directory.path().join(format!("signal-{signal:?}.sock"));
            let listener = UnixListener::bind(&socket).expect("signal listener");
            let (connection, connection_state) = TestConnection::new(None);
            let (hooks, sender, hook_state) = ChannelHooks::new(false);
            let service =
                WorkerIpcService::new(Box::new(connection), context(), Duration::from_secs(30));
            let worker = tokio::spawn(service.serve_with_shutdown_hooks(listener, hooks));
            tokio::task::yield_now().await;

            sender.send(signal).expect("inject signal");
            let stopped = timeout(Duration::from_secs(2), worker)
                .await
                .expect("signal worker timeout")
                .expect("signal worker task")
                .expect("signal worker result");

            assert_eq!(stopped, expected);
            assert_eq!(connection_state.close_count.load(Ordering::SeqCst), 1);
            assert_once_cleanup(&hook_state);
            assert_complete_phase_order(&hook_state, expected);
            assert!(UnixStream::connect(&socket).await.is_err());
        }
    }

    #[tokio::test]
    async fn close_idle_and_competing_triggers_share_the_same_shutdown_once_path() {
        // Explicit close.
        let close_directory = tempfile::tempdir().expect("close tempdir");
        let close_socket = close_directory.path().join("close.sock");
        let close_listener = UnixListener::bind(&close_socket).expect("close listener");
        let (close_connection, close_connection_state) = TestConnection::new(None);
        let (close_hooks, _close_signal, close_hook_state) = ChannelHooks::new(false);
        let close_service = WorkerIpcService::new(
            Box::new(close_connection),
            context(),
            Duration::from_secs(30),
        );
        let close_worker =
            tokio::spawn(close_service.serve_with_shutdown_hooks(close_listener, close_hooks));
        let mut close_client = BufReader::new(
            UnixStream::connect(&close_socket)
                .await
                .expect("close client"),
        );
        close_client
            .get_mut()
            .write_all(b"{\"id\":\"close\",\"type\":\"close\"}\n")
            .await
            .expect("close request");
        assert_eq!(read_response(&mut close_client).await.id(), "close");
        let close_stop = close_worker
            .await
            .expect("close worker task")
            .expect("close worker result");
        assert_eq!(close_stop, WorkerStop::CloseRequested);
        assert_eq!(close_connection_state.close_count.load(Ordering::SeqCst), 1);
        assert_once_cleanup(&close_hook_state);
        assert_complete_phase_order(&close_hook_state, close_stop);

        // Idle expiry under an injected clock.
        let start = Instant::now();
        let idle_clock = TestClock::new(start);
        let idle_directory = tempfile::tempdir().expect("idle hook tempdir");
        let idle_socket = idle_directory.path().join("idle-hook.sock");
        let idle_listener = UnixListener::bind(&idle_socket).expect("idle hook listener");
        let (idle_connection, idle_connection_state) = TestConnection::new(None);
        let (idle_hooks, _idle_signal, idle_hook_state) = ChannelHooks::new(false);
        let idle_service = WorkerIpcService::with_clock(
            Box::new(idle_connection),
            context_at(start),
            Duration::from_secs(10),
            Arc::new(idle_clock.clone()),
        );
        let idle_worker =
            tokio::spawn(idle_service.serve_with_shutdown_hooks(idle_listener, idle_hooks));
        tokio::task::yield_now().await;
        idle_clock.advance(Duration::from_secs(10));
        let idle_stop = idle_worker
            .await
            .expect("idle hook worker task")
            .expect("idle hook worker result");
        assert_eq!(idle_stop, WorkerStop::IdleTimeout);
        assert_eq!(idle_connection_state.close_count.load(Ordering::SeqCst), 1);
        assert_once_cleanup(&idle_hook_state);
        assert_complete_phase_order(&idle_hook_state, idle_stop);

        // A queued close and multiple injected signals may race, but exactly
        // one final reason and one set of side effects is selected.
        let race_directory = tempfile::tempdir().expect("race tempdir");
        let race_socket = race_directory.path().join("race.sock");
        let race_listener = UnixListener::bind(&race_socket).expect("race listener");
        let (race_connection, race_connection_state) = TestConnection::new(None);
        let (race_hooks, race_signal, race_hook_state) = ChannelHooks::new(false);
        let race_service = WorkerIpcService::new(
            Box::new(race_connection),
            context(),
            Duration::from_secs(30),
        );
        let race_worker =
            tokio::spawn(race_service.serve_with_shutdown_hooks(race_listener, race_hooks));
        let mut race_client = UnixStream::connect(&race_socket)
            .await
            .expect("race client");
        race_client
            .write_all(b"{\"id\":\"race\",\"type\":\"close\"}\n")
            .await
            .expect("race close");
        race_signal
            .send(WorkerSignal::Interrupt)
            .expect("race interrupt");
        race_signal
            .send(WorkerSignal::Terminate)
            .expect("race terminate");
        let race_stop = race_worker
            .await
            .expect("race worker task")
            .expect("race worker result");
        assert!(matches!(
            race_stop,
            WorkerStop::CloseRequested | WorkerStop::SignalInterrupt | WorkerStop::SignalTerminate
        ));
        assert_eq!(race_connection_state.close_count.load(Ordering::SeqCst), 1);
        assert_once_cleanup(&race_hook_state);
        assert_complete_phase_order(&race_hook_state, race_stop);
    }

    #[tokio::test]
    async fn client_disconnect_cancels_inflight_backend_before_server_stream_closes() {
        let (connection, state) = TestConnection::new(None);
        state.block_list.store(true, Ordering::SeqCst);
        let (mut client, task) = start_client(connection).await;
        client
            .write_all(b"{\"id\":\"blocked\",\"type\":\"listTools\"}\n")
            .await
            .expect("blocked request");
        timeout(Duration::from_secs(2), state.list_started.notified())
            .await
            .expect("backend request started");

        client.shutdown().await.expect("half-close client");
        assert_eq!(
            timeout(Duration::from_secs(2), task)
                .await
                .expect("worker did not acknowledge disconnect")
                .expect("client task"),
            ClientStop::Disconnected
        );
        assert!(state.list_cancelled.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn shutdown_cancels_and_joins_inflight_clients_before_backend_close() {
        let directory = tempfile::tempdir().expect("cancel tempdir");
        let socket = directory.path().join("cancel.sock");
        let listener = UnixListener::bind(&socket).expect("cancel listener");
        let (connection, state) = TestConnection::new(None);
        state.block_list.store(true, Ordering::SeqCst);
        let (hooks, sender, hook_state) = ChannelHooks::new(false);
        let service =
            WorkerIpcService::new(Box::new(connection), context(), Duration::from_secs(30));
        let worker = tokio::spawn(service.serve_with_shutdown_hooks(listener, hooks));
        let mut client = UnixStream::connect(&socket).await.expect("cancel client");
        client
            .write_all(b"{\"id\":\"blocked\",\"type\":\"listTools\"}\n")
            .await
            .expect("blocked request");
        timeout(Duration::from_secs(2), state.list_started.notified())
            .await
            .expect("blocked request start");

        sender
            .send(WorkerSignal::Terminate)
            .expect("inject terminate");
        assert_eq!(
            worker
                .await
                .expect("cancel worker task")
                .expect("cancel worker result"),
            WorkerStop::SignalTerminate
        );
        assert!(state.list_cancelled.load(Ordering::SeqCst));
        assert!(state.close_saw_cancelled_client.load(Ordering::SeqCst));
        assert_eq!(state.close_count.load(Ordering::SeqCst), 1);
        assert_once_cleanup(&hook_state);
    }

    #[tokio::test]
    async fn cleanup_failures_are_aggregated_without_skipping_or_repeating_steps() {
        let directory = tempfile::tempdir().expect("failure tempdir");
        let socket = directory.path().join("failure.sock");
        let listener = UnixListener::bind(&socket).expect("failure listener");
        let (connection, connection_state) = TestConnection::new(None);
        connection_state.close_fails.store(true, Ordering::SeqCst);
        let (hooks, sender, hook_state) = ChannelHooks::new(true);
        let service =
            WorkerIpcService::new(Box::new(connection), context(), Duration::from_secs(30));
        let worker = tokio::spawn(service.serve_with_shutdown_hooks(listener, hooks));
        sender
            .send(WorkerSignal::Interrupt)
            .expect("inject failing shutdown");

        let error = worker
            .await
            .expect("failure worker task")
            .expect_err("shutdown failures must be reported");
        let WorkerError::Shutdown(shutdown) = &error else {
            panic!("expected aggregated shutdown error, got {error:?}");
        };
        assert_eq!(shutdown.stop(), WorkerStop::SignalInterrupt);
        assert_eq!(
            shutdown
                .failures()
                .iter()
                .map(WorkerShutdownFailure::step)
                .collect::<Vec<_>>(),
            vec![
                WorkerShutdownStep::BackendClose,
                WorkerShutdownStep::SocketCleanup,
                WorkerShutdownStep::PidCleanup,
                WorkerShutdownStep::LockRelease,
            ]
        );
        assert_eq!(connection_state.close_count.load(Ordering::SeqCst), 1);
        assert_once_cleanup(&hook_state);
        assert_complete_phase_order(&hook_state, WorkerStop::SignalInterrupt);
        let visible = format!("{error}");
        assert!(!visible.contains("backend-secret"));
        assert!(!visible.contains("injected cleanup detail"));
    }

    #[tokio::test]
    async fn unix_hooks_never_remove_replaced_or_symlinked_artifacts() {
        let temporary_root = tempfile::tempdir().expect("artifact tempdir");
        let paths = DaemonPaths::from_runtime_parent(
            temporary_root.path(),
            &server_id("shutdown-artifacts"),
        )
        .expect("daemon paths");
        let socket_listener =
            std::os::unix::net::UnixListener::bind(&paths.socket).expect("artifact socket");
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))
            .expect("artifact socket permissions");
        for path in [&paths.pid, &paths.lock] {
            fs::write(path, b"owned").expect("artifact file");
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                .expect("artifact permissions");
        }
        let mut hooks = UnixWorkerShutdownHooks::new(paths.clone()).expect("Unix hooks");

        // Replace the PID with another inode and the socket with a symlink
        // after ownership capture. Neither path may be unlinked.
        let replacement_pid = paths.runtime_dir.join("replacement.pid.tmp");
        fs::write(&replacement_pid, b"replacement").expect("replacement pid");
        fs::set_permissions(&replacement_pid, fs::Permissions::from_mode(0o600))
            .expect("replacement pid permissions");
        fs::rename(&replacement_pid, &paths.pid).expect("replace pid identity");
        fs::remove_file(&paths.socket).expect("remove owned socket name");
        let outside = temporary_root.path().join("outside");
        fs::write(&outside, b"outside").expect("outside file");
        symlink(&outside, &paths.socket).expect("replacement socket symlink");

        assert!(hooks.remove_socket().is_err());
        assert!(hooks.remove_pid().is_err());
        hooks.release_lock().expect("owned lock cleanup");
        // Identity tokens are consumed even on failure, so retries are no-ops
        // rather than repeated side effects against a changed path.
        hooks.remove_socket().expect("socket cleanup is once-only");
        hooks.remove_pid().expect("pid cleanup is once-only");

        assert!(
            fs::symlink_metadata(&paths.socket)
                .expect("replacement socket remains")
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&outside).expect("outside remains"), b"outside");
        assert_eq!(
            fs::read(&paths.pid).expect("replacement pid remains"),
            b"replacement"
        );
        assert!(!paths.lock.exists());
        drop(socket_listener);
    }
}

#[cfg(test)]
mod bootstrap_tests {
    use std::{collections::BTreeMap, io::Cursor, time::Duration};

    use tempfile::TempDir;

    use super::*;
    use crate::config::{config_hash, server_id};

    fn bootstrap_server(command: &str) -> ServerDefinition {
        let raw = json!({
            "command": command,
            "env": {"TOKEN": "bootstrap-secret"}
        });
        ServerDefinition {
            name: "bootstrap-fixture".to_owned(),
            id: server_id("bootstrap-fixture"),
            config_hash: config_hash(&raw),
            transport: TransportConfig::Stdio {
                command: command.to_owned(),
                args: Vec::new(),
                env: BTreeMap::from([("TOKEN".to_owned(), "bootstrap-secret".to_owned())]),
                cwd: None,
            },
            filter: ToolFilterConfig::default(),
        }
    }

    #[test]
    fn bootstrap_envelope_round_trips_only_through_serialized_input() {
        let root = TempDir::new().expect("runtime root");
        let server = bootstrap_server("/definitely/not/executed");
        let paths = DaemonPaths::from_runtime_parent(root.path(), &server.id).expect("paths");
        let parent_environment =
            BTreeMap::from([("PARENT_SECRET".to_owned(), "parent-secret-value".to_owned())]);
        let envelope = WorkerBootstrapEnvelope::new(
            &server,
            &paths,
            &parent_environment,
            Duration::from_secs(30),
            Duration::from_secs(5),
        )
        .expect("envelope");
        let encoded = serde_json::to_vec(&envelope).expect("serialize");
        let decoded: WorkerBootstrapEnvelope =
            serde_json::from_slice(&encoded).expect("deserialize");
        let validated = decoded.validate().expect("validate");

        assert_eq!(validated.server, server);
        assert_eq!(validated.parent_environment, parent_environment);
        assert_eq!(validated.paths.socket, paths.socket);
        assert_eq!(validated.idle_timeout, Duration::from_secs(30));
        assert_eq!(validated.startup_timeout, Duration::from_secs(5));
        assert!(
            String::from_utf8(encoded)
                .unwrap()
                .contains("bootstrap-secret")
        );
        assert!(!format!("{}", WorkerBootstrapError::InvalidInput).contains("secret"));
    }

    #[tokio::test]
    async fn initialization_failure_sends_no_ready_and_removes_owned_artifacts() {
        let root = TempDir::new().expect("runtime root");
        let server = bootstrap_server("/definitely/missing/mcp-server");
        let paths = DaemonPaths::from_runtime_parent(root.path(), &server.id).expect("paths");
        let envelope = WorkerBootstrapEnvelope::new(
            &server,
            &paths,
            &BTreeMap::new(),
            Duration::from_secs(30),
            Duration::from_secs(1),
        )
        .expect("envelope");
        let mut input = Cursor::new(serde_json::to_vec(&envelope).expect("serialize"));
        let mut ready = Vec::new();

        let error = run_worker(&mut input, &mut ready)
            .await
            .expect_err("backend must fail");

        assert_eq!(error, WorkerBootstrapError::BackendInitialization);
        assert!(ready.is_empty());
        assert!(!paths.socket.exists());
        assert!(!paths.pid.exists());
        assert!(!paths.lock.exists());
    }
}
