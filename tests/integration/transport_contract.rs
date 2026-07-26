#![forbid(unsafe_code)]

#[path = "../support/mod.rs"]
mod support;

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use mcp_cli::{
    CancellationFlag, CommandContext, ConfigHash, ConnectionManager, ConnectionResourceRegistry,
    DirectConnectionManager, McpConnection, ServerDefinition, ServerId, ToolFilterConfig, ToolInfo,
    ToolResult, TransportConfig, connection::rmcp_adapter::RmcpDirectConnector,
};
use serde_json::{Value, json};
use support::{
    MemoryWriter, MockHttpScript, MockHttpServer, MockResponse, RequestMatcher, ScriptedResponse,
};
use tempfile::TempDir;

const INSTRUCTIONS: &str = "Shared transport contract instructions.";
const SESSION_ID: &str = "transport-contract-session";
const SERVER_NAME: &str = "transport-contract";

#[derive(Clone, Copy, Debug)]
enum TransportCase {
    Stdio,
    Http,
}

#[derive(Debug, PartialEq)]
struct ContractSnapshot {
    instructions: Option<String>,
    tools: Vec<ToolInfo>,
    result: ToolResult,
}

enum RunningFixture {
    Stdio {
        _temp: TempDir,
        observation_path: PathBuf,
    },
    Http(MockHttpServer),
}

fn rpc_result(result: Value) -> Value {
    json!({"jsonrpc": "2.0", "result": result})
}

fn expected_tools() -> Vec<ToolInfo> {
    vec![
        ToolInfo {
            name: "alpha".to_owned(),
            description: Some("first contract page".to_owned()),
            input_schema: json!({"type": "object", "properties": {}}),
        },
        ToolInfo {
            name: "omega".to_owned(),
            description: Some("second contract page".to_owned()),
            input_schema: json!({
                "type": "object",
                "properties": {"value": {"type": "integer"}},
                "required": ["value"]
            }),
        },
    ]
}

fn expected_result() -> ToolResult {
    json!({
        "content": [{"type": "text", "text": "contract result"}],
        "isError": false,
        "structuredContent": {"accepted": true, "value": 42},
        "vendorExtension": {
            "traceId": "transport-neutral-trace",
            "future": [null, 7, {"nested": true}]
        }
    })
}

fn context() -> CommandContext {
    CommandContext {
        deadline: mcp_cli::Deadline::new(Instant::now() + Duration::from_secs(15)),
        cancellation: Arc::new(CancellationFlag::default()),
        diagnostics: Arc::new(mcp_cli::WriterDiagnosticSink::new(
            MemoryWriter::default(),
            true,
            mcp_cli::SecretSet::new(),
        )),
    }
}

async fn bounded<T>(future: impl Future<Output = T>, operation: &str) -> T {
    tokio::time::timeout(Duration::from_secs(12), future)
        .await
        .unwrap_or_else(|_| panic!("{operation} exceeded its bounded test deadline"))
}

async fn read_observation(path: &std::path::Path, predicate: impl Fn(&Value) -> bool) -> Value {
    bounded(
        async {
            loop {
                if let Ok(bytes) = tokio::fs::read(path).await
                    && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
                    && predicate(&value)
                {
                    return value;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        },
        "stdio fixture observation",
    )
    .await
}

async fn start_fixture(case: TransportCase) -> (RunningFixture, ServerDefinition) {
    match case {
        TransportCase::Stdio => {
            let temp = tempfile::tempdir().expect("stdio fixture directory");
            let observation_path = temp.path().join("observation.json");
            let script_path = temp.path().join("script.json");
            let script = json!({
                "observation_path": observation_path,
                "instructions": INSTRUCTIONS,
                "tool_pages": [
                    {
                        "cursor": null,
                        "tools": [{
                            "name": "alpha",
                            "description": "first contract page",
                            "inputSchema": {"type": "object", "properties": {}}
                        }],
                        "next_cursor": "page-2"
                    },
                    {
                        "cursor": "page-2",
                        "tools": [{
                            "name": "omega",
                            "description": "second contract page",
                            "inputSchema": {
                                "type": "object",
                                "properties": {"value": {"type": "integer"}},
                                "required": ["value"]
                            }
                        }]
                    }
                ],
                "call_result": expected_result(),
                "eof_behavior": "exit"
            });
            std::fs::write(
                &script_path,
                serde_json::to_vec(&script).expect("serialize stdio script"),
            )
            .expect("write stdio script");

            let definition = ServerDefinition {
                name: SERVER_NAME.to_owned(),
                id: ServerId("c".repeat(64)),
                config_hash: ConfigHash([11; 32]),
                transport: TransportConfig::Stdio {
                    command: PathBuf::from(env!("CARGO_BIN_EXE_mock-stdio-server"))
                        .to_string_lossy()
                        .into_owned(),
                    args: vec![
                        "--fixture-script".to_owned(),
                        script_path.to_string_lossy().into_owned(),
                    ],
                    env: BTreeMap::new(),
                    cwd: Some(temp.path().to_path_buf()),
                },
                filter: ToolFilterConfig::default(),
            };
            (
                RunningFixture::Stdio {
                    _temp: temp,
                    observation_path,
                },
                definition,
            )
        }
        TransportCase::Http => {
            let fixture = MockHttpServer::start(MockHttpScript::new(vec![
                ScriptedResponse::new(
                    RequestMatcher::rpc("initialize"),
                    MockResponse::Json {
                        body: rpc_result(json!({
                            "protocolVersion": "2025-03-26",
                            "capabilities": {"tools": {"listChanged": false}},
                            "serverInfo": {"name": "contract-http", "version": "1.0.0"},
                            "instructions": INSTRUCTIONS
                        })),
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
                        body: rpc_result(json!({
                            "tools": [{
                                "name": "alpha",
                                "description": "first contract page",
                                "inputSchema": {"type": "object", "properties": {}}
                            }],
                            "nextCursor": "page-2"
                        })),
                        session_id: None,
                    },
                ),
                ScriptedResponse::new(
                    RequestMatcher::rpc_cursor("tools/list", Some("page-2")),
                    MockResponse::Json {
                        body: rpc_result(json!({
                            "tools": [{
                                "name": "omega",
                                "description": "second contract page",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {"value": {"type": "integer"}},
                                    "required": ["value"]
                                }
                            }]
                        })),
                        session_id: None,
                    },
                ),
                ScriptedResponse::new(
                    RequestMatcher::rpc("tools/call"),
                    MockResponse::Json {
                        body: rpc_result(expected_result()),
                        session_id: None,
                    },
                ),
                ScriptedResponse::new(RequestMatcher::http("DELETE"), MockResponse::Empty),
            ]))
            .await
            .expect("start random loopback HTTP fixture");
            assert!(fixture.url().starts_with("http://127.0.0.1:"));
            let definition = ServerDefinition {
                name: SERVER_NAME.to_owned(),
                id: ServerId("d".repeat(64)),
                config_hash: ConfigHash([12; 32]),
                transport: TransportConfig::Http {
                    url: url::Url::parse(fixture.url()).expect("loopback fixture URL"),
                    headers: BTreeMap::new(),
                },
                filter: ToolFilterConfig::default(),
            };
            (RunningFixture::Http(fixture), definition)
        }
    }
}

impl RunningFixture {
    async fn assert_initialized_without_operations(&self) {
        match self {
            Self::Stdio {
                observation_path, ..
            } => {
                let observation = read_observation(observation_path, |value| {
                    value["events"] == json!(["initialize", "initialized"])
                })
                .await;
                assert_eq!(observation["protocol_errors"], json!([]));
            }
            Self::Http(fixture) => {
                bounded(
                    fixture.wait_for_requests(3),
                    "HTTP initialization lifecycle",
                )
                .await;
                let rpc_methods = fixture
                    .requests()
                    .into_iter()
                    .filter_map(|request| request.rpc_method().map(str::to_owned))
                    .collect::<Vec<_>>();
                assert_eq!(
                    rpc_methods,
                    ["initialize", "notifications/initialized"],
                    "no domain operation may be sent before initialization completes"
                );
                assert_eq!(fixture.protocol_errors(), Vec::<String>::new());
            }
        }
    }

    async fn assert_final_protocol_order_and_shutdown(&mut self) {
        match self {
            Self::Stdio {
                observation_path, ..
            } => {
                let observation =
                    read_observation(observation_path, |value| value["eof_seen"] == true).await;
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
                assert_eq!(observation["protocol_errors"], json!([]));
            }
            Self::Http(fixture) => {
                bounded(fixture.wait_for_requests(7), "HTTP contract lifecycle").await;
                bounded(
                    fixture.wait_for_no_connections(),
                    "HTTP contract socket cleanup",
                )
                .await;
                let requests = fixture.requests();
                assert_eq!(
                    requests
                        .iter()
                        .filter_map(|request| request.rpc_method())
                        .collect::<Vec<_>>(),
                    [
                        "initialize",
                        "notifications/initialized",
                        "tools/list",
                        "tools/list",
                        "tools/call"
                    ]
                );
                assert!(requests.iter().any(|request| request.method == "DELETE"));
                assert_eq!(fixture.protocol_errors(), Vec::<String>::new());
                fixture.shutdown().await.expect("join HTTP fixture tasks");
            }
        }
    }
}

async fn close_and_consume(connection: Box<dyn McpConnection>, ctx: &CommandContext) {
    // `close(self: Box<Self>, ...)` consumes the only command/domain handle.
    // After this boundary no list/call/instructions operation is representable.
    bounded(connection.close(ctx), "transport close")
        .await
        .expect("transport closes cleanly");
}

async fn run_contract(case: TransportCase) -> ContractSnapshot {
    let (mut fixture, definition) = start_fixture(case).await;
    let ctx = context();
    let resources = ConnectionResourceRegistry::new();
    let manager =
        DirectConnectionManager::single_target(Arc::new(RmcpDirectConnector), resources.clone());

    let connection = bounded(manager.acquire(&ctx, &definition), "transport initialize")
        .await
        .expect("transport initializes through the common connector");
    assert_eq!(resources.active_resource_count(), 1);
    fixture.assert_initialized_without_operations().await;

    let instructions = connection.instructions().map(str::to_owned);
    let tools = bounded(connection.list_tools(&ctx), "transport tools/list")
        .await
        .expect("transport lists every page");
    let result = bounded(
        connection.call_tool(
            &ctx,
            "omega",
            serde_json::Map::from_iter([("value".to_owned(), json!(42))]),
        ),
        "transport tools/call",
    )
    .await
    .expect("transport preserves the complete tool result");

    close_and_consume(connection, &ctx).await;
    assert_eq!(
        resources.active_resource_count(),
        0,
        "close must release all command-owned resources"
    );
    fixture.assert_final_protocol_order_and_shutdown().await;

    ContractSnapshot {
        instructions,
        tools,
        result,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stdio_and_http_obey_the_same_connection_contract() {
    let expected = ContractSnapshot {
        instructions: Some(INSTRUCTIONS.to_owned()),
        tools: expected_tools(),
        result: expected_result(),
    };

    let mut snapshots = Vec::new();
    for case in [TransportCase::Stdio, TransportCase::Http] {
        let snapshot = run_contract(case).await;
        assert_eq!(snapshot, expected, "{case:?} leaked a transport difference");
        snapshots.push(snapshot);
    }

    assert_eq!(snapshots[0], snapshots[1]);
}
