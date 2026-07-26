#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    fs,
    os::unix::{fs::symlink, net::UnixListener as StdUnixListener},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

use mcp_cli::{
    CancellationFlag, CommandContext, ConnectionMode, DiagnosticSink, McpConnection, SystemClock,
    config::{config_hash, server_id},
    daemon::{
        DaemonPaths, MetadataStore, PidMetadata,
        client::DaemonClient,
        worker::{TEST_DAEMON_CALL_DELAY_ENV, TEST_DAEMON_PING_DELAY_ENV},
    },
    runtime::Deadline,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    task::JoinSet,
    time::timeout,
};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(8);
const IPC_TIMEOUT: Duration = Duration::from_secs(3);
const ENV_SECRET: &str = "daemon-env-secret-8-6";
const HEADER_SECRET: &str = "daemon-header-secret-8-6";
const USER_PAYLOAD: &str = "daemon-user-payload-8-6";

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

#[derive(Clone)]
struct ServerSpec {
    name: String,
    ping_delay_ms: u64,
    call_delay_ms: u64,
    version: String,
}

impl ServerSpec {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            ping_delay_ms: 0,
            call_delay_ms: 0,
            version: "v1".to_owned(),
        }
    }
}

struct LinuxFixture {
    root: TempDir,
    config: PathBuf,
    definitions: BTreeMap<String, Value>,
    observation_dirs: BTreeMap<String, PathBuf>,
}

impl LinuxFixture {
    fn new(specs: impl IntoIterator<Item = ServerSpec>) -> Self {
        let root = tempfile::tempdir().expect("isolated Linux daemon root");
        let config = root.path().join("mcp_servers.json");
        let mut fixture = Self {
            root,
            config,
            definitions: BTreeMap::new(),
            observation_dirs: BTreeMap::new(),
        };
        for spec in specs {
            fixture.install(spec);
        }
        fixture.write_config();
        fixture
    }

    fn install(&mut self, spec: ServerSpec) {
        let observation = self.root.path().join(format!("{}-latest.json", spec.name));
        let observation_dir = self.root.path().join(format!("{}-observations", spec.name));
        let script = self.root.path().join(format!("{}-script.json", spec.name));
        fs::write(
            &script,
            serde_json::to_vec(&json!({
                "observation_path": observation,
                "observation_dir": observation_dir,
                "capture_env": ["DAEMON_ENV_SECRET"],
                "instructions": format!("Linux daemon fixture {}", spec.name),
                "tool_pages": [{
                    "tools": [{
                        "name": "echo",
                        "description": "Linux daemon fixture tool",
                        "inputSchema": {"type": "object"}
                    }]
                }],
                "call_result": {"content": [{"type": "text", "text": "ok"}]},
                "eof_behavior": "exit"
            }))
            .expect("script JSON"),
        )
        .expect("write script");

        let mut env = serde_json::Map::new();
        env.insert("DAEMON_ENV_SECRET".to_owned(), json!(ENV_SECRET));
        env.insert("FIXTURE_VERSION".to_owned(), json!(spec.version));
        if spec.ping_delay_ms != 0 {
            env.insert(
                TEST_DAEMON_PING_DELAY_ENV.to_owned(),
                json!(spec.ping_delay_ms.to_string()),
            );
        }
        if spec.call_delay_ms != 0 {
            env.insert(
                TEST_DAEMON_CALL_DELAY_ENV.to_owned(),
                json!(spec.call_delay_ms.to_string()),
            );
        }
        self.definitions.insert(
            spec.name.clone(),
            json!({
                "command": Path::new(env!("CARGO_BIN_EXE_mock-stdio-server")),
                "args": ["--fixture-script", script],
                "env": env,
                // This known field is validated even for stdio and proves a
                // header-shaped secret never reaches worker argv.
                "headers": {"Authorization": HEADER_SECRET}
            }),
        );
        self.observation_dirs.insert(spec.name, observation_dir);
    }

    fn write_config(&self) {
        fs::write(
            &self.config,
            serde_json::to_vec(&json!({"mcpServers": self.definitions})).expect("config JSON"),
        )
        .expect("write config");
    }

    fn update_version(&mut self, name: &str, version: &str) {
        self.definitions
            .get_mut(name)
            .and_then(Value::as_object_mut)
            .and_then(|definition| definition.get_mut("env"))
            .and_then(Value::as_object_mut)
            .expect("fixture env")
            .insert("FIXTURE_VERSION".to_owned(), json!(version));
        self.write_config();
    }

    fn paths(&self, name: &str) -> DaemonPaths {
        DaemonPaths::from_runtime_parent(self.root.path(), &server_id(name))
            .expect("isolated daemon paths")
    }

    fn config_hash(&self, name: &str) -> mcp_cli::ConfigHash {
        config_hash(self.definitions.get(name).expect("known definition"))
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mcp-cli"));
        command
            .current_dir(self.root.path())
            .env_clear()
            .env("HOME", self.root.path())
            .env("USERPROFILE", self.root.path())
            .env("TMPDIR", self.root.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("MCP_MAX_RETRIES", "0")
            .env("MCP_TIMEOUT", "25")
            .env("MCP_DAEMON_TIMEOUT", "60")
            .env("MCP_DEBUG", "1")
            .env("NO_COLOR", "1")
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .stdin(Stdio::null());
        command.output().expect("mcp-cli process")
    }

    fn observations(&self, name: &str) -> Vec<Value> {
        let directory = self.observation_dirs.get(name).expect("known server");
        let Ok(entries) = fs::read_dir(directory) else {
            return Vec::new();
        };
        let mut values = entries
            .filter_map(Result::ok)
            .filter_map(|entry| fs::read(entry.path()).ok())
            .filter_map(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .collect::<Vec<_>>();
        values.sort_by_key(|value| value["pid"].as_u64().unwrap_or_default());
        values
    }

    fn assert_no_backend_started(&self, name: &str) {
        assert!(
            self.observations(name).is_empty(),
            "security failure unexpectedly used direct fallback"
        );
    }
}

impl Drop for LinuxFixture {
    fn drop(&mut self) {
        let mut pids = Vec::new();
        let runtime = self
            .root
            .path()
            .join(format!("mcp-cli-{}", rustix::process::getuid().as_raw()));
        if let Ok(entries) = fs::read_dir(runtime) {
            for entry in entries.filter_map(Result::ok) {
                if entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "pid")
                    && let Ok(bytes) = fs::read(entry.path())
                    && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
                    && let Some(pid) = value["pid"].as_u64()
                {
                    pids.push(pid as u32);
                }
            }
        }
        for directory in self.observation_dirs.values() {
            if let Ok(entries) = fs::read_dir(directory) {
                for entry in entries.filter_map(Result::ok) {
                    if let Ok(bytes) = fs::read(entry.path())
                        && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
                        && let Some(pid) = value["pid"].as_u64()
                    {
                        pids.push(pid as u32);
                    }
                }
            }
        }
        pids.sort_unstable();
        pids.dedup();
        for pid in &pids {
            signal(*pid, "-TERM");
        }
        thread::sleep(Duration::from_millis(100));
        for pid in pids {
            if process_alive(pid) {
                signal(pid, "-KILL");
            }
        }
    }
}

fn assert_success(output: &Output) {
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        !output.stdout.is_empty(),
        "successful command had no output"
    );
}

fn assert_security_failure(output: &Output) {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "security error polluted stdout");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Error [SECURITY_ERROR]:"), "{stderr}");
    assert!(!stderr.contains("selected direct"), "{stderr}");
}

fn assert_direct_fallback(output: &Output) {
    assert_success(output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("selected direct"), "{stderr}");
}

fn process_alive(pid: u32) -> bool {
    let Ok(stat) = fs::read(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(closing) = stat.iter().rposition(|byte| *byte == b')') else {
        return false;
    };
    !matches!(stat.get(closing + 2), Some(b'Z' | b'X'))
}

fn signal(pid: u32, signal: &str) {
    let _ = Command::new("kill")
        .args([signal, &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn wait_for_process_exit(pid: u32) {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    while process_alive(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!process_alive(pid), "process {pid} remained alive");
}

fn wait_for_absent(path: &Path) {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    while path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!path.exists(), "artifact remained: {}", path.display());
}

async fn close_worker(paths: &DaemonPaths) {
    let mut stream = timeout(IPC_TIMEOUT, UnixStream::connect(&paths.socket))
        .await
        .expect("close connect timeout")
        .expect("close connect");
    stream
        .write_all(b"{\"id\":\"cleanup\",\"type\":\"close\"}\n")
        .await
        .expect("write close");
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    timeout(IPC_TIMEOUT, reader.read_line(&mut response))
        .await
        .expect("close response timeout")
        .expect("close response");
    assert_eq!(
        serde_json::from_str::<Value>(&response).expect("close JSON")["data"],
        "closing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_reuse_server_isolation_hash_replacement_and_argv_secrecy() {
    let mut fixture = LinuxFixture::new([ServerSpec::new("alpha"), ServerSpec::new("beta")]);

    let first = fixture.run(&[
        "call",
        "alpha",
        "echo",
        &format!("{{\"payload\":\"{USER_PAYLOAD}\"}}"),
    ]);
    assert_success(&first);
    let alpha_paths = fixture.paths("alpha");
    let first_metadata = MetadataStore::new(alpha_paths.clone())
        .read()
        .expect("first worker metadata");

    let second = fixture.run(&["info", "alpha"]);
    assert_success(&second);
    let reused = MetadataStore::new(alpha_paths.clone())
        .read()
        .expect("reused metadata");
    assert_eq!(reused.pid, first_metadata.pid, "worker was not reused");
    assert_eq!(
        fixture.observations("alpha").len(),
        1,
        "backend was not reused"
    );

    let beta = fixture.run(&["info", "beta"]);
    assert_success(&beta);
    let beta_paths = fixture.paths("beta");
    let beta_pid = MetadataStore::new(beta_paths.clone())
        .read()
        .expect("beta metadata")
        .pid;
    assert_ne!(first_metadata.pid, beta_pid);
    assert_ne!(alpha_paths.socket, beta_paths.socket);

    let cmdline = fs::read(format!("/proc/{}/cmdline", first_metadata.pid)).expect("cmdline");
    let argv = cmdline
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(argv.len(), 2, "worker argv was not executable + __daemon");
    assert_eq!(argv[1], b"__daemon");
    for secret in [ENV_SECRET, HEADER_SECRET, USER_PAYLOAD] {
        assert!(
            !cmdline
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "worker argv leaked {secret}"
        );
    }
    assert!(
        !cmdline
            .windows(fixture.config.as_os_str().len())
            .any(|window| { window == fixture.config.as_os_str().as_encoded_bytes() }),
        "worker argv leaked config path"
    );
    assert!(
        fs::read(format!("/proc/{}/environ", first_metadata.pid))
            .expect("worker environ")
            .is_empty(),
        "worker inherited an environment"
    );

    fixture.update_version("alpha", "v2");
    let replaced = fixture.run(&["info", "alpha"]);
    assert_success(&replaced);
    let new_metadata = MetadataStore::new(alpha_paths.clone())
        .read()
        .expect("replacement metadata");
    assert_ne!(new_metadata.pid, first_metadata.pid);
    assert_eq!(new_metadata.config_hash, fixture.config_hash("alpha"));
    wait_for_process_exit(first_metadata.pid);

    close_worker(&alpha_paths).await;
    close_worker(&beta_paths).await;
    wait_for_absent(&alpha_paths.pid);
    wait_for_absent(&beta_paths.pid);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_clients_dead_pid_recovery_and_missing_socket_fallback() {
    let fixture = LinuxFixture::new([ServerSpec::new("concurrent")]);
    assert_success(&fixture.run(&["info", "concurrent"]));
    let paths = fixture.paths("concurrent");
    let original = MetadataStore::new(paths.clone()).read().expect("metadata");

    let mut clients = JoinSet::new();
    for _ in 0..8 {
        let socket = paths.socket.clone();
        clients.spawn(async move {
            let ctx = context();
            let client = DaemonClient::connect(&ctx, socket).await.expect("client");
            assert_eq!(client.mode(), ConnectionMode::Daemon);
            let tools = client.list_tools(&ctx).await.expect("list tools");
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].name, "echo");
            Box::new(client).close(&ctx).await.expect("client close");
        });
    }
    while let Some(result) = clients.join_next().await {
        result.expect("concurrent client task");
    }

    signal(original.pid, "-KILL");
    wait_for_process_exit(original.pid);
    assert!(
        paths.pid.exists(),
        "dead worker PID artifact vanished before recovery"
    );
    assert!(
        paths.socket.exists(),
        "dead worker socket artifact vanished before recovery"
    );

    let recovered = fixture.run(&["info", "concurrent"]);
    assert_success(&recovered);
    let replacement = MetadataStore::new(paths.clone())
        .read()
        .expect("replacement");
    assert_ne!(replacement.pid, original.pid);

    fs::remove_file(&paths.socket).expect("remove published socket");
    let missing_socket = fixture.run(&["info", "concurrent"]);
    assert_direct_fallback(&missing_socket);
    assert!(
        process_alive(replacement.pid),
        "manager killed missing-socket worker"
    );

    signal(replacement.pid, "-TERM");
    wait_for_process_exit(replacement.pid);
    wait_for_absent(&paths.pid);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ping_and_request_timeouts_fallback_only_after_cancellation_acknowledgement() {
    let mut ping_spec = ServerSpec::new("ping-timeout");
    ping_spec.ping_delay_ms = 5_500;
    let ping_fixture = LinuxFixture::new([ping_spec]);
    let ping = ping_fixture.run(&["info", "ping-timeout"]);
    assert_direct_fallback(&ping);
    let ping_paths = ping_fixture.paths("ping-timeout");
    assert!(
        !ping_paths.pid.exists() && !ping_paths.socket.exists() && !ping_paths.lock.exists(),
        "failed ping worker was not reclaimed"
    );
    let ping_observations = ping_fixture.observations("ping-timeout");
    assert_eq!(ping_observations.len(), 2);
    for observation in ping_observations {
        let pid = observation["pid"].as_u64().expect("backend pid") as u32;
        wait_for_process_exit(pid);
    }

    let mut call_spec = ServerSpec::new("request-timeout");
    call_spec.call_delay_ms = 5_500;
    let call_fixture = LinuxFixture::new([call_spec]);
    let call = call_fixture.run(&[
        "call",
        "request-timeout",
        "echo",
        &format!("{{\"payload\":\"{USER_PAYLOAD}\"}}"),
    ]);
    assert_direct_fallback(&call);
    assert_eq!(
        serde_json::from_slice::<Value>(&call.stdout).expect("call JSON"),
        json!({"content": [{"type": "text", "text": "ok"}]})
    );
    let observations = call_fixture.observations("request-timeout");
    assert_eq!(observations.len(), 2, "daemon and direct backends expected");
    assert_eq!(
        observations
            .iter()
            .map(|observation| observation["calls"].as_array().expect("calls").len())
            .sum::<usize>(),
        1,
        "timed-out daemon request reached the backend or direct fallback duplicated the call"
    );
    let paths = call_fixture.paths("request-timeout");
    close_worker(&paths).await;
    wait_for_absent(&paths.pid);
}

#[test]
fn symlinks_unsafe_artifacts_and_unrelated_or_reused_pids_fail_closed() {
    // Runtime-directory symlink.
    let runtime_fixture = LinuxFixture::new([ServerSpec::new("runtime-link")]);
    let runtime = runtime_fixture
        .root
        .path()
        .join(format!("mcp-cli-{}", rustix::process::getuid().as_raw()));
    let outside_dir = runtime_fixture.root.path().join("outside-runtime");
    fs::create_dir(&outside_dir).expect("outside runtime");
    symlink(&outside_dir, &runtime).expect("runtime symlink");
    let output = runtime_fixture.run(&["info", "runtime-link"]);
    assert_security_failure(&output);
    runtime_fixture.assert_no_backend_started("runtime-link");

    // Socket symlink.
    let socket_fixture = LinuxFixture::new([ServerSpec::new("socket-link")]);
    let socket_paths = socket_fixture.paths("socket-link");
    let socket_target = socket_fixture.root.path().join("socket-target");
    fs::write(&socket_target, b"outside").expect("socket target");
    symlink(&socket_target, &socket_paths.socket).expect("socket symlink");
    let output = socket_fixture.run(&["info", "socket-link"]);
    assert_security_failure(&output);
    socket_fixture.assert_no_backend_started("socket-link");
    assert_eq!(
        fs::read(&socket_target).expect("target remains"),
        b"outside"
    );

    // PID symlink.
    let pid_fixture = LinuxFixture::new([ServerSpec::new("pid-link")]);
    let pid_paths = pid_fixture.paths("pid-link");
    let pid_target = pid_fixture.root.path().join("pid-target");
    fs::write(&pid_target, b"outside").expect("PID target");
    symlink(&pid_target, &pid_paths.pid).expect("PID symlink");
    let output = pid_fixture.run(&["info", "pid-link"]);
    assert_security_failure(&output);
    pid_fixture.assert_no_backend_started("pid-link");
    assert_eq!(fs::read(&pid_target).expect("target remains"), b"outside");

    // Wrong artifact type.
    let unsafe_fixture = LinuxFixture::new([ServerSpec::new("unsafe-lock")]);
    let unsafe_paths = unsafe_fixture.paths("unsafe-lock");
    fs::create_dir(&unsafe_paths.lock).expect("unsafe lock directory");
    let output = unsafe_fixture.run(&["info", "unsafe-lock"]);
    assert_security_failure(&output);
    unsafe_fixture.assert_no_backend_started("unsafe-lock");

    // A live unrelated (or PID-reused) process in valid-looking metadata is
    // never signalled and cannot be hidden by direct fallback.
    let unrelated_fixture = LinuxFixture::new([ServerSpec::new("unrelated")]);
    let unrelated_paths = unrelated_fixture.paths("unrelated");
    let mut unrelated: Child = Command::new("sleep")
        .arg("30")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("unrelated process");
    let unrelated_pid = unrelated.id();
    let listener = StdUnixListener::bind(&unrelated_paths.socket).expect("decoy socket");
    MetadataStore::new(unrelated_paths.clone())
        .write(&PidMetadata {
            pid: unrelated_pid,
            config_hash: unrelated_fixture.config_hash("unrelated"),
            started_at: UNIX_EPOCH,
        })
        .expect("decoy metadata");
    let output = unrelated_fixture.run(&["info", "unrelated"]);
    assert_security_failure(&output);
    unrelated_fixture.assert_no_backend_started("unrelated");
    assert!(
        process_alive(unrelated_pid),
        "manager killed unrelated/PID-reused process"
    );
    unrelated.kill().expect("kill unrelated process");
    unrelated.wait().expect("reap unrelated process");
    drop(listener);
}
