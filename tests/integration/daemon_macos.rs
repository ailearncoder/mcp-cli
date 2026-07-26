#![cfg(target_os = "macos")]

use std::{
    collections::BTreeMap,
    fs,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink},
        net::UnixListener as StdUnixListener,
    },
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
const ENV_SECRET: &str = "daemon-env-secret-8-7";
const HEADER_SECRET: &str = "daemon-header-secret-8-7";
const USER_PAYLOAD: &str = "daemon-user-payload-8-7";

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

struct MacFixture {
    root: TempDir,
    config: PathBuf,
    definitions: BTreeMap<String, Value>,
    observation_dirs: BTreeMap<String, PathBuf>,
}

impl MacFixture {
    fn new(specs: impl IntoIterator<Item = ServerSpec>) -> Self {
        // Keep this lifecycle fixture in a short isolated root so it exercises
        // the full ServerId socket layout. Production macOS paths compact the
        // socket stem only when the complete path would exceed `sun_path`.
        let root = tempfile::Builder::new()
            .prefix("m")
            .rand_bytes(2)
            .tempdir_in("/tmp")
            .expect("isolated macOS daemon root");
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
                "instructions": format!("macOS daemon fixture {}", spec.name),
                "tool_pages": [{
                    "tools": [{
                        "name": "echo",
                        "description": "macOS daemon fixture tool",
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

impl Drop for MacFixture {
    fn drop(&mut self) {
        let mut pids = Vec::new();
        let runtime = self
            .root
            .path()
            .join(format!("mcp-cli-{}", rustix::process::getuid().as_raw()));
        if let Ok(entries) = fs::read_dir(runtime) {
            for entry in entries.filter_map(Result::ok) {
                if entry.path().extension().is_some_and(|value| value == "pid")
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
        let deadline = Instant::now() + Duration::from_millis(500);
        while pids.iter().any(|pid| process_alive(*pid)) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
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
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
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

fn assert_private_artifacts(paths: &DaemonPaths) {
    let expected_uid = rustix::process::getuid().as_raw();
    let runtime = fs::symlink_metadata(&paths.runtime_dir).expect("runtime metadata");
    let pid = fs::symlink_metadata(&paths.pid).expect("PID metadata");
    let lock = fs::symlink_metadata(&paths.lock).expect("lock metadata");
    let socket = fs::symlink_metadata(&paths.socket).expect("socket metadata");
    assert!(runtime.is_dir() && !runtime.file_type().is_symlink());
    assert!(pid.is_file() && lock.is_file());
    assert!(socket.file_type().is_socket());
    for metadata in [&runtime, &pid, &lock, &socket] {
        assert_eq!(metadata.uid(), expected_uid);
        assert!(!metadata.file_type().is_symlink());
    }
    assert_eq!(runtime.mode() & 0o7777, 0o700);
    assert_eq!(pid.mode() & 0o7777, 0o600);
    assert_eq!(lock.mode() & 0o7777, 0o600);
    assert_eq!(socket.mode() & 0o7777, 0o600);
}

mod platform_argv {
    use std::{ffi::c_void, io, os::raw::c_int};

    const CTL_KERN: c_int = 1;
    const KERN_ARGMAX: c_int = 8;
    const KERN_PROCARGS2: c_int = 49;
    const MAX_SAFE_ARGUMENT_BYTES: usize = 16 * 1024 * 1024;

    unsafe extern "C" {
        fn sysctl(
            name: *mut c_int,
            namelen: u32,
            oldp: *mut c_void,
            oldlenp: *mut usize,
            newp: *mut c_void,
            newlen: usize,
        ) -> c_int;
    }

    pub(super) fn read(pid: u32) -> io::Result<(Vec<Vec<u8>>, Vec<u8>)> {
        let mut argmax_name = [CTL_KERN, KERN_ARGMAX];
        let mut argmax: c_int = 0;
        let mut argmax_size = std::mem::size_of::<c_int>();
        // SAFETY: all pointers refer to valid initialized writable storage and the query is read-only.
        if unsafe {
            sysctl(
                argmax_name.as_mut_ptr(),
                argmax_name.len() as u32,
                (&mut argmax as *mut c_int).cast(),
                &mut argmax_size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if argmax <= 0 || argmax as usize > MAX_SAFE_ARGUMENT_BYTES {
            return Err(io::Error::other("unsafe macOS process argument limit"));
        }

        let mut bytes = vec![0_u8; argmax as usize];
        let mut size = bytes.len();
        let mut args_name = [CTL_KERN, KERN_PROCARGS2, pid as c_int];
        // SAFETY: bytes is writable for `size` bytes and sysctl does not retain the pointers.
        if unsafe {
            sysctl(
                args_name.as_mut_ptr(),
                args_name.len() as u32,
                bytes.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        bytes.truncate(size);
        let argv = parse(&bytes)?;
        Ok((argv, bytes))
    }

    fn parse(bytes: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        let int_size = std::mem::size_of::<c_int>();
        if bytes.len() < int_size {
            return Err(io::Error::other("truncated macOS process arguments"));
        }
        let argc = c_int::from_ne_bytes(
            bytes[..int_size]
                .try_into()
                .map_err(|_| io::Error::other("invalid argc"))?,
        );
        if argc <= 0 || argc > 1024 {
            return Err(io::Error::other("unsafe macOS process argument count"));
        }
        let mut cursor = int_size;
        while cursor < bytes.len() && bytes[cursor] != 0 {
            cursor += 1;
        }
        while cursor < bytes.len() && bytes[cursor] == 0 {
            cursor += 1;
        }
        let mut argv = Vec::with_capacity(argc as usize);
        for _ in 0..argc {
            let start = cursor;
            while cursor < bytes.len() && bytes[cursor] != 0 {
                cursor += 1;
            }
            if start == cursor || cursor == bytes.len() {
                return Err(io::Error::other("malformed macOS process arguments"));
            }
            argv.push(bytes[start..cursor].to_vec());
            while cursor < bytes.len() && bytes[cursor] == 0 {
                cursor += 1;
            }
        }
        Ok(argv)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_reuse_isolation_hash_replacement_permissions_identity_and_argv() {
    let mut fixture = MacFixture::new([ServerSpec::new("alpha"), ServerSpec::new("beta")]);

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
    assert_private_artifacts(&alpha_paths);

    let reused_output = fixture.run(&["info", "alpha"]);
    assert_success(&reused_output);
    let reused = MetadataStore::new(alpha_paths.clone())
        .read()
        .expect("reused metadata");
    assert_eq!(reused.pid, first_metadata.pid, "worker was not reused");
    assert_eq!(fixture.observations("alpha").len(), 1);

    let beta = fixture.run(&["info", "beta"]);
    assert_success(&beta);
    let beta_paths = fixture.paths("beta");
    let beta_pid = MetadataStore::new(beta_paths.clone())
        .read()
        .expect("beta metadata")
        .pid;
    assert_ne!(first_metadata.pid, beta_pid);
    assert_ne!(alpha_paths.socket, beta_paths.socket);
    assert_private_artifacts(&beta_paths);

    let (argv, raw_arguments) = platform_argv::read(first_metadata.pid).expect("macOS argv query");
    assert_eq!(argv.len(), 2, "worker argv was not executable + __daemon");
    assert!(
        argv[0].starts_with(b"/"),
        "worker executable was not absolute"
    );
    assert_eq!(argv[1], b"__daemon");
    for secret in [ENV_SECRET, HEADER_SECRET, USER_PAYLOAD] {
        assert!(
            !raw_arguments
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "worker argv leaked {secret}"
        );
    }
    assert!(
        !raw_arguments
            .windows(fixture.config.as_os_str().as_bytes().len())
            .any(|window| window == fixture.config.as_os_str().as_bytes()),
        "worker argv leaked config path"
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
    assert_private_artifacts(&alpha_paths);

    close_worker(&alpha_paths).await;
    close_worker(&beta_paths).await;
    wait_for_absent(&alpha_paths.pid);
    wait_for_absent(&alpha_paths.socket);
    wait_for_absent(&alpha_paths.lock);
    wait_for_absent(&beta_paths.pid);
    wait_for_absent(&beta_paths.socket);
    wait_for_absent(&beta_paths.lock);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_ipc_dead_recovery_missing_socket_fallback_and_signal_cleanup() {
    let fixture = MacFixture::new([ServerSpec::new("concurrent")]);
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
    assert!(paths.pid.exists() && paths.socket.exists());
    let recovered = fixture.run(&["info", "concurrent"]);
    assert_success(&recovered);
    let replacement = MetadataStore::new(paths.clone())
        .read()
        .expect("replacement");
    assert_ne!(replacement.pid, original.pid);

    fs::remove_file(&paths.socket).expect("remove published socket");
    let missing_socket = fixture.run(&["info", "concurrent"]);
    assert_direct_fallback(&missing_socket);
    assert!(process_alive(replacement.pid));

    signal(replacement.pid, "-TERM");
    wait_for_process_exit(replacement.pid);
    wait_for_absent(&paths.pid);
    wait_for_absent(&paths.socket);
    wait_for_absent(&paths.lock);

    for (name, signal_name) in [("signal-int", "-INT"), ("signal-term", "-TERM")] {
        let signal_fixture = MacFixture::new([ServerSpec::new(name)]);
        assert_success(&signal_fixture.run(&["info", name]));
        let signal_paths = signal_fixture.paths(name);
        let worker = MetadataStore::new(signal_paths.clone())
            .read()
            .expect("signal metadata");
        let backend_pid = signal_fixture.observations(name)[0]["pid"]
            .as_u64()
            .expect("backend pid") as u32;
        signal(worker.pid, signal_name);
        wait_for_process_exit(worker.pid);
        wait_for_process_exit(backend_pid);
        wait_for_absent(&signal_paths.pid);
        wait_for_absent(&signal_paths.socket);
        wait_for_absent(&signal_paths.lock);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn operational_ping_and_request_timeouts_fallback_after_acknowledgement() {
    let mut ping_spec = ServerSpec::new("ping-timeout");
    ping_spec.ping_delay_ms = 5_500;
    let ping_fixture = MacFixture::new([ping_spec]);
    let ping = ping_fixture.run(&["info", "ping-timeout"]);
    assert_direct_fallback(&ping);
    let ping_paths = ping_fixture.paths("ping-timeout");
    assert!(!ping_paths.pid.exists() && !ping_paths.socket.exists() && !ping_paths.lock.exists());
    let ping_observations = ping_fixture.observations("ping-timeout");
    assert_eq!(ping_observations.len(), 2);
    for observation in ping_observations {
        wait_for_process_exit(observation["pid"].as_u64().expect("backend pid") as u32);
    }

    let mut call_spec = ServerSpec::new("request-timeout");
    call_spec.call_delay_ms = 5_500;
    let call_fixture = MacFixture::new([call_spec]);
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
        "timed-out daemon request reached the backend or fallback duplicated the call"
    );
    let paths = call_fixture.paths("request-timeout");
    close_worker(&paths).await;
    wait_for_absent(&paths.pid);
    wait_for_absent(&paths.socket);
    wait_for_absent(&paths.lock);
}

#[test]
fn security_errors_fail_closed_for_permissions_symlinks_unsafe_artifacts_and_unrelated_pid() {
    let permission_fixture = MacFixture::new([ServerSpec::new("socket-mode")]);
    assert_success(&permission_fixture.run(&["info", "socket-mode"]));
    let permission_paths = permission_fixture.paths("socket-mode");
    let worker_pid = MetadataStore::new(permission_paths.clone())
        .read()
        .expect("permission metadata")
        .pid;
    fs::set_permissions(&permission_paths.socket, fs::Permissions::from_mode(0o666))
        .expect("broaden socket mode");
    let output = permission_fixture.run(&["info", "socket-mode"]);
    assert_security_failure(&output);
    assert_eq!(permission_fixture.observations("socket-mode").len(), 1);
    fs::set_permissions(&permission_paths.socket, fs::Permissions::from_mode(0o600))
        .expect("restore socket mode");
    signal(worker_pid, "-TERM");
    wait_for_process_exit(worker_pid);

    let runtime_fixture = MacFixture::new([ServerSpec::new("runtime-link")]);
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

    let socket_fixture = MacFixture::new([ServerSpec::new("socket-link")]);
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

    let pid_fixture = MacFixture::new([ServerSpec::new("pid-link")]);
    let pid_paths = pid_fixture.paths("pid-link");
    let pid_target = pid_fixture.root.path().join("pid-target");
    fs::write(&pid_target, b"outside").expect("PID target");
    symlink(&pid_target, &pid_paths.pid).expect("PID symlink");
    let output = pid_fixture.run(&["info", "pid-link"]);
    assert_security_failure(&output);
    pid_fixture.assert_no_backend_started("pid-link");
    assert_eq!(fs::read(&pid_target).expect("target remains"), b"outside");

    let unsafe_fixture = MacFixture::new([ServerSpec::new("unsafe-lock")]);
    let unsafe_paths = unsafe_fixture.paths("unsafe-lock");
    fs::create_dir(&unsafe_paths.lock).expect("unsafe lock directory");
    let output = unsafe_fixture.run(&["info", "unsafe-lock"]);
    assert_security_failure(&output);
    unsafe_fixture.assert_no_backend_started("unsafe-lock");

    let unrelated_fixture = MacFixture::new([ServerSpec::new("unrelated")]);
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
    fs::set_permissions(&unrelated_paths.socket, fs::Permissions::from_mode(0o600))
        .expect("private decoy socket");
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
        "manager signalled unrelated PID"
    );
    unrelated.kill().expect("kill unrelated process");
    unrelated.wait().expect("reap unrelated process");
    drop(listener);
}
