#![forbid(unsafe_code)]

use std::{path::PathBuf, process::Output};

use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;

fn mock_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mock-stdio-server"))
}

struct Fixture {
    _temp: TempDir,
    config: PathBuf,
    observation: PathBuf,
}

impl Fixture {
    fn new(call_result: Value, list_delay_ms: u64) -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let observation = temp.path().join("observation.json");
        let script = temp.path().join("script.json");
        let config = temp.path().join("mcp_servers.json");
        std::fs::write(
            &script,
            serde_json::to_vec(&json!({
                "observation_path": observation,
                "capture_env": ["DISPATCH_SECRET"],
                "instructions": "Dispatcher fixture instructions.",
                "tool_pages": [{
                    "cursor": null,
                    "tools": [{
                        "name": "echo",
                        "description": "Echo arguments",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"value": {"type": "integer"}}
                        }
                    }]
                }],
                "call_result": call_result,
                "stderr_chunks": ["fixture diagnostic dispatch-secret-value\n"],
                "list_response_delay_ms": list_delay_ms,
                "eof_behavior": "exit"
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            &config,
            serde_json::to_vec(&json!({
                "mcpServers": {
                    "fixture": {
                        "command": mock_binary(),
                        "args": ["--fixture-script", script],
                        "env": {"DISPATCH_SECRET": "dispatch-secret-value"}
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        Self {
            _temp: temp,
            config,
            observation,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("mcp-cli").expect("mcp-cli binary");
        command
            .env("MCP_NO_DAEMON", "1")
            .env("MCP_MAX_RETRIES", "0")
            .env("NO_COLOR", "1")
            .env_remove("MCP_CONFIG_PATH")
            .arg("--config")
            .arg(&self.config);
        command
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut command = self.command();
        command.args(args);
        if let Some(stdin) = stdin {
            command.write_stdin(stdin);
        }
        command.output().expect("direct CLI process")
    }

    fn observation(&self) -> Value {
        serde_json::from_slice(&std::fs::read(&self.observation).expect("observation file"))
            .expect("observation JSON")
    }
}

fn successful_fixture() -> Fixture {
    Fixture::new(
        json!({
            "content": [{"type": "text", "text": "ok"}],
            "isError": false,
            "structuredContent": {"accepted": true},
            "x-extension": {"preserved": true}
        }),
        0,
    )
}

fn assert_success(output: &Output) {
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("[server] fixture:"), "{stderr}");
    assert!(stderr.contains("[REDACTED]"), "{stderr}");
    assert!(!stderr.contains("dispatch-secret-value"), "{stderr}");
    assert!(!output.stdout.contains(&0x1b));
}

#[test]
fn real_binary_routes_list_three_info_forms_grep_and_two_call_inputs() {
    let cases: &[(&[&str], Option<&str>, &str)] = &[
        (&[], None, "fixture\n  • echo\n"),
        (&["fixture"], None, "Server: fixture\n"),
        (&["info", "fixture"], None, "Server: fixture\n"),
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
        (&["grep", "e*"], None, "fixture echo\n"),
        (
            &["call", "fixture", "echo", "{\"value\":7}"],
            None,
            "{\"content\":[{\"text\":\"ok\",\"type\":\"text\"}],\"isError\":false,\"structuredContent\":{\"accepted\":true},\"x-extension\":{\"preserved\":true}}\n",
        ),
        (
            &["call", "fixture/echo"],
            Some("{\"value\":8}"),
            "{\"content\":[{\"text\":\"ok\",\"type\":\"text\"}],\"isError\":false,\"structuredContent\":{\"accepted\":true},\"x-extension\":{\"preserved\":true}}\n",
        ),
    ];

    for (args, stdin, expected_stdout) in cases {
        let fixture = successful_fixture();
        let output = fixture.run(args, *stdin);
        assert_success(&output);
        let stdout = String::from_utf8(output.stdout).unwrap();
        if expected_stdout == &"Server: fixture\n" {
            assert!(
                stdout.starts_with(expected_stdout),
                "args={args:?}: {stdout}"
            );
            assert!(stdout.contains("Dispatcher fixture instructions."));
        } else {
            assert_eq!(&stdout, expected_stdout, "args={args:?}");
        }
        let observation = fixture.observation();
        assert_eq!(
            observation["events"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|event| event.as_str() == Some("initialize"))
                .count(),
            1,
            "configuration/connection path must initialize once"
        );
        assert_eq!(
            observation["eof_seen"], true,
            "child must be closed and reaped"
        );
    }
}

#[test]
fn help_and_version_have_no_runtime_or_configuration_side_effects() {
    for argument in ["--help", "--version"] {
        let missing = tempfile::tempdir().unwrap().path().join("missing.json");
        let mut command = Command::cargo_bin("mcp-cli").unwrap();
        let output = command
            .env("MCP_TIMEOUT", "not-a-number")
            .args(["--config"])
            .arg(missing)
            .arg(argument)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{argument}");
        assert!(!output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
}

fn process_is_alive(pid: u64) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        std::process::Command::new("ps")
            .args(["-p", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(windows)]
    {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .expect("tasklist");
        String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
    }
}

#[cfg(unix)]
#[test]
fn sigint_cancels_direct_work_cleans_child_and_exits_130_without_structured_error() {
    use std::{process::Stdio, thread, time::Duration};

    let fixture = Fixture::new(json!({"content": []}), 30_000);
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_mcp-cli"))
        .env("MCP_NO_DAEMON", "1")
        .env("MCP_MAX_RETRIES", "0")
        .env("MCP_TIMEOUT", "60")
        .env("NO_COLOR", "1")
        .args(["--config"])
        .arg(&fixture.config)
        .args(["info", "fixture"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mcp-cli");

    let mut fixture_pid = None;
    for _ in 0..500 {
        if let Ok(bytes) = std::fs::read(&fixture.observation)
            && let Ok(observation) = serde_json::from_slice::<Value>(&bytes)
            && observation["events"]
                .as_array()
                .is_some_and(|events| events.iter().any(|event| event == "tools/list"))
        {
            fixture_pid = observation["pid"].as_u64();
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let fixture_pid = fixture_pid.expect("fixture reached tools/list");
    let status = std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("send SIGINT");
    assert!(status.success());

    let output = child.wait_with_output().expect("wait mcp-cli");
    assert_eq!(output.status.code(), Some(130), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Error ["));
    assert!(
        !process_is_alive(fixture_pid),
        "cancelled fixture process {fixture_pid} remained"
    );
}

#[test]
fn main_boundary_preserves_client_tool_and_timeout_exit_codes_once() {
    let fixture = successful_fixture();
    let client = fixture.run(&["info", "missing"], None);
    assert_eq!(client.status.code(), Some(1));
    assert!(client.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&client.stderr)
            .matches("Error [")
            .count(),
        1
    );

    let business = Fixture::new(
        json!({
            "content": [{"type": "text", "text": "failed"}],
            "isError": true
        }),
        0,
    )
    .run(&["call", "fixture/echo", "{}"], None);
    assert_eq!(business.status.code(), Some(2));
    assert!(business.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&business.stderr)
            .matches("Error [")
            .count(),
        1
    );

    let timeout_fixture = Fixture::new(json!({"content": []}), 2_000);
    let mut command = timeout_fixture.command();
    let timeout = command
        .env("MCP_TIMEOUT", "1")
        .args(["info", "fixture"])
        .output()
        .unwrap();
    assert_eq!(timeout.status.code(), Some(3), "{timeout:?}");
    assert!(timeout.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&timeout.stderr);
    assert_eq!(stderr.matches("Error [TIMEOUT]:").count(), 1, "{stderr}");
    let observation = timeout_fixture.observation();
    let pid = observation["pid"].as_u64().expect("fixture pid");
    assert!(
        !process_is_alive(pid),
        "timed-out fixture process {pid} remained"
    );
}
