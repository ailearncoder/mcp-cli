#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    io::{self, Cursor, Write},
    num::NonZeroUsize,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use mcp_cli::{
    BoxFuture, CallInput, CancellationFlag, CliError, CommandContext, CommandDispatcher,
    CommandSpec, ConfigHash, ConnectionError, ConnectionManager, ConnectionMode, Deadline,
    DiagnosticSink, ErrorClass, JsonObject, McpConnection, PlainTextPresenter, Presenter,
    RuntimeConfig, SecretSet, ServerDefinition, ServerId, StylePolicy, ToolFilterConfig, ToolInfo,
    ToolResult, TransportConfig, WriterDiagnosticSink, render_structured_error,
};
use proptest::prelude::*;
use serde_json::{Value, json};

const DEBUG_PREFIX: &[u8] = b"[mcp-cli] debug:";

#[derive(Clone, Debug)]
struct Fixture {
    token: String,
    tag: u16,
    argument: i32,
}

impl Fixture {
    fn alpha_name(&self) -> String {
        format!("alpha-{}", self.token)
    }

    fn zeta_name(&self) -> String {
        format!("zeta-{}", self.token)
    }

    fn alpha_description(&self) -> String {
        format!("Alpha {}", self.token)
    }

    fn zeta_description(&self) -> String {
        format!("Zeta {}", self.token)
    }

    fn parameter_description(&self) -> String {
        format!("Value {}", self.token)
    }

    fn instructions(&self) -> String {
        format!("Use {} carefully", self.token)
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "value": {
                    "type": "integer",
                    "description": self.parameter_description(),
                }
            },
            "required": ["value"],
            "x-property-tag": self.tag,
        })
    }

    fn tools(&self) -> Vec<ToolInfo> {
        // Deliberately return reverse lexical order so the command path, not
        // the adapter, is responsible for deterministic presentation.
        vec![
            ToolInfo {
                name: self.zeta_name(),
                description: Some(self.zeta_description()),
                input_schema: json!({"type": "object"}),
            },
            ToolInfo {
                name: self.alpha_name(),
                description: Some(self.alpha_description()),
                input_schema: self.schema(),
            },
        ]
    }

    fn result(&self) -> ToolResult {
        json!({
            "content": [{"type": "text", "text": format!("result-{}", self.token)}],
            "isError": false,
            "structuredContent": {"argument": self.argument, "tag": self.tag},
            "x-extension": {"preserved": true},
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    ListSuccess,
    InfoSuccess,
    InfoSchemaSuccess,
    GrepSuccess,
    CallSuccess,
    ListPartialFailure,
    GrepPartialFailure,
    BusinessError,
    BusinessResult,
    TransientError,
    NonTransientError,
    AuthError,
    HttpStatus(u16),
}

const CASES: &[Case] = &[
    Case::ListSuccess,
    Case::InfoSuccess,
    Case::InfoSchemaSuccess,
    Case::GrepSuccess,
    Case::CallSuccess,
    Case::ListPartialFailure,
    Case::GrepPartialFailure,
    Case::BusinessError,
    Case::BusinessResult,
    Case::TransientError,
    Case::NonTransientError,
    Case::AuthError,
    Case::HttpStatus(400),
    Case::HttpStatus(401),
    Case::HttpStatus(403),
    Case::HttpStatus(429),
    Case::HttpStatus(502),
    Case::HttpStatus(503),
    Case::HttpStatus(504),
];

#[derive(Clone, Debug, Default)]
struct MemoryWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl MemoryWriter {
    fn bytes(&self) -> Vec<u8> {
        self.bytes.lock().expect("memory writer lock").clone()
    }
}

impl Write for MemoryWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes
            .lock()
            .map_err(|_| io::Error::other("memory writer lock poisoned"))?
            .extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct InMemoryManager {
    mode: ConnectionMode,
    case: Case,
    fixture: Fixture,
}

impl ConnectionManager for InMemoryManager {
    fn acquire<'a>(
        &'a self,
        context: &'a CommandContext,
        server: &'a ServerDefinition,
    ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, CliError>> {
        context.diagnostics.debug(&format!(
            "in-memory {:?} adapter acquired {}",
            self.mode, server.name
        ));
        let connection = InMemoryConnection {
            mode: self.mode,
            case: self.case,
            server: server.name.clone(),
            fixture: self.fixture.clone(),
            instructions: self.fixture.instructions(),
        };
        Box::pin(async move { Ok(Box::new(connection) as Box<dyn McpConnection>) })
    }
}

struct InMemoryConnection {
    mode: ConnectionMode,
    case: Case,
    server: String,
    fixture: Fixture,
    instructions: String,
}

impl McpConnection for InMemoryConnection {
    fn list_tools<'a>(
        &'a self,
        context: &'a CommandContext,
    ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
        context
            .diagnostics
            .debug(&format!("in-memory {:?} list_tools", self.mode));
        let result = if self.server == "broken"
            && matches!(
                self.case,
                Case::ListPartialFailure | Case::GrepPartialFailure
            ) {
            Err(ConnectionError::new("mode-private partial failure")
                .with_class(ErrorClass::Transient))
        } else {
            Ok(self.fixture.tools())
        };
        Box::pin(async move { result })
    }

    fn call_tool<'a>(
        &'a self,
        context: &'a CommandContext,
        _name: &'a str,
        _args: JsonObject,
    ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
        context
            .diagnostics
            .debug(&format!("in-memory {:?} call_tool", self.mode));
        let result = match self.case {
            Case::BusinessError => Err(ConnectionError::new("mode-private business failure")
                .with_class(ErrorClass::Business)),
            Case::BusinessResult => Ok(json!({
                "content": [{"type": "text", "text": "business failure"}],
                "isError": true,
            })),
            Case::TransientError => Err(ConnectionError::new("mode-private transient failure")
                .with_class(ErrorClass::Transient)),
            Case::NonTransientError => {
                Err(ConnectionError::new("mode-private nontransient failure")
                    .with_class(ErrorClass::NonTransient))
            }
            Case::AuthError => {
                Err(ConnectionError::new("mode-private auth failure").with_class(ErrorClass::Auth))
            }
            Case::HttpStatus(status) => {
                Err(ConnectionError::new("mode-private HTTP failure").with_http_status(status))
            }
            _ => Ok(self.fixture.result()),
        };
        Box::pin(async move { result })
    }

    fn instructions(&self) -> Option<&str> {
        Some(&self.instructions)
    }

    fn close<'a>(
        self: Box<Self>,
        context: &'a CommandContext,
    ) -> BoxFuture<'a, Result<(), ConnectionError>> {
        context
            .diagnostics
            .debug(&format!("in-memory {:?} close", self.mode));
        Box::pin(async { Ok(()) })
    }

    fn mode(&self) -> ConnectionMode {
        self.mode
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Observation {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: u8,
}

fn fixture_strategy() -> impl Strategy<Value = Fixture> {
    ("[a-z]{1,10}", any::<u16>(), any::<i32>()).prop_map(|(token, tag, argument)| Fixture {
        token,
        tag,
        argument,
    })
}

fn test_epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn server(name: &str, byte: u8) -> ServerDefinition {
    ServerDefinition {
        name: name.to_owned(),
        id: ServerId(format!("{byte:064x}")),
        config_hash: ConfigHash([byte; 32]),
        transport: TransportConfig::Stdio {
            command: format!("run-{name}"),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        },
        filter: ToolFilterConfig::default(),
    }
}

fn definitions(case: Case) -> BTreeMap<String, ServerDefinition> {
    let mut servers = BTreeMap::from([("target".to_owned(), server("target", 1))]);
    if matches!(case, Case::ListPartialFailure | Case::GrepPartialFailure) {
        servers.insert("broken".to_owned(), server("broken", 2));
    }
    servers
}

fn command(case: Case, fixture: &Fixture) -> CommandSpec {
    match case {
        Case::ListSuccess | Case::ListPartialFailure => CommandSpec::List {
            with_descriptions: true,
        },
        Case::InfoSuccess => CommandSpec::Info {
            server: "target".to_owned(),
            tool: None,
            with_descriptions: true,
        },
        Case::InfoSchemaSuccess => CommandSpec::Info {
            server: "target".to_owned(),
            tool: Some(fixture.alpha_name()),
            with_descriptions: true,
        },
        Case::GrepSuccess | Case::GrepPartialFailure => CommandSpec::Grep {
            pattern: "alpha-*".to_owned(),
            with_descriptions: true,
        },
        Case::CallSuccess
        | Case::BusinessError
        | Case::BusinessResult
        | Case::TransientError
        | Case::NonTransientError
        | Case::AuthError
        | Case::HttpStatus(_) => CommandSpec::Call {
            server: "target".to_owned(),
            tool: fixture.alpha_name(),
            inline_json: Some(format!(r#"{{"value":{}}}"#, fixture.argument)),
        },
    }
}

async fn observe(
    mode: ConnectionMode,
    case: Case,
    fixture: &Fixture,
) -> Result<Observation, TestCaseError> {
    let stderr = MemoryWriter::default();
    let diagnostics = Arc::new(WriterDiagnosticSink::new(
        stderr.clone(),
        true,
        SecretSet::new(),
    ));
    let context_diagnostics: Arc<dyn DiagnosticSink> = diagnostics;
    let context = CommandContext {
        deadline: Deadline::new(test_epoch() + Duration::from_secs(3_600)),
        cancellation: Arc::new(CancellationFlag::default()),
        diagnostics: context_diagnostics,
    };
    let manager: Arc<dyn ConnectionManager> = Arc::new(InMemoryManager {
        mode,
        case,
        fixture: fixture.clone(),
    });
    let runtime = RuntimeConfig {
        concurrency: NonZeroUsize::new(2).expect("non-zero test concurrency"),
        max_retries: 0,
        ..RuntimeConfig::default()
    };
    let dispatcher = CommandDispatcher::managed(manager, &runtime);
    let mut input = CallInput::new(Cursor::new(Vec::<u8>::new()), true);

    let result = dispatcher
        .dispatch(
            &context,
            &definitions(case),
            &command(case, fixture),
            &mut input,
        )
        .await;

    let (stdout, exit_code) = match result {
        Ok(outcome) => (
            PlainTextPresenter
                .render(outcome, StylePolicy::plain())
                .map_err(|error| {
                    TestCaseError::fail(format!("presenter rejected generated outcome: {error}"))
                })?,
            0,
        ),
        Err(error) => {
            let exit_code = error.canonical_exit_code().as_u8();
            let mut error_writer = stderr.clone();
            render_structured_error(&mut error_writer, &error).map_err(|write_error| {
                TestCaseError::fail(format!("structured error render failed: {write_error}"))
            })?;
            (Vec::new(), exit_code)
        }
    };

    Ok(Observation {
        stdout,
        stderr: stderr.bytes(),
        exit_code,
    })
}

fn json_line(value: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(value).expect("generated JSON is serializable");
    bytes.push(b'\n');
    bytes
}

fn target_list(fixture: &Fixture) -> String {
    format!(
        "target\n  • {} - {}\n  • {} - {}\n",
        fixture.alpha_name(),
        fixture.alpha_description(),
        fixture.zeta_name(),
        fixture.zeta_description(),
    )
}

fn structured_error(
    kind: &str,
    message: &str,
    details: Option<&str>,
    suggestion: Option<&str>,
) -> Vec<u8> {
    let mut rendered = format!("Error [{kind}]: {message}");
    if let Some(details) = details {
        rendered.push_str("\n  Details: ");
        rendered.push_str(details);
    }
    if let Some(suggestion) = suggestion {
        rendered.push_str("\n  Suggestion: ");
        rendered.push_str(suggestion);
    }
    rendered.push('\n');
    rendered.into_bytes()
}

// Independent oracle: expected bytes and exits are assembled directly from
// generated domain values and the public contract. It does not call command
// handlers, production formatters, presenters, or error constructors.
fn oracle(case: Case, fixture: &Fixture) -> Observation {
    let alpha = fixture.alpha_name();
    let network_stderr = || {
        structured_error(
            "NETWORK_ERROR",
            "Failed to communicate with server \"target\"",
            Some("Failed while calling tool"),
            Some("Check network connectivity, the server address, and server availability"),
        )
    };
    let business_stderr = || {
        structured_error(
            "TOOL_EXECUTION_FAILED",
            &format!("Tool \"{alpha}\" execution failed on server \"target\""),
            Some("The MCP server reported a tool business execution failure"),
            Some(&format!(
                "Run 'mcp-cli info target {alpha}' and verify the arguments match the input schema"
            )),
        )
    };

    match case {
        Case::ListSuccess => Observation {
            stdout: target_list(fixture).into_bytes(),
            stderr: Vec::new(),
            exit_code: 0,
        },
        Case::InfoSuccess => Observation {
            stdout: format!(
                "Server: target\nTransport: stdio\nCommand: run-target\n\nInstructions:\n  {}\n\nTools (2):\n  {}\n    {}\n    Parameters:\n      • value (integer, required) - {}\n  {}\n    {}\n",
                fixture.instructions(),
                alpha,
                fixture.alpha_description(),
                fixture.parameter_description(),
                fixture.zeta_name(),
                fixture.zeta_description(),
            )
            .into_bytes(),
            stderr: Vec::new(),
            exit_code: 0,
        },
        Case::InfoSchemaSuccess => Observation {
            stdout: json_line(&fixture.schema()),
            stderr: Vec::new(),
            exit_code: 0,
        },
        Case::GrepSuccess => Observation {
            stdout: format!(
                "target {} - {}\n",
                fixture.alpha_name(),
                fixture.alpha_description()
            )
            .into_bytes(),
            stderr: Vec::new(),
            exit_code: 0,
        },
        Case::CallSuccess => Observation {
            stdout: json_line(&fixture.result()),
            stderr: Vec::new(),
            exit_code: 0,
        },
        Case::ListPartialFailure => Observation {
            stdout: format!(
                "broken\n  <error: Failed to communicate with server \"broken\">\n\n{}",
                target_list(fixture)
            )
            .into_bytes(),
            stderr: Vec::new(),
            exit_code: 0,
        },
        Case::GrepPartialFailure => Observation {
            stdout: format!(
                "target {} - {}\n",
                fixture.alpha_name(),
                fixture.alpha_description()
            )
            .into_bytes(),
            stderr: b"[mcp-cli] warning: Server \"broken\" could not be searched (NETWORK_ERROR); continuing\n".to_vec(),
            exit_code: 0,
        },
        Case::BusinessError => Observation {
            stdout: Vec::new(),
            stderr: business_stderr(),
            exit_code: 2,
        },
        Case::BusinessResult => Observation {
            stdout: Vec::new(),
            stderr: structured_error(
                "TOOL_EXECUTION_FAILED",
                &format!("Tool \"{alpha}\" execution failed on server \"target\""),
                Some(&format!(
                    "Server message: business failure\n  Input schema: {}",
                    fixture.schema()
                )),
                Some("Retry with a JSON object matching the input schema shown above"),
            ),
            exit_code: 2,
        },
        Case::TransientError | Case::NonTransientError => Observation {
            stdout: Vec::new(),
            stderr: network_stderr(),
            exit_code: 3,
        },
        Case::AuthError => Observation {
            stdout: Vec::new(),
            stderr: structured_error(
                "AUTH_ERROR",
                "Authentication or authorization failed for target server",
                Some("Authentication failed while calling tool"),
                Some(
                    "Check the Authorization header, credentials, and access permissions in config",
                ),
            ),
            exit_code: 4,
        },
        Case::HttpStatus(status @ (401 | 403)) => Observation {
            stdout: Vec::new(),
            stderr: structured_error(
                "AUTH_ERROR",
                "Authentication or authorization failed for server \"target\"",
                Some(&format!("HTTP status: {status}")),
                Some(
                    "Check the Authorization header, credentials, and access permissions in config",
                ),
            ),
            exit_code: 4,
        },
        Case::HttpStatus(status) => Observation {
            stdout: Vec::new(),
            stderr: structured_error(
                "NETWORK_ERROR",
                "Failed to communicate with server \"target\"",
                Some(&format!("HTTP status: {status}")),
                Some("Check network connectivity, the server address, and server availability"),
            ),
            exit_code: 3,
        },
    }
}

fn without_debug(stderr: &[u8]) -> Vec<u8> {
    stderr
        .split_inclusive(|byte| *byte == b'\n')
        .filter(|line| !line.starts_with(DEBUG_PREFIX))
        .flatten()
        .copied()
        .collect()
}

async fn verify_fixture(fixture: Fixture) -> Result<(), TestCaseError> {
    for &case in CASES {
        let direct = observe(ConnectionMode::Direct, case, &fixture).await?;
        let daemon = observe(ConnectionMode::Daemon, case, &fixture).await?;
        let expected = oracle(case, &fixture);
        let direct_non_debug = without_debug(&direct.stderr);
        let daemon_non_debug = without_debug(&daemon.stderr);

        prop_assert_eq!(&direct.stdout, &daemon.stdout, "case {:?}", case);
        prop_assert_eq!(direct.exit_code, daemon.exit_code, "case {:?}", case);
        prop_assert_eq!(&direct_non_debug, &daemon_non_debug, "case {:?}", case);

        prop_assert_eq!(&direct.stdout, &expected.stdout, "case {:?}", case);
        prop_assert_eq!(direct.exit_code, expected.exit_code, "case {:?}", case);
        prop_assert_eq!(&direct_non_debug, &expected.stderr, "case {:?}", case);

        prop_assert!(
            direct
                .stderr
                .windows(b"Direct".len())
                .any(|window| window == b"Direct"),
            "direct run emitted no direct-only debug record for {:?}",
            case
        );
        prop_assert!(
            daemon
                .stderr
                .windows(b"Daemon".len())
                .any(|window| window == b"Daemon"),
            "daemon run emitted no daemon-only debug record for {:?}",
            case
        );
        prop_assert_ne!(&direct.stderr, &daemon.stderr, "case {:?}", case);
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 13: direct 与 daemon 的可观察等价性
    // **Validates: Requirements 6.10**
    #[test]
    fn property_13_direct_and_daemon_are_observationally_equivalent(
        fixture in fixture_strategy()
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("property runtime");
        runtime.block_on(verify_fixture(fixture))?;
    }
}
