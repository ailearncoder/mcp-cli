#![cfg(unix)]

use std::{
    collections::BTreeMap, fs, os::unix::fs::PermissionsExt, path::Path, sync::Arc, time::Duration,
};

use mcp_cli::{
    CancellationFlag, CommandContext, DiagnosticSink, ServerDefinition, SystemClock,
    ToolFilterConfig, TransportConfig,
    config::{config_hash, server_id},
    daemon::{
        DaemonPaths, MetadataStore,
        worker::{CurrentExecutableDaemonSpawner, DaemonSpawnError, DaemonSpawner},
    },
    runtime::Deadline,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::timeout,
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
        deadline: Deadline::after(&SystemClock, Duration::from_secs(15)),
        cancellation: Arc::new(CancellationFlag::default()),
        diagnostics: Arc::new(NullDiagnostics),
    }
}

fn server(fixture: &Path, script: &Path) -> ServerDefinition {
    let name = "spawn-fixture";
    let raw = json!({
        "command": fixture,
        "args": ["--fixture-script", script],
        "env": {"WORKER_BOOTSTRAP_SECRET": "secret-only-on-stdin"}
    });
    ServerDefinition {
        name: name.to_owned(),
        id: server_id(name),
        config_hash: config_hash(&raw),
        transport: TransportConfig::Stdio {
            command: fixture.to_string_lossy().into_owned(),
            args: vec![
                "--fixture-script".to_owned(),
                script.to_string_lossy().into_owned(),
            ],
            env: BTreeMap::from([(
                "WORKER_BOOTSTRAP_SECRET".to_owned(),
                "secret-only-on-stdin".to_owned(),
            )]),
            cwd: None,
        },
        filter: ToolFilterConfig::default(),
    }
}

async fn response_line(reader: &mut BufReader<UnixStream>) -> Value {
    let mut line = String::new();
    timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .expect("response timeout")
        .expect("response read");
    serde_json::from_str(line.trim_end()).expect("response JSON")
}

#[tokio::test]
async fn current_executable_worker_uses_stdin_and_publishes_ready_atomically() {
    // Keep the full hashed socket path within sockaddr_un::sun_path even when
    // the ambient TMPDIR is long (for example, in CI).
    let root = tempfile::Builder::new()
        .prefix("m")
        .rand_bytes(2)
        .tempdir_in("/tmp")
        .expect("isolated runtime");
    let observation = root.path().join("observation.json");
    let script = root.path().join("script.json");
    fs::write(
        &script,
        serde_json::to_vec(&json!({
            "observation_path": observation,
            "capture_env": ["WORKER_BOOTSTRAP_SECRET"],
            "instructions": "daemon fixture",
            "tool_pages": [{"tools": []}],
            "call_result": {"content": []}
        }))
        .expect("script JSON"),
    )
    .expect("write script");

    let definition = server(Path::new(env!("CARGO_BIN_EXE_mock-stdio-server")), &script);
    let paths =
        DaemonPaths::from_runtime_parent(root.path(), &definition.id).expect("daemon paths");
    let spawner = CurrentExecutableDaemonSpawner::with_executable(
        Path::new(env!("CARGO_BIN_EXE_mcp-cli")).to_path_buf(),
        Duration::from_secs(30),
    );

    let mut ready = spawner
        .spawn(&context(), &definition, &paths)
        .await
        .expect("worker ready");
    let metadata = MetadataStore::new(paths.clone()).read().expect("metadata");
    assert_eq!(metadata.pid, ready.pid());
    assert_eq!(metadata.config_hash, definition.config_hash);
    assert!(paths.socket.exists());
    assert_eq!(
        fs::metadata(&paths.pid).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    assert_eq!(
        fs::metadata(&paths.lock).unwrap().permissions().mode() & 0o7777,
        0o600
    );

    #[cfg(target_os = "linux")]
    {
        let cmdline = fs::read(format!("/proc/{}/cmdline", ready.pid())).expect("worker cmdline");
        let environment =
            fs::read(format!("/proc/{}/environ", ready.pid())).expect("worker environment");
        assert!(
            !cmdline
                .windows(b"secret-only-on-stdin".len())
                .any(|part| part == b"secret-only-on-stdin")
        );
        assert!(
            !cmdline
                .windows(definition.name.len())
                .any(|part| part == definition.name.as_bytes())
        );
        assert_eq!(
            cmdline
                .split(|byte| *byte == 0)
                .filter(|part| !part.is_empty())
                .count(),
            2
        );
        assert!(cmdline.ends_with(b"__daemon\0"));
        assert!(environment.is_empty(), "worker environment must be empty");
    }

    let duplicate = spawner.spawn(&context(), &definition, &paths).await;
    assert!(matches!(
        duplicate,
        Err(DaemonSpawnError::WorkerExitedBeforeReady) | Err(DaemonSpawnError::TransferBootstrap)
    ));
    assert_eq!(
        MetadataStore::new(paths.clone()).read().unwrap().pid,
        ready.pid()
    );

    let stream = UnixStream::connect(&paths.socket)
        .await
        .expect("IPC connect");
    let mut reader = BufReader::new(stream);
    reader
        .get_mut()
        .write_all(b"{\"id\":\"ready\",\"type\":\"ping\"}\n")
        .await
        .expect("ping");
    let ping = response_line(&mut reader).await;
    assert_eq!(ping["id"], "ready");
    assert_eq!(ping["success"], true);
    reader
        .get_mut()
        .write_all(b"{\"id\":\"close\",\"type\":\"close\"}\n")
        .await
        .expect("close");
    assert_eq!(response_line(&mut reader).await["id"], "close");
    drop(reader);

    let status = timeout(Duration::from_secs(5), ready.child_mut().wait())
        .await
        .expect("worker exit timeout")
        .expect("worker wait");
    assert!(status.success());
    assert!(!paths.socket.exists());
    assert!(!paths.pid.exists());
    assert!(!paths.lock.exists());

    let observed: Value = serde_json::from_slice(&fs::read(observation).expect("observation"))
        .expect("observation JSON");
    assert_eq!(observed["events"][0], "initialize");
    assert_eq!(observed["events"][1], "initialized");
    assert_eq!(
        observed["env"]["WORKER_BOOTSTRAP_SECRET"],
        "secret-only-on-stdin"
    );
}
