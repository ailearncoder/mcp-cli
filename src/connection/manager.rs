//! Platform-aware connection selection and daemon-to-direct fallback.

use std::sync::Arc;

#[cfg(unix)]
use tokio::sync::Mutex;

use crate::{
    config::ServerDefinition,
    error::CliError,
    runtime::{BoxFuture, CommandContext, RuntimeConfig},
};
#[cfg(unix)]
use crate::{
    domain::{ConnectionMode, JsonObject, ToolInfo, ToolResult},
    error::ErrorKind,
    policy::retry::ErrorClass,
};

#[cfg(unix)]
use super::ConnectionError;
use super::{
    ConnectionManager, ConnectionResourceRegistry, DirectConnectionManager, DirectConnector,
    McpConnection,
};

/// The production connection selector shared by list, info, grep, and call.
///
/// Commands see only [`ConnectionManager`]. On Unix this manager prefers the
/// private per-server daemon when enabled, while every direct-only platform or
/// configuration uses the exact same direct connector without touching daemon
/// paths.
pub struct ManagedConnectionManager {
    direct: Arc<DirectConnectionManager>,
    #[cfg(unix)]
    daemon: Option<Arc<dyn DaemonBackend>>,
    #[cfg(unix)]
    resources: ConnectionResourceRegistry,
}

impl ManagedConnectionManager {
    pub fn new(
        connector: Arc<dyn DirectConnector>,
        resources: ConnectionResourceRegistry,
        runtime: &RuntimeConfig,
    ) -> Self {
        let direct = Arc::new(DirectConnectionManager::batch(
            connector,
            resources.clone(),
            runtime,
        ));
        Self {
            direct,
            #[cfg(unix)]
            daemon: runtime.daemon_enabled.then(|| {
                Arc::new(UnixDaemonBackend::new(runtime.daemon_idle_timeout))
                    as Arc<dyn DaemonBackend>
            }),
            #[cfg(unix)]
            resources,
        }
    }

    #[cfg(all(unix, test))]
    fn with_daemon_backend(
        connector: Arc<dyn DirectConnector>,
        resources: ConnectionResourceRegistry,
        runtime: &RuntimeConfig,
        daemon: Option<Arc<dyn DaemonBackend>>,
    ) -> Self {
        Self {
            direct: Arc::new(DirectConnectionManager::batch(
                connector,
                resources.clone(),
                runtime,
            )),
            daemon,
            resources,
        }
    }

    async fn direct(
        &self,
        context: &CommandContext,
        server: &ServerDefinition,
        reason: Option<&'static str>,
    ) -> Result<Box<dyn McpConnection>, CliError> {
        if let Some(reason) = reason {
            context
                .diagnostics
                .debug(&format!("selected direct mode after daemon {reason}"));
        } else {
            context.diagnostics.debug("selected direct mode");
        }
        self.direct.acquire(context, server).await
    }
}

impl ConnectionManager for ManagedConnectionManager {
    fn acquire<'a>(
        &'a self,
        context: &'a CommandContext,
        server: &'a ServerDefinition,
    ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, CliError>> {
        Box::pin(async move {
            #[cfg(unix)]
            if let Some(daemon) = &self.daemon {
                match daemon.acquire(context, server).await {
                    Ok(connection) => {
                        context.diagnostics.debug("selected daemon mode");
                        let connection = self
                            .resources
                            .register_connection_without_retry(context, connection);
                        return Ok(Box::new(FallbackConnection::new(
                            connection,
                            Arc::clone(&self.direct),
                            server.clone(),
                        )) as Box<dyn McpConnection>);
                    }
                    Err(DaemonFailure::Operational(reason)) => {
                        return self.direct(context, server, Some(reason)).await;
                    }
                    Err(DaemonFailure::Cancelled) => {
                        return Err(CliError::cancelled(
                            &server.name,
                            "acquiring daemon connection",
                        ));
                    }
                    Err(DaemonFailure::Security(reason)) => {
                        return Err(CliError::security_error("Unsafe daemon state", reason));
                    }
                }
            }

            self.direct(context, server, None).await
        })
    }
}

/// A daemon connection that switches to direct only after the daemon request
/// future has returned. `DaemonClient` closes and drains its stream before an
/// operational error is returned, so reaching `fallback` is the cancellation
/// acknowledgement required to prevent a tool call from being sent twice.
#[cfg(unix)]
struct FallbackConnection {
    state: Mutex<FallbackState>,
    direct: Arc<DirectConnectionManager>,
    server: ServerDefinition,
    instructions: Option<String>,
}

#[cfg(unix)]
struct FallbackState {
    connection: Option<Box<dyn McpConnection>>,
    direct_selected: bool,
}

#[cfg(unix)]
impl FallbackConnection {
    fn new(
        connection: Box<dyn McpConnection>,
        direct: Arc<DirectConnectionManager>,
        server: ServerDefinition,
    ) -> Self {
        Self {
            instructions: connection.instructions().map(str::to_owned),
            state: Mutex::new(FallbackState {
                connection: Some(connection),
                direct_selected: false,
            }),
            direct,
            server,
        }
    }

    async fn fallback(
        &self,
        context: &CommandContext,
        state: &mut FallbackState,
        reason: &'static str,
    ) -> Result<(), ConnectionError> {
        let daemon = state
            .connection
            .take()
            .expect("daemon operation owns a connection");
        // DaemonClient already performed stream shutdown + acknowledgement on
        // request failure. This close is idempotent and releases registry
        // ownership before a direct connection is acquired.
        let _ = daemon.close(context).await;
        context
            .diagnostics
            .debug(&format!("daemon {reason}; selected direct fallback"));
        let direct = self
            .direct
            .acquire(context, &self.server)
            .await
            .map_err(cli_to_connection_error)?;
        state.connection = Some(direct);
        state.direct_selected = true;
        Ok(())
    }
}

#[cfg(unix)]
impl McpConnection for FallbackConnection {
    fn list_tools<'a>(
        &'a self,
        context: &'a CommandContext,
    ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            let result = state
                .connection
                .as_deref()
                .ok_or_else(|| ConnectionError::new("connection is closed"))?
                .list_tools(context)
                .await;
            match result {
                Ok(tools) => Ok(tools),
                Err(error) if !state.direct_selected && daemon_error_allows_fallback(&error) => {
                    let reason = if error.is_timeout() {
                        "request timed out"
                    } else {
                        "request failed operationally"
                    };
                    self.fallback(context, &mut state, reason).await?;
                    state
                        .connection
                        .as_deref()
                        .expect("fallback installed direct connection")
                        .list_tools(context)
                        .await
                }
                Err(error) => Err(error),
            }
        })
    }

    fn call_tool<'a>(
        &'a self,
        context: &'a CommandContext,
        name: &'a str,
        args: JsonObject,
    ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            let result = state
                .connection
                .as_deref()
                .ok_or_else(|| ConnectionError::new("connection is closed"))?
                .call_tool(context, name, args.clone())
                .await;
            match result {
                Ok(result) => Ok(result),
                Err(error) if !state.direct_selected && daemon_error_allows_fallback(&error) => {
                    let reason = if error.is_timeout() {
                        "request timed out"
                    } else {
                        "request failed operationally"
                    };
                    self.fallback(context, &mut state, reason).await?;
                    state
                        .connection
                        .as_deref()
                        .expect("fallback installed direct connection")
                        .call_tool(context, name, args)
                        .await
                }
                Err(error) => Err(error),
            }
        })
    }

    fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    fn close<'a>(
        self: Box<Self>,
        context: &'a CommandContext,
    ) -> BoxFuture<'a, Result<(), ConnectionError>> {
        Box::pin(async move {
            match self.state.into_inner().connection {
                Some(connection) => connection.close(context).await,
                None => Ok(()),
            }
        })
    }

    fn mode(&self) -> ConnectionMode {
        ConnectionMode::Daemon
    }
}

#[cfg(unix)]
fn daemon_error_allows_fallback(error: &ConnectionError) -> bool {
    !error.is_cancelled() && (error.is_timeout() || error.error_class() == ErrorClass::Transient)
}

#[cfg(unix)]
fn cli_to_connection_error(error: CliError) -> ConnectionError {
    if error.kind == ErrorKind::Timeout {
        ConnectionError::timed_out("direct fallback timed out")
    } else if error.error_class() == ErrorClass::Cancelled {
        ConnectionError::cancelled("direct fallback was cancelled")
    } else {
        ConnectionError::new("direct fallback failed").with_class(error.error_class())
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DaemonFailure {
    Operational(&'static str),
    Security(&'static str),
    Cancelled,
}

#[cfg(unix)]
trait DaemonBackend: Send + Sync {
    fn acquire<'a>(
        &'a self,
        context: &'a CommandContext,
        server: &'a ServerDefinition,
    ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, DaemonFailure>>;
}

#[cfg(unix)]
mod unix_backend {
    use std::{io, path::Path, sync::Arc, time::Duration};

    use tokio::{process::Child, task::JoinHandle};

    use crate::{
        config::{ConfigHash, ServerDefinition},
        connection::{ConnectionError, McpConnection},
        daemon::{
            DaemonPaths, MetadataError, MetadataStore, PidMetadata,
            client::DaemonClient,
            metadata::{ProcessInspector, ProcessStatus},
            paths::ArtifactIdentity,
            worker::{CurrentExecutableDaemonSpawner, DaemonSpawner},
        },
        policy::retry::ErrorClass,
        runtime::{BoxFuture, CommandContext},
    };

    use super::{DaemonBackend, DaemonFailure};

    pub(super) struct UnixDaemonBackend {
        spawner: Arc<dyn DaemonSpawner>,
        reapers: std::sync::Mutex<Vec<JoinHandle<()>>>,
    }

    impl UnixDaemonBackend {
        pub(super) fn new(idle_timeout: Duration) -> Self {
            Self {
                spawner: Arc::new(CurrentExecutableDaemonSpawner::new(idle_timeout)),
                reapers: std::sync::Mutex::new(Vec::new()),
            }
        }

        async fn acquire_inner(
            &self,
            context: &CommandContext,
            server: &ServerDefinition,
        ) -> Result<Box<dyn McpConnection>, DaemonFailure> {
            let paths = DaemonPaths::new(&server.id)
                .map_err(|_| DaemonFailure::Security("daemon runtime path validation failed"))?;
            let store = MetadataStore::new(paths.clone());
            let persisted = match store.read_with_identity() {
                Ok((metadata, pid_identity)) => Some((metadata, pid_identity)),
                Err(MetadataError::Io { source, .. })
                    if source.kind() == io::ErrorKind::NotFound =>
                {
                    None
                }
                Err(_) => {
                    return Err(DaemonFailure::Security(
                        "daemon metadata format, owner, type, or path validation failed",
                    ));
                }
            };
            let inspector = ProcessInspector::new().map_err(|_| {
                DaemonFailure::Security("daemon process inspector initialization failed")
            })?;

            if persisted.is_none() {
                // A first start is allowed only when no server-specific runtime
                // artifacts exist. Existing artifacts are validated before an
                // operational fallback, so a dangling symlink or wrong type
                // can never be hidden by direct mode.
                match validated_artifacts_absent(&paths) {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err(DaemonFailure::Operational(
                            "startup artifacts already existed without metadata",
                        ));
                    }
                    Err(error) => return Err(error),
                }
            }

            if let Some((metadata, pid_identity)) = persisted {
                let artifacts = capture_artifacts(&paths, pid_identity)?;
                match inspector.inspect(&metadata).map_err(|_| {
                    DaemonFailure::Security("daemon process identity validation failed")
                })? {
                    ProcessStatus::Dead => {
                        cleanup_dead(&paths, &inspector, &metadata)?;
                    }
                    ProcessStatus::Verified(_) => {
                        let socket_exists = artifacts.socket.is_some();
                        match existing_state(
                            true,
                            socket_exists,
                            metadata.config_hash,
                            server.config_hash,
                        ) {
                            ExistingState::Reuse => {
                                // Revalidate immediately before trusting the socket.
                                require_same_process(&inspector, &metadata)?;
                                return connect_client(context, &paths.socket).await;
                            }
                            ExistingState::Dead => unreachable!("verified process is live"),
                            ExistingState::MissingSocket => {
                                return Err(DaemonFailure::Operational("socket was missing"));
                            }
                            ExistingState::StaleHash => {
                                stop_stale_worker(context, &paths, &inspector, metadata).await?;
                            }
                        }
                    }
                }
            }

            self.spawn_and_connect(context, server, &paths, &inspector)
                .await
        }

        async fn spawn_and_connect(
            &self,
            context: &CommandContext,
            server: &ServerDefinition,
            paths: &DaemonPaths,
            inspector: &ProcessInspector,
        ) -> Result<Box<dyn McpConnection>, DaemonFailure> {
            let mut ready = self
                .spawner
                .spawn(context, server, paths)
                .await
                .map_err(|error| match error {
                    crate::daemon::worker::DaemonSpawnError::InvalidPaths => {
                        DaemonFailure::Security("daemon spawner rejected runtime paths")
                    }
                    crate::daemon::worker::DaemonSpawnError::Cancelled => DaemonFailure::Cancelled,
                    _ => DaemonFailure::Operational("startup or ready failed"),
                })?;

            let metadata = match MetadataStore::new(paths.clone()).read() {
                Ok(metadata) => metadata,
                Err(MetadataError::Io { source, .. })
                    if source.kind() == io::ErrorKind::NotFound =>
                {
                    terminate_owned_child(ready.child_mut()).await;
                    return Err(DaemonFailure::Operational(
                        "metadata disappeared after ready",
                    ));
                }
                Err(_) => {
                    terminate_owned_child(ready.child_mut()).await;
                    return Err(DaemonFailure::Security(
                        "spawned daemon metadata validation failed",
                    ));
                }
            };
            if metadata.pid != ready.pid() || metadata.config_hash != server.config_hash {
                terminate_owned_child(ready.child_mut()).await;
                return Err(DaemonFailure::Security(
                    "spawned daemon metadata did not match the owned child",
                ));
            }
            if !matches!(inspector.inspect(&metadata), Ok(ProcessStatus::Verified(_))) {
                terminate_owned_child(ready.child_mut()).await;
                return Err(DaemonFailure::Security(
                    "spawned daemon process identity did not match metadata",
                ));
            }

            match connect_client(context, &paths.socket).await {
                Ok(client) => {
                    self.reap(ready.into_child());
                    Ok(client)
                }
                Err(error) => {
                    terminate_owned_child(ready.child_mut()).await;
                    let _ = cleanup_dead(paths, inspector, &metadata);
                    Err(error)
                }
            }
        }

        fn reap(&self, mut child: Child) {
            let task = tokio::spawn(async move {
                let _ = child.wait().await;
            });
            let mut reapers = self
                .reapers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            reapers.retain(|reaper| !reaper.is_finished());
            reapers.push(task);
        }
    }

    impl DaemonBackend for UnixDaemonBackend {
        fn acquire<'a>(
            &'a self,
            context: &'a CommandContext,
            server: &'a ServerDefinition,
        ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, DaemonFailure>> {
            Box::pin(self.acquire_inner(context, server))
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum ExistingState {
        Reuse,
        Dead,
        MissingSocket,
        StaleHash,
    }

    pub(super) fn existing_state(
        alive: bool,
        socket_exists: bool,
        actual_hash: ConfigHash,
        expected_hash: ConfigHash,
    ) -> ExistingState {
        if !alive {
            ExistingState::Dead
        } else if !socket_exists {
            ExistingState::MissingSocket
        } else if actual_hash != expected_hash {
            ExistingState::StaleHash
        } else {
            ExistingState::Reuse
        }
    }

    async fn connect_client(
        context: &CommandContext,
        socket: &Path,
    ) -> Result<Box<dyn McpConnection>, DaemonFailure> {
        DaemonClient::connect(context, socket)
            .await
            .map(|client| Box::new(client) as Box<dyn McpConnection>)
            .map_err(connection_failure)
    }

    fn connection_failure(error: ConnectionError) -> DaemonFailure {
        if error.is_cancelled() {
            DaemonFailure::Cancelled
        } else if error.is_timeout() || error.error_class() == ErrorClass::Transient {
            DaemonFailure::Operational("ping or IPC handshake failed")
        } else {
            DaemonFailure::Security("daemon socket or IPC integrity validation failed")
        }
    }

    async fn stop_stale_worker(
        context: &CommandContext,
        paths: &DaemonPaths,
        inspector: &ProcessInspector,
        metadata: PidMetadata,
    ) -> Result<(), DaemonFailure> {
        // Identity is checked both before opening the control channel and again
        // before any eventual SIGTERM decision.
        require_same_process(inspector, &metadata)?;
        let client = DaemonClient::connect(context, &paths.socket)
            .await
            .map_err(connection_failure)?;
        client
            .shutdown_worker(context)
            .await
            .map_err(connection_failure)?;

        let remaining =
            context.remaining_capped(&crate::runtime::SystemClock, Duration::from_secs(5));
        if remaining.is_zero() {
            return Err(DaemonFailure::Operational(
                "stale configuration shutdown timed out",
            ));
        }
        let deadline = tokio::time::Instant::now() + remaining;
        loop {
            if context.is_cancelled() {
                return Err(DaemonFailure::Cancelled);
            }
            match inspector.inspect(&metadata).map_err(|_| {
                DaemonFailure::Security("stale daemon process identity validation failed")
            })? {
                ProcessStatus::Dead => {
                    cleanup_dead(paths, inspector, &metadata)?;
                    return Ok(());
                }
                ProcessStatus::Verified(_) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                // A graceful close is an operational action; SIGTERM is only
                // permitted after a fresh, race-resistant identity check.
                inspector.terminate_verified(&metadata).map_err(|_| {
                    DaemonFailure::Security("stale daemon termination identity validation failed")
                })?;
                return wait_for_verified_exit(context, paths, inspector, &metadata).await;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_for_verified_exit(
        context: &CommandContext,
        paths: &DaemonPaths,
        inspector: &ProcessInspector,
        metadata: &PidMetadata,
    ) -> Result<(), DaemonFailure> {
        let remaining =
            context.remaining_capped(&crate::runtime::SystemClock, Duration::from_secs(5));
        if remaining.is_zero() {
            return Err(DaemonFailure::Operational(
                "stale configuration termination timed out",
            ));
        }
        let deadline = tokio::time::Instant::now() + remaining;
        loop {
            match inspector.inspect(metadata).map_err(|_| {
                DaemonFailure::Security("terminated daemon identity became ambiguous")
            })? {
                ProcessStatus::Dead => {
                    cleanup_dead(paths, inspector, metadata)?;
                    return Ok(());
                }
                ProcessStatus::Verified(_) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(DaemonFailure::Operational(
                    "stale configuration worker did not stop after termination",
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn require_same_process(
        inspector: &ProcessInspector,
        metadata: &PidMetadata,
    ) -> Result<(), DaemonFailure> {
        match inspector
            .inspect(metadata)
            .map_err(|_| DaemonFailure::Security("daemon process identity validation failed"))?
        {
            ProcessStatus::Verified(_) => Ok(()),
            ProcessStatus::Dead => Err(DaemonFailure::Operational(
                "daemon exited during identity validation",
            )),
        }
    }

    #[derive(Clone, Copy)]
    pub(super) struct CapturedArtifacts {
        socket: Option<ArtifactIdentity>,
        pid: ArtifactIdentity,
        lock: Option<ArtifactIdentity>,
    }

    pub(super) fn capture_artifacts(
        paths: &DaemonPaths,
        pid: ArtifactIdentity,
    ) -> Result<CapturedArtifacts, DaemonFailure> {
        let socket = paths.capture_socket_identity().map_err(|_| {
            DaemonFailure::Security("daemon socket owner, type, or path validation failed")
        })?;
        let lock = paths.capture_lock_identity().map_err(|_| {
            DaemonFailure::Security("daemon lock owner, type, or path validation failed")
        })?;
        Ok(CapturedArtifacts { socket, pid, lock })
    }

    fn validated_artifacts_absent(paths: &DaemonPaths) -> Result<bool, DaemonFailure> {
        let socket = paths.capture_socket_identity().map_err(|_| {
            DaemonFailure::Security("daemon socket owner, type, or path validation failed")
        })?;
        let pid = paths.capture_pid_identity().map_err(|_| {
            DaemonFailure::Security("daemon PID owner, type, or path validation failed")
        })?;
        let lock = paths.capture_lock_identity().map_err(|_| {
            DaemonFailure::Security("daemon lock owner, type, or path validation failed")
        })?;
        Ok(socket.is_none() && pid.is_none() && lock.is_none())
    }

    pub(super) fn cleanup_captured(
        paths: &DaemonPaths,
        artifacts: CapturedArtifacts,
    ) -> Result<(), DaemonFailure> {
        if let Some(identity) = artifacts.socket {
            paths.remove_socket_if_owned(identity).map_err(|_| {
                DaemonFailure::Security("dead daemon socket changed during cleanup")
            })?;
        }
        paths
            .remove_pid_if_owned(artifacts.pid)
            .map_err(|_| DaemonFailure::Security("dead daemon metadata changed during cleanup"))?;
        if let Some(identity) = artifacts.lock {
            paths
                .remove_lock_if_owned(identity)
                .map_err(|_| DaemonFailure::Security("dead daemon lock changed during cleanup"))?;
        }
        Ok(())
    }

    fn cleanup_dead(
        paths: &DaemonPaths,
        inspector: &ProcessInspector,
        expected: &PidMetadata,
    ) -> Result<(), DaemonFailure> {
        let (current, pid_identity) = match MetadataStore::new(paths.clone()).read_with_identity() {
            Ok(value) => value,
            Err(MetadataError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                return if validated_artifacts_absent(paths)? {
                    Ok(())
                } else {
                    Err(DaemonFailure::Security(
                        "daemon metadata disappeared while other artifacts remained",
                    ))
                };
            }
            Err(_) => {
                return Err(DaemonFailure::Security(
                    "dead daemon metadata revalidation failed",
                ));
            }
        };
        if &current != expected {
            return Err(DaemonFailure::Security(
                "dead daemon metadata changed before cleanup",
            ));
        }
        match inspector.inspect(&current).map_err(|_| {
            DaemonFailure::Security("dead daemon process identity revalidation failed")
        })? {
            ProcessStatus::Dead => {
                let artifacts = capture_artifacts(paths, pid_identity)?;
                cleanup_captured(paths, artifacts)
            }
            ProcessStatus::Verified(_) => Err(DaemonFailure::Security(
                "daemon became live before dead artifact cleanup",
            )),
        }
    }

    async fn terminate_owned_child(child: &mut Child) {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

#[cfg(unix)]
use unix_backend::UnixDaemonBackend;

#[cfg(all(unix, test))]
mod tests {
    use std::{
        collections::{BTreeMap, VecDeque},
        fs,
        os::unix::{fs::PermissionsExt, net::UnixListener},
        path::PathBuf,
        sync::{
            Arc, Mutex as StdMutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::{Duration, Instant, UNIX_EPOCH},
    };

    use serde_json::json;

    use crate::{
        config::{ConfigHash, ServerId, ToolFilterConfig, TransportConfig, server_id},
        output::DiagnosticSink,
        runtime::{CancellationFlag, Deadline},
    };

    use super::*;

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

    fn server(hash: u8) -> ServerDefinition {
        ServerDefinition {
            name: "fixture".into(),
            id: ServerId("a".repeat(64)),
            config_hash: ConfigHash([hash; 32]),
            transport: TransportConfig::Stdio {
                command: "fixture".into(),
                args: Vec::new(),
                env: BTreeMap::new(),
                cwd: Some(PathBuf::from("/tmp")),
            },
            filter: ToolFilterConfig::default(),
        }
    }

    #[derive(Default)]
    struct Trace {
        daemon_acquires: AtomicUsize,
        direct_connects: AtomicUsize,
        daemon_calls: AtomicUsize,
        direct_calls: AtomicUsize,
        acknowledged: AtomicBool,
    }

    struct FakeDaemonBackend {
        trace: Arc<Trace>,
        outcomes: StdMutex<VecDeque<Result<Box<dyn McpConnection>, DaemonFailure>>>,
    }

    impl DaemonBackend for FakeDaemonBackend {
        fn acquire<'a>(
            &'a self,
            _context: &'a CommandContext,
            _server: &'a ServerDefinition,
        ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, DaemonFailure>> {
            self.trace.daemon_acquires.fetch_add(1, Ordering::SeqCst);
            let result = self
                .outcomes
                .lock()
                .expect("outcomes")
                .pop_front()
                .expect("scripted outcome");
            Box::pin(async move { result })
        }
    }

    struct FakeDirectConnector(Arc<Trace>);

    impl DirectConnector for FakeDirectConnector {
        fn connect<'a>(
            &'a self,
            _context: &'a CommandContext,
            _server: &'a ServerDefinition,
        ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, ConnectionError>> {
            self.0.direct_connects.fetch_add(1, Ordering::SeqCst);
            let connection = FakeConnection {
                trace: Arc::clone(&self.0),
                daemon: false,
                fail: None,
            };
            Box::pin(async move { Ok(Box::new(connection) as Box<dyn McpConnection>) })
        }
    }

    struct FakeConnection {
        trace: Arc<Trace>,
        daemon: bool,
        fail: Option<ErrorClass>,
    }

    impl McpConnection for FakeConnection {
        fn list_tools<'a>(
            &'a self,
            _context: &'a CommandContext,
        ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
            if self.daemon {
                self.trace.daemon_calls.fetch_add(1, Ordering::SeqCst);
            } else {
                self.trace.direct_calls.fetch_add(1, Ordering::SeqCst);
            }
            let failure = self.fail;
            let acknowledged = Arc::clone(&self.trace);
            Box::pin(async move {
                if let Some(class) = failure {
                    acknowledged.acknowledged.store(true, Ordering::SeqCst);
                    Err(ConnectionError::new("scripted daemon failure").with_class(class))
                } else {
                    Ok(vec![ToolInfo {
                        name: "echo".into(),
                        description: None,
                        input_schema: json!({"type":"object"}),
                    }])
                }
            })
        }

        fn call_tool<'a>(
            &'a self,
            _context: &'a CommandContext,
            _name: &'a str,
            _args: JsonObject,
        ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
            Box::pin(async { Ok(json!({"ok":true})) })
        }

        fn instructions(&self) -> Option<&str> {
            None
        }

        fn close<'a>(
            self: Box<Self>,
            _context: &'a CommandContext,
        ) -> BoxFuture<'a, Result<(), ConnectionError>> {
            Box::pin(async { Ok(()) })
        }

        fn mode(&self) -> ConnectionMode {
            if self.daemon {
                ConnectionMode::Daemon
            } else {
                ConnectionMode::Direct
            }
        }
    }

    fn manager(
        trace: Arc<Trace>,
        daemon: Option<Arc<dyn DaemonBackend>>,
    ) -> ManagedConnectionManager {
        let runtime = RuntimeConfig {
            max_retries: 0,
            ..RuntimeConfig::default()
        };
        ManagedConnectionManager::with_daemon_backend(
            Arc::new(FakeDirectConnector(trace)),
            ConnectionResourceRegistry::new(),
            &runtime,
            daemon,
        )
    }

    fn daemon_connection(trace: &Arc<Trace>, fail: Option<ErrorClass>) -> Box<dyn McpConnection> {
        Box::new(FakeConnection {
            trace: Arc::clone(trace),
            daemon: true,
            fail,
        })
    }

    #[test]
    fn state_plan_covers_reuse_hash_change_dead_pid_and_missing_socket() {
        use super::unix_backend::{ExistingState, existing_state};
        let old = ConfigHash([1; 32]);
        let new = ConfigHash([2; 32]);
        assert_eq!(existing_state(true, true, old, old), ExistingState::Reuse);
        assert_eq!(
            existing_state(true, true, old, new),
            ExistingState::StaleHash
        );
        assert_eq!(existing_state(false, true, old, old), ExistingState::Dead);
        assert_eq!(
            existing_state(true, false, old, old),
            ExistingState::MissingSocket
        );
    }

    #[tokio::test]
    async fn first_spawn_and_existing_reuse_both_select_daemon_without_direct() {
        let trace = Arc::new(Trace::default());
        let backend = Arc::new(FakeDaemonBackend {
            trace: Arc::clone(&trace),
            outcomes: StdMutex::new(VecDeque::from([
                Ok(daemon_connection(&trace, None)),
                Ok(daemon_connection(&trace, None)),
            ])),
        });
        let manager = manager(Arc::clone(&trace), Some(backend));
        let ctx = context();
        for _ in 0..2 {
            let connection = manager.acquire(&ctx, &server(1)).await.unwrap();
            connection.close(&ctx).await.unwrap();
        }
        assert_eq!(trace.daemon_acquires.load(Ordering::SeqCst), 2);
        assert_eq!(trace.direct_connects.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn ready_ping_and_acquire_operational_failures_fallback_to_same_direct_connector() {
        for reason in ["ready failed", "ping failed", "missing socket", "dead pid"] {
            let trace = Arc::new(Trace::default());
            let backend = Arc::new(FakeDaemonBackend {
                trace: Arc::clone(&trace),
                outcomes: StdMutex::new(VecDeque::from([Err(DaemonFailure::Operational(reason))])),
            });
            let manager = manager(Arc::clone(&trace), Some(backend));
            let ctx = context();
            let connection = manager.acquire(&ctx, &server(1)).await.unwrap();
            assert_eq!(connection.mode(), ConnectionMode::Direct);
            connection.close(&ctx).await.unwrap();
            assert_eq!(trace.direct_connects.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn request_fallback_waits_for_acknowledgement_and_calls_direct_once() {
        let trace = Arc::new(Trace::default());
        let backend = Arc::new(FakeDaemonBackend {
            trace: Arc::clone(&trace),
            outcomes: StdMutex::new(VecDeque::from([Ok(daemon_connection(
                &trace,
                Some(ErrorClass::Transient),
            ))])),
        });
        let manager = manager(Arc::clone(&trace), Some(backend));
        let ctx = context();
        let connection = manager.acquire(&ctx, &server(1)).await.unwrap();
        let tools = connection.list_tools(&ctx).await.unwrap();
        assert_eq!(tools[0].name, "echo");
        assert!(trace.acknowledged.load(Ordering::SeqCst));
        assert_eq!(trace.daemon_calls.load(Ordering::SeqCst), 1);
        assert_eq!(trace.direct_calls.load(Ordering::SeqCst), 1);
        assert_eq!(trace.direct_connects.load(Ordering::SeqCst), 1);
        connection.close(&ctx).await.unwrap();
    }

    #[tokio::test]
    async fn security_and_nontransient_request_fail_closed_without_direct() {
        let trace = Arc::new(Trace::default());
        let backend = Arc::new(FakeDaemonBackend {
            trace: Arc::clone(&trace),
            outcomes: StdMutex::new(VecDeque::from([Err(DaemonFailure::Security(
                "unsafe metadata",
            ))])),
        });
        let security_manager = manager(Arc::clone(&trace), Some(backend));
        let ctx = context();
        let error = match security_manager.acquire(&ctx, &server(1)).await {
            Ok(_) => panic!("security state unexpectedly acquired"),
            Err(error) => error,
        };
        assert_eq!(error.kind, ErrorKind::SecurityError);
        assert_eq!(trace.direct_connects.load(Ordering::SeqCst), 0);

        let trace = Arc::new(Trace::default());
        let backend = Arc::new(FakeDaemonBackend {
            trace: Arc::clone(&trace),
            outcomes: StdMutex::new(VecDeque::from([Ok(daemon_connection(
                &trace,
                Some(ErrorClass::NonTransient),
            ))])),
        });
        let manager = manager(Arc::clone(&trace), Some(backend));
        let ctx = context();
        let connection = manager.acquire(&ctx, &server(1)).await.unwrap();
        assert!(connection.list_tools(&ctx).await.is_err());
        assert_eq!(trace.direct_connects.load(Ordering::SeqCst), 0);
        connection.close(&ctx).await.unwrap();
    }

    #[test]
    fn dead_worker_cleanup_removes_only_captured_owned_artifacts() {
        use super::unix_backend::{capture_artifacts, cleanup_captured};
        use crate::daemon::{DaemonPaths, MetadataStore, PidMetadata};

        let root = tempfile::tempdir().expect("runtime root");
        let paths = DaemonPaths::from_runtime_parent(root.path(), &server_id("dead-worker"))
            .expect("paths");
        // Bind the socket in a guaranteed-short path to stay within SUN_LEN
        // even when TMPDIR is long (e.g. CI runners).
        let sock_dir = tempfile::Builder::new()
            .tempdir_in("/tmp")
            .expect("short socket dir");
        let short_socket = sock_dir.path().join("s");
        let listener = UnixListener::bind(&short_socket).expect("socket");
        fs::rename(&short_socket, &paths.socket).expect("move socket");
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))
            .expect("socket permissions");
        fs::write(&paths.lock, b"owned lock").expect("lock");
        fs::set_permissions(&paths.lock, fs::Permissions::from_mode(0o600))
            .expect("lock permissions");
        let store = MetadataStore::new(paths.clone());
        store
            .write(&PidMetadata {
                pid: 4242,
                config_hash: ConfigHash([7; 32]),
                started_at: UNIX_EPOCH + Duration::from_secs(7),
            })
            .expect("metadata");
        let (_, pid_identity) = store.read_with_identity().expect("metadata identity");
        let captured = capture_artifacts(&paths, pid_identity).expect("capture artifacts");

        cleanup_captured(&paths, captured).expect("safe cleanup");
        assert!(!paths.socket.exists());
        assert!(!paths.pid.exists());
        assert!(!paths.lock.exists());
        drop(listener);
    }

    #[tokio::test]
    async fn direct_only_never_observes_daemon_backend() {
        let trace = Arc::new(Trace::default());
        let manager = manager(Arc::clone(&trace), None);
        let ctx = context();
        let connection = manager.acquire(&ctx, &server(1)).await.unwrap();
        assert_eq!(connection.mode(), ConnectionMode::Direct);
        connection.close(&ctx).await.unwrap();
        assert_eq!(trace.daemon_acquires.load(Ordering::SeqCst), 0);
        assert_eq!(trace.direct_connects.load(Ordering::SeqCst), 1);
    }
}
