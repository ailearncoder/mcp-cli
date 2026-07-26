#![cfg(unix)]
#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt, symlink},
    path::{Path, PathBuf},
    process::{Command as StdCommand, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use mcp_cli::{
    CancellationFlag, CommandContext, DiagnosticSink, ServerDefinition, SystemClock,
    ToolFilterConfig, TransportConfig,
    config::{config_hash, server_id},
    daemon::{
        DaemonPaths, IPC_MAX_FRAME_SIZE, IpcOperation, IpcRequest, MetadataStore,
        worker::{
            CurrentExecutableDaemonSpawner, DaemonReady, DaemonSpawnError, DaemonSpawner,
            WorkerStartupFault,
        },
    },
    runtime::Deadline,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::{sleep, timeout},
};

const IO_TIMEOUT: Duration = Duration::from_secs(3);
const PROCESS_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Default)]
struct NullDiagnostics;

impl DiagnosticSink for NullDiagnostics {
    fn warning(&self, _message: &str) {}
    fn debug(&self, _message: &str) {}
    fn server_stderr(&self, _server: &str, _bytes: &[u8]) {}
}

fn context() -> CommandContext {
    CommandContext {
        deadline: Deadline::after(&SystemClock, Duration::from_secs(20)),
        cancellation: Arc::new(CancellationFlag::default()),
        diagnostics: Arc::new(NullDiagnostics),
    }
}

struct Fixture {
    _root: TempDir,
    script: PathBuf,
    observation: PathBuf,
    server: ServerDefinition,
    paths: DaemonPaths,
}

impl Fixture {
    fn new(name: &str, list_delay_ms: u64) -> Self {
        // Keep the full hashed socket path within sockaddr_un::sun_path even
        // when the ambient TMPDIR is long (for example, in CI).
        let root = tempfile::Builder::new()
            .prefix("m")
            .rand_bytes(2)
            .tempdir_in("/tmp")
            .expect("isolated daemon fixture");
        let runtime_parent = root.path();
        let observation = root.path().join("backend-observation.json");
        let script = root.path().join("backend-script.json");
        fs::write(
            &script,
            serde_json::to_vec(&json!({
                "observation_path": observation,
                "instructions": format!("instructions for {name}"),
                "tool_pages": [{
                    "tools": [{
                        "name": "echo",
                        "description": "worker fixture",
                        "inputSchema": {"type": "object"}
                    }]
                }],
                "call_result": {"content": [{"type": "text", "text": "ok"}]},
                "list_response_delay_ms": list_delay_ms
            }))
            .expect("fixture script JSON"),
        )
        .expect("write fixture script");

        let command = Path::new(env!("CARGO_BIN_EXE_mock-stdio-server"));
        let raw = json!({
            "command": command,
            "args": ["--fixture-script", script],
            "fixtureIdentity": name
        });
        let server = ServerDefinition {
            name: name.to_owned(),
            id: server_id(name),
            config_hash: config_hash(&raw),
            transport: TransportConfig::Stdio {
                command: command.to_string_lossy().into_owned(),
                args: vec![
                    "--fixture-script".to_owned(),
                    script.to_string_lossy().into_owned(),
                ],
                env: BTreeMap::new(),
                cwd: None,
            },
            filter: ToolFilterConfig::default(),
        };
        let paths = DaemonPaths::from_runtime_parent(runtime_parent, &server.id)
            .expect("secure daemon paths");
        Self {
            _root: root,
            script,
            observation,
            server,
            paths,
        }
    }

    fn spawner(&self, idle_timeout: Duration) -> CurrentExecutableDaemonSpawner {
        CurrentExecutableDaemonSpawner::with_executable(
            Path::new(env!("CARGO_BIN_EXE_mcp-cli")).to_path_buf(),
            idle_timeout,
        )
    }

    async fn spawn(&self, idle_timeout: Duration) -> DaemonReady {
        match self
            .spawner(idle_timeout)
            .spawn(&context(), &self.server, &self.paths)
            .await
        {
            Ok(ready) => ready,
            Err(error) => panic!(
                "worker ready failed: {error:?}; runtime={:?}; observation={:?}",
                runtime_entries(&self.paths),
                fs::read_to_string(&self.observation).ok()
            ),
        }
    }
}

async fn read_response(reader: &mut BufReader<UnixStream>) -> Value {
    let mut line = Vec::new();
    let read = timeout(IO_TIMEOUT, reader.read_until(b'\n', &mut line))
        .await
        .expect("IPC response timeout")
        .expect("IPC response read");
    assert!(read > 0, "worker closed before response");
    assert_eq!(line.last(), Some(&b'\n'));
    serde_json::from_slice(&line).expect("IPC response JSON")
}

async fn connect(paths: &DaemonPaths) -> BufReader<UnixStream> {
    let stream = timeout(IO_TIMEOUT, UnixStream::connect(&paths.socket))
        .await
        .expect("IPC connect timeout")
        .expect("IPC connect");
    BufReader::new(stream)
}

async fn ping(reader: &mut BufReader<UnixStream>, id: &str) -> Value {
    let frame = format!("{{\"id\":\"{id}\",\"type\":\"ping\"}}\n");
    reader
        .get_mut()
        .write_all(frame.as_bytes())
        .await
        .expect("write ping");
    let response = read_response(reader).await;
    assert_eq!(response["id"], id);
    assert_eq!(response["success"], true);
    assert_eq!(response["data"], "pong");
    response
}

async fn wait_for_exit(ready: &mut DaemonReady) {
    let status = timeout(PROCESS_TIMEOUT, ready.child_mut().wait())
        .await
        .expect("worker exit timeout")
        .expect("wait worker");
    assert!(status.success(), "worker failed with {status}");
}

async fn request_close(ready: &mut DaemonReady, paths: &DaemonPaths) {
    let mut client = connect(paths).await;
    client
        .get_mut()
        .write_all(b"{\"id\":\"close\",\"type\":\"close\"}\n")
        .await
        .expect("write close");
    let response = read_response(&mut client).await;
    assert_eq!(response["id"], "close");
    assert_eq!(response["success"], true);
    drop(client);
    wait_for_exit(ready).await;
}

fn runtime_entries(paths: &DaemonPaths) -> Vec<PathBuf> {
    let mut entries = fs::read_dir(&paths.runtime_dir)
        .expect("read runtime directory")
        .map(|entry| entry.expect("runtime entry").path())
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

fn assert_clean(paths: &DaemonPaths) {
    assert!(!paths.socket.exists(), "socket was not removed");
    assert!(!paths.pid.exists(), "PID file was not removed");
    assert!(!paths.lock.exists(), "lock file was not removed");
    assert_eq!(runtime_entries(paths), Vec::<PathBuf>::new());
}

fn process_alive(pid: u32) -> bool {
    StdCommand::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

async fn observation_pid(path: &Path) -> u32 {
    let deadline = Instant::now() + IO_TIMEOUT;
    loop {
        if let Ok(bytes) = fs::read(path)
            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            && let Some(pid) = value["pid"].as_u64()
        {
            return u32::try_from(pid).expect("backend PID fits u32");
        }
        assert!(Instant::now() < deadline, "backend observation timeout");
        sleep(Duration::from_millis(20)).await;
    }
}

async fn assert_process_gone(pid: u32) {
    let deadline = Instant::now() + IO_TIMEOUT;
    while process_alive(pid) && Instant::now() < deadline {
        sleep(Duration::from_millis(20)).await;
    }
    assert!(!process_alive(pid), "process {pid} remains alive");
}

fn padded_request(target: usize) -> Vec<u8> {
    let base = IpcRequest::new(
        "boundary",
        IpcOperation::CallTool {
            tool_name: "echo".to_owned(),
            args: json!({"padding": ""}).as_object().expect("object").clone(),
        },
    )
    .expect("base request");
    let base_len = serde_json::to_vec(&base).expect("serialize base").len();
    assert!(target >= base_len);
    let request = IpcRequest::new(
        "boundary",
        IpcOperation::CallTool {
            tool_name: "echo".to_owned(),
            args: json!({"padding": "x".repeat(target - base_len)})
                .as_object()
                .expect("object")
                .clone(),
        },
    )
    .expect("padded request");
    let encoded = serde_json::to_vec(&request).expect("serialize padded request");
    assert_eq!(encoded.len(), target);
    encoded
}

async fn expect_client_close(mut client: BufReader<UnixStream>) {
    let mut byte = [0_u8; 1];
    let read = timeout(IO_TIMEOUT, client.read(&mut byte))
        .await
        .expect("malformed client was not closed")
        .expect("read malformed client close");
    assert_eq!(read, 0, "malformed client received unexpected bytes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workers_are_private_independent_and_leave_no_artifacts_or_children() {
    let first = Fixture::new("worker-alpha", 0);
    let second = Fixture::new("worker-beta", 0);
    assert_ne!(first.server.id, second.server.id);
    assert_ne!(first.server.config_hash, second.server.config_hash);
    assert_ne!(first.paths.socket, second.paths.socket);
    assert_ne!(first.paths.pid, second.paths.pid);

    let mut first_ready = first.spawn(Duration::from_secs(30)).await;
    let mut second_ready = second.spawn(Duration::from_secs(30)).await;
    assert_ne!(first_ready.pid(), second_ready.pid());

    for (fixture, pid) in [(&first, first_ready.pid()), (&second, second_ready.pid())] {
        let runtime = fs::symlink_metadata(&fixture.paths.runtime_dir).expect("runtime metadata");
        assert!(runtime.is_dir());
        assert!(!runtime.file_type().is_symlink());
        assert_eq!(runtime.uid(), rustix::process::getuid().as_raw());
        assert_eq!(runtime.mode() & 0o7777, 0o700);

        let pid_metadata = fs::symlink_metadata(&fixture.paths.pid).expect("PID metadata");
        let lock_metadata = fs::symlink_metadata(&fixture.paths.lock).expect("lock metadata");
        let socket_metadata = fs::symlink_metadata(&fixture.paths.socket).expect("socket metadata");
        assert!(pid_metadata.is_file());
        assert!(lock_metadata.is_file());
        assert!(socket_metadata.file_type().is_socket());
        for metadata in [&pid_metadata, &lock_metadata, &socket_metadata] {
            assert_eq!(metadata.uid(), rustix::process::getuid().as_raw());
            assert!(!metadata.file_type().is_symlink());
        }
        assert_eq!(pid_metadata.mode() & 0o7777, 0o600);
        assert_eq!(lock_metadata.mode() & 0o7777, 0o600);

        let stored = MetadataStore::new(fixture.paths.clone())
            .read()
            .expect("read PID metadata");
        assert_eq!(stored.pid, pid);
        assert_eq!(stored.config_hash, fixture.server.config_hash);
        let value: Value =
            serde_json::from_slice(&fs::read(&fixture.paths.pid).expect("PID bytes"))
                .expect("PID JSON");
        assert_eq!(
            value
                .as_object()
                .expect("PID object")
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "config_hash".to_owned(),
                "pid".to_owned(),
                "started_at".to_owned(),
            ])
        );
        ping(&mut connect(&fixture.paths).await, "isolation").await;
    }

    let first_backend = observation_pid(&first.observation).await;
    let second_backend = observation_pid(&second.observation).await;
    request_close(&mut first_ready, &first.paths).await;
    ping(&mut connect(&second.paths).await, "still-serving").await;
    request_close(&mut second_ready, &second.paths).await;
    assert_clean(&first.paths);
    assert_clean(&second.paths);
    assert_process_gone(first_backend).await;
    assert_process_gone(second_backend).await;

    let unsafe_root = tempfile::tempdir().expect("unsafe path root");
    let runtime = unsafe_root
        .path()
        .join(format!("mcp-cli-{}", rustix::process::getuid().as_raw()));
    let outside = unsafe_root.path().join("outside");
    fs::create_dir(&outside).expect("outside directory");
    symlink(&outside, &runtime).expect("runtime symlink");
    assert!(
        DaemonPaths::from_runtime_parent(unsafe_root.path(), &server_id("unsafe-runtime")).is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_ipc_handles_concurrency_order_framing_boundaries_and_client_isolation() {
    let fixture = Fixture::new("worker-ipc", 650);
    let mut ready = fixture.spawn(Duration::from_secs(30)).await;

    let mut slow = connect(&fixture.paths).await;
    slow.get_mut()
        .write_all(
            b"{\"id\":\"slow\",\"type\":\"listTools\"}\n{\"id\":\"after\",\"type\":\"ping\"}\n",
        )
        .await
        .expect("write ordered requests");
    let mut fast = connect(&fixture.paths).await;
    fast.get_mut()
        .write_all(b"{\"id\":\"fast\",\"type\":\"ping\"}\r\n")
        .await
        .expect("write concurrent ping");
    let fast_response = timeout(Duration::from_millis(400), read_response(&mut fast))
        .await
        .expect("different client was blocked");
    assert_eq!(fast_response["id"], "fast");
    assert_eq!(read_response(&mut slow).await["id"], "slow");
    assert_eq!(read_response(&mut slow).await["id"], "after");

    let mut framed = connect(&fixture.paths).await;
    framed
        .get_mut()
        .write_all(b"{\"id\":\"split\",")
        .await
        .expect("write split prefix");
    sleep(Duration::from_millis(20)).await;
    framed
        .get_mut()
        .write_all(
            b"\"type\":\"ping\"}\n{\"id\":\"glued-a\",\"type\":\"ping\"}\r\n{\"id\":\"glued-b\",\"type\":\"ping\"}\n",
        )
        .await
        .expect("write split remainder and glued frames");
    assert_eq!(read_response(&mut framed).await["id"], "split");
    assert_eq!(read_response(&mut framed).await["id"], "glued-a");
    assert_eq!(read_response(&mut framed).await["id"], "glued-b");

    framed
        .get_mut()
        .write_all(
            b"{not-json}\n{\"type\":\"ping\"}\n{\"id\":\"unknown\",\"type\":\"mystery\"}\n{\"id\":\"recover\",\"type\":\"ping\"}\n",
        )
        .await
        .expect("write recoverable errors");
    let invalid = read_response(&mut framed).await;
    assert_eq!(invalid["id"], "");
    assert_eq!(invalid["success"], false);
    assert_eq!(invalid["error"]["code"], "INVALID_JSON");
    assert_eq!(invalid["error"]["message"], "Invalid JSON request");
    let missing = read_response(&mut framed).await;
    assert_eq!(missing["id"], "");
    assert_eq!(missing["error"]["code"], "MISSING_ID");
    assert_eq!(missing["error"]["message"], "Request ID is required");
    let unknown = read_response(&mut framed).await;
    assert_eq!(unknown["id"], "unknown");
    assert_eq!(unknown["error"]["code"], "UNKNOWN_TYPE");
    assert_eq!(unknown["error"]["message"], "Unknown request type");
    assert_eq!(read_response(&mut framed).await["id"], "recover");

    let mut exact = connect(&fixture.paths).await;
    let mut exact_wire = padded_request(IPC_MAX_FRAME_SIZE);
    exact_wire.push(b'\n');
    exact
        .get_mut()
        .write_all(&exact_wire)
        .await
        .expect("write exact 1 MiB request");
    let exact_response = read_response(&mut exact).await;
    assert_eq!(exact_response["id"], "boundary");
    assert_eq!(exact_response["success"], true);

    let mut oversized = connect(&fixture.paths).await;
    let mut oversized_wire = padded_request(IPC_MAX_FRAME_SIZE + 1);
    oversized_wire.push(b'\n');
    let _ = oversized.get_mut().write_all(&oversized_wire).await;
    expect_client_close(oversized).await;
    ping(&mut fast, "after-oversize").await;

    for malformed in [
        vec![0xff, b'\n'],
        b"{\"id\":\"cr\",\"type\":\"ping\"}\rX".to_vec(),
    ] {
        let mut client = connect(&fixture.paths).await;
        let _ = client.get_mut().write_all(&malformed).await;
        expect_client_close(client).await;
        ping(&mut fast, "after-framing-error").await;
    }

    let mut truncated = connect(&fixture.paths).await;
    truncated
        .get_mut()
        .write_all(b"{\"id\":\"truncated\",\"type\":\"ping\"}")
        .await
        .expect("write truncated frame");
    truncated
        .get_mut()
        .shutdown()
        .await
        .expect("half-close truncated client");
    expect_client_close(truncated).await;
    ping(&mut fast, "after-truncated").await;

    drop(slow);
    drop(fast);
    drop(framed);
    drop(exact);
    let backend = observation_pid(&fixture.observation).await;
    request_close(&mut ready, &fixture.paths).await;
    assert_clean(&fixture.paths);
    assert_process_gone(backend).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_faults_never_publish_ready_and_reclaim_every_partial_resource() {
    for fault in [
        WorkerStartupFault::BeforeBackend,
        WorkerStartupFault::BeforeSocket,
        WorkerStartupFault::BeforePid,
        WorkerStartupFault::BeforeReady,
    ] {
        let fixture = Fixture::new(&format!("startup-{fault:?}"), 0);
        let result = fixture
            .spawner(Duration::from_secs(30))
            .with_startup_fault(fault)
            .spawn(&context(), &fixture.server, &fixture.paths)
            .await;
        assert!(
            matches!(
                result,
                Err(DaemonSpawnError::WorkerExitedBeforeReady)
                    | Err(DaemonSpawnError::TransferBootstrap)
            ),
            "fault {fault:?} unexpectedly published ready"
        );
        assert_clean(&fixture.paths);
        assert!(UnixStream::connect(&fixture.paths.socket).await.is_err());
        if fixture.observation.exists() {
            let backend = observation_pid(&fixture.observation).await;
            assert_process_gone(backend).await;
        }
        assert!(
            fixture.script.exists(),
            "test input must remain outside runtime"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_close_sigint_and_sigterm_reap_backend_and_remove_all_artifacts() {
    let idle = Fixture::new("shutdown-idle", 0);
    let mut idle_ready = idle.spawn(Duration::from_millis(250)).await;
    let idle_backend = observation_pid(&idle.observation).await;
    wait_for_exit(&mut idle_ready).await;
    assert_clean(&idle.paths);
    assert_process_gone(idle_backend).await;

    let close = Fixture::new("shutdown-close", 0);
    let mut close_ready = close.spawn(Duration::from_secs(30)).await;
    let close_backend = observation_pid(&close.observation).await;
    request_close(&mut close_ready, &close.paths).await;
    assert_clean(&close.paths);
    assert_process_gone(close_backend).await;

    for (name, signal) in [("shutdown-sigint", "-INT"), ("shutdown-sigterm", "-TERM")] {
        let fixture = Fixture::new(name, 0);
        let mut ready = fixture.spawn(Duration::from_secs(30)).await;
        let backend = observation_pid(&fixture.observation).await;
        let status = StdCommand::new("kill")
            .args([signal, &ready.pid().to_string()])
            .status()
            .expect("send worker signal");
        assert!(status.success());
        wait_for_exit(&mut ready).await;
        assert_clean(&fixture.paths);
        assert_process_gone(backend).await;
    }
}
