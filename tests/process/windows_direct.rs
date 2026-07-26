#![forbid(unsafe_code)]
#![cfg(windows)]

use std::{
    ffi::OsStr,
    io::Write,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tempfile::TempDir;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(15);

fn mock_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mock-stdio-server"))
}

struct WindowsFixture {
    root: TempDir,
    config: PathBuf,
    observation: PathBuf,
}

impl WindowsFixture {
    fn new(call_result: Value) -> Self {
        let root = tempfile::tempdir().expect("isolated Windows fixture directory");
        let observation = root.path().join("observation.json");
        let script = root.path().join("script.json");
        let config = root.path().join("mcp_servers.json");
        std::fs::write(
            &script,
            serde_json::to_vec(&json!({
                "observation_path": observation,
                "instructions": "Windows direct fixture instructions.",
                "tool_pages": [{
                    "cursor": null,
                    "tools": [{
                        "name": "echo",
                        "description": "Echo arguments",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "value": {"type": "integer"}
                            }
                        }
                    }, {
                        "name": "group/read",
                        "description": "Read a group",
                        "inputSchema": {"type": "object"}
                    }]
                }],
                "call_result": call_result,
                "stderr_chunks": ["windows fixture diagnostic\n"],
                "eof_behavior": "exit"
            }))
            .expect("serialize fixture script"),
        )
        .expect("write fixture script");
        std::fs::write(
            &config,
            serde_json::to_vec(&json!({
                "mcpServers": {
                    "fixture": {
                        "command": mock_binary(),
                        "args": ["--fixture-script", script]
                    }
                }
            }))
            .expect("serialize MCP configuration"),
        )
        .expect("write MCP configuration");
        Self {
            root,
            config,
            observation,
        }
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_mcp-cli"));
        command
            .current_dir(self.root.path())
            .env("HOME", self.root.path())
            .env("USERPROFILE", self.root.path())
            .env("TMP", self.root.path())
            .env("TEMP", self.root.path())
            .env("TMPDIR", self.root.path())
            .env("XDG_RUNTIME_DIR", self.root.path())
            // These settings enable/prefer daemon behavior on Unix. Windows
            // must ignore that preference and remain direct-only.
            .env("MCP_NO_DAEMON", "0")
            .env("MCP_DAEMON_TIMEOUT", "1")
            .env("MCP_MAX_RETRIES", "0")
            .env("MCP_TIMEOUT", "10")
            .env("MCP_DEBUG", "1")
            .env("NO_COLOR", "1")
            .env_remove("MCP_CONFIG_PATH")
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn().expect("spawn Windows mcp-cli process");
        if let Some(input) = stdin {
            child
                .stdin
                .as_mut()
                .expect("piped stdin")
                .write_all(input.as_bytes())
                .expect("write command stdin");
        }
        drop(child.stdin.take());

        let deadline = Instant::now() + PROCESS_TIMEOUT;
        loop {
            if child
                .try_wait()
                .expect("poll Windows mcp-cli process")
                .is_some()
            {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("Windows mcp-cli process exceeded {PROCESS_TIMEOUT:?}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        child
            .wait_with_output()
            .expect("collect Windows mcp-cli output")
    }

    fn observation(&self) -> Value {
        serde_json::from_slice(
            &std::fs::read(&self.observation).expect("mock stdio observation file"),
        )
        .expect("mock stdio observation JSON")
    }

    fn assert_direct_only(&self, output: &Output) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("selected direct mode"), "{stderr}");
        assert!(!stderr.contains("selected daemon mode"), "{stderr}");
        assert!(!stderr.contains("daemon worker failed"), "{stderr}");
        assert_no_daemon_artifacts(self.root.path());
    }
}

fn successful_fixture() -> WindowsFixture {
    WindowsFixture::new(json!({
        "content": [{"type": "text", "text": "windows-ok"}],
        "isError": false,
        "structuredContent": {"accepted": true, "platform": "windows"},
        "x-extension": {"preserved": true}
    }))
}

fn assert_success(output: &Output) {
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(!output.stdout.contains(&0x1b), "stdout contains ANSI");
    assert!(!output.stderr.contains(&0x1b), "stderr contains ANSI");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("[server] fixture: windows fixture diagnostic"),
        "{stderr}"
    );
}

fn assert_closed(observation: &Value) {
    assert_eq!(observation["eof_seen"], true, "{observation:#}");
    assert!(
        observation["protocol_errors"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "{observation:#}"
    );
}

#[test]
fn all_public_commands_use_direct_stdio_and_keep_cross_platform_output_contract() {
    let cases: &[(&[&str], Option<&str>, &str)] = &[
        (&[], None, "fixture\n  • echo\n  • group/read\n"),
        (
            &["info", "fixture", "echo"],
            None,
            "{\"properties\":{\"value\":{\"type\":\"integer\"}},\"type\":\"object\"}\n",
        ),
        (
            &["info", "fixture/echo"],
            None,
            "{\"properties\":{\"value\":{\"type\":\"integer\"}},\"type\":\"object\"}\n",
        ),
        (&["grep", "**/READ"], None, "fixture group/read\n"),
    ];

    for (args, stdin, expected_stdout) in cases {
        let fixture = successful_fixture();
        let output = fixture.run(args, *stdin);
        assert_success(&output);
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            *expected_stdout,
            "args={args:?}"
        );
        let observation = fixture.observation();
        assert_closed(&observation);
        assert_eq!(observation["events"][0], "initialize");
        assert_eq!(observation["events"][1], "initialized");
        fixture.assert_direct_only(&output);
    }

    let fixture = successful_fixture();
    let output = fixture.run(&["info", "fixture"], None);
    assert_success(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.starts_with("Server: fixture\nTransport: stdio\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Windows direct fixture instructions."),
        "{stdout}"
    );
    assert!(stdout.find("  echo\n").unwrap() < stdout.find("  group/read\n").unwrap());
    assert_closed(&fixture.observation());
    fixture.assert_direct_only(&output);
}

#[test]
fn call_split_and_slash_syntax_emit_only_the_complete_json_result() {
    let expected = json!({
        "content": [{"type": "text", "text": "windows-ok"}],
        "isError": false,
        "structuredContent": {"accepted": true, "platform": "windows"},
        "x-extension": {"preserved": true}
    });

    for (args, stdin, expected_arguments) in [
        (
            vec!["call", "fixture", "echo", r#"{"value":7}"#],
            None,
            json!({"value": 7}),
        ),
        (
            vec!["call", "fixture/echo"],
            Some(r#"{"value":8}"#),
            json!({"value": 8}),
        ),
    ] {
        let fixture = successful_fixture();
        let output = fixture.run(&args, stdin);
        assert_success(&output);
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            expected
        );
        assert_eq!(output.stdout.last(), Some(&b'\n'));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("diagnostic"));
        let observation = fixture.observation();
        assert_eq!(
            observation.pointer("/calls/0/arguments"),
            Some(&expected_arguments)
        );
        assert_closed(&observation);
        fixture.assert_direct_only(&output);
    }
}

#[test]
fn direct_errors_stay_on_stderr_and_hidden_daemon_entry_is_not_invoked() {
    let fixture = successful_fixture();
    let unknown = fixture.run(&["info", "missing"], None);
    assert_eq!(unknown.status.code(), Some(1), "{unknown:?}");
    assert!(unknown.stdout.is_empty(), "{unknown:?}");
    let stderr = String::from_utf8_lossy(&unknown.stderr);
    assert_eq!(
        stderr.matches("Error [SERVER_NOT_FOUND]:").count(),
        1,
        "{stderr}"
    );
    assert!(
        !fixture.observation.exists(),
        "validation should not start stdio fixture"
    );
    assert_no_daemon_artifacts(fixture.root.path());

    let fixture = successful_fixture();
    let hidden = fixture.run(&["__daemon"], None);
    assert_eq!(hidden.status.code(), Some(1), "{hidden:?}");
    assert!(hidden.stdout.is_empty(), "{hidden:?}");
    let stderr = String::from_utf8_lossy(&hidden.stderr);
    assert!(stderr.contains("Error [UNKNOWN_COMMAND]:"), "{stderr}");
    assert!(!stderr.contains("daemon worker failed"), "{stderr}");
    assert!(
        !fixture.observation.exists(),
        "hidden worker must not start on Windows"
    );
    assert_no_daemon_artifacts(fixture.root.path());

    let fixture = WindowsFixture::new(json!({
        "content": [{"type": "text", "text": "failed"}],
        "isError": true
    }));
    let business = fixture.run(&["call", "fixture/echo", "{}"], None);
    assert_eq!(business.status.code(), Some(2), "{business:?}");
    assert!(business.stdout.is_empty(), "{business:?}");
    let stderr = String::from_utf8_lossy(&business.stderr);
    assert_eq!(
        stderr.matches("Error [TOOL_EXECUTION_FAILED]:").count(),
        1,
        "{stderr}"
    );
    assert_closed(&fixture.observation());
    fixture.assert_direct_only(&business);
}

fn assert_no_daemon_artifacts(root: &Path) {
    let mut pending = vec![root.to_path_buf()];
    let mut suspicious = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).expect("scan isolated fixture directory") {
            let entry = entry.expect("fixture directory entry");
            let path = entry.path();
            let file_type = entry.file_type().expect("fixture entry type");
            if file_type.is_dir() {
                pending.push(path.clone());
            }
            let name = path
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            let extension = path
                .extension()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            if name.starts_with("mcp-cli-") || matches!(extension.as_str(), "sock" | "pid" | "lock")
            {
                suspicious.push(path);
            }
        }
    }
    assert!(
        suspicious.is_empty(),
        "Windows direct mode created daemon runtime/PID/socket/lock artifacts: {suspicious:?}"
    );
}
