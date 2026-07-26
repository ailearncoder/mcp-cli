#![cfg(unix)]
#![forbid(unsafe_code)]

#[path = "../support/mod.rs"]
mod support;

use std::{
    collections::BTreeSet,
    future::pending,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use mcp_cli::{
    BoxFuture, CommandContext, ConnectionError, ConnectionMode, Deadline, DiagnosticSink,
    JsonObject, McpConnection, ToolInfo, ToolResult,
    daemon::worker::{
        WorkerCleanupError, WorkerError, WorkerIpcService, WorkerShutdownFailure,
        WorkerShutdownHooks, WorkerShutdownPhase, WorkerShutdownStep, WorkerSignal, WorkerStop,
    },
};
use proptest::{prelude::*, test_runner::TestCaseError};
use serde_json::json;
use support::{FakeClock, TestCancellationToken};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Notify, mpsc},
    task::JoinHandle,
    time::timeout,
};

const CASES: u32 = 128;
const IDLE_TIMEOUT: Duration = Duration::from_millis(50);
const IO_TIMEOUT: Duration = Duration::from_secs(1);
const SCENARIO_TIMEOUT: Duration = Duration::from_secs(4);
const SECRET: &str = "property-17-secret-must-not-leak";

#[derive(Default)]
struct SilentDiagnostics;

impl DiagnosticSink for SilentDiagnostics {
    fn warning(&self, _message: &str) {}
    fn debug(&self, _message: &str) {}
    fn server_stderr(&self, _server: &str, _bytes: &[u8]) {}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Trigger {
    CloseRequest,
    IdleExpiry,
    Sigint,
    Sigterm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ModelReason {
    CloseRequest,
    IdleExpiry,
    Sigint,
    Sigterm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModelPhase {
    Draining(ModelReason),
    AcceptStopped,
    ClientsCancelled,
    ClientsJoined,
    BackendClosed,
    SocketUnlinked,
    PidUnlinked,
    LockReleased,
    Closed(ModelReason),
}

#[derive(Clone, Copy, Debug)]
struct FailurePlan {
    backend: bool,
    socket: bool,
    pid: bool,
    lock: bool,
}

/// Pure shutdown oracle. It owns no production worker values and never calls
/// the production shutdown state machine.
struct ShutdownOracle {
    reason: ModelReason,
    phases: Vec<ModelPhase>,
    failure_steps: Vec<&'static str>,
    visible_error: Option<String>,
}

impl ShutdownOracle {
    fn serial(triggers: &[Trigger], failures: FailurePlan) -> Self {
        assert!(!triggers.is_empty());
        let reason = model_reason(triggers[0]);
        let phases = model_phases(reason);
        let mut failure_steps = Vec::new();
        if failures.backend {
            failure_steps.push("MCP backend close");
        }
        if failures.socket {
            failure_steps.push("socket cleanup");
        }
        if failures.pid {
            failure_steps.push("PID cleanup");
        }
        if failures.lock {
            failure_steps.push("lock release");
        }
        let visible_error = (!failure_steps.is_empty()).then(|| {
            format!(
                "daemon shutdown completed with failures in {}",
                failure_steps.join(", ")
            )
        });
        Self {
            reason,
            phases,
            failure_steps,
            visible_error,
        }
    }

    fn competing_reasons(triggers: &[Trigger]) -> BTreeSet<ModelReason> {
        triggers.iter().copied().map(model_reason).collect()
    }
}

fn model_reason(trigger: Trigger) -> ModelReason {
    match trigger {
        Trigger::CloseRequest => ModelReason::CloseRequest,
        Trigger::IdleExpiry => ModelReason::IdleExpiry,
        Trigger::Sigint => ModelReason::Sigint,
        Trigger::Sigterm => ModelReason::Sigterm,
    }
}

fn model_phases(reason: ModelReason) -> Vec<ModelPhase> {
    vec![
        ModelPhase::Draining(reason),
        ModelPhase::AcceptStopped,
        ModelPhase::ClientsCancelled,
        ModelPhase::ClientsJoined,
        ModelPhase::BackendClosed,
        ModelPhase::SocketUnlinked,
        ModelPhase::PidUnlinked,
        ModelPhase::LockReleased,
        ModelPhase::Closed(reason),
    ]
}

fn observed_reason(stop: WorkerStop) -> Result<ModelReason, TestCaseError> {
    match stop {
        WorkerStop::CloseRequested => Ok(ModelReason::CloseRequest),
        WorkerStop::IdleTimeout => Ok(ModelReason::IdleExpiry),
        WorkerStop::SignalInterrupt => Ok(ModelReason::Sigint),
        WorkerStop::SignalTerminate => Ok(ModelReason::Sigterm),
        WorkerStop::AcceptFailure => Err(TestCaseError::fail(
            "unexpected accept failure in isolated Unix fixture",
        )),
    }
}

fn observed_phase(phase: WorkerShutdownPhase) -> Result<ModelPhase, TestCaseError> {
    match phase {
        WorkerShutdownPhase::Draining(stop) => Ok(ModelPhase::Draining(observed_reason(stop)?)),
        WorkerShutdownPhase::AcceptStopped => Ok(ModelPhase::AcceptStopped),
        WorkerShutdownPhase::ClientsCancelled => Ok(ModelPhase::ClientsCancelled),
        WorkerShutdownPhase::ClientsJoined => Ok(ModelPhase::ClientsJoined),
        WorkerShutdownPhase::BackendClosed => Ok(ModelPhase::BackendClosed),
        WorkerShutdownPhase::SocketCleaned => Ok(ModelPhase::SocketUnlinked),
        WorkerShutdownPhase::PidCleaned => Ok(ModelPhase::PidUnlinked),
        WorkerShutdownPhase::LockReleased => Ok(ModelPhase::LockReleased),
        WorkerShutdownPhase::Closed(stop) => Ok(ModelPhase::Closed(observed_reason(stop)?)),
    }
}

#[derive(Default)]
struct BackendState {
    list_started: Notify,
    close_started: Notify,
    release_close: Notify,
    client_cancelled: AtomicBool,
    close_saw_cancelled_client: AtomicBool,
    close_count: AtomicUsize,
    backend_fails: AtomicBool,
}

struct CancellationMarker(Arc<BackendState>);

impl Drop for CancellationMarker {
    fn drop(&mut self) {
        self.0.client_cancelled.store(true, Ordering::SeqCst);
    }
}

struct PropertyConnection {
    state: Arc<BackendState>,
}

impl McpConnection for PropertyConnection {
    fn list_tools<'a>(
        &'a self,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let _marker = CancellationMarker(Arc::clone(&state));
            state.list_started.notify_one();
            pending::<()>().await;
            #[allow(unreachable_code)]
            Ok(Vec::new())
        })
    }

    fn call_tool<'a>(
        &'a self,
        _ctx: &'a CommandContext,
        _name: &'a str,
        _args: JsonObject,
    ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
        Box::pin(async { Ok(json!(null)) })
    }

    fn instructions(&self) -> Option<&str> {
        None
    }

    fn close<'a>(
        self: Box<Self>,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<(), ConnectionError>> {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            state.close_count.fetch_add(1, Ordering::SeqCst);
            state.close_saw_cancelled_client.store(
                state.client_cancelled.load(Ordering::SeqCst),
                Ordering::SeqCst,
            );
            state.close_started.notify_one();
            state.release_close.notified().await;
            if state.backend_fails.load(Ordering::SeqCst) {
                Err(ConnectionError::new(format!("{SECRET}: backend close")))
            } else {
                Ok(())
            }
        })
    }

    fn mode(&self) -> ConnectionMode {
        ConnectionMode::Daemon
    }
}

#[derive(Default)]
struct HookState {
    phases: Mutex<Vec<WorkerShutdownPhase>>,
    phase_changed: Notify,
    socket_count: AtomicUsize,
    pid_count: AtomicUsize,
    lock_count: AtomicUsize,
}

struct PropertyHooks {
    signals: mpsc::UnboundedReceiver<WorkerSignal>,
    state: Arc<HookState>,
    failures: FailurePlan,
}

impl PropertyHooks {
    fn cleanup_result(fails: bool, step: &str) -> Result<(), WorkerCleanupError> {
        if fails {
            Err(WorkerCleanupError::new(io::Error::other(format!(
                "{SECRET}: {step}"
            ))))
        } else {
            Ok(())
        }
    }
}

impl WorkerShutdownHooks for PropertyHooks {
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
        Self::cleanup_result(self.failures.socket, "socket cleanup")
    }

    fn remove_pid(&mut self) -> Result<(), WorkerCleanupError> {
        self.state.pid_count.fetch_add(1, Ordering::SeqCst);
        Self::cleanup_result(self.failures.pid, "PID cleanup")
    }

    fn release_lock(&mut self) -> Result<(), WorkerCleanupError> {
        self.state.lock_count.fetch_add(1, Ordering::SeqCst);
        Self::cleanup_result(self.failures.lock, "lock release")
    }

    fn observe_phase(&mut self, phase: WorkerShutdownPhase) {
        self.state.phases.lock().expect("phase lock").push(phase);
        self.state.phase_changed.notify_waiters();
    }
}

struct RunningWorker {
    _directory: tempfile::TempDir,
    _blocked: UnixStream,
    socket: PathBuf,
    clock: FakeClock,
    backend: Arc<BackendState>,
    hooks: Arc<HookState>,
    signals: mpsc::UnboundedSender<WorkerSignal>,
    control: UnixStream,
    task: JoinHandle<Result<WorkerStop, WorkerError>>,
}

fn context(start: Instant) -> CommandContext {
    CommandContext {
        deadline: Deadline::new(
            start
                .checked_add(Duration::from_secs(3_600))
                .expect("bounded command deadline"),
        ),
        cancellation: Arc::new(TestCancellationToken::default()),
        diagnostics: Arc::new(SilentDiagnostics),
    }
}

async fn start_worker(
    start: Instant,
    failures: FailurePlan,
) -> Result<RunningWorker, TestCaseError> {
    let directory = tempfile::tempdir()
        .map_err(|error| TestCaseError::fail(format!("temporary directory failed: {error}")))?;
    let socket = directory.path().join("property-17.sock");
    let listener = UnixListener::bind(&socket)
        .map_err(|error| TestCaseError::fail(format!("Unix listener bind failed: {error}")))?;
    let clock = FakeClock::new(start);
    let backend = Arc::new(BackendState::default());
    backend
        .backend_fails
        .store(failures.backend, Ordering::SeqCst);
    let connection = PropertyConnection {
        state: Arc::clone(&backend),
    };
    let hooks_state = Arc::new(HookState::default());
    let (signals, signal_receiver) = mpsc::unbounded_channel();
    let hooks = PropertyHooks {
        signals: signal_receiver,
        state: Arc::clone(&hooks_state),
        failures,
    };
    let service = WorkerIpcService::with_clock(
        Box::new(connection),
        context(start),
        IDLE_TIMEOUT,
        Arc::new(clock.clone()),
    );
    let task = tokio::spawn(service.serve_with_shutdown_hooks(listener, hooks));

    let mut blocked = connect(&socket).await?;
    blocked
        .write_all(b"{\"id\":\"blocked\",\"type\":\"listTools\"}\n")
        .await
        .map_err(|error| TestCaseError::fail(format!("blocked request write failed: {error}")))?;
    timeout(IO_TIMEOUT, backend.list_started.notified())
        .await
        .map_err(|_| TestCaseError::fail("blocked client did not enter backend operation"))?;
    let control = connect(&socket).await?;

    // Keeping the blocked peer alive proves shutdown cancellation and join
    // occur before backend close.
    Ok(RunningWorker {
        _directory: directory,
        _blocked: blocked,
        socket,
        clock,
        backend,
        hooks: hooks_state,
        signals,
        control,
        task,
    })
}

async fn connect(socket: &Path) -> Result<UnixStream, TestCaseError> {
    timeout(IO_TIMEOUT, UnixStream::connect(socket))
        .await
        .map_err(|_| TestCaseError::fail("timed out connecting isolated Unix client"))?
        .map_err(|error| TestCaseError::fail(format!("Unix client connect failed: {error}")))
}

async fn send_close(control: &mut UnixStream, id: &str) -> Result<(), TestCaseError> {
    let frame = format!("{{\"id\":\"{id}\",\"type\":\"close\"}}\n");
    control
        .write_all(frame.as_bytes())
        .await
        .map_err(|error| TestCaseError::fail(format!("close request write failed: {error}")))?;
    let mut line = String::new();
    timeout(IO_TIMEOUT, BufReader::new(control).read_line(&mut line))
        .await
        .map_err(|_| TestCaseError::fail("timed out waiting for close acknowledgement"))?
        .map_err(|error| TestCaseError::fail(format!("close response read failed: {error}")))?;
    prop_assert!(line.contains("\"success\":true"));
    Ok(())
}

async fn activate_first(worker: &mut RunningWorker, trigger: Trigger) -> Result<(), TestCaseError> {
    match trigger {
        Trigger::CloseRequest => send_close(&mut worker.control, "winner").await,
        Trigger::IdleExpiry => {
            worker.clock.advance(IDLE_TIMEOUT);
            Ok(())
        }
        Trigger::Sigint => worker
            .signals
            .send(WorkerSignal::Interrupt)
            .map_err(|_| TestCaseError::fail("signal fixture closed before SIGINT injection")),
        Trigger::Sigterm => worker
            .signals
            .send(WorkerSignal::Terminate)
            .map_err(|_| TestCaseError::fail("signal fixture closed before SIGTERM injection")),
    }
}

fn activate_late(worker: &mut RunningWorker, trigger: Trigger, sequence: usize) {
    match trigger {
        Trigger::CloseRequest => {
            let frame = format!("{{\"id\":\"late-{sequence}\",\"type\":\"close\"}}\n");
            let _ = worker.control.try_write(frame.as_bytes());
        }
        Trigger::IdleExpiry => {
            worker.clock.advance(IDLE_TIMEOUT);
        }
        Trigger::Sigint => {
            let _ = worker.signals.send(WorkerSignal::Interrupt);
        }
        Trigger::Sigterm => {
            let _ = worker.signals.send(WorkerSignal::Terminate);
        }
    }
}

async fn wait_for_backend_close(worker: &RunningWorker) -> Result<(), TestCaseError> {
    timeout(IO_TIMEOUT, worker.backend.close_started.notified())
        .await
        .map_err(|_| TestCaseError::fail("shutdown did not reach backend close"))?;
    Ok(())
}

fn production_phases(state: &HookState) -> Result<Vec<ModelPhase>, TestCaseError> {
    state
        .phases
        .lock()
        .expect("phase lock")
        .iter()
        .copied()
        .map(observed_phase)
        .collect()
}

fn production_failure_steps(
    failures: &[WorkerShutdownFailure],
) -> Result<Vec<&'static str>, TestCaseError> {
    failures
        .iter()
        .map(|failure| match failure.step() {
            WorkerShutdownStep::BackendClose => Ok("MCP backend close"),
            WorkerShutdownStep::SocketCleanup => Ok("socket cleanup"),
            WorkerShutdownStep::PidCleanup => Ok("PID cleanup"),
            WorkerShutdownStep::LockRelease => Ok("lock release"),
            WorkerShutdownStep::ClientJoin => Err(TestCaseError::fail(
                "client join failed in deterministic cancellation fixture",
            )),
        })
        .collect()
}

fn assert_exactly_once(state: &HookState, backend: &BackendState) -> Result<(), TestCaseError> {
    prop_assert_eq!(backend.close_count.load(Ordering::SeqCst), 1);
    prop_assert_eq!(state.socket_count.load(Ordering::SeqCst), 1);
    prop_assert_eq!(state.pid_count.load(Ordering::SeqCst), 1);
    prop_assert_eq!(state.lock_count.load(Ordering::SeqCst), 1);
    prop_assert!(backend.client_cancelled.load(Ordering::SeqCst));
    prop_assert!(backend.close_saw_cancelled_client.load(Ordering::SeqCst));

    let phases = state.phases.lock().expect("phase lock");
    for phase in [
        WorkerShutdownPhase::AcceptStopped,
        WorkerShutdownPhase::ClientsCancelled,
        WorkerShutdownPhase::ClientsJoined,
        WorkerShutdownPhase::BackendClosed,
        WorkerShutdownPhase::SocketCleaned,
        WorkerShutdownPhase::PidCleaned,
        WorkerShutdownPhase::LockReleased,
    ] {
        prop_assert_eq!(
            phases.iter().filter(|observed| **observed == phase).count(),
            1,
            "phase {:?} was skipped or repeated",
            phase
        );
    }
    prop_assert_eq!(
        phases
            .iter()
            .filter(|phase| matches!(phase, WorkerShutdownPhase::Draining(_)))
            .count(),
        1
    );
    prop_assert_eq!(
        phases
            .iter()
            .filter(|phase| matches!(phase, WorkerShutdownPhase::Closed(_)))
            .count(),
        1
    );
    Ok(())
}

async fn finish_and_assert(
    worker: RunningWorker,
    oracle: &ShutdownOracle,
) -> Result<(), TestCaseError> {
    worker.backend.release_close.notify_one();
    let result = timeout(IO_TIMEOUT, worker.task)
        .await
        .map_err(|_| TestCaseError::fail("worker exceeded bounded shutdown timeout"))?
        .map_err(|error| TestCaseError::fail(format!("worker task join failed: {error}")))?;

    let observed_stop = match (&oracle.visible_error, result) {
        (None, Ok(stop)) => stop,
        (Some(expected_error), Err(WorkerError::Shutdown(shutdown))) => {
            let observed_steps = production_failure_steps(shutdown.failures())?;
            prop_assert_eq!(observed_steps.as_slice(), oracle.failure_steps.as_slice());
            let visible = shutdown.to_string();
            prop_assert_eq!(&visible, expected_error);
            prop_assert_eq!(
                shutdown.to_string(),
                visible.as_str(),
                "error text must be stable"
            );
            prop_assert!(!visible.contains(SECRET));
            shutdown.stop()
        }
        (None, Err(error)) => {
            return Err(TestCaseError::fail(format!(
                "unexpected shutdown failure: {error}"
            )));
        }
        (Some(expected), Ok(stop)) => {
            return Err(TestCaseError::fail(format!(
                "expected shutdown error {expected:?}, got successful {stop:?}"
            )));
        }
        (Some(_), Err(error)) => {
            let visible = error.to_string();
            prop_assert!(!visible.contains(SECRET));
            return Err(TestCaseError::fail(format!(
                "unexpected worker error variant: {visible}"
            )));
        }
    };

    prop_assert_eq!(observed_reason(observed_stop)?, oracle.reason);
    let phases = production_phases(&worker.hooks)?;
    prop_assert_eq!(phases.as_slice(), oracle.phases.as_slice());
    assert_exactly_once(&worker.hooks, &worker.backend)?;
    Ok(())
}

async fn run_serial_sequence(
    start: Instant,
    triggers: &[Trigger],
    failures: FailurePlan,
) -> Result<(), TestCaseError> {
    let oracle = ShutdownOracle::serial(triggers, failures);
    let mut worker = start_worker(start, failures).await?;
    activate_first(&mut worker, triggers[0]).await?;
    wait_for_backend_close(&worker).await?;

    // Listener ownership is dropped before client cancellation/backend close.
    prop_assert!(
        timeout(IO_TIMEOUT, UnixStream::connect(&worker.socket))
            .await
            .map_err(|_| TestCaseError::fail("post-drain connect did not terminate"))?
            .is_err(),
        "worker continued accepting after Draining"
    );

    // Repeated and differently ordered triggers are delivered while shutdown
    // is deliberately paused in backend close. They must not select another
    // reason or repeat any side effect.
    for (index, trigger) in triggers.iter().copied().enumerate().skip(1) {
        activate_late(&mut worker, trigger, index);
    }
    finish_and_assert(worker, &oracle).await
}

async fn run_competing_sequence(start: Instant, triggers: &[Trigger]) -> Result<(), TestCaseError> {
    let no_failures = FailurePlan {
        backend: false,
        socket: false,
        pid: false,
        lock: false,
    };
    let allowed = ShutdownOracle::competing_reasons(triggers);
    let worker = start_worker(start, no_failures).await?;

    // Current-thread execution means these synchronous operations become
    // ready as one competition set before the worker is polled again.
    for (index, trigger) in triggers.iter().copied().enumerate() {
        match trigger {
            Trigger::CloseRequest => {
                let frame = format!("{{\"id\":\"race-{index}\",\"type\":\"close\"}}\n");
                let _ = worker.control.try_write(frame.as_bytes());
            }
            Trigger::IdleExpiry => {
                worker.clock.advance(IDLE_TIMEOUT);
            }
            Trigger::Sigint => {
                let _ = worker.signals.send(WorkerSignal::Interrupt);
            }
            Trigger::Sigterm => {
                let _ = worker.signals.send(WorkerSignal::Terminate);
            }
        }
    }

    wait_for_backend_close(&worker).await?;
    worker.backend.release_close.notify_one();
    let stop = timeout(IO_TIMEOUT, worker.task)
        .await
        .map_err(|_| TestCaseError::fail("competing shutdown exceeded bounded timeout"))?
        .map_err(|error| TestCaseError::fail(format!("competing worker join failed: {error}")))?
        .map_err(|error| TestCaseError::fail(format!("competing shutdown failed: {error}")))?;
    let reason = observed_reason(stop)?;
    prop_assert!(allowed.contains(&reason));
    prop_assert_eq!(production_phases(&worker.hooks)?, model_phases(reason));
    assert_exactly_once(&worker.hooks, &worker.backend)
}

fn trigger_strategy() -> impl Strategy<Value = Trigger> {
    prop_oneof![
        Just(Trigger::CloseRequest),
        Just(Trigger::IdleExpiry),
        Just(Trigger::Sigint),
        Just(Trigger::Sigterm),
    ]
}

fn complete_trigger_sequence(mut generated: Vec<Trigger>, order_seed: usize) -> Vec<Trigger> {
    generated.extend([
        Trigger::CloseRequest,
        Trigger::IdleExpiry,
        Trigger::Sigint,
        Trigger::Sigterm,
    ]);
    let rotation = order_seed % generated.len();
    generated.rotate_left(rotation);
    generated.push(generated[0]);
    generated
}

fn opaque_test_epoch() -> Instant {
    use std::sync::OnceLock;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: CASES,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 17: daemon 关闭幂等
    // **Validates: Requirements 7.11**
    #[test]
    fn property_17_daemon_shutdown_is_idempotent(
        generated in prop::collection::vec(trigger_strategy(), 1..=10),
        order_seed in any::<usize>(),
        backend_fails in any::<bool>(),
        socket_fails in any::<bool>(),
        pid_fails in any::<bool>(),
        lock_fails in any::<bool>(),
        epoch_offset_ms in 0_u64..=100_000,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("property runtime");
        let triggers = complete_trigger_sequence(generated, order_seed);
        let failures = FailurePlan {
            backend: backend_fails,
            socket: socket_fails,
            pid: pid_fails,
            lock: lock_fails,
        };
        let start = opaque_test_epoch()
            .checked_add(Duration::from_millis(epoch_offset_ms))
            .expect("bounded fake-clock epoch");

        runtime.block_on(async {
            timeout(SCENARIO_TIMEOUT, async {
                run_serial_sequence(start, &triggers, failures).await?;
                run_competing_sequence(
                    start
                        .checked_add(Duration::from_secs(1_000))
                        .expect("bounded competing epoch"),
                    &triggers,
                )
                .await
            })
            .await
            .map_err(|_| TestCaseError::fail("property scenario exceeded bounded timeout"))?
        })?;
    }
}
