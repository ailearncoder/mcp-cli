#![cfg(unix)]
#![forbid(unsafe_code)]

#[path = "../support/mod.rs"]
mod support;

use std::{
    collections::{BTreeMap, VecDeque},
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
    daemon::{
        IpcOutcome, IpcResponse,
        worker::{WorkerError, WorkerIpcService, WorkerStop},
    },
};
use proptest::{prelude::*, test_runner::TestCaseError};
use serde_json::json;
use support::{FakeClock, TestCancellationToken};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{
        Notify,
        mpsc::{UnboundedReceiver, error::TryRecvError},
    },
    task::JoinHandle,
    time::timeout,
};

const CASES: u32 = 128;
const IO_TIMEOUT: Duration = Duration::from_secs(1);
const SCENARIO_TIMEOUT: Duration = Duration::from_secs(5);
const PARTIAL_NOISE_BYTES: usize = 9_000;

#[derive(Default)]
struct SilentDiagnostics;

impl DiagnosticSink for SilentDiagnostics {
    fn warning(&self, _message: &str) {}
    fn debug(&self, _message: &str) {}
    fn server_stderr(&self, _server: &str, _bytes: &[u8]) {}
}

#[derive(Default)]
struct BackendState {
    list_failures: Mutex<VecDeque<bool>>,
    block_next_list: AtomicBool,
    list_started: Notify,
    release_list: Notify,
    close_count: AtomicUsize,
}

struct PropertyConnection {
    state: Arc<BackendState>,
}

impl McpConnection for PropertyConnection {
    fn list_tools<'a>(
        &'a self,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
        let should_block = self.state.block_next_list.swap(false, Ordering::SeqCst);
        let should_fail = self
            .state
            .list_failures
            .lock()
            .expect("list failure queue lock")
            .pop_front()
            .unwrap_or(false);
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            if should_block {
                state.list_started.notify_one();
                state.release_list.notified().await;
            }
            if should_fail {
                Err(ConnectionError::new("scripted list failure"))
            } else {
                Ok(vec![ToolInfo {
                    name: "property-tool".to_owned(),
                    description: None,
                    input_schema: json!({"type":"object"}),
                }])
            }
        })
    }

    fn call_tool<'a>(
        &'a self,
        _ctx: &'a CommandContext,
        name: &'a str,
        args: JsonObject,
    ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
        Box::pin(async move {
            if name == "fail-call" {
                Err(ConnectionError::new("scripted call failure"))
            } else {
                Ok(json!({"tool":name,"args":args}))
            }
        })
    }

    fn instructions(&self) -> Option<&str> {
        Some("property instructions")
    }

    fn close<'a>(
        self: Box<Self>,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<(), ConnectionError>> {
        self.state.close_count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn mode(&self) -> ConnectionMode {
        ConnectionMode::Daemon
    }
}

#[derive(Clone, Copy, Debug)]
enum InvalidKind {
    InvalidJson,
    MissingId,
    UnknownType,
    InvalidArguments,
    InvalidId,
}

#[derive(Clone, Copy, Debug)]
enum ValidKind {
    Ping,
    ListSuccess,
    ListFailure,
    CallSuccess,
    CallFailure,
    GetInstructions,
}

#[derive(Clone, Copy, Debug)]
enum EventKind {
    Connect(u8),
    Disconnect(u8),
    PartialNoise(u8),
    Invalid(u8, InvalidKind),
    Valid(u8, ValidKind),
    AdvanceOnly,
}

#[derive(Clone, Copy, Debug)]
struct Event {
    advance_seed: u16,
    kind: EventKind,
}

struct IdleModel {
    now: Instant,
    active_deadline: Instant,
    timeout: Duration,
    alive: bool,
}

impl IdleModel {
    fn new(start: Instant, timeout: Duration) -> Self {
        Self {
            now: start,
            active_deadline: start.checked_add(timeout).expect("bounded model deadline"),
            timeout,
            alive: true,
        }
    }

    fn advance(&mut self, duration: Duration) {
        self.now = self
            .now
            .checked_add(duration)
            .expect("bounded model advance");
        if self.now >= self.active_deadline {
            self.alive = false;
        }
    }

    fn complete_valid_request(&mut self) {
        assert!(self.alive);
        assert!(self.now < self.active_deadline);
        self.active_deadline = self
            .now
            .checked_add(self.timeout)
            .expect("bounded model deadline reset");
    }

    fn bounded_advance(&self, seed: u16) -> Duration {
        let remaining_ms = self.active_deadline.duration_since(self.now).as_millis() as u64;
        let timeout_third = (self.timeout.as_millis() as u64 / 3).max(1);
        let cap = remaining_ms.saturating_sub(1).min(timeout_third);
        Duration::from_millis(u64::from(seed) % (cap + 1))
    }
}

type Clients = BTreeMap<u8, BufReader<UnixStream>>;

struct RunningWorker {
    _directory: tempfile::TempDir,
    socket: PathBuf,
    state: Arc<BackendState>,
    deadlines: UnboundedReceiver<Instant>,
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

fn start_worker(clock: FakeClock, idle_timeout: Duration) -> Result<RunningWorker, TestCaseError> {
    let directory = tempfile::tempdir()
        .map_err(|error| TestCaseError::fail(format!("temporary directory failed: {error}")))?;
    let socket = directory.path().join("property-16.sock");
    let listener = UnixListener::bind(&socket)
        .map_err(|error| TestCaseError::fail(format!("Unix listener bind failed: {error}")))?;
    let state = Arc::new(BackendState::default());
    let connection = PropertyConnection {
        state: Arc::clone(&state),
    };
    let (service, deadlines) = WorkerIpcService::with_clock_and_idle_deadline_observer(
        Box::new(connection),
        context(clock.current()),
        idle_timeout,
        Arc::new(clock),
    );
    let task = tokio::spawn(service.serve(listener));
    Ok(RunningWorker {
        _directory: directory,
        socket,
        state,
        deadlines,
        task,
    })
}

async fn observed_deadline(
    deadlines: &mut UnboundedReceiver<Instant>,
) -> Result<Instant, TestCaseError> {
    timeout(IO_TIMEOUT, deadlines.recv())
        .await
        .map_err(|_| TestCaseError::fail("timed out waiting for idle deadline observation"))?
        .ok_or_else(|| TestCaseError::fail("idle deadline observer closed unexpectedly"))
}

fn assert_no_deadline_change(
    deadlines: &mut UnboundedReceiver<Instant>,
) -> Result<(), TestCaseError> {
    match deadlines.try_recv() {
        Err(TryRecvError::Empty) => Ok(()),
        Err(TryRecvError::Disconnected) => {
            Err(TestCaseError::fail("idle deadline observer disconnected"))
        }
        Ok(deadline) => Err(TestCaseError::fail(format!(
            "non-valid event changed idle deadline to {deadline:?}"
        ))),
    }
}

async fn connect_client(socket: &Path) -> Result<BufReader<UnixStream>, TestCaseError> {
    let stream = timeout(IO_TIMEOUT, UnixStream::connect(socket))
        .await
        .map_err(|_| TestCaseError::fail("timed out connecting local Unix client"))?
        .map_err(|error| TestCaseError::fail(format!("Unix client connect failed: {error}")))?;
    Ok(BufReader::new(stream))
}

async fn ensure_client<'a>(
    clients: &'a mut Clients,
    socket: &Path,
    client_id: u8,
) -> Result<&'a mut BufReader<UnixStream>, TestCaseError> {
    if let std::collections::btree_map::Entry::Vacant(entry) = clients.entry(client_id) {
        let client = connect_client(socket).await?;
        entry.insert(client);
    }
    Ok(clients
        .get_mut(&client_id)
        .expect("client was inserted or already present"))
}

async fn read_response(client: &mut BufReader<UnixStream>) -> Result<IpcResponse, TestCaseError> {
    let mut line = Vec::new();
    let read = timeout(IO_TIMEOUT, client.read_until(b'\n', &mut line))
        .await
        .map_err(|_| TestCaseError::fail("timed out waiting for local IPC response"))?
        .map_err(|error| TestCaseError::fail(format!("IPC response read failed: {error}")))?;
    if read == 0 {
        return Err(TestCaseError::fail("worker closed before IPC response"));
    }
    serde_json::from_slice(&line)
        .map_err(|error| TestCaseError::fail(format!("invalid IPC response JSON: {error}")))
}

fn assert_response_success(
    response: &IpcResponse,
    expected_success: bool,
) -> Result<(), TestCaseError> {
    let success = matches!(response.outcome(), IpcOutcome::Success(_));
    prop_assert_eq!(success, expected_success);
    Ok(())
}

async fn send_invalid(
    clients: &mut Clients,
    socket: &Path,
    client_id: u8,
    kind: InvalidKind,
    sequence: usize,
) -> Result<(), TestCaseError> {
    let id = format!("invalid-{sequence}");
    let frame = match kind {
        InvalidKind::InvalidJson => b"{not-json}\n".to_vec(),
        InvalidKind::MissingId => b"{\"type\":\"ping\"}\n".to_vec(),
        InvalidKind::UnknownType => {
            format!("{{\"id\":\"{id}\",\"type\":\"unknown\"}}\n").into_bytes()
        }
        InvalidKind::InvalidArguments => {
            format!("{{\"id\":\"{id}\",\"type\":\"callTool\",\"toolName\":\"x\",\"args\":[]}}\n")
                .into_bytes()
        }
        InvalidKind::InvalidId => b"{\"id\":\"\",\"type\":\"ping\"}\n".to_vec(),
    };
    let client = ensure_client(clients, socket, client_id).await?;
    client
        .get_mut()
        .write_all(&frame)
        .await
        .map_err(|error| TestCaseError::fail(format!("invalid frame write failed: {error}")))?;
    let response = read_response(client).await?;
    assert_response_success(&response, false)
}

async fn send_partial_noise(
    clients: &mut Clients,
    socket: &Path,
    client_id: u8,
) -> Result<(), TestCaseError> {
    let client = ensure_client(clients, socket, client_id).await?;
    client
        .get_mut()
        .write_all(&vec![b' '; PARTIAL_NOISE_BYTES])
        .await
        .map_err(|error| TestCaseError::fail(format!("partial noise write failed: {error}")))?;
    tokio::task::yield_now().await;
    client
        .get_mut()
        .write_all(b"\n")
        .await
        .map_err(|error| TestCaseError::fail(format!("noise terminator write failed: {error}")))?;
    let response = read_response(client).await?;
    assert_response_success(&response, false)
}

async fn send_valid(
    clients: &mut Clients,
    socket: &Path,
    state: &BackendState,
    client_id: u8,
    kind: ValidKind,
    sequence: usize,
) -> Result<(), TestCaseError> {
    let id = format!("valid-{sequence}");
    let (frame, expected_success) = match kind {
        ValidKind::Ping => (format!("{{\"id\":\"{id}\",\"type\":\"ping\"}}\n"), true),
        ValidKind::ListSuccess => {
            state
                .list_failures
                .lock()
                .expect("list failure queue lock")
                .push_back(false);
            (
                format!("{{\"id\":\"{id}\",\"type\":\"listTools\"}}\n"),
                true,
            )
        }
        ValidKind::ListFailure => {
            state
                .list_failures
                .lock()
                .expect("list failure queue lock")
                .push_back(true);
            (
                format!("{{\"id\":\"{id}\",\"type\":\"listTools\"}}\n"),
                false,
            )
        }
        ValidKind::CallSuccess => (
            format!(
                "{{\"id\":\"{id}\",\"type\":\"callTool\",\"toolName\":\"ok-call\",\"args\":{{\"n\":{sequence}}}}}\n"
            ),
            true,
        ),
        ValidKind::CallFailure => (
            format!(
                "{{\"id\":\"{id}\",\"type\":\"callTool\",\"toolName\":\"fail-call\",\"args\":{{}}}}\n"
            ),
            false,
        ),
        ValidKind::GetInstructions => (
            format!("{{\"id\":\"{id}\",\"type\":\"getInstructions\"}}\n"),
            true,
        ),
    };
    let client = ensure_client(clients, socket, client_id).await?;
    client
        .get_mut()
        .write_all(frame.as_bytes())
        .await
        .map_err(|error| TestCaseError::fail(format!("valid frame write failed: {error}")))?;
    let response = read_response(client).await?;
    assert_response_success(&response, expected_success)
}

async fn finish_idle(
    worker: RunningWorker,
    expected_close_count: usize,
) -> Result<(), TestCaseError> {
    let stopped = timeout(IO_TIMEOUT, worker.task)
        .await
        .map_err(|_| TestCaseError::fail("worker did not stop at fake-clock idle deadline"))?
        .map_err(|error| TestCaseError::fail(format!("worker task join failed: {error}")))?
        .map_err(|error| TestCaseError::fail(format!("worker failed: {error}")))?;
    prop_assert_eq!(stopped, WorkerStop::IdleTimeout);
    prop_assert_eq!(
        worker.state.close_count.load(Ordering::SeqCst),
        expected_close_count
    );
    Ok(())
}

fn mandatory_events() -> Vec<EventKind> {
    vec![
        EventKind::Connect(0),
        EventKind::Connect(1),
        EventKind::Connect(2),
        EventKind::PartialNoise(0),
        EventKind::Invalid(0, InvalidKind::InvalidJson),
        EventKind::Invalid(1, InvalidKind::MissingId),
        EventKind::Invalid(2, InvalidKind::UnknownType),
        EventKind::Invalid(0, InvalidKind::InvalidArguments),
        EventKind::Invalid(1, InvalidKind::InvalidId),
        EventKind::Valid(0, ValidKind::Ping),
        EventKind::Valid(1, ValidKind::ListSuccess),
        EventKind::Valid(2, ValidKind::ListFailure),
        EventKind::Valid(0, ValidKind::CallSuccess),
        EventKind::Valid(1, ValidKind::CallFailure),
        EventKind::Valid(2, ValidKind::GetInstructions),
        EventKind::Disconnect(1),
        EventKind::Connect(1),
        EventKind::AdvanceOnly,
    ]
}

async fn run_core_sequence(
    start: Instant,
    idle_timeout: Duration,
    generated: Vec<Event>,
    order_seed: usize,
) -> Result<(), TestCaseError> {
    let clock = FakeClock::new(start);
    let mut worker = start_worker(clock.clone(), idle_timeout)?;
    let mut model = IdleModel::new(start, idle_timeout);
    prop_assert_eq!(
        observed_deadline(&mut worker.deadlines).await?,
        model.active_deadline
    );

    let mut events = mandatory_events()
        .into_iter()
        .enumerate()
        .map(|(index, kind)| Event {
            advance_seed: (order_seed as u16).wrapping_add((index as u16).wrapping_mul(97)),
            kind,
        })
        .chain(generated)
        .collect::<Vec<_>>();
    let rotation = order_seed % events.len();
    events.rotate_left(rotation);

    let mut clients = Clients::new();
    for (sequence, event) in events.into_iter().enumerate() {
        let advance = model.bounded_advance(event.advance_seed);
        clock.advance(advance);
        model.advance(advance);
        prop_assert!(model.alive);

        match event.kind {
            EventKind::Connect(client_id) => {
                clients.remove(&client_id);
                clients.insert(client_id, connect_client(&worker.socket).await?);
                tokio::task::yield_now().await;
                assert_no_deadline_change(&mut worker.deadlines)?;
            }
            EventKind::Disconnect(client_id) => {
                clients.remove(&client_id);
                tokio::task::yield_now().await;
                assert_no_deadline_change(&mut worker.deadlines)?;
            }
            EventKind::PartialNoise(client_id) => {
                send_partial_noise(&mut clients, &worker.socket, client_id).await?;
                assert_no_deadline_change(&mut worker.deadlines)?;
            }
            EventKind::Invalid(client_id, kind) => {
                send_invalid(&mut clients, &worker.socket, client_id, kind, sequence).await?;
                assert_no_deadline_change(&mut worker.deadlines)?;
            }
            EventKind::Valid(client_id, kind) => {
                send_valid(
                    &mut clients,
                    &worker.socket,
                    &worker.state,
                    client_id,
                    kind,
                    sequence,
                )
                .await?;
                model.complete_valid_request();
                prop_assert_eq!(
                    observed_deadline(&mut worker.deadlines).await?,
                    model.active_deadline,
                    "production deadline diverged after {:?}",
                    kind
                );
            }
            EventKind::AdvanceOnly => {
                tokio::task::yield_now().await;
                assert_no_deadline_change(&mut worker.deadlines)?;
            }
        }
        prop_assert!(
            !worker.task.is_finished(),
            "worker stopped before modeled deadline"
        );
    }

    let final_advance = model.active_deadline.duration_since(model.now);
    clock.advance(final_advance);
    model.advance(final_advance);
    prop_assert!(!model.alive);
    drop(clients);
    assert_no_deadline_change(&mut worker.deadlines)?;
    finish_idle(worker, 1).await
}

async fn run_boundary_probe(
    start: Instant,
    idle_timeout: Duration,
    overshoot: Duration,
) -> Result<(), TestCaseError> {
    let clock = FakeClock::new(start);
    let mut worker = start_worker(clock.clone(), idle_timeout)?;
    let mut model = IdleModel::new(start, idle_timeout);
    prop_assert_eq!(
        observed_deadline(&mut worker.deadlines).await?,
        model.active_deadline
    );

    let mut client = connect_client(&worker.socket).await?;
    let before_deadline = model
        .active_deadline
        .duration_since(model.now)
        .checked_sub(Duration::from_millis(1))
        .expect("generated timeout is at least 20 ms");
    clock.advance(before_deadline);
    model.advance(before_deadline);
    prop_assert!(model.alive);

    worker.state.block_next_list.store(true, Ordering::SeqCst);
    client
        .get_mut()
        .write_all(b"{\"id\":\"boundary\",\"type\":\"listTools\"}\n")
        .await
        .map_err(|error| TestCaseError::fail(format!("boundary request write failed: {error}")))?;
    timeout(IO_TIMEOUT, worker.state.list_started.notified())
        .await
        .map_err(|_| TestCaseError::fail("boundary request did not start"))?;

    let to_boundary = model.active_deadline.duration_since(model.now);
    let advance = to_boundary
        .checked_add(overshoot)
        .expect("bounded boundary overshoot");
    clock.advance(advance);
    model.advance(advance);
    prop_assert!(!model.alive);
    worker.state.release_list.notify_one();

    finish_idle(worker, 1).await?;
    drop(client);
    Ok(())
}

fn invalid_kind() -> impl Strategy<Value = InvalidKind> {
    prop_oneof![
        Just(InvalidKind::InvalidJson),
        Just(InvalidKind::MissingId),
        Just(InvalidKind::UnknownType),
        Just(InvalidKind::InvalidArguments),
        Just(InvalidKind::InvalidId),
    ]
}

fn valid_kind() -> impl Strategy<Value = ValidKind> {
    prop_oneof![
        Just(ValidKind::Ping),
        Just(ValidKind::ListSuccess),
        Just(ValidKind::ListFailure),
        Just(ValidKind::CallSuccess),
        Just(ValidKind::CallFailure),
        Just(ValidKind::GetInstructions),
    ]
}

fn generated_event() -> impl Strategy<Value = Event> {
    (
        any::<u16>(),
        prop_oneof![
            (0_u8..4).prop_map(EventKind::Connect),
            (0_u8..4).prop_map(EventKind::Disconnect),
            (0_u8..4).prop_map(EventKind::PartialNoise),
            (0_u8..4, invalid_kind()).prop_map(|(client, kind)| EventKind::Invalid(client, kind)),
            (0_u8..4, valid_kind()).prop_map(|(client, kind)| EventKind::Valid(client, kind)),
            Just(EventKind::AdvanceOnly),
        ],
    )
        .prop_map(|(advance_seed, kind)| Event { advance_seed, kind })
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

    // Feature: mcp-cli, Property 16: 只有有效请求延长 daemon 生命周期
    // **Validates: Requirements 7.9**
    #[test]
    fn property_16_only_completed_valid_requests_extend_idle_deadline(
        idle_timeout_ms in 20_u64..=120,
        generated in prop::collection::vec(generated_event(), 0..=8),
        order_seed in any::<usize>(),
        crossed_overshoot_ms in 1_u64..=20,
        epoch_offset_ms in 0_u64..=10_000,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("property runtime");
        let start = opaque_test_epoch()
            .checked_add(Duration::from_millis(epoch_offset_ms))
            .expect("bounded fake-clock epoch offset");
        let idle_timeout = Duration::from_millis(idle_timeout_ms);

        runtime.block_on(async {
            timeout(SCENARIO_TIMEOUT, async {
                run_core_sequence(start, idle_timeout, generated, order_seed).await?;
                run_boundary_probe(
                    start.checked_add(Duration::from_secs(20_000)).expect("bounded exact probe"),
                    idle_timeout,
                    Duration::ZERO,
                )
                .await?;
                run_boundary_probe(
                    start.checked_add(Duration::from_secs(40_000)).expect("bounded crossing probe"),
                    idle_timeout,
                    Duration::from_millis(crossed_overshoot_ms),
                )
                .await
            })
            .await
            .map_err(|_| TestCaseError::fail("property scenario exceeded bounded timeout"))?
        })?;
    }
}
