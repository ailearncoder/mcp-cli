#![cfg(unix)]
//! Fail-closed Unix socket adapter for daemon IPC.
//!
//! One mutex owns the complete request/response exchange, so a socket never
//! has more than one in-flight request. Once an exchange starts writing, every
//! transport, timeout, cancellation, framing, decoding, or correlation failure
//! shuts down and drops the stream before the error is returned. This is the
//! hand-off boundary that lets the connection manager safely choose direct
//! fallback without leaving an IPC request running on this client stream.

use std::{
    fs, io,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::{Mutex, MutexGuard},
};

use crate::{
    connection::{ConnectionError, McpConnection},
    daemon::{
        FrameError, IpcErrorCode, IpcOperation, IpcOutcome, IpcRequest, IpcResponse, NdjsonCodec,
        encode_message,
    },
    domain::{ConnectionMode, JsonObject, ToolInfo, ToolResult},
    policy::retry::ErrorClass,
    runtime::{Clock, CommandContext, SystemClock},
};

/// Per-stage upper bound for connecting, pinging, and every daemon request.
pub const DAEMON_IPC_CAP: Duration = Duration::from_secs(5);
const IPC_CLOSE_ACK_CAP: Duration = Duration::from_secs(1);

static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

/// A transport-independent MCP connection backed by one private Unix stream.
pub struct DaemonClient {
    state: Mutex<ClientState>,
    instructions: Option<String>,
    clock: std::sync::Arc<dyn Clock>,
}

struct ClientState {
    stream: Option<UnixStream>,
    codec: NdjsonCodec,
    client_id: u64,
    next_request_id: u64,
}

impl DaemonClient {
    /// Connects, verifies ping correlation, and caches daemon instructions.
    /// Each of those three stages receives its own `min(5s, remaining)` budget
    /// while still sharing the command's absolute deadline.
    pub async fn connect(
        context: &CommandContext,
        socket_path: impl AsRef<Path>,
    ) -> Result<Self, ConnectionError> {
        Self::connect_with_clock(
            context,
            socket_path.as_ref(),
            std::sync::Arc::new(SystemClock),
        )
        .await
    }

    async fn connect_with_clock(
        context: &CommandContext,
        socket_path: &Path,
        clock: std::sync::Arc<dyn Clock>,
    ) -> Result<Self, ConnectionError> {
        validate_socket(socket_path)?;
        let deadline = local_deadline(context, &*clock, "connecting to daemon")?;
        let stream =
            match await_stage(context, &*clock, deadline, UnixStream::connect(socket_path)).await {
                StageResult::Completed(Ok(stream)) => stream,
                StageResult::Completed(Err(source)) => {
                    return Err(operational_io("daemon IPC connection failed", source));
                }
                StageResult::TimedOut => {
                    return Err(ConnectionError::timed_out(
                        "daemon IPC connection timed out",
                    ));
                }
                StageResult::Cancelled => {
                    return Err(ConnectionError::cancelled(
                        "daemon IPC connection was cancelled",
                    ));
                }
            };

        let client_id = allocate_client_id()?;
        let mut client = Self {
            state: Mutex::new(ClientState {
                stream: Some(stream),
                codec: NdjsonCodec::new(),
                client_id,
                next_request_id: 1,
            }),
            instructions: None,
            clock,
        };
        client.ping(context).await?;
        client.instructions = client.fetch_instructions(context).await?;
        Ok(client)
    }

    /// Checks that the worker responds with a correlated canonical `pong`.
    pub async fn ping(&self, context: &CommandContext) -> Result<(), ConnectionError> {
        let data = self.request(context, IpcOperation::Ping).await?;
        if data == Value::String("pong".to_owned()) {
            Ok(())
        } else {
            self.abort_current_stream().await;
            Err(operational_protocol("daemon ping response was invalid"))
        }
    }

    /// Requests a worker-wide graceful shutdown. This is used only after the
    /// manager has validated metadata and detected a stale configuration hash;
    /// normal `McpConnection::close` still closes only this IPC client.
    pub async fn shutdown_worker(&self, context: &CommandContext) -> Result<(), ConnectionError> {
        let data = self.request(context, IpcOperation::Close).await?;
        if data == Value::String("closing".to_owned()) {
            self.abort_current_stream().await;
            Ok(())
        } else {
            self.abort_current_stream().await;
            Err(operational_protocol("daemon close response was invalid"))
        }
    }

    async fn fetch_instructions(
        &self,
        context: &CommandContext,
    ) -> Result<Option<String>, ConnectionError> {
        let data = self.request(context, IpcOperation::GetInstructions).await?;
        match data {
            Value::Null => Ok(None),
            Value::String(instructions) => Ok(Some(instructions)),
            _ => {
                self.abort_current_stream().await;
                Err(operational_protocol(
                    "daemon instructions response was invalid",
                ))
            }
        }
    }

    async fn request(
        &self,
        context: &CommandContext,
        operation: IpcOperation,
    ) -> Result<Value, ConnectionError> {
        let deadline = local_deadline(context, &*self.clock, "waiting for daemon response")?;
        let mut state = match acquire_state(context, &*self.clock, deadline, &self.state).await {
            LockResult::Acquired(state) => state,
            LockResult::TimedOut => {
                return Err(ConnectionError::timed_out("daemon IPC request timed out"));
            }
            LockResult::Cancelled => {
                return Err(ConnectionError::cancelled(
                    "daemon IPC request was cancelled",
                ));
            }
        };

        let id = state.next_id()?;
        let request = IpcRequest::new(id.clone(), operation)
            .map_err(|source| operational_frame("daemon IPC request was invalid", source))?;
        let frame = match encode_message(&request) {
            Ok(frame) => frame,
            Err(source) => {
                abort_state(&mut state).await;
                return Err(operational_frame(
                    "daemon IPC request framing failed",
                    source,
                ));
            }
        };

        let exchange = {
            let ClientState { stream, codec, .. } = &mut *state;
            let Some(stream) = stream.as_mut() else {
                return Err(operational_protocol("daemon IPC stream is closed"));
            };
            exchange(context, &*self.clock, deadline, stream, codec, &frame).await
        };

        let response = match exchange {
            Ok(response) => response,
            Err(failure) => {
                abort_state(&mut state).await;
                return Err(failure.into_connection_error());
            }
        };
        if response.id() != id {
            abort_state(&mut state).await;
            return Err(operational_protocol(
                "daemon IPC response correlation failed",
            ));
        }

        match response.into_parts().1 {
            IpcOutcome::Success(data) => Ok(data),
            IpcOutcome::Failure(error) => Err(response_error(error.code())),
        }
    }

    async fn decode_domain<T>(
        &self,
        context: &CommandContext,
        operation: IpcOperation,
        message: &'static str,
    ) -> Result<T, ConnectionError>
    where
        T: DeserializeOwned,
    {
        let value = self.request(context, operation).await?;
        match serde_json::from_value(value) {
            Ok(value) => Ok(value),
            Err(_) => {
                self.abort_current_stream().await;
                Err(operational_protocol(message))
            }
        }
    }

    async fn abort_current_stream(&self) {
        let mut state = self.state.lock().await;
        abort_state(&mut state).await;
    }
}

impl ClientState {
    fn next_id(&mut self) -> Result<String, ConnectionError> {
        let request_id = self.next_request_id;
        self.next_request_id = request_id.checked_add(1).ok_or_else(|| {
            operational_protocol("daemon IPC request identifier space was exhausted")
        })?;
        // ASCII only, no controls, 37 bytes at the maximum values.
        Ok(format!("dc-{:016x}-{:016x}", self.client_id, request_id))
    }
}

impl McpConnection for DaemonClient {
    fn list_tools<'a>(
        &'a self,
        context: &'a CommandContext,
    ) -> crate::runtime::BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
        Box::pin(async move {
            self.decode_domain(
                context,
                IpcOperation::ListTools,
                "daemon tools response was invalid",
            )
            .await
        })
    }

    fn call_tool<'a>(
        &'a self,
        context: &'a CommandContext,
        name: &'a str,
        args: JsonObject,
    ) -> crate::runtime::BoxFuture<'a, Result<ToolResult, ConnectionError>> {
        Box::pin(async move {
            self.request(
                context,
                IpcOperation::CallTool {
                    tool_name: name.to_owned(),
                    args,
                },
            )
            .await
        })
    }

    fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    fn close<'a>(
        self: Box<Self>,
        _context: &'a CommandContext,
    ) -> crate::runtime::BoxFuture<'a, Result<(), ConnectionError>> {
        Box::pin(async move {
            let mut state = self.state.into_inner();
            abort_state(&mut state).await;
            Ok(())
        })
    }

    fn mode(&self) -> ConnectionMode {
        ConnectionMode::Daemon
    }
}

async fn exchange(
    context: &CommandContext,
    clock: &dyn Clock,
    deadline: Instant,
    stream: &mut UnixStream,
    codec: &mut NdjsonCodec,
    frame: &[u8],
) -> Result<IpcResponse, ExchangeFailure> {
    // `write_all` can have sent an arbitrary prefix before returning, so every
    // branch from this point is treated as post-send and closes the stream.
    match await_stage(context, clock, deadline, stream.write_all(frame)).await {
        StageResult::Completed(Ok(())) => {}
        StageResult::Completed(Err(source)) => return Err(ExchangeFailure::Io(source)),
        StageResult::TimedOut => return Err(ExchangeFailure::Timeout),
        StageResult::Cancelled => return Err(ExchangeFailure::Cancelled),
    }

    let mut input = [0_u8; 8192];
    loop {
        let read = match await_stage(context, clock, deadline, stream.read(&mut input)).await {
            StageResult::Completed(Ok(read)) => read,
            StageResult::Completed(Err(source)) => return Err(ExchangeFailure::Io(source)),
            StageResult::TimedOut => return Err(ExchangeFailure::Timeout),
            StageResult::Cancelled => return Err(ExchangeFailure::Cancelled),
        };
        if read == 0 {
            let framing = codec.finish().err();
            return Err(framing.map_or(ExchangeFailure::Eof, ExchangeFailure::Frame));
        }

        let responses = codec
            .push_messages::<IpcResponse>(&input[..read])
            .map_err(ExchangeFailure::Frame)?;
        if responses.is_empty() {
            continue;
        }
        if responses.len() != 1 || codec.buffered_len() != 0 {
            return Err(ExchangeFailure::Protocol);
        }
        return Ok(responses.into_iter().next().expect("one response checked"));
    }
}

async fn abort_state(state: &mut ClientState) {
    state.codec = NdjsonCodec::new();
    if let Some(mut stream) = state.stream.take() {
        // Half-close requests cancellation at the worker. The worker monitors
        // its read half while awaiting the backend and drops that future before
        // closing its write half. Draining to peer EOF is therefore the normal
        // cancellation acknowledgement; the grace cap prevents an untrusted
        // or pre-upgrade peer from blocking command cleanup indefinitely.
        let _ = stream.shutdown().await;
        let drain = async {
            let mut input = [0_u8; 8192];
            loop {
                match stream.read(&mut input).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        };
        let _ = tokio::time::timeout(IPC_CLOSE_ACK_CAP, drain).await;
        drop(stream);
    }
}

fn validate_socket(path: &Path) -> Result<(), ConnectionError> {
    let parent = path
        .parent()
        .ok_or_else(|| security_error("daemon socket has no parent directory"))?;
    let parent_metadata = fs::symlink_metadata(parent)
        .map_err(|source| operational_io("daemon socket parent is unavailable", source))?;
    if parent_metadata.file_type().is_symlink()
        || !parent_metadata.is_dir()
        || parent_metadata.uid() != rustix::process::getuid().as_raw()
    {
        return Err(security_error("daemon socket parent is unsafe"));
    }

    let metadata = fs::symlink_metadata(path)
        .map_err(|source| operational_io("daemon socket is unavailable", source))?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_socket()
        || metadata.uid() != rustix::process::getuid().as_raw()
    {
        return Err(security_error("daemon socket is unsafe"));
    }
    Ok(())
}

fn allocate_client_id() -> Result<u64, ConnectionError> {
    NEXT_CLIENT_ID
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
            current.checked_add(1)
        })
        .map_err(|_| operational_protocol("daemon IPC client identifier space was exhausted"))
}

fn local_deadline(
    context: &CommandContext,
    clock: &dyn Clock,
    operation: &'static str,
) -> Result<Instant, ConnectionError> {
    if context.is_cancelled() {
        return Err(ConnectionError::cancelled(format!(
            "{operation} was cancelled"
        )));
    }
    let budget = context.remaining_capped(clock, DAEMON_IPC_CAP);
    if budget.is_zero() {
        return Err(ConnectionError::timed_out(format!("{operation} timed out")));
    }
    Ok(context.deadline.local_deadline(clock, DAEMON_IPC_CAP))
}

enum StageResult<T> {
    Completed(T),
    TimedOut,
    Cancelled,
}

async fn await_stage<T>(
    context: &CommandContext,
    clock: &dyn Clock,
    deadline: Instant,
    future: impl std::future::Future<Output = T>,
) -> StageResult<T> {
    tokio::select! {
        biased;
        _ = wait_for_cancellation(context) => StageResult::Cancelled,
        _ = clock.sleep_until(deadline) => StageResult::TimedOut,
        result = future => StageResult::Completed(result),
    }
}

async fn wait_for_cancellation(context: &CommandContext) {
    loop {
        if context.is_cancelled() {
            return;
        }
        // CancellationToken is intentionally object-safe and polling-only.
        // A short bounded poll makes cancellation observable during blocked OS
        // I/O without coupling this adapter to a concrete token type.
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

enum LockResult<'a> {
    Acquired(MutexGuard<'a, ClientState>),
    TimedOut,
    Cancelled,
}

async fn acquire_state<'a>(
    context: &CommandContext,
    clock: &dyn Clock,
    deadline: Instant,
    state: &'a Mutex<ClientState>,
) -> LockResult<'a> {
    match await_stage(context, clock, deadline, state.lock()).await {
        StageResult::Completed(state) => LockResult::Acquired(state),
        StageResult::TimedOut => LockResult::TimedOut,
        StageResult::Cancelled => LockResult::Cancelled,
    }
}

enum ExchangeFailure {
    Io(io::Error),
    Timeout,
    Cancelled,
    Eof,
    Frame(FrameError),
    Protocol,
}

impl ExchangeFailure {
    fn into_connection_error(self) -> ConnectionError {
        match self {
            Self::Io(source) => operational_io("daemon IPC transport failed", source),
            Self::Timeout => ConnectionError::timed_out("daemon IPC request timed out"),
            Self::Cancelled => ConnectionError::cancelled("daemon IPC request was cancelled"),
            Self::Eof => operational_protocol("daemon IPC stream ended before a response"),
            Self::Frame(source) => operational_frame("daemon IPC response framing failed", source),
            Self::Protocol => operational_protocol("daemon IPC response sequence was invalid"),
        }
    }
}

fn response_error(code: IpcErrorCode) -> ConnectionError {
    let class = match code {
        IpcErrorCode::NotConnected => ErrorClass::Transient,
        IpcErrorCode::ExecutionError => ErrorClass::Business,
        IpcErrorCode::InvalidJson
        | IpcErrorCode::MissingId
        | IpcErrorCode::UnknownType
        | IpcErrorCode::InvalidArguments
        | IpcErrorCode::FrameTooLarge
        | IpcErrorCode::InvalidUtf8
        | IpcErrorCode::TruncatedFrame
        | IpcErrorCode::Internal => ErrorClass::NonTransient,
    };
    ConnectionError::new(code.message()).with_class(class)
}

fn operational_io(message: &'static str, source: io::Error) -> ConnectionError {
    ConnectionError::with_source(message, source).with_class(ErrorClass::Transient)
}

fn operational_frame(
    message: &'static str,
    source: impl std::error::Error + Send + Sync + 'static,
) -> ConnectionError {
    ConnectionError::with_source(message, source).with_class(ErrorClass::Transient)
}

fn operational_protocol(message: &'static str) -> ConnectionError {
    ConnectionError::new(message).with_class(ErrorClass::Transient)
}

fn security_error(message: &'static str) -> ConnectionError {
    ConnectionError::new(message).with_class(ErrorClass::NonTransient)
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{
            Arc, Mutex as StdMutex,
            atomic::{AtomicBool, Ordering},
        },
    };

    use serde_json::json;
    use tempfile::TempDir;
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
        sync::{Notify, watch},
        time::timeout,
    };

    use super::*;
    use crate::{
        daemon::{IPC_MAX_FRAME_SIZE, IpcResponse, decode_message},
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

    #[derive(Clone)]
    struct TestClock {
        now: Arc<watch::Sender<Instant>>,
    }

    impl TestClock {
        fn new(start: Instant) -> Self {
            let (now, _) = watch::channel(start);
            Self { now: Arc::new(now) }
        }

        fn advance(&self, duration: Duration) {
            let next = (*self.now.borrow()) + duration;
            self.now.send_replace(next);
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

    struct Loopback {
        _directory: TempDir,
        path: PathBuf,
        listener: UnixListener,
    }

    impl Loopback {
        fn new() -> Self {
            let directory = tempfile::tempdir().expect("loopback directory");
            let path = directory.path().join("daemon.sock");
            let listener = UnixListener::bind(&path).expect("bind loopback");
            Self {
                _directory: directory,
                path,
                listener,
            }
        }
    }

    fn context_at(start: Instant, duration: Duration) -> (CommandContext, Arc<CancellationFlag>) {
        let cancellation = Arc::new(CancellationFlag::default());
        (
            CommandContext {
                deadline: Deadline::new(start + duration),
                cancellation: cancellation.clone(),
                diagnostics: Arc::new(NullDiagnostics),
            },
            cancellation,
        )
    }

    async fn read_request(reader: &mut BufReader<UnixStream>) -> IpcRequest {
        let mut frame = Vec::new();
        reader
            .read_until(b'\n', &mut frame)
            .await
            .expect("read request");
        assert_eq!(frame.pop(), Some(b'\n'));
        decode_message(&frame).expect("decode request")
    }

    async fn write_success(stream: &mut UnixStream, id: &str, data: Value) {
        let response = IpcResponse::success(id, data).expect("response");
        stream
            .write_all(&encode_message(&response).expect("encode response"))
            .await
            .expect("write response");
    }

    async fn complete_handshake(reader: &mut BufReader<UnixStream>) {
        let ping = read_request(reader).await;
        assert_eq!(ping.operation(), &IpcOperation::Ping);
        write_success(reader.get_mut(), ping.id(), json!("pong")).await;
        let instructions = read_request(reader).await;
        assert_eq!(instructions.operation(), &IpcOperation::GetInstructions);
        write_success(
            reader.get_mut(),
            instructions.id(),
            json!("loopback instructions"),
        )
        .await;
    }

    async fn spawn_connected(
        loopback: &Loopback,
        context: &CommandContext,
        clock: Arc<dyn Clock>,
    ) -> (DaemonClient, BufReader<UnixStream>) {
        let connect = DaemonClient::connect_with_clock(context, &loopback.path, clock);
        let accept = async {
            let (stream, _) = loopback.listener.accept().await.expect("accept");
            let mut reader = BufReader::new(stream);
            complete_handshake(&mut reader).await;
            reader
        };
        let (client, reader) = tokio::join!(connect, accept);
        (client.expect("connect client"), reader)
    }

    #[tokio::test]
    async fn maps_domain_values_uses_unique_valid_ids_and_serializes_requests() {
        let start = Instant::now();
        let clock = Arc::new(TestClock::new(start));
        let (context, _) = context_at(start, Duration::from_secs(30));
        let loopback = Loopback::new();
        let (client, mut server) = timeout(
            Duration::from_secs(2),
            spawn_connected(&loopback, &context, clock),
        )
        .await
        .expect("handshake timed out");
        assert_eq!(client.instructions(), Some("loopback instructions"));
        assert_eq!(client.mode(), ConnectionMode::Daemon);

        let first = client.list_tools(&context);
        tokio::pin!(first);
        let first_request = timeout(Duration::from_secs(2), async {
            tokio::select! {
                request = read_request(&mut server) => request,
                result = &mut first => panic!("first completed before response: {result:?}"),
            }
        })
        .await
        .expect("first request was not sent");
        assert_eq!(first_request.operation(), &IpcOperation::ListTools);
        assert!(crate::daemon::validate_request_id(first_request.id()).is_ok());

        let second = client.call_tool(&context, "echo", JsonObject::new());
        tokio::pin!(second);
        let mut unexpected = Vec::new();
        assert!(
            timeout(Duration::from_millis(25), async {
                tokio::select! {
                    read = server.read_until(b'\n', &mut unexpected) => {
                        panic!("second request was sent before first response: {read:?}")
                    }
                    result = &mut second => {
                        panic!("second completed before first response: {result:?}")
                    }
                }
            })
            .await
            .is_err(),
            "second exchange must wait for the first exchange mutex"
        );
        assert!(unexpected.is_empty());

        write_success(
            server.get_mut(),
            first_request.id(),
            json!([{"name":"echo","description":null,"input_schema":{"type":"object"}}]),
        )
        .await;
        let tools = timeout(Duration::from_secs(2), &mut first)
            .await
            .expect("first response timed out")
            .expect("tools");
        assert_eq!(tools[0].name, "echo");

        let second_request = timeout(Duration::from_secs(2), async {
            tokio::select! {
                request = read_request(&mut server) => request,
                result = &mut second => panic!("second completed before response: {result:?}"),
            }
        })
        .await
        .expect("second request was not sent");
        assert!(matches!(
            second_request.operation(),
            IpcOperation::CallTool { tool_name, .. } if tool_name == "echo"
        ));
        assert_ne!(first_request.id(), second_request.id());
        write_success(
            server.get_mut(),
            second_request.id(),
            json!({"content":[{"type":"text","text":"ok"}]}),
        )
        .await;
        assert_eq!(
            timeout(Duration::from_secs(2), &mut second)
                .await
                .expect("second response timed out")
                .expect("call result"),
            json!({"content":[{"type":"text","text":"ok"}]})
        );
    }

    #[tokio::test]
    async fn canonical_daemon_failure_is_mapped_without_leaking_payload() {
        let start = Instant::now();
        let clock = Arc::new(TestClock::new(start));
        let (context, _) = context_at(start, Duration::from_secs(30));
        let loopback = Loopback::new();
        let (client, mut server) = spawn_connected(&loopback, &context, clock).await;

        let request = client.call_tool(&context, "fails", JsonObject::new());
        let response = async {
            let request = read_request(&mut server).await;
            let failure =
                IpcResponse::failure(request.id(), IpcErrorCode::ExecutionError).expect("failure");
            server
                .get_mut()
                .write_all(&encode_message(&failure).expect("encode"))
                .await
                .expect("write");
        };
        let (result, ()) = tokio::join!(request, response);
        let error = result.expect_err("canonical failure");
        assert_eq!(error.message(), IpcErrorCode::ExecutionError.message());
        assert_eq!(error.error_class(), ErrorClass::Business);
        assert!(!format!("{error:?}").contains("fails"));
    }

    #[tokio::test]
    async fn wrong_id_half_frame_and_oversized_response_close_before_returning() {
        enum BadResponse {
            WrongId,
            HalfFrame,
            Oversized,
        }
        for bad in [
            BadResponse::WrongId,
            BadResponse::HalfFrame,
            BadResponse::Oversized,
        ] {
            let start = Instant::now();
            let clock = Arc::new(TestClock::new(start));
            let (context, _) = context_at(start, Duration::from_secs(30));
            let loopback = Loopback::new();
            let (client, mut server) = spawn_connected(&loopback, &context, clock).await;
            let peer_closed = Arc::new(AtomicBool::new(false));
            let observed = peer_closed.clone();

            let request = client.list_tools(&context);
            let server_side = async move {
                let request = read_request(&mut server).await;
                match bad {
                    BadResponse::WrongId => {
                        write_success(server.get_mut(), "different-id", json!([])).await;
                    }
                    BadResponse::HalfFrame => {
                        server
                            .get_mut()
                            .write_all(
                                format!("{{\"id\":\"{}\",\"success\":true", request.id())
                                    .as_bytes(),
                            )
                            .await
                            .expect("partial response");
                        server.get_mut().shutdown().await.expect("half close");
                    }
                    BadResponse::Oversized => {
                        let oversized = vec![b'x'; IPC_MAX_FRAME_SIZE + 1];
                        let _ = server.get_mut().write_all(&oversized).await;
                    }
                }
                let mut byte = [0_u8; 1];
                let read = timeout(Duration::from_secs(2), server.get_mut().read(&mut byte))
                    .await
                    .expect("client did not close")
                    .expect("read client close");
                observed.store(read == 0, Ordering::SeqCst);
            };
            let (result, ()) = tokio::join!(request, server_side);
            assert!(result.is_err());
            assert!(peer_closed.load(Ordering::SeqCst));
        }
    }

    #[tokio::test]
    async fn five_second_and_shorter_remaining_budgets_close_post_send_request() {
        for remaining in [Duration::from_secs(30), Duration::from_secs(2)] {
            let start = Instant::now();
            let clock = Arc::new(TestClock::new(start));
            let (context, _) = context_at(start, remaining);
            let loopback = Loopback::new();
            let (client, mut server) = spawn_connected(&loopback, &context, clock.clone()).await;
            let received = Arc::new(Notify::new());
            let signal = received.clone();
            let peer_closed = Arc::new(AtomicBool::new(false));
            let observed = peer_closed.clone();

            let request = client.list_tools(&context);
            let server_side = async move {
                let _ = read_request(&mut server).await;
                signal.notify_one();
                let mut byte = [0_u8; 1];
                let read = server.get_mut().read(&mut byte).await.expect("read EOF");
                observed.store(read == 0, Ordering::SeqCst);
            };
            tokio::pin!(request);
            tokio::pin!(server_side);
            tokio::select! {
                _ = received.notified() => {}
                result = &mut request => panic!("request completed too early: {result:?}"),
                _ = &mut server_side => panic!("server observed close before timeout"),
            }
            clock.advance(remaining.min(DAEMON_IPC_CAP));
            let result = request.await.expect_err("request timeout");
            server_side.await;
            assert!(result.is_timeout());
            assert!(peer_closed.load(Ordering::SeqCst));
        }
    }

    #[tokio::test]
    async fn cancellation_after_send_closes_stream_before_error_returns() {
        let start = Instant::now();
        let clock = Arc::new(TestClock::new(start));
        let (context, cancellation) = context_at(start, Duration::from_secs(30));
        let loopback = Loopback::new();
        let (client, mut server) = spawn_connected(&loopback, &context, clock).await;
        let received = Arc::new(Notify::new());
        let signal = received.clone();
        let closed = Arc::new(AtomicBool::new(false));
        let observed = closed.clone();

        let request = client.list_tools(&context);
        let server_side = async move {
            let _ = read_request(&mut server).await;
            signal.notify_one();
            let mut byte = [0_u8; 1];
            let read = server.get_mut().read(&mut byte).await.expect("read EOF");
            observed.store(read == 0, Ordering::SeqCst);
        };
        tokio::pin!(request);
        tokio::pin!(server_side);
        tokio::select! {
            _ = received.notified() => {}
            result = &mut request => panic!("request completed too early: {result:?}"),
            _ = &mut server_side => panic!("server closed too early"),
        }
        cancellation.cancel();
        let error = request.await.expect_err("cancelled request");
        server_side.await;
        assert!(error.is_cancelled());
        assert!(closed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn expired_connect_does_not_open_socket() {
        let start = Instant::now();
        let clock = Arc::new(TestClock::new(start));
        let (context, _) = context_at(start, Duration::ZERO);
        let loopback = Loopback::new();

        let error = match DaemonClient::connect_with_clock(&context, &loopback.path, clock).await {
            Ok(_) => panic!("expired connect unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.is_timeout());
        assert!(
            timeout(Duration::from_millis(25), loopback.listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn close_drops_only_current_stream_and_never_sends_worker_close() {
        let start = Instant::now();
        let clock = Arc::new(TestClock::new(start));
        let (context, _) = context_at(start, Duration::from_secs(30));
        let loopback = Loopback::new();
        let operations = Arc::new(StdMutex::new(Vec::new()));
        let recorded = operations.clone();

        let server = async {
            let (first, _) = loopback.listener.accept().await.expect("first accept");
            let mut first = BufReader::new(first);
            for _ in 0..2 {
                let request = read_request(&mut first).await;
                recorded
                    .lock()
                    .expect("operations")
                    .push(request.operation().clone());
                let data = if matches!(request.operation(), IpcOperation::Ping) {
                    json!("pong")
                } else {
                    Value::Null
                };
                write_success(first.get_mut(), request.id(), data).await;
            }
            let mut trailing = Vec::new();
            first
                .read_to_end(&mut trailing)
                .await
                .expect("first client EOF");
            assert!(trailing.is_empty(), "close operation must not be sent");

            // A fresh client can still connect, proving the listener/worker
            // was not asked to terminate by McpConnection::close.
            let (second, _) = loopback.listener.accept().await.expect("second accept");
            let mut second = BufReader::new(second);
            complete_handshake(&mut second).await;
        };
        let clients = async {
            let first = DaemonClient::connect_with_clock(&context, &loopback.path, clock.clone())
                .await
                .expect("first client");
            Box::new(first).close(&context).await.expect("client close");
            let second = DaemonClient::connect_with_clock(&context, &loopback.path, clock)
                .await
                .expect("second client");
            Box::new(second)
                .close(&context)
                .await
                .expect("second close");
        };
        let ((), ()) = tokio::join!(server, clients);
        assert!(
            operations
                .lock()
                .expect("operations")
                .iter()
                .all(|operation| !matches!(operation, IpcOperation::Close))
        );
    }
}
