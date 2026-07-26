#![forbid(unsafe_code)]

#[path = "../support/mod.rs"]
mod support;

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use mcp_cli::{
    CancellationFlag, ClassifyError, CommandContext, ConfigHash, ConnectionManager,
    ConnectionResourceRegistry, DirectConnectionManager, DirectConnector, ErrorClass, ErrorKind,
    RetryPolicy, SecretSet, ServerDefinition, ServerId, SystemClock, ToolFilterConfig,
    TransportConfig, WriterDiagnosticSink, connection::rmcp_adapter::RmcpDirectConnector,
};
use serde_json::{Value, json};
use support::{
    FixedJitter, MemoryWriter, MockHttpScript, MockHttpServer, MockResponse, RequestMatcher,
    ScriptedResponse,
};

const SERVER_NAME: &str = "scripted-http";
const SESSION_ID: &str = "local-session-7";
const AUTHORIZATION: &str = "Bearer local-authorization-secret";
const COOKIE: &str = "session=local-cookie-secret";
const TENANT: &str = "tenant-local";

fn response(result: Value) -> Value {
    json!({"jsonrpc": "2.0", "result": result})
}

fn initialize_response() -> Value {
    response(json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "mock-http-server", "version": "1.0.0"},
        "instructions": "Use this local HTTP fixture only."
    }))
}

fn success_script(list_response: MockResponse, call_response: MockResponse) -> MockHttpScript {
    MockHttpScript::new(vec![
        ScriptedResponse::new(
            RequestMatcher::rpc("initialize"),
            MockResponse::Json {
                body: initialize_response(),
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
            list_response,
        ),
        ScriptedResponse::new(
            RequestMatcher::rpc_cursor("tools/list", Some("page-2")),
            MockResponse::Sse {
                messages: vec![response(json!({
                    "tools": [{
                        "name": "omega",
                        "description": "second page over POST SSE",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"value": {"type": "integer"}},
                            "required": ["value"]
                        }
                    }]
                }))],
                session_id: None,
            },
        ),
        ScriptedResponse::new(RequestMatcher::rpc("tools/call"), call_response),
        ScriptedResponse::new(RequestMatcher::http("DELETE"), MockResponse::Empty),
    ])
}

fn complete_list_page() -> MockResponse {
    MockResponse::Json {
        body: response(json!({
            "tools": [{
                "name": "alpha",
                "description": "first page over JSON",
                "inputSchema": {"type": "object", "properties": {}}
            }],
            "nextCursor": "page-2"
        })),
        session_id: None,
    }
}

fn complete_call_result() -> MockResponse {
    MockResponse::Sse {
        messages: vec![response(json!({
            "content": [{"type": "text", "text": "complete result"}],
            "isError": false,
            "structuredContent": {"accepted": true, "value": 42},
            "vendorExtension": {
                "traceId": "trace-http",
                "future": [null, 7, {"nested": true}]
            }
        }))],
        session_id: None,
    }
}

fn server(url: &str) -> ServerDefinition {
    ServerDefinition {
        name: SERVER_NAME.to_owned(),
        id: ServerId("b".repeat(64)),
        config_hash: ConfigHash([9; 32]),
        transport: TransportConfig::Http {
            url: url::Url::parse(url).expect("fixture URL"),
            headers: BTreeMap::from([
                ("Authorization".to_owned(), AUTHORIZATION.to_owned()),
                ("Cookie".to_owned(), COOKIE.to_owned()),
                ("X-Tenant".to_owned(), TENANT.to_owned()),
            ]),
        },
        filter: ToolFilterConfig::default(),
    }
}

fn context(
    writer: &MemoryWriter,
    cancellation: Arc<CancellationFlag>,
    timeout: Duration,
) -> CommandContext {
    let mut secrets = SecretSet::new();
    secrets.register_header("Authorization", AUTHORIZATION);
    secrets.register_header("Cookie", COOKIE);
    secrets.register_header("X-Tenant", TENANT);
    CommandContext {
        deadline: mcp_cli::Deadline::new(Instant::now() + timeout),
        cancellation,
        diagnostics: Arc::new(WriterDiagnosticSink::new(writer.clone(), true, secrets)),
    }
}

async fn bounded<T>(future: impl Future<Output = T>, operation: &str) -> T {
    tokio::time::timeout(Duration::from_secs(8), future)
        .await
        .unwrap_or_else(|_| panic!("{operation} exceeded its bounded test deadline"))
}

fn assert_no_secret(value: &str) {
    for secret in [
        AUTHORIZATION,
        COOKIE,
        TENANT,
        "local-authorization-secret",
        "local-cookie-secret",
    ] {
        assert!(!value.contains(secret), "secret leaked through {value:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_transport_runs_session_lifecycle_pagination_sse_and_complete_call() {
    let mut fixture =
        MockHttpServer::start(success_script(complete_list_page(), complete_call_result()))
            .await
            .expect("start loopback fixture");
    let diagnostics = MemoryWriter::default();
    let ctx = context(
        &diagnostics,
        Arc::new(CancellationFlag::default()),
        Duration::from_secs(10),
    );
    let definition = server(fixture.url());

    let connection = bounded(
        RmcpDirectConnector.connect(&ctx, &definition),
        "HTTP initialize",
    )
    .await
    .expect("HTTP connection should initialize");
    assert_eq!(
        connection.instructions(),
        Some("Use this local HTTP fixture only.")
    );

    let tools = bounded(connection.list_tools(&ctx), "HTTP paginated tools/list")
        .await
        .expect("both HTTP tool pages");
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
        "HTTP tools/call",
    )
    .await
    .expect("complete call result");
    assert_eq!(
        result["structuredContent"],
        json!({"accepted": true, "value": 42})
    );
    assert_eq!(result["vendorExtension"]["traceId"], "trace-http");
    assert_eq!(
        result["vendorExtension"]["future"],
        json!([null, 7, {"nested": true}])
    );

    bounded(connection.close(&ctx), "HTTP close")
        .await
        .expect("HTTP transport close");
    bounded(fixture.wait_for_requests(7), "all lifecycle requests").await;
    bounded(fixture.wait_for_no_connections(), "HTTP sockets to close").await;

    let requests = fixture.requests();
    assert!(fixture.url().starts_with("http://127.0.0.1:"));
    assert!(requests.iter().all(|request| request.path == "/mcp"));
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
    assert_eq!(
        requests
            .iter()
            .map(|request| request.method.as_str())
            .filter(|method| matches!(*method, "GET" | "DELETE"))
            .collect::<Vec<_>>(),
        ["GET", "DELETE"]
    );

    let initialize = requests
        .iter()
        .find(|request| request.rpc_method() == Some("initialize"))
        .unwrap();
    assert_eq!(initialize.session_id, None);
    assert_eq!(initialize.protocol_version, None);
    for request in requests
        .iter()
        .filter(|request| request.rpc_method() != Some("initialize"))
    {
        assert_eq!(request.session_id.as_deref(), Some(SESSION_ID));
        assert_eq!(request.protocol_version.as_deref(), Some("2025-03-26"));
    }
    for request in &requests {
        assert_eq!(request.headers.get("authorization").unwrap(), AUTHORIZATION);
        assert_eq!(request.headers.get("cookie").unwrap(), COOKIE);
        assert_eq!(request.headers.get("x-tenant").unwrap(), TENANT);
    }

    let list_requests = requests
        .iter()
        .filter(|request| request.rpc_method() == Some("tools/list"))
        .collect::<Vec<_>>();
    assert_eq!(
        list_requests[0]
            .body
            .as_ref()
            .unwrap()
            .pointer("/params/cursor"),
        None
    );
    assert_eq!(
        list_requests[1]
            .body
            .as_ref()
            .unwrap()
            .pointer("/params/cursor"),
        Some(&json!("page-2"))
    );
    let call = requests
        .iter()
        .find(|request| request.rpc_method() == Some("tools/call"))
        .unwrap();
    assert_eq!(
        call.body.as_ref().unwrap().pointer("/params/name"),
        Some(&json!("omega"))
    );
    assert_eq!(
        call.body.as_ref().unwrap().pointer("/params/arguments"),
        Some(&json!({"value": 42}))
    );
    assert_eq!(fixture.protocol_errors(), Vec::<String>::new());

    assert_no_secret(&diagnostics.string());
    assert_no_secret(&format!("{definition:?}"));
    assert_no_secret(&format!("{result:?}"));
    fixture.shutdown().await.expect("join fixture tasks");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_statuses_are_typed_without_leaking_configured_headers() {
    for status in [401_u16, 403, 429, 502, 503, 504] {
        let mut fixture = MockHttpServer::start(MockHttpScript::new(vec![ScriptedResponse::new(
            RequestMatcher::rpc("initialize"),
            MockResponse::Status(status),
        )]))
        .await
        .expect("start status fixture");
        let diagnostics = MemoryWriter::default();
        let ctx = context(
            &diagnostics,
            Arc::new(CancellationFlag::default()),
            Duration::from_secs(5),
        );
        let definition = server(fixture.url());
        let registry = ConnectionResourceRegistry::new();
        let manager = DirectConnectionManager::with_retry_components(
            Arc::new(RmcpDirectConnector),
            registry.clone(),
            RetryPolicy::new(0, Duration::from_millis(1)),
            Arc::new(SystemClock),
            Box::new(FixedJitter::new(10_000)),
        );

        let error = match bounded(manager.acquire(&ctx, &definition), "HTTP status failure").await {
            Ok(_) => panic!("scripted status must fail initialize"),
            Err(error) => error,
        };
        assert_eq!(registry.active_resource_count(), 0);
        assert_eq!(
            error.kind,
            if matches!(status, 401 | 403) {
                ErrorKind::AuthError
            } else {
                ErrorKind::NetworkError
            }
        );
        assert_eq!(
            error.class(),
            if matches!(status, 401 | 403) {
                ErrorClass::Auth
            } else {
                ErrorClass::Transient
            }
        );
        assert!(
            error
                .details
                .as_deref()
                .unwrap()
                .contains(&status.to_string())
        );
        assert!(error.message.contains(SERVER_NAME));
        assert_no_secret(&format!("{error} {error:?} {}", diagnostics.string()));

        bounded(
            fixture.wait_for_no_connections(),
            "failed HTTP socket close",
        )
        .await;
        assert_eq!(fixture.protocol_errors(), Vec::<String>::new());
        fixture.shutdown().await.expect("join status fixture");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_is_a_safe_network_error_without_socket_or_task_leaks() {
    let mut fixture = MockHttpServer::start(MockHttpScript::new(vec![ScriptedResponse::new(
        RequestMatcher::rpc("initialize"),
        MockResponse::Disconnect,
    )]))
    .await
    .expect("start disconnect fixture");
    let diagnostics = MemoryWriter::default();
    let ctx = context(
        &diagnostics,
        Arc::new(CancellationFlag::default()),
        Duration::from_secs(5),
    );
    let definition = server(fixture.url());

    let error = match bounded(
        RmcpDirectConnector.connect(&ctx, &definition),
        "disconnected initialize",
    )
    .await
    {
        Ok(_) => panic!("disconnect must fail initialize"),
        Err(error) => error,
    };
    assert_eq!(error.http_status(), None);
    assert_no_secret(&format!("{error} {error:?} {}", diagnostics.string()));
    bounded(fixture.wait_for_no_connections(), "disconnect socket close").await;
    fixture.shutdown().await.expect("join disconnect fixture");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_stops_an_in_flight_request_and_close_deletes_the_session() {
    let mut fixture =
        MockHttpServer::start(success_script(MockResponse::Hold, complete_call_result()))
            .await
            .expect("start cancellation fixture");
    let diagnostics = MemoryWriter::default();
    let cancellation = Arc::new(CancellationFlag::default());
    let ctx = context(&diagnostics, cancellation.clone(), Duration::from_secs(10));
    let definition = server(fixture.url());
    let registry = ConnectionResourceRegistry::new();
    let manager = DirectConnectionManager::with_retry_components(
        Arc::new(RmcpDirectConnector),
        registry.clone(),
        RetryPolicy::new(0, Duration::ZERO),
        Arc::new(SystemClock),
        Box::new(FixedJitter::new(10_000)),
    );
    let connection = bounded(
        manager.acquire(&ctx, &definition),
        "cancellation fixture initialize",
    )
    .await
    .expect("initialize cancellation fixture");
    assert_eq!(registry.active_resource_count(), 1);

    let (result, ()) = tokio::join!(connection.list_tools(&ctx), async {
        bounded(fixture.wait_for_requests(4), "held tools request capture").await;
        cancellation.cancel();
    });
    let error = result.expect_err("cancelled request must stop");
    assert!(error.is_cancelled());
    assert_no_secret(&format!("{error} {error:?}"));
    assert_eq!(
        registry.active_resource_count(),
        0,
        "automatic cancellation cleanup must release the command registry"
    );

    bounded(fixture.wait_for_requests(5), "DELETE after cancellation").await;
    bounded(fixture.wait_for_no_connections(), "cancelled sockets close").await;
    assert!(
        fixture
            .requests()
            .iter()
            .any(|request| request.method == "DELETE")
    );
    assert_eq!(fixture.protocol_errors(), Vec::<String>::new());
    fixture.shutdown().await.expect("join cancellation fixture");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deadline_stops_a_delayed_request_then_close_reaps_all_http_work() {
    let mut fixture =
        MockHttpServer::start(success_script(MockResponse::Hold, complete_call_result()))
            .await
            .expect("start timeout fixture");
    let diagnostics = MemoryWriter::default();
    let ctx = context(
        &diagnostics,
        Arc::new(CancellationFlag::default()),
        Duration::from_millis(500),
    );
    let definition = server(fixture.url());
    let registry = ConnectionResourceRegistry::new();
    let manager = DirectConnectionManager::with_retry_components(
        Arc::new(RmcpDirectConnector),
        registry.clone(),
        RetryPolicy::new(0, Duration::ZERO),
        Arc::new(SystemClock),
        Box::new(FixedJitter::new(10_000)),
    );
    let connection = bounded(
        manager.acquire(&ctx, &definition),
        "timeout fixture initialize",
    )
    .await
    .expect("initialize timeout fixture");
    assert_eq!(registry.active_resource_count(), 1);

    let error = bounded(connection.list_tools(&ctx), "deadline enforcement")
        .await
        .expect_err("held request must reach command deadline");
    assert!(error.is_timeout());
    assert_no_secret(&format!("{error} {error:?}"));
    assert_eq!(
        registry.active_resource_count(),
        0,
        "automatic deadline cleanup must release the command registry"
    );

    bounded(fixture.wait_for_requests(5), "DELETE after timeout").await;
    bounded(fixture.wait_for_no_connections(), "timed-out sockets close").await;
    assert!(
        fixture
            .requests()
            .iter()
            .any(|request| request.method == "DELETE")
    );
    assert_eq!(fixture.protocol_errors(), Vec::<String>::new());
    fixture.shutdown().await.expect("join timeout fixture");
}
