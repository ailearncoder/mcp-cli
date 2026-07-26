#![forbid(unsafe_code)]

#[path = "../support/mod.rs"]
mod support;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use mcp_cli::{
    CancellationFlag, CommandContext, ConfigHash, ConnectionManager, ConnectionResourceRegistry,
    DirectConnectionManager, DirectConnector, McpConnection, SecretSet, ServerDefinition, ServerId,
    ToolFilterConfig, TransportConfig, WriterDiagnosticSink,
    connection::rmcp_adapter::RmcpDirectConnector,
};
use serde_json::{Value, json};
use support::MemoryWriter;
use tempfile::TempDir;

const SERVER_NAME: &str = "scripted-local";
const SECRET: &str = "configured-super-secret";

fn mock_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mock-stdio-server"))
}

fn write_script(
    temp: &TempDir,
    name: &str,
    eof_behavior: &str,
    list_response_delay_ms: u64,
) -> (PathBuf, PathBuf) {
    let observation = temp.path().join(format!("{name}-observation.json"));
    let script = temp.path().join(format!("{name}-script.json"));
    let value = json!({
        "observation_path": observation,
        "capture_env": ["HOME", "PATH", "FIXTURE_ONLY"],
        "instructions": "Use this fixture only for local transport tests.",
        "tool_pages": [
            {
                "cursor": null,
                "tools": [{
                    "name": "alpha",
                    "description": "first page",
                    "inputSchema": {"type": "object", "properties": {}}
                }],
                "next_cursor": "page-2"
            },
            {
                "cursor": "page-2",
                "tools": [{
                    "name": "omega",
                    "description": "second page",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"value": {"type": "integer"}},
                        "required": ["value"]
                    }
                }]
            }
        ],
        "call_result": {
            "content": [{"type": "text", "text": "scripted business failure"}],
            "isError": true,
            "structuredContent": {"accepted": false},
            "vendorExtension": {"traceId": "trace-stdio", "future": [null, 7]}
        },
        "stderr_chunks": ["diagnostic before configured-super-", "secret after\n"],
        "list_response_delay_ms": list_response_delay_ms,
        "eof_behavior": eof_behavior
    });
    std::fs::write(&script, serde_json::to_vec(&value).unwrap()).unwrap();
    (script, observation)
}

fn server(script: &Path, cwd: &Path) -> ServerDefinition {
    ServerDefinition {
        name: SERVER_NAME.to_owned(),
        id: ServerId("a".repeat(64)),
        config_hash: ConfigHash([7; 32]),
        transport: TransportConfig::Stdio {
            command: mock_binary().to_string_lossy().into_owned(),
            args: vec![
                "--fixture-script".to_owned(),
                script.to_string_lossy().into_owned(),
                "--".to_owned(),
                "*.fixture".to_owned(),
                "$(touch should-not-exist)".to_owned(),
                "semi;colon".to_owned(),
                "$HOME".to_owned(),
            ],
            env: BTreeMap::from([
                ("PATH".to_owned(), "configured-path-wins".to_owned()),
                ("FIXTURE_ONLY".to_owned(), SECRET.to_owned()),
            ]),
            cwd: Some(cwd.to_path_buf()),
        },
        filter: ToolFilterConfig::default(),
    }
}

fn context(writer: &MemoryWriter) -> CommandContext {
    context_with_cancellation(
        writer,
        Arc::new(CancellationFlag::default()),
        Duration::from_secs(15),
    )
}

fn context_with_cancellation(
    writer: &MemoryWriter,
    cancellation: Arc<CancellationFlag>,
    timeout: Duration,
) -> CommandContext {
    let mut secrets = SecretSet::new();
    secrets.register_env("FIXTURE_ONLY", SECRET);
    CommandContext {
        deadline: mcp_cli::Deadline::new(Instant::now() + timeout),
        cancellation,
        diagnostics: Arc::new(WriterDiagnosticSink::new(writer.clone(), true, secrets)),
    }
}

async fn bounded<T>(future: impl Future<Output = T>, operation: &str) -> T {
    tokio::time::timeout(Duration::from_secs(12), future)
        .await
        .unwrap_or_else(|_| panic!("{operation} exceeded its bounded test deadline"))
}

async fn read_observation_when(path: &Path, predicate: impl Fn(&Value) -> bool) -> Value {
    bounded(
        async {
            loop {
                if let Ok(bytes) = tokio::fs::read(path).await
                    && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
                    && predicate(&value)
                {
                    return value;
                }
                tokio::task::yield_now().await;
            }
        },
        "fixture observation polling",
    )
    .await
}

#[cfg(target_os = "linux")]
fn process_state(pid: u64) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let suffix = stat.rsplit_once(") ")?.1;
    suffix.chars().next()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_state(pid: u64) -> Option<char> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "state="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .chars()
        .next()
}

#[cfg(windows)]
fn process_state(pid: u64) -> Option<char> {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.contains(&format!("\"{pid}\"")).then_some('R')
}

async fn assert_process_reaped(pid: u64) {
    bounded(
        async {
            loop {
                if process_state(pid).is_none() {
                    return;
                }
                tokio::task::yield_now().await;
            }
        },
        "child process reaping",
    )
    .await;
    assert_eq!(process_state(pid), None, "child or zombie remained: {pid}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_stdio_transport_preserves_launch_protocol_results_and_diagnostics() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("working-directory");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::write(cwd.join("would-expand.fixture"), b"marker").unwrap();
    let (script, observation_path) = write_script(&temp, "normal", "exit", 0);
    let diagnostics = MemoryWriter::default();
    let ctx = context(&diagnostics);

    let connection = bounded(
        RmcpDirectConnector.connect(&ctx, &server(&script, &cwd)),
        "stdio connect",
    )
    .await
    .expect("real stdio connection should initialize");
    assert_eq!(
        connection.instructions(),
        Some("Use this fixture only for local transport tests.")
    );

    let tools = bounded(connection.list_tools(&ctx), "paginated tools/list")
        .await
        .expect("all tool pages should be collected");
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "omega"]
    );
    assert_eq!(tools[1].input_schema["required"], json!(["value"]));

    let result = bounded(
        connection.call_tool(
            &ctx,
            "omega",
            serde_json::Map::from_iter([("value".to_owned(), json!(42))]),
        ),
        "tools/call",
    )
    .await
    .expect("business errors are complete MCP results, not transport failures");
    assert_eq!(result["isError"], true);
    assert_eq!(result["vendorExtension"]["traceId"], "trace-stdio");
    assert_eq!(result["vendorExtension"]["future"], json!([null, 7]));

    bounded(connection.close(&ctx), "normal stdio close")
        .await
        .expect("normal EOF close should succeed");

    let observation = read_observation_when(&observation_path, |value| {
        value["eof_seen"] == true
            && value["events"]
                .as_array()
                .is_some_and(|events| events.len() >= 6)
    })
    .await;
    let pid = observation["pid"].as_u64().unwrap();
    assert_process_reaped(pid).await;

    let observed_cwd = observation["cwd"]
        .as_str()
        .expect("fixture cwd should be a string");
    assert_eq!(
        std::fs::canonicalize(observed_cwd).expect("canonical observed cwd"),
        std::fs::canonicalize(&cwd).expect("canonical configured cwd")
    );
    assert_eq!(observation["env"]["PATH"], "configured-path-wins");
    assert_eq!(observation["env"]["FIXTURE_ONLY"], SECRET);
    assert_eq!(
        observation["env"]["HOME"],
        std::env::var("HOME")
            .map(Value::String)
            .unwrap_or(Value::Null)
    );
    assert_eq!(
        observation["passthrough"],
        json!([
            "*.fixture",
            "$(touch should-not-exist)",
            "semi;colon",
            "$HOME"
        ])
    );
    assert_eq!(
        observation["argv"],
        json!([
            "--fixture-script",
            script.to_string_lossy(),
            "--",
            "*.fixture",
            "$(touch should-not-exist)",
            "semi;colon",
            "$HOME"
        ])
    );
    assert!(!cwd.join("should-not-exist").exists());
    assert_eq!(
        observation["events"],
        json!([
            "initialize",
            "initialized",
            "tools/list",
            "tools/list",
            "tools/call",
            "eof"
        ])
    );
    assert_eq!(observation["list_cursors"], json!([null, "page-2"]));
    assert_eq!(observation["calls"][0]["name"], "omega");
    assert_eq!(observation["calls"][0]["arguments"], json!({"value": 42}));
    assert_eq!(observation["protocol_errors"], json!([]));

    let stderr = diagnostics.string();
    assert!(stderr.contains("[server] scripted-local:"));
    assert!(stderr.contains("diagnostic before"));
    assert!(stderr.contains("[REDACTED]"));
    assert!(!stderr.contains(SECRET));
    assert!(!result.to_string().contains("diagnostic before"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stubborn_stdio_fixture_is_killed_waited_and_leaves_no_child_or_zombie() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("stubborn-working-directory");
    std::fs::create_dir(&cwd).unwrap();
    let (script, observation_path) = write_script(&temp, "stubborn", "ignore", 0);
    let diagnostics = MemoryWriter::default();
    let ctx = context(&diagnostics);

    let registry = ConnectionResourceRegistry::new();
    let manager =
        DirectConnectionManager::single_target(Arc::new(RmcpDirectConnector), registry.clone());
    let connection: Box<dyn McpConnection> = bounded(
        manager.acquire(&ctx, &server(&script, &cwd)),
        "stubborn stdio connect",
    )
    .await
    .expect("stubborn fixture should still initialize");
    assert_eq!(registry.active_resource_count(), 1);
    let started = read_observation_when(&observation_path, |value| {
        value["events"] == json!(["initialize", "initialized"])
    })
    .await;
    let pid = started["pid"].as_u64().unwrap();
    assert!(
        process_state(pid).is_some(),
        "fixture must be alive before close"
    );

    // The server deliberately keeps stdout/stderr open after stdin EOF. The
    // adapter may report its transport grace timeout, but must still kill and
    // wait for the owned child before returning.
    let close_result = bounded(connection.close(&ctx), "forced stdio close").await;
    if let Err(error) = close_result {
        assert!(
            error
                .message()
                .contains("timed out closing MCP stdio transport")
        );
    }
    assert_eq!(
        registry.active_resource_count(),
        0,
        "child, pipes, stderr task, and connection registration must release together"
    );
    assert_process_reaped(pid).await;
    let final_observation =
        read_observation_when(&observation_path, |value| value["eof_seen"] == true).await;
    assert_eq!(final_observation["protocol_errors"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_during_stdio_request_kills_reaps_tasks_and_releases_registry() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("cancelled-working-directory");
    std::fs::create_dir(&cwd).unwrap();
    let (script, observation_path) = write_script(&temp, "cancelled-request", "ignore", 30_000);
    let diagnostics = MemoryWriter::default();
    let cancellation = Arc::new(CancellationFlag::default());
    let ctx =
        context_with_cancellation(&diagnostics, cancellation.clone(), Duration::from_secs(15));
    let registry = ConnectionResourceRegistry::new();
    let manager =
        DirectConnectionManager::single_target(Arc::new(RmcpDirectConnector), registry.clone());
    let connection = bounded(
        manager.acquire(&ctx, &server(&script, &cwd)),
        "cancelled stdio connect",
    )
    .await
    .expect("stdio fixture should initialize");
    let started = read_observation_when(&observation_path, |value| {
        value["events"] == json!(["initialize", "initialized"])
    })
    .await;
    let pid = started["pid"].as_u64().unwrap();
    assert_eq!(registry.active_resource_count(), 1);

    let (result, ()) = tokio::join!(connection.list_tools(&ctx), async {
        read_observation_when(&observation_path, |value| {
            value["events"]
                .as_array()
                .is_some_and(|events| events.iter().any(|event| event == "tools/list"))
        })
        .await;
        cancellation.cancel();
    });

    assert!(result.expect_err("cancelled stdio request").is_cancelled());
    assert_eq!(
        registry.active_resource_count(),
        0,
        "automatic cleanup must release the child, pipes, stderr task, and registration"
    );
    assert_process_reaped(pid).await;
    assert!(!diagnostics.string().contains(SECRET));
}
