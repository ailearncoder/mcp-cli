#![cfg(unix)]
#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use mcp_cli::{
    CancellationFlag, CommandContext, DiagnosticSink, ServerDefinition, SystemClock,
    ToolFilterConfig, TransportConfig,
    config::{config_hash, server_id},
    daemon::{
        DaemonPaths,
        worker::{CurrentExecutableDaemonSpawner, DaemonReady, DaemonSpawner},
    },
    runtime::Deadline,
};
use rustix::process::{Pid, Signal, kill_process, test_kill_process};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::time::timeout;

const OPERATION_DELAY_MS: u64 = 200;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(4);
const PROCESS_TIMEOUT: Duration = Duration::from_secs(12);

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

#[derive(Clone, Copy, Debug)]
enum ShutdownSignal {
    Interrupt,
    Terminate,
}

impl ShutdownSignal {
    fn rustix(self) -> Signal {
        match self {
            Self::Interrupt => Signal::INT,
            Self::Terminate => Signal::TERM,
        }
    }

    fn expected_direct_exit(self) -> i32 {
        match self {
            Self::Interrupt => 130,
            Self::Terminate => 143,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Interrupt => "sigint",
            Self::Terminate => "sigterm",
        }
    }
}

struct SignalFixture {
    root: TempDir,
    config: PathBuf,
    script: PathBuf,
    observation: PathBuf,
    server: ServerDefinition,
    paths: DaemonPaths,
}

impl SignalFixture {
    fn new(name: &str) -> Self {
        let root = tempfile::tempdir().expect("isolated signal fixture");
        let config = root.path().join("mcp_servers.json");
        let script = root.path().join("mock-script.json");
        let observation = root.path().join("mock-observation.json");
        fs::write(
            &script,
            serde_json::to_vec(&json!({
                "observation_path": observation,
                "instructions": "signal process fixture",
                "tool_pages": [{
                    "tools": [{
                        "name": "echo",
                        "description": "signal fixture tool",
                        "inputSchema": {"type": "object"}
                    }]
                }],
                "list_response_delay_ms": OPERATION_DELAY_MS,
                "eof_behavior": "exit"
            }))
            .expect("fixture script JSON"),
        )
        .expect("write fixture script");

        let mock = Path::new(env!("CARGO_BIN_EXE_mock-stdio-server"));
        let raw = json!({
            "command": mock,
            "args": ["--fixture-script", script]
        });
        fs::write(
            &config,
            serde_json::to_vec(&json!({"mcpServers": {name: raw.clone()}})).expect("config JSON"),
        )
        .expect("write config");

        let server = ServerDefinition {
            name: name.to_owned(),
            id: server_id(name),
            config_hash: config_hash(&raw),
            transport: TransportConfig::Stdio {
                command: mock.to_string_lossy().into_owned(),
                args: vec![
                    "--fixture-script".to_owned(),
                    script.to_string_lossy().into_owned(),
                ],
                env: BTreeMap::new(),
                cwd: Some(root.path().to_path_buf()),
            },
            filter: ToolFilterConfig::default(),
        };
        let paths = DaemonPaths::from_runtime_parent(root.path(), &server.id)
            .expect("isolated daemon paths");
        Self {
            root,
            config,
            script,
            observation,
            server,
            paths,
        }
    }

    fn prepare_direct(&self) {
        fs::remove_dir(&self.paths.runtime_dir).expect("remove fixture-created empty runtime");
    }

    fn spawn_direct(&self) -> Child {
        Command::new(env!("CARGO_BIN_EXE_mcp-cli"))
            .current_dir(self.root.path())
            .env_clear()
            .env("HOME", self.root.path())
            .env("USERPROFILE", self.root.path())
            .env("TMPDIR", self.root.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("MCP_NO_DAEMON", "1")
            .env("MCP_MAX_RETRIES", "0")
            .env("MCP_TIMEOUT", "20")
            .env("NO_COLOR", "1")
            .arg("--config")
            .arg(&self.config)
            .args(["info", &self.server.name])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn direct CLI")
    }

    async fn spawn_worker(&self) -> DaemonReady {
        CurrentExecutableDaemonSpawner::with_executable(
            Path::new(env!("CARGO_BIN_EXE_mcp-cli")).to_path_buf(),
            Duration::from_secs(30),
        )
        .spawn(&context(), &self.server, &self.paths)
        .await
        .expect("spawn daemon worker")
    }

    fn wait_for_inflight_list(&self) -> Value {
        wait_for_observation(&self.observation, |observation| {
            observation["events"]
                .as_array()
                .is_some_and(|events| events.iter().any(|event| event == "tools/list"))
        })
    }

    fn observation(&self) -> Value {
        read_observation(&self.observation).expect("final backend observation")
    }

    fn assert_no_direct_daemon_artifacts(&self) {
        let runtime = self
            .root
            .path()
            .join(format!("mcp-cli-{}", rustix::process::getuid().as_raw()));
        assert!(
            !runtime.exists(),
            "direct mode created daemon runtime artifacts at {}",
            runtime.display()
        );
    }

    fn assert_daemon_artifacts_removed(&self) {
        for artifact in [&self.paths.socket, &self.paths.pid, &self.paths.lock] {
            assert!(
                !artifact.exists(),
                "daemon artifact remained after signal: {}",
                artifact.display()
            );
        }
        let entries = fs::read_dir(&self.paths.runtime_dir)
            .expect("read isolated runtime directory")
            .map(|entry| entry.expect("runtime entry").path())
            .collect::<Vec<_>>();
        assert!(
            entries.is_empty(),
            "runtime artifacts remained: {entries:?}"
        );
    }
}

impl Drop for SignalFixture {
    fn drop(&mut self) {
        let _ = &self.script;
        if let Ok(observation) = read_observation(&self.observation)
            && let Some(raw_pid) = observation["pid"].as_u64()
            && let Some(pid) = process_id(raw_pid as u32)
            && test_kill_process(pid).is_ok()
        {
            let _ = kill_process(pid, Signal::KILL);
        }

        if let Ok(bytes) = fs::read(&self.paths.pid)
            && let Ok(metadata) = serde_json::from_slice::<Value>(&bytes)
            && let Some(raw_pid) = metadata["pid"].as_u64()
            && let Some(pid) = process_id(raw_pid as u32)
            && test_kill_process(pid).is_ok()
        {
            let _ = kill_process(pid, Signal::KILL);
        }
    }
}

fn process_id(raw: u32) -> Option<Pid> {
    i32::try_from(raw).ok().and_then(Pid::from_raw)
}

fn send_signal(raw_pid: u32, signal: ShutdownSignal) {
    let pid = process_id(raw_pid).expect("valid positive process ID");
    kill_process(pid, signal.rustix()).expect("deliver Unix signal");
}

fn process_exists(raw_pid: u32) -> bool {
    process_id(raw_pid).is_some_and(|pid| test_kill_process(pid).is_ok())
}

fn read_observation(path: &Path) -> Result<Value, ()> {
    fs::read(path)
        .map_err(|_| ())
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|_| ()))
}

fn wait_for_observation(path: &Path, predicate: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + OBSERVATION_TIMEOUT;
    loop {
        if let Ok(observation) = read_observation(path)
            && predicate(&observation)
        {
            return observation;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for observation {}",
            path.display()
        );
        thread::sleep(POLL_INTERVAL);
    }
}

fn wait_for_std_child(mut child: Child) -> Output {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    let status = loop {
        match child.try_wait().expect("poll child status") {
            Some(status) => break status,
            None if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "process {} did not exit within {PROCESS_TIMEOUT:?}",
                    child.id()
                );
            }
        }
    };
    Output {
        status,
        stdout: read_pipe(child.stdout.take()),
        stderr: read_pipe(child.stderr.take()),
    }
}

fn read_pipe(mut pipe: Option<impl Read>) -> Vec<u8> {
    let mut bytes = Vec::new();
    if let Some(pipe) = pipe.as_mut() {
        pipe.read_to_end(&mut bytes).expect("read process pipe");
    }
    bytes
}

async fn wait_for_worker(ready: &mut DaemonReady) -> ExitStatus {
    timeout(PROCESS_TIMEOUT, ready.child_mut().wait())
        .await
        .expect("worker shutdown exceeded bound")
        .expect("wait and reap worker")
}

async fn wait_for_process_absent(raw_pid: u32) {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    while process_exists(raw_pid) && Instant::now() < deadline {
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    assert!(
        !process_exists(raw_pid),
        "backend process {raw_pid} remained alive or zombie"
    );
}

#[test]
fn direct_sigint_and_sigterm_close_backend_and_preserve_signal_exit_codes() {
    for signal in [ShutdownSignal::Interrupt, ShutdownSignal::Terminate] {
        let fixture = SignalFixture::new(&format!("direct-{}", signal.label()));
        fixture.prepare_direct();
        let child = fixture.spawn_direct();
        let cli_pid = child.id();
        let inflight = fixture.wait_for_inflight_list();
        let backend_pid = inflight["pid"].as_u64().expect("backend PID") as u32;

        send_signal(cli_pid, signal);
        let output = wait_for_std_child(child);
        let final_observation = fixture.observation();

        assert_eq!(
            output.status.code(),
            Some(signal.expected_direct_exit()),
            "{output:?}"
        );
        assert!(
            output.stdout.is_empty(),
            "signal polluted stdout: {output:?}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            stderr.matches("Error [").count(),
            0,
            "signal rendered a duplicate Structured_Error: {stderr}"
        );
        assert!(
            final_observation["events"]
                .as_array()
                .is_some_and(|events| events.iter().any(|event| event == "tools/list")),
            "backend operation was not in flight: {final_observation:#}"
        );
        assert!(!process_exists(backend_pid), "backend was not reaped");
        assert!(!process_exists(cli_pid), "direct CLI was not reaped");
        fixture.assert_no_direct_daemon_artifacts();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_sigint_and_sigterm_shutdown_once_and_remove_all_artifacts() {
    for signal in [ShutdownSignal::Interrupt, ShutdownSignal::Terminate] {
        let fixture = SignalFixture::new(&format!("daemon-{}", signal.label()));
        let mut ready = fixture.spawn_worker().await;
        let worker_pid = ready.pid();
        let mut client = UnixStream::connect(&fixture.paths.socket).expect("connect daemon client");
        client
            .write_all(b"{\"id\":\"inflight\",\"type\":\"listTools\"}\n")
            .expect("start daemon backend operation");
        let inflight = fixture.wait_for_inflight_list();
        let backend_pid = inflight["pid"].as_u64().expect("backend PID") as u32;

        send_signal(worker_pid, signal);
        send_signal(worker_pid, signal);
        let status = wait_for_worker(&mut ready).await;
        drop(client);
        wait_for_process_absent(backend_pid).await;
        let final_observation = fixture.observation();

        assert!(status.success(), "worker signal shutdown failed: {status}");
        assert!(
            final_observation["events"]
                .as_array()
                .is_some_and(|events| events.iter().any(|event| event == "tools/list")),
            "backend operation was not in flight: {final_observation:#}"
        );
        assert!(!process_exists(worker_pid), "worker was not reaped");
        fixture.assert_daemon_artifacts_removed();
    }
}
