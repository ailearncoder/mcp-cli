#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Local, script-driven MCP stdio server used only by transport tests.
#[derive(Debug, Parser)]
#[command(name = "mock-stdio-server", hide = true)]
struct FixtureArgs {
    #[arg(long)]
    fixture_script: PathBuf,

    /// Literal values after `--` are intentionally accepted so integration
    /// tests can prove the connector never invokes a shell.
    #[arg(last = true, allow_hyphen_values = true)]
    passthrough: Vec<OsString>,
}

#[derive(Debug, Deserialize)]
struct FixtureScript {
    observation_path: PathBuf,
    /// Optional directory containing one observation file per backend PID.
    /// This avoids cross-process overwrite races in daemon fallback tests.
    #[serde(default)]
    observation_dir: Option<PathBuf>,
    #[serde(default)]
    capture_env: Vec<String>,
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    tool_pages: Vec<ToolPage>,
    #[serde(default = "default_call_result")]
    call_result: Value,
    #[serde(default)]
    stderr_chunks: Vec<String>,
    #[serde(default)]
    list_response_delay_ms: u64,
    #[serde(default)]
    call_response_delay_ms: u64,
    #[serde(default)]
    eof_behavior: EofBehavior,
}

#[derive(Debug, Deserialize)]
struct ToolPage {
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    tools: Vec<Value>,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EofBehavior {
    #[default]
    Exit,
    Ignore,
    Delay,
}

#[derive(Debug, Serialize)]
struct Observation {
    pid: u32,
    argv: Vec<String>,
    passthrough: Vec<String>,
    cwd: String,
    env: BTreeMap<String, Option<String>>,
    events: Vec<String>,
    list_cursors: Vec<Option<String>>,
    calls: Vec<Value>,
    protocol_errors: Vec<String>,
    eof_seen: bool,
}

impl Observation {
    fn capture(script: &FixtureScript, args: &FixtureArgs) -> Result<Self, String> {
        let cwd = std::env::current_dir()
            .map_err(|error| format!("failed to read cwd: {error}"))?
            .to_string_lossy()
            .into_owned();
        let env = script
            .capture_env
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    std::env::var_os(name).map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        Ok(Self {
            pid: std::process::id(),
            argv: std::env::args_os()
                .skip(1)
                .map(|value| value.to_string_lossy().into_owned())
                .collect(),
            passthrough: args
                .passthrough
                .iter()
                .map(|value| value.to_string_lossy().into_owned())
                .collect(),
            cwd,
            env,
            events: Vec::new(),
            list_cursors: Vec::new(),
            calls: Vec::new(),
            protocol_errors: Vec::new(),
            eof_seen: false,
        })
    }

    fn record(&mut self, event: impl Into<String>, script: &FixtureScript) -> Result<(), String> {
        self.events.push(event.into());
        self.persist_all(script)
    }

    fn persist(&self, path: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec(self)
            .map_err(|error| format!("failed to serialize observation: {error}"))?;
        std::fs::write(path, &bytes)
            .map_err(|error| format!("failed to write observation {}: {error}", path.display()))?;
        Ok(())
    }

    fn persist_all(&self, script: &FixtureScript) -> Result<(), String> {
        self.persist(&script.observation_path)?;
        if let Some(directory) = &script.observation_dir {
            std::fs::create_dir_all(directory).map_err(|error| {
                format!(
                    "failed to create observation directory {}: {error}",
                    directory.display()
                )
            })?;
            self.persist(&directory.join(format!("{}.json", self.pid)))?;
        }
        Ok(())
    }
}

fn default_call_result() -> Value {
    json!({"content": []})
}

fn main() -> ExitCode {
    match run(FixtureArgs::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("mock-stdio-server: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: FixtureArgs) -> Result<(), String> {
    let bytes = std::fs::read(&args.fixture_script).map_err(|error| {
        format!(
            "failed to read fixture script {}: {error}",
            args.fixture_script.display()
        )
    })?;
    let script: FixtureScript = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "invalid fixture script {}: {error}",
            args.fixture_script.display()
        )
    })?;
    let mut observation = Observation::capture(&script, &args)?;
    observation.persist_all(&script)?;

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    let mut initialized = false;
    let mut line = String::new();

    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| format!("failed to read stdin: {error}"))?;
        if read == 0 {
            observation.eof_seen = true;
            observation.record("eof", &script)?;
            match script.eof_behavior {
                EofBehavior::Exit => return Ok(()),
                EofBehavior::Ignore => loop {
                    std::thread::park();
                },
                EofBehavior::Delay => {
                    std::thread::sleep(Duration::from_secs(30));
                    return Ok(());
                }
            }
        }

        let message: Value = serde_json::from_str(line.trim_end())
            .map_err(|error| format!("invalid JSON-RPC frame: {error}"))?;
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").cloned();
        match method {
            Some("initialize") => {
                observation.record("initialize", &script)?;
                let result = json!({
                    "protocolVersion": "2025-03-26",
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "mock-stdio-server", "version": "1.0.0"},
                    "instructions": script.instructions,
                });
                write_response(&mut writer, id, result)?;
            }
            Some("notifications/initialized") => {
                initialized = true;
                observation.record("initialized", &script)?;
                let mut stderr = io::stderr().lock();
                for chunk in &script.stderr_chunks {
                    stderr
                        .write_all(chunk.as_bytes())
                        .and_then(|_| stderr.flush())
                        .map_err(|error| format!("failed to write stderr: {error}"))?;
                }
            }
            Some("tools/list") => {
                require_initialized(initialized, "tools/list", &mut observation, &script)?;
                let cursor = message
                    .pointer("/params/cursor")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                observation.list_cursors.push(cursor.clone());
                observation.record("tools/list", &script)?;
                if script.list_response_delay_ms > 0 {
                    std::thread::sleep(Duration::from_millis(script.list_response_delay_ms));
                }
                let page = script
                    .tool_pages
                    .iter()
                    .find(|page| page.cursor == cursor)
                    .ok_or_else(|| format!("no scripted tools/list page for cursor {cursor:?}"))?;
                let mut result = serde_json::Map::new();
                result.insert("tools".to_owned(), Value::Array(page.tools.clone()));
                if let Some(next_cursor) = &page.next_cursor {
                    result.insert("nextCursor".to_owned(), json!(next_cursor));
                }
                write_response(&mut writer, id, Value::Object(result))?;
            }
            Some("tools/call") => {
                require_initialized(initialized, "tools/call", &mut observation, &script)?;
                observation
                    .calls
                    .push(message.get("params").cloned().unwrap_or(Value::Null));
                observation.record("tools/call", &script)?;
                if script.call_response_delay_ms > 0 {
                    std::thread::sleep(Duration::from_millis(script.call_response_delay_ms));
                }
                write_response(&mut writer, id, script.call_result.clone())?;
            }
            Some(other) => {
                observation
                    .protocol_errors
                    .push(format!("unexpected method {other}"));
                observation.persist_all(&script)?;
                if id.is_some() {
                    write_error(&mut writer, id, -32601, "method not found")?;
                }
            }
            None => {
                observation
                    .protocol_errors
                    .push("frame missing method".to_owned());
                observation.persist_all(&script)?;
            }
        }
    }
}

fn require_initialized(
    initialized: bool,
    method: &str,
    observation: &mut Observation,
    script: &FixtureScript,
) -> Result<(), String> {
    if initialized {
        return Ok(());
    }
    observation
        .protocol_errors
        .push(format!("{method} arrived before initialized"));
    observation.persist_all(script)?;
    Err(format!("{method} arrived before initialized"))
}

fn write_response(writer: &mut impl Write, id: Option<Value>, result: Value) -> Result<(), String> {
    let id = id.ok_or_else(|| "request missing id".to_owned())?;
    write_frame(
        writer,
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    )
}

fn write_error(
    writer: &mut impl Write,
    id: Option<Value>,
    code: i64,
    message: &str,
) -> Result<(), String> {
    write_frame(
        writer,
        &json!({"jsonrpc": "2.0", "id": id.unwrap_or(Value::Null), "error": {
            "code": code,
            "message": message,
        }}),
    )
}

fn write_frame(writer: &mut impl Write, value: &Value) -> Result<(), String> {
    const WRITE_CHUNK_SIZE: usize = 1024;

    let mut frame = serde_json::to_vec(value)
        .map_err(|error| format!("failed to serialize response: {error}"))?;
    frame.push(b'\n');

    // Keep each synchronous fixture write small. In particular, this avoids
    // submitting a single schema string larger than a Windows anonymous-pipe
    // buffer while preserving the full NDJSON frame seen by the client.
    for chunk in frame.chunks(WRITE_CHUNK_SIZE) {
        writer
            .write_all(chunk)
            .map_err(|error| format!("failed to write response: {error}"))?;
    }
    writer
        .flush()
        .map_err(|error| format!("failed to write response: {error}"))
}
