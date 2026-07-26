#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::Output,
    thread,
    time::{Duration, Instant},
};

use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;

fn mock_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mock-stdio-server"))
}

struct ServerSpec {
    name: &'static str,
    instructions: &'static str,
    tools: Vec<Value>,
    call_result: Value,
    stderr: &'static str,
    list_delay_ms: u64,
    allowed_tools: Vec<&'static str>,
    disabled_tools: Vec<&'static str>,
}

impl ServerSpec {
    fn new(name: &'static str, tools: Vec<Value>) -> Self {
        Self {
            name,
            instructions: "Direct CLI fixture instructions.",
            tools,
            call_result: json!({"content": [], "isError": false}),
            stderr: "fixture diagnostic\n",
            list_delay_ms: 0,
            allowed_tools: Vec::new(),
            disabled_tools: Vec::new(),
        }
    }
}

struct StdioFixture {
    temp: TempDir,
    config: PathBuf,
    observations: BTreeMap<String, PathBuf>,
}

impl StdioFixture {
    fn new(specs: Vec<ServerSpec>) -> Self {
        let temp = tempfile::tempdir().expect("isolated fixture directory");
        let config = temp.path().join("mcp_servers.json");
        let mut observations = BTreeMap::new();
        let mut servers = serde_json::Map::new();

        for spec in specs {
            let observation = temp.path().join(format!("{}-observation.json", spec.name));
            let script = temp.path().join(format!("{}-script.json", spec.name));
            std::fs::write(
                &script,
                serde_json::to_vec(&json!({
                    "observation_path": observation,
                    "instructions": spec.instructions,
                    "tool_pages": [{
                        "cursor": null,
                        "tools": spec.tools,
                    }],
                    "call_result": spec.call_result,
                    "stderr_chunks": [spec.stderr],
                    "list_response_delay_ms": spec.list_delay_ms,
                    "eof_behavior": "exit"
                }))
                .unwrap(),
            )
            .unwrap();

            let mut definition = serde_json::Map::new();
            definition.insert("command".to_owned(), json!(mock_binary()));
            definition.insert("args".to_owned(), json!(["--fixture-script", script]));
            if !spec.allowed_tools.is_empty() {
                definition.insert("allowedTools".to_owned(), json!(spec.allowed_tools));
            }
            if !spec.disabled_tools.is_empty() {
                definition.insert("disabledTools".to_owned(), json!(spec.disabled_tools));
            }
            servers.insert(spec.name.to_owned(), Value::Object(definition));
            observations.insert(spec.name.to_owned(), observation);
        }

        std::fs::write(
            &config,
            serde_json::to_vec(&json!({"mcpServers": servers})).unwrap(),
        )
        .unwrap();
        Self {
            temp,
            config,
            observations,
        }
    }

    fn command(&self) -> Command {
        isolated_command(self.temp.path(), &self.config)
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut command = self.command();
        command.args(args);
        if let Some(stdin) = stdin {
            command.write_stdin(stdin);
        }
        command.output().expect("direct CLI process")
    }

    fn observation(&self, server: &str) -> Value {
        let path = self.observations.get(server).expect("known observation");
        serde_json::from_slice(&std::fs::read(path).expect("observation file"))
            .expect("observation JSON")
    }

    fn assert_closed(&self, server: &str) {
        let observation = self.observation(server);
        assert_eq!(observation["eof_seen"], true, "{observation:#}");
        let pid = observation["pid"].as_u64().expect("fixture pid");
        assert!(!process_is_alive(pid), "fixture child {pid} remained alive");
    }

    fn was_started(&self, server: &str) -> bool {
        self.observations
            .get(server)
            .expect("known observation")
            .exists()
    }
}

struct RawFixture {
    temp: TempDir,
    config: PathBuf,
}

impl RawFixture {
    fn new(server_definition: Value) -> Self {
        let temp = tempfile::tempdir().expect("isolated raw fixture");
        let config = temp.path().join("mcp_servers.json");
        std::fs::write(
            &config,
            serde_json::to_vec(&json!({"mcpServers": {"target": server_definition}})).unwrap(),
        )
        .unwrap();
        Self { temp, config }
    }

    fn command(&self) -> Command {
        isolated_command(self.temp.path(), &self.config)
    }
}

fn isolated_command(root: &Path, config: &Path) -> Command {
    let mut command = Command::cargo_bin("mcp-cli").expect("mcp-cli binary");
    command
        .current_dir(root)
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("TMPDIR", root)
        .env("MCP_NO_DAEMON", "1")
        .env("MCP_MAX_RETRIES", "0")
        .env("MCP_TIMEOUT", "10")
        .env("NO_COLOR", "1")
        .env_remove("MCP_CONFIG_PATH")
        .env_remove("MCP_DEBUG")
        .env_remove("MCP_CONCURRENCY")
        .env_remove("MCP_RETRY_DELAY")
        .arg("--config")
        .arg(config);
    command
}

fn tool(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": {
                "value": {"type": "integer", "description": "input value"}
            }
        }
    })
}

fn assert_success(output: &Output) {
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(!output.stdout.contains(&0x1b), "stdout contains ANSI");
    assert!(!output.stderr.contains(&0x1b), "stderr contains ANSI");
}

fn assert_error(output: &Output, exit: i32, kind: &str) {
    assert_eq!(output.status.code(), Some(exit), "{output:?}");
    assert!(
        output.stdout.is_empty(),
        "error polluted stdout: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.matches("Error [").count(), 1, "{stderr}");
    assert!(stderr.contains(&format!("Error [{kind}]:")), "{stderr}");
    assert!(!stderr.contains('\u{1b}'), "{stderr}");
}

#[test]
fn list_info_and_grep_have_description_switches_stable_sorting_and_zero_result_success() {
    let alpha = ServerSpec::new(
        "alpha",
        vec![tool("z/read.2", "second"), tool("a/read.1", "first")],
    );
    let zeta = ServerSpec::new("zeta", vec![tool("GROUP/READ.X", "third")]);
    let fixture = StdioFixture::new(vec![zeta, alpha]);

    let list = fixture.run(&["--with-descriptions"], None);
    assert_success(&list);
    assert_eq!(
        String::from_utf8(list.stdout).unwrap(),
        "alpha\n  • a/read.1 - first\n  • z/read.2 - second\n\nzeta\n  • GROUP/READ.X - third\n"
    );
    let list_stderr = String::from_utf8_lossy(&list.stderr);
    assert!(list_stderr.contains("[server] alpha: fixture diagnostic"));
    assert!(list_stderr.contains("[server] zeta: fixture diagnostic"));
    fixture.assert_closed("alpha");
    fixture.assert_closed("zeta");

    let fixture = StdioFixture::new(vec![ServerSpec::new(
        "alpha",
        vec![tool("z/read.2", "second"), tool("a/read.1", "first")],
    )]);
    let info = fixture.run(&["info", "alpha", "--with-descriptions"], None);
    assert_success(&info);
    let info_stdout = String::from_utf8(info.stdout).unwrap();
    assert!(info_stdout.starts_with("Server: alpha\nTransport: stdio\n"));
    assert!(info_stdout.contains("Instructions:\n  Direct CLI fixture instructions."));
    assert!(info_stdout.find("  a/read.1\n").unwrap() < info_stdout.find("  z/read.2\n").unwrap());
    assert!(info_stdout.contains("    first"));
    assert!(info_stdout.contains("input value"));
    fixture.assert_closed("alpha");

    let fixture = StdioFixture::new(vec![
        ServerSpec::new("zeta", vec![tool("GROUP/READ.X", "third")]),
        ServerSpec::new(
            "alpha",
            vec![tool("z/read.2", "second"), tool("a/read.1", "first")],
        ),
    ]);
    let grep = fixture.run(&["grep", "**/READ.?", "-d"], None);
    assert_success(&grep);
    assert_eq!(
        String::from_utf8(grep.stdout).unwrap(),
        "alpha a/read.1 - first\nalpha z/read.2 - second\nzeta GROUP/READ.X - third\n"
    );
    assert!(!grep.stderr.is_empty());
    fixture.assert_closed("alpha");
    fixture.assert_closed("zeta");

    let fixture = StdioFixture::new(vec![ServerSpec::new(
        "alpha",
        vec![tool("write_file", "writes")],
    )]);
    let no_results = fixture.run(&["grep", "read_*"], None);
    assert_success(&no_results);
    assert_eq!(no_results.stdout, b"No matching tools found.\n");
    fixture.assert_closed("alpha");
}

#[test]
fn call_inline_and_stdin_preserve_complete_json_and_pipeline_parseability() {
    let complete = json!({
        "content": [{"type": "text", "text": "complete"}],
        "isError": false,
        "structuredContent": {"accepted": true, "nested": [null, 7]},
        "x-extension": {"preserved": true}
    });
    let mut spec = ServerSpec::new("target", vec![tool("echo", "echo arguments")]);
    spec.call_result = complete.clone();
    spec.stderr = "diagnostic only\n";
    let fixture = StdioFixture::new(vec![spec]);

    let inline = fixture.run(
        &["call", "target", "echo", r#"{"source":"inline"}"#],
        Some(r#"{"source":"stdin"}"#),
    );
    assert_success(&inline);
    assert_eq!(
        serde_json::from_slice::<Value>(&inline.stdout).unwrap(),
        complete
    );
    assert!(!String::from_utf8_lossy(&inline.stdout).contains("diagnostic"));
    let stderr = String::from_utf8_lossy(&inline.stderr);
    assert!(stderr.contains("[server] target: diagnostic only"));
    let observation = fixture.observation("target");
    assert_eq!(
        observation.pointer("/calls/0/arguments"),
        Some(&json!({"source": "inline"}))
    );
    fixture.assert_closed("target");

    let mut spec = ServerSpec::new("target", vec![tool("echo", "echo arguments")]);
    spec.call_result = complete.clone();
    let fixture = StdioFixture::new(vec![spec]);
    let stdin = fixture.run(&["call", "target/echo"], Some(r#"{"source":"stdin"}"#));
    assert_success(&stdin);
    let parsed_once: Value = serde_json::from_slice(&stdin.stdout).unwrap();
    let pipeline_bytes = serde_json::to_vec(&parsed_once).unwrap();
    let parsed_twice: Value = serde_json::from_slice(&pipeline_bytes).unwrap();
    assert_eq!(
        parsed_twice, complete,
        "serde_json simulates a call | jq consumer"
    );
    assert_eq!(
        fixture.observation("target").pointer("/calls/0/arguments"),
        Some(&json!({"source": "stdin"}))
    );
    fixture.assert_closed("target");

    let mut spec = ServerSpec::new("target", vec![tool("echo", "echo arguments")]);
    spec.call_result = complete;
    let fixture = StdioFixture::new(vec![spec]);
    let whitespace = fixture.run(&["call", "target", "echo"], Some(" \t\r\n"));
    assert_success(&whitespace);
    assert_eq!(
        fixture.observation("target").pointer("/calls/0/arguments"),
        Some(&json!({}))
    );
    fixture.assert_closed("target");
}

#[test]
fn call_validation_unknown_and_disabled_targets_do_not_start_or_call_servers() {
    let mut spec = ServerSpec::new("target", vec![tool("echo", "echo arguments")]);
    spec.allowed_tools = vec!["*"];
    spec.disabled_tools = vec!["danger"];
    let fixture = StdioFixture::new(vec![spec]);

    let unknown_server = fixture.run(&["call", "missing", "echo", "{}"], None);
    assert_error(&unknown_server, 1, "SERVER_NOT_FOUND");
    assert!(!fixture.was_started("target"));

    let disabled = fixture.run(&["call", "target", "danger", "{}"], None);
    assert_error(&disabled, 1, "TOOL_DISABLED");
    assert!(!fixture.was_started("target"));

    let invalid_json = fixture.run(&["call", "target", "echo", "{\n  \"value\": ]"], None);
    assert_error(&invalid_json, 1, "INVALID_JSON");
    let stderr = String::from_utf8_lossy(&invalid_json.stderr);
    assert!(stderr.contains("line 2"), "{stderr}");
    assert!(stderr.contains("column"), "{stderr}");
    assert!(!fixture.was_started("target"));

    for malformed in [
        r#"{path:"test"}"#,
        "path=./README.md",
        r#"{"path": test}"#,
        r#"{"path": "test",}"#,
        "{'path': 'test'}",
        "just plain text",
    ] {
        let fixture = StdioFixture::new(vec![ServerSpec::new(
            "target",
            vec![tool("echo", "echo arguments")],
        )]);
        let output = fixture.run(&["call", "target", "echo", malformed], None);
        assert_error(&output, 1, "INVALID_JSON");
        assert!(
            !fixture.was_started("target"),
            "malformed JSON must be rejected before connecting: {malformed:?}"
        );
    }

    let non_object = fixture.run(&["call", "target", "echo", "[]"], None);
    assert_error(&non_object, 1, "INVALID_ARGUMENTS");
    assert!(!fixture.was_started("target"));

    let unknown_tool = fixture.run(&["call", "target", "missing", "{}"], None);
    assert_error(&unknown_tool, 1, "TOOL_NOT_FOUND");
    assert_eq!(
        fixture.observation("target")["calls"],
        json!([]),
        "rejected tool must never be called"
    );
    fixture.assert_closed("target");
}

#[test]
fn business_error_uses_exit_two_and_never_writes_result_json_to_stdout() {
    let mut spec = ServerSpec::new("target", vec![tool("echo", "echo arguments")]);
    spec.call_result = json!({
        "content": [{"type": "text", "text": "failed"}],
        "isError": true,
        "structuredContent": {"reason": "business"}
    });
    let fixture = StdioFixture::new(vec![spec]);

    let output = fixture.run(&["call", "target", "echo", "{}"], None);

    assert_error(&output, 2, "TOOL_EXECUTION_FAILED");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("structuredContent"));
    assert_eq!(
        fixture.observation("target")["calls"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    fixture.assert_closed("target");
}

#[test]
fn network_timeout_and_auth_failures_keep_stable_exit_kind_status_and_cleanup() {
    let network = RawFixture::new(json!({
        "command": "/definitely/missing/mcp-cli-task-6-15-server"
    }));
    let output = network.command().args(["info", "target"]).output().unwrap();
    assert_error(&output, 3, "NETWORK_ERROR");

    let mut timeout_spec = ServerSpec::new("target", vec![tool("echo", "echo arguments")]);
    timeout_spec.list_delay_ms = 3_000;
    let timeout = StdioFixture::new(vec![timeout_spec]);
    let output = timeout
        .command()
        .env("MCP_TIMEOUT", "1")
        .args(["info", "target"])
        .output()
        .unwrap();
    assert_error(&output, 3, "TIMEOUT");
    let observation = timeout.observation("target");
    let pid = observation["pid"].as_u64().unwrap();
    assert!(
        !process_is_alive(pid),
        "timed-out child {pid} remained alive"
    );

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("random loopback listener");
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "CLI never connected to auth fixture"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("auth fixture accept failed: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = stream.read(&mut chunk).expect("read auth request");
            if count == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..count]);
        }
        assert!(request.starts_with(b"POST "), "{request:?}");
        stream
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
    });
    let auth = RawFixture::new(json!({"url": format!("http://{address}/mcp")}));
    let output = auth.command().args(["info", "target"]).output().unwrap();
    server.join().expect("auth fixture thread");
    assert_error(&output, 4, "AUTH_ERROR");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("401"), "{stderr}");
    assert!(stderr.contains("target"), "{stderr}");
}

fn process_is_alive(pid: u64) -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
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
