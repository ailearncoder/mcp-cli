#![forbid(unsafe_code)]

#[path = "../support/mod.rs"]
mod support;

use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{ExitStatus, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use support::{
    CapturedRequest, MockHttpScript, MockHttpServer, MockResponse, RequestMatcher, ScriptedResponse,
};
use tempfile::TempDir;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(20);
const SERVER_TIMEOUT: Duration = Duration::from_secs(5);
const SESSION_ID: &str = "process-session";
const HTTP_SECRET: &str = "process-http-secret";
const UNSET_ENV: &str = "<unset>";

#[derive(Clone, Copy, Debug)]
enum Mode {
    Direct,
    #[cfg(unix)]
    Daemon,
}

struct IsolatedRoot {
    temp: TempDir,
}

impl IsolatedRoot {
    fn new() -> Self {
        Self {
            temp: tempfile::tempdir().expect("isolated process root"),
        }
    }

    fn path(&self) -> &Path {
        self.temp.path()
    }
}

#[cfg(unix)]
impl Drop for IsolatedRoot {
    fn drop(&mut self) {
        use std::net::Shutdown;
        use std::os::unix::net::UnixStream;

        let Ok(entries) = std::fs::read_dir(self.path()) else {
            return;
        };
        let runtime_dirs = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("mcp-cli-"))
            })
            .collect::<Vec<_>>();

        for runtime in &runtime_dirs {
            if let Ok(entries) = std::fs::read_dir(runtime) {
                for socket in entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "sock"))
                {
                    if let Ok(mut stream) = UnixStream::connect(socket) {
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                        let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
                        let _ =
                            stream.write_all(b"{\"id\":\"process-cleanup\",\"type\":\"close\"}\n");
                        let _ = stream.shutdown(Shutdown::Write);
                        let mut response = [0_u8; 256];
                        let _ = stream.read(&mut response);
                    }
                }
            }
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        while runtime_dirs.iter().any(|runtime| {
            std::fs::read_dir(runtime).is_ok_and(|entries| {
                entries.filter_map(Result::ok).any(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "pid")
                })
            })
        }) && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(20));
        }

        for runtime in runtime_dirs {
            let Ok(entries) = std::fs::read_dir(runtime) else {
                continue;
            };
            for pid_file in entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "pid"))
            {
                let Some(pid) = std::fs::read(&pid_file)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .and_then(|value| value["pid"].as_u64())
                else {
                    continue;
                };
                let _ = std::process::Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .status();
            }
        }
    }
}

fn mock_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mock-stdio-server"))
}

fn tool(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": {
                "value": {"type": "integer", "description": "input value"}
            },
            "x-schema-extension": {"preserved": true}
        }
    })
}

#[derive(Clone)]
struct StdioSpec {
    name: &'static str,
    instructions: &'static str,
    tools: Vec<Value>,
    call_result: Value,
    stderr: &'static str,
    list_delay_ms: u64,
    allowed_tools: Vec<&'static str>,
    disabled_tools: Vec<&'static str>,
}

impl StdioSpec {
    fn new(name: &'static str, tools: Vec<Value>) -> Self {
        Self {
            name,
            instructions: "Process fixture instructions.",
            tools,
            call_result: complete_result("stdio"),
            stderr: "",
            list_delay_ms: 0,
            allowed_tools: Vec::new(),
            disabled_tools: Vec::new(),
        }
    }
}

struct StdioFixture {
    root: IsolatedRoot,
    config: PathBuf,
    observations: BTreeMap<String, PathBuf>,
}

impl StdioFixture {
    fn new(specs: Vec<StdioSpec>) -> Self {
        let root = IsolatedRoot::new();
        let config = root.path().join("mcp_servers.json");
        let mut observations = BTreeMap::new();
        let mut servers = serde_json::Map::new();

        for spec in specs {
            let observation = root.path().join(format!("{}-observation.json", spec.name));
            let observation_dir = root.path().join(format!("{}-observations", spec.name));
            let script = root.path().join(format!("{}-script.json", spec.name));
            std::fs::write(
                &script,
                serde_json::to_vec(&json!({
                    "observation_path": observation,
                    "observation_dir": observation_dir,
                    "instructions": spec.instructions,
                    "tool_pages": [{"cursor": null, "tools": spec.tools}],
                    "call_result": spec.call_result,
                    "stderr_chunks": [spec.stderr],
                    "list_response_delay_ms": spec.list_delay_ms,
                    "eof_behavior": "exit"
                }))
                .expect("stdio fixture script JSON"),
            )
            .expect("write stdio fixture script");

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
            serde_json::to_vec(&json!({"mcpServers": servers})).expect("stdio config JSON"),
        )
        .expect("write stdio config");
        Self {
            root,
            config,
            observations,
        }
    }

    fn with_failed_server(self, name: &str) -> Self {
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&self.config).expect("read fixture config"))
                .expect("fixture config JSON");
        config["mcpServers"][name] = json!({
            "command": self.root.path().join("missing-process-server")
        });
        std::fs::write(
            &self.config,
            serde_json::to_vec(&config).expect("updated fixture config JSON"),
        )
        .expect("write updated fixture config");
        self
    }

    fn run(
        &self,
        mode: Mode,
        args: &[&str],
        stdin: Option<&str>,
        environment: &[(&str, &str)],
    ) -> Output {
        run_cli(
            self.root.path(),
            &self.config,
            mode,
            args,
            stdin.map(str::as_bytes),
            environment,
        )
    }

    fn observation(&self, server: &str) -> Option<Value> {
        let path = self.observations.get(server).expect("known observation");
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    }
}

struct HttpConfig {
    root: IsolatedRoot,
    config: PathBuf,
}

impl HttpConfig {
    fn new(url: &str) -> Self {
        let root = IsolatedRoot::new();
        let config = root.path().join("mcp_servers.json");
        std::fs::write(
            &config,
            serde_json::to_vec(&json!({
                "mcpServers": {
                    "remote": {
                        "url": url,
                        "headers": {"Authorization": format!("Bearer {HTTP_SECRET}")}
                    }
                }
            }))
            .expect("HTTP process config JSON"),
        )
        .expect("write HTTP process config");
        Self { root, config }
    }

    fn run(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        run_cli(
            self.root.path(),
            &self.config,
            Mode::Direct,
            args,
            None,
            environment,
        )
    }
}

fn run_cli(
    root: &Path,
    config: &Path,
    mode: Mode,
    args: &[&str],
    stdin: Option<&[u8]>,
    environment: &[(&str, &str)],
) -> Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_mcp-cli"));
    command
        .current_dir(root)
        .env_clear()
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("TMPDIR", root)
        .env("MCP_TIMEOUT", "10")
        .env("MCP_MAX_RETRIES", "0")
        .env("MCP_RETRY_DELAY", "1")
        .env("MCP_DAEMON_TIMEOUT", "60")
        .env("NO_COLOR", "1")
        .arg("--config")
        .arg(config)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match mode {
        Mode::Direct => {
            command.env("MCP_NO_DAEMON", "1");
        }
        #[cfg(unix)]
        Mode::Daemon => {
            command.env_remove("MCP_NO_DAEMON");
        }
    }
    for (name, value) in environment {
        if *value == UNSET_ENV {
            command.env_remove(name);
        } else {
            command.env(name, value);
        }
    }

    let mut child = command.spawn().expect("spawn real mcp-cli binary");
    if let Some(mut child_stdin) = child.stdin.take()
        && let Some(input) = stdin
    {
        child_stdin.write_all(input).expect("write CLI stdin");
    }
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let stdout_task = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).expect("read CLI stdout");
        bytes
    });
    let stderr_task = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).expect("read CLI stderr");
        bytes
    });

    let deadline = Instant::now() + PROCESS_TIMEOUT;
    let status: ExitStatus = loop {
        match child.try_wait().expect("poll CLI process") {
            Some(status) => break status,
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("mcp-cli process exceeded {PROCESS_TIMEOUT:?}: args={args:?}");
            }
        }
    };
    Output {
        status,
        stdout: stdout_task.join().expect("stdout reader thread"),
        stderr: stderr_task.join().expect("stderr reader thread"),
    }
}

fn complete_result(source: &str) -> Value {
    json!({
        "content": [{"type": "text", "text": "complete"}],
        "isError": false,
        "structuredContent": {"source": source, "nested": [null, 7]},
        "x-result-extension": {"preserved": true}
    })
}

fn assert_success(output: &Output) {
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(!output.stdout.contains(&0x1b), "stdout contained ANSI");
    assert!(!output.stderr.contains(&0x1b), "stderr contained ANSI");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("Error ["),
        "successful command rendered an error: {output:?}"
    );
}

fn assert_error(output: &Output, code: i32, kind: &str) {
    assert_eq!(output.status.code(), Some(code), "{output:?}");
    assert!(
        output.stdout.is_empty(),
        "error polluted stdout: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.matches("Error [").count(), 1, "{stderr}");
    assert!(stderr.starts_with(&format!("Error [{kind}]:")), "{stderr}");
    assert!(!stderr.contains('\u{1b}'), "{stderr}");
    assert!(
        !stderr.contains(HTTP_SECRET),
        "HTTP secret leaked: {stderr}"
    );
}

fn rpc_result(result: Value) -> Value {
    json!({"jsonrpc": "2.0", "result": result})
}

fn initialize_result() -> Value {
    rpc_result(json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "process-http", "version": "1.0.0"},
        "instructions": "Process HTTP instructions."
    }))
}

fn http_tools() -> Vec<Value> {
    vec![
        tool("z/read.2", "HTTP second"),
        tool("a/read.1", "HTTP first"),
    ]
}

fn successful_http_script(prefix: Vec<ScriptedResponse>) -> MockHttpScript {
    let mut responses = prefix;
    responses.extend([
        ScriptedResponse::new(
            RequestMatcher::rpc("initialize"),
            MockResponse::Json {
                body: initialize_result(),
                session_id: Some(SESSION_ID.to_owned()),
            },
        ),
        ScriptedResponse::new(
            RequestMatcher::rpc("notifications/initialized"),
            MockResponse::Accepted,
        ),
        ScriptedResponse::new(RequestMatcher::http("GET"), MockResponse::OpenGetSse),
        ScriptedResponse::new(
            RequestMatcher::rpc_cursor("tools/list", None),
            MockResponse::Json {
                body: rpc_result(json!({"tools": http_tools()})),
                session_id: None,
            },
        ),
        ScriptedResponse::new(
            RequestMatcher::rpc("tools/call"),
            MockResponse::Sse {
                messages: vec![rpc_result(complete_result("http"))],
                session_id: None,
            },
        ),
        ScriptedResponse::new(RequestMatcher::http("DELETE"), MockResponse::Empty),
    ]);
    MockHttpScript::new(responses)
}

async fn run_http(
    script: MockHttpScript,
    args: &[&str],
    environment: &[(&str, &str)],
) -> (Output, Vec<CapturedRequest>, String) {
    let mut server = MockHttpServer::start(script)
        .await
        .expect("start loopback HTTP process fixture");
    assert!(server.url().starts_with("http://127.0.0.1:"));
    let url = server.url().to_owned();
    let config = HttpConfig::new(&url);
    let output = config.run(args, environment);
    tokio::time::timeout(SERVER_TIMEOUT, server.wait_for_no_connections())
        .await
        .expect("HTTP process fixture connections did not close");
    let requests = server.requests();
    assert_eq!(server.protocol_errors(), Vec::<String>::new());
    tokio::time::timeout(SERVER_TIMEOUT, server.shutdown())
        .await
        .expect("HTTP process fixture shutdown timed out")
        .expect("HTTP process fixture shutdown");
    (output, requests, url)
}

fn assert_http_headers(requests: &[CapturedRequest]) {
    assert!(!requests.is_empty());
    for request in requests {
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some("Bearer process-http-secret")
        );
        assert_eq!(request.path, "/mcp");
    }
}

fn count_rpc(requests: &[CapturedRequest], method: &str) -> usize {
    requests
        .iter()
        .filter(|request| request.rpc_method() == Some(method))
        .count()
}

#[test]
fn stdio_direct_matrix_covers_syntax_input_descriptions_filtering_and_repeatability() {
    let mut spec = StdioSpec::new(
        "target",
        vec![
            tool("z/read.2", "second"),
            tool("read_secret", "must stay hidden"),
            tool("a/read.1", "first"),
            tool("hidden", "not allowed"),
            tool("echo", "echo arguments"),
        ],
    );
    spec.stderr = "fixture diagnostic\n";
    spec.allowed_tools = vec!["*read*", "echo"];
    spec.disabled_tools = vec!["read_secret"];
    let fixture = StdioFixture::new(vec![spec]);

    let list_plain = fixture.run(Mode::Direct, &[], None, &[]);
    assert_success(&list_plain);
    assert_eq!(
        list_plain.stdout,
        b"target\n  \xe2\x80\xa2 a/read.1\n  \xe2\x80\xa2 echo\n  \xe2\x80\xa2 z/read.2\n"
    );
    assert_eq!(list_plain.stderr, b"[server] target: fixture diagnostic\n");

    let list_descriptions = fixture.run(Mode::Direct, &["--with-descriptions"], None, &[]);
    assert_success(&list_descriptions);
    assert_eq!(
        list_descriptions.stdout,
        b"target\n  \xe2\x80\xa2 a/read.1 - first\n  \xe2\x80\xa2 echo - echo arguments\n  \xe2\x80\xa2 z/read.2 - second\n"
    );

    let shorthand = fixture.run(Mode::Direct, &["target", "-d"], None, &[]);
    let explicit = fixture.run(
        Mode::Direct,
        &["info", "target", "--with-descriptions"],
        None,
        &[],
    );
    assert_success(&shorthand);
    assert_success(&explicit);
    assert_eq!(shorthand.stdout, explicit.stdout);
    let info = String::from_utf8(explicit.stdout.clone()).expect("info UTF-8");
    assert!(info.starts_with("Server: target\nTransport: stdio\nCommand: "));
    assert!(info.contains("Process fixture instructions."));
    assert!(info.contains("  a/read.1\n    first\n"));
    assert!(!info.contains("read_secret"));
    assert!(!info.contains("hidden"));

    let split_schema = fixture.run(Mode::Direct, &["info", "target", "echo"], None, &[]);
    let slash_schema = fixture.run(Mode::Direct, &["info", "target/echo"], None, &[]);
    assert_success(&split_schema);
    assert_success(&slash_schema);
    assert_eq!(split_schema.stdout, slash_schema.stdout);
    assert_eq!(
        serde_json::from_slice::<Value>(&split_schema.stdout).expect("schema JSON"),
        tool("echo", "ignored")["inputSchema"]
    );

    let grep = fixture.run(Mode::Direct, &["grep", "**/READ.?", "-d"], None, &[]);
    assert_success(&grep);
    assert_eq!(
        grep.stdout,
        b"target a/read.1 - first\ntarget z/read.2 - second\n"
    );

    for args in [
        Vec::<&str>::new(),
        vec!["info", "target"],
        vec!["grep", "**/READ.?", "-d"],
    ] {
        let first = fixture.run(Mode::Direct, &args, None, &[]);
        let second = fixture.run(Mode::Direct, &args, None, &[]);
        assert_success(&first);
        assert_success(&second);
        assert_eq!(first.stdout, second.stdout, "non-repeatable args={args:?}");
    }

    let inline = fixture.run(
        Mode::Direct,
        &["call", "target", "echo", r#"{"source":"inline"}"#],
        Some(r#"{"source":"stdin"}"#),
        &[],
    );
    assert_success(&inline);
    assert_eq!(
        serde_json::from_slice::<Value>(&inline.stdout).expect("complete stdio result"),
        complete_result("stdio")
    );
    assert_eq!(
        fixture
            .observation("target")
            .expect("inline observation")
            .pointer("/calls/0/arguments"),
        Some(&json!({"source": "inline"}))
    );

    let stdin = fixture.run(
        Mode::Direct,
        &["call", "target/echo"],
        Some(r#"{"source":"stdin"}"#),
        &[],
    );
    assert_success(&stdin);
    assert_eq!(
        fixture
            .observation("target")
            .expect("stdin observation")
            .pointer("/calls/0/arguments"),
        Some(&json!({"source": "stdin"}))
    );

    let denied = StdioFixture::new(vec![{
        let mut denied = StdioSpec::new("target", vec![tool("echo", "echo")]);
        denied.allowed_tools = vec!["echo"];
        denied.disabled_tools = vec!["danger"];
        denied
    }]);
    let output = denied.run(Mode::Direct, &["call", "target/danger", "{}"], None, &[]);
    assert_error(&output, 1, "TOOL_DISABLED");
    assert!(
        denied.observation("target").is_none(),
        "pre-call authorization started the server"
    );
}

#[test]
fn list_partial_failure_preserves_success_and_routes_failure_as_business_output() {
    let fixture = StdioFixture::new(vec![StdioSpec::new(
        "good",
        vec![tool("echo", "available")],
    )])
    .with_failed_server("bad");
    let output = fixture.run(Mode::Direct, &[], None, &[]);
    assert_success(&output);
    let stdout = String::from_utf8(output.stdout.clone()).expect("partial list UTF-8");
    assert!(stdout.starts_with("bad\n  <error: "), "{stdout}");
    assert!(stdout.contains("\n\ngood\n  \u{2022} echo\n"), "{stdout}");
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_direct_matrix_covers_all_commands_targets_headers_and_complete_results() {
    let (list, list_requests, _) = run_http(successful_http_script(Vec::new()), &["-d"], &[]).await;
    assert_success(&list);
    assert_eq!(
        list.stdout,
        b"remote\n  \xe2\x80\xa2 a/read.1 - HTTP first\n  \xe2\x80\xa2 z/read.2 - HTTP second\n"
    );
    assert!(list.stderr.is_empty());
    assert_http_headers(&list_requests);
    assert_eq!(count_rpc(&list_requests, "tools/list"), 1);

    let (info, info_requests, url) = run_http(
        successful_http_script(Vec::new()),
        &["info", "remote", "--with-descriptions"],
        &[],
    )
    .await;
    assert_success(&info);
    assert_eq!(
        String::from_utf8(info.stdout).expect("HTTP info UTF-8"),
        format!(
            "Server: remote\nTransport: HTTP\nURL: {url}\n\nInstructions:\n  Process HTTP instructions.\n\nTools (2):\n  a/read.1\n    HTTP first\n    Parameters:\n      • value (integer, optional) - input value\n  z/read.2\n    HTTP second\n    Parameters:\n      • value (integer, optional) - input value\n"
        )
    );
    assert_http_headers(&info_requests);

    let (split, _, _) = run_http(
        successful_http_script(Vec::new()),
        &["info", "remote", "a/read.1"],
        &[],
    )
    .await;
    let (slash, _, _) = run_http(
        successful_http_script(Vec::new()),
        &["info", "remote/a/read.1"],
        &[],
    )
    .await;
    assert_success(&split);
    assert_success(&slash);
    assert_eq!(split.stdout, slash.stdout);
    assert_eq!(
        serde_json::from_slice::<Value>(&slash.stdout).expect("HTTP schema JSON"),
        tool("a/read.1", "ignored")["inputSchema"]
    );

    let (grep, _, _) = run_http(
        successful_http_script(Vec::new()),
        &["grep", "**/read.?", "-d"],
        &[],
    )
    .await;
    assert_success(&grep);
    assert_eq!(
        grep.stdout,
        b"remote a/read.1 - HTTP first\nremote z/read.2 - HTTP second\n"
    );

    let (call, call_requests, _) = run_http(
        successful_http_script(Vec::new()),
        &["call", "remote/a/read.1", r#"{"value":42}"#],
        &[],
    )
    .await;
    assert_success(&call);
    assert!(call.stderr.is_empty());
    let result: Value = serde_json::from_slice(&call.stdout).expect("complete HTTP result JSON");
    assert_eq!(result, complete_result("http"));
    assert_eq!(
        serde_json::from_slice::<Value>(
            &serde_json::to_vec(&result).expect("HTTP result reserialization")
        )
        .expect("HTTP result second parse"),
        complete_result("http")
    );
    let call_request = call_requests
        .iter()
        .find(|request| request.rpc_method() == Some("tools/call"))
        .expect("captured HTTP call");
    assert_eq!(
        call_request
            .body
            .as_ref()
            .and_then(|body| body.pointer("/params/arguments")),
        Some(&json!({"value": 42}))
    );
    assert_http_headers(&call_requests);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_retry_auth_timeout_debug_and_color_contracts_are_process_visible() {
    let transient_prefix = vec![ScriptedResponse::new(
        RequestMatcher::rpc("initialize"),
        MockResponse::Status(503),
    )];
    let (quiet, quiet_requests, _) = run_http(
        successful_http_script(transient_prefix.clone()),
        &["grep", "**"],
        &[("MCP_MAX_RETRIES", "1")],
    )
    .await;
    assert_success(&quiet);
    assert_eq!(quiet.stdout, b"remote a/read.1\nremote z/read.2\n");
    assert!(quiet.stderr.is_empty(), "{quiet:?}");
    assert_eq!(count_rpc(&quiet_requests, "initialize"), 2);

    let (debug, debug_requests, _) = run_http(
        successful_http_script(transient_prefix),
        &["grep", "**"],
        &[("MCP_MAX_RETRIES", "1"), ("MCP_DEBUG", "1")],
    )
    .await;
    assert_success(&debug);
    assert_eq!(debug.status.code(), quiet.status.code());
    assert_eq!(debug.stdout, quiet.stdout);
    let debug_stderr = String::from_utf8(debug.stderr).expect("debug stderr UTF-8");
    assert!(debug_stderr.contains("retry scheduled next_attempt=1"));
    assert!(debug_stderr.contains("error_class=transient"));
    assert!(debug_stderr.contains("selected direct mode"));
    assert!(!debug_stderr.contains(HTTP_SECRET));
    assert!(!debug_stderr.contains('\u{1b}'));
    assert_eq!(count_rpc(&debug_requests, "initialize"), 2);

    for status in [401_u16, 403] {
        let script = MockHttpScript::new(vec![ScriptedResponse::new(
            RequestMatcher::rpc("initialize"),
            MockResponse::Status(status),
        )]);
        let (auth, requests, _) =
            run_http(script, &["info", "remote"], &[("MCP_MAX_RETRIES", "5")]).await;
        assert_error(&auth, 4, "AUTH_ERROR");
        let stderr = String::from_utf8_lossy(&auth.stderr);
        assert!(stderr.contains(&status.to_string()), "{stderr}");
        assert!(stderr.contains("remote"), "{stderr}");
        assert_eq!(count_rpc(&requests, "initialize"), 1, "auth retried");
    }

    let timeout_script = MockHttpScript::new(vec![ScriptedResponse::new(
        RequestMatcher::rpc("initialize"),
        MockResponse::Hold,
    )]);
    let started = Instant::now();
    let (timeout, requests, _) = run_http(
        timeout_script,
        &["info", "remote"],
        &[("MCP_TIMEOUT", "1"), ("MCP_MAX_RETRIES", "5")],
    )
    .await;
    assert_error(&timeout, 3, "TIMEOUT");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(count_rpc(&requests, "initialize"), 1);

    let (non_tty, _, _) = run_http(
        successful_http_script(Vec::new()),
        &["grep", "**"],
        &[("NO_COLOR", UNSET_ENV)],
    )
    .await;
    assert_success(&non_tty);
    assert!(!non_tty.stdout.contains(&0x1b));
    assert!(!non_tty.stderr.contains(&0x1b));
}

#[cfg(unix)]
#[test]
fn unix_daemon_and_direct_modes_are_externally_equivalent_for_all_commands() {
    let fixture = StdioFixture::new(vec![StdioSpec::new(
        "target",
        vec![tool("echo", "echo arguments"), tool("read", "read data")],
    )]);
    let commands: Vec<(Vec<&str>, Option<&str>)> = vec![
        (vec![], None),
        (vec!["info", "target", "-d"], None),
        (vec!["grep", "*", "-d"], None),
        (vec!["call", "target/echo"], Some(r#"{"mode":"same"}"#)),
    ];

    for (args, stdin) in commands {
        let direct = fixture.run(Mode::Direct, &args, stdin, &[("MCP_DEBUG", "1")]);
        let daemon = fixture.run(Mode::Daemon, &args, stdin, &[("MCP_DEBUG", "1")]);
        assert_success(&direct);
        assert_success(&daemon);
        assert_eq!(
            direct.status.code(),
            daemon.status.code(),
            "mode exit mismatch for {args:?}"
        );
        assert_eq!(
            direct.stdout, daemon.stdout,
            "mode stdout mismatch for {args:?}"
        );
        for stderr in [&direct.stderr, &daemon.stderr] {
            let text = String::from_utf8_lossy(stderr);
            assert!(
                text.lines().all(|line| line.starts_with("[mcp-cli]")),
                "non-debug mode difference for {args:?}: {text}"
            );
            assert!(!text.contains('\u{1b}'));
        }
    }
}
