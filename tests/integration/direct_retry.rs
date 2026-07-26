#![forbid(unsafe_code)]

#[path = "../support/mod.rs"]
mod support;

use std::{
    collections::BTreeMap,
    future::pending,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use mcp_cli::{
    BoxFuture, CancellationFlag, ClassifyError, CommandContext, ConfigHash, ConnectionError,
    ConnectionManager, ConnectionMode, ConnectionResourceRegistry, DirectConnectionManager,
    ErrorClass, ErrorKind, JsonObject, McpConnection, RetryPolicy, ServerDefinition, ServerId,
    ToolFilterConfig, ToolInfo, ToolResult, TransportConfig,
};
use serde_json::json;
use support::{
    DiagnosticEvent, FakeClock, FixedJitter, MockConnectionHandle, MockConnector,
    MockMcpConnection, RecordingDiagnosticSink,
};

fn server() -> ServerDefinition {
    ServerDefinition {
        name: "retry-target".to_owned(),
        id: ServerId("c".repeat(64)),
        config_hash: ConfigHash([3; 32]),
        transport: TransportConfig::Stdio {
            command: "unused-mock-command".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        },
        filter: ToolFilterConfig::default(),
    }
}

fn context(
    clock: &FakeClock,
    cancellation: Arc<CancellationFlag>,
    diagnostics: Arc<RecordingDiagnosticSink>,
    budget: Duration,
) -> CommandContext {
    CommandContext {
        deadline: mcp_cli::Deadline::after(clock, budget),
        cancellation,
        diagnostics,
    }
}

fn manager(
    connector: Arc<MockConnector>,
    clock: Arc<FakeClock>,
    retry_limit: u32,
    base_delay: Duration,
) -> DirectConnectionManager {
    DirectConnectionManager::with_retry_components(
        connector,
        ConnectionResourceRegistry::new(),
        RetryPolicy::new(retry_limit, base_delay),
        clock,
        Box::new(FixedJitter::new(10_000)),
    )
}

fn successful_connection() -> (MockMcpConnection, MockConnectionHandle) {
    MockMcpConnection::new(ConnectionMode::Direct)
}

#[tokio::test]
async fn transient_connect_list_and_call_each_invoke_one_underlying_operation_per_attempt() {
    let clock = Arc::new(FakeClock::new(Instant::now()));
    let diagnostics = Arc::new(RecordingDiagnosticSink::default());
    let cancellation = Arc::new(CancellationFlag::default());
    let ctx = context(
        clock.as_ref(),
        cancellation,
        Arc::clone(&diagnostics),
        Duration::from_secs(30),
    );
    let connector = Arc::new(MockConnector::new());
    connector.queue_error(ConnectionError::with_source(
        "first connect",
        std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
    ));
    connector.queue_error(ConnectionError::new("second connect").with_http_status(503));
    let (connection, handle) = successful_connection();
    handle.queue_list_result(Err(ConnectionError::new("first list").with_http_status(504)));
    handle.queue_list_result(Ok(vec![ToolInfo {
        name: "echo".to_owned(),
        description: None,
        input_schema: json!({"type": "object"}),
    }]));
    handle.queue_call_result(Err(ConnectionError::new("first call").with_http_status(429)));
    handle.queue_call_result(Ok(json!({"content": [], "ok": true})));
    connector.queue_connection(connection);
    let manager = manager(
        Arc::clone(&connector),
        Arc::clone(&clock),
        3,
        Duration::ZERO,
    );

    let connection = manager
        .acquire(&ctx, &server())
        .await
        .expect("third connect");
    let tools = connection.list_tools(&ctx).await.expect("second list");
    let result = connection
        .call_tool(&ctx, "echo", JsonObject::new())
        .await
        .expect("second call");

    assert_eq!(connector.calls().len(), 3);
    assert_eq!(tools.len(), 1);
    assert_eq!(result["ok"], true);
    assert_eq!(
        handle
            .calls()
            .iter()
            .filter(|call| matches!(call, support::ConnectionCall::ListTools))
            .count(),
        2
    );
    assert_eq!(
        handle
            .calls()
            .iter()
            .filter(|call| matches!(call, support::ConnectionCall::CallTool { .. }))
            .count(),
        2
    );
    connection.close(&ctx).await.expect("close");
}

#[tokio::test]
async fn auth_and_business_failures_are_not_retried() {
    let clock = Arc::new(FakeClock::new(Instant::now()));
    let diagnostics = Arc::new(RecordingDiagnosticSink::default());
    let ctx = context(
        clock.as_ref(),
        Arc::new(CancellationFlag::default()),
        diagnostics,
        Duration::from_secs(30),
    );

    let auth_connector = Arc::new(MockConnector::new());
    auth_connector.queue_error(ConnectionError::new("auth").with_http_status(401));
    let (unused, _) = successful_connection();
    auth_connector.queue_connection(unused);
    let auth_manager = manager(
        Arc::clone(&auth_connector),
        Arc::clone(&clock),
        5,
        Duration::ZERO,
    );
    let auth = match auth_manager.acquire(&ctx, &server()).await {
        Ok(_) => panic!("401 must fail"),
        Err(error) => error,
    };
    assert_eq!(auth.kind, ErrorKind::AuthError);
    assert_eq!(auth_connector.calls().len(), 1);

    let non_transient_connector = Arc::new(MockConnector::new());
    non_transient_connector
        .queue_error(ConnectionError::new("protocol failure").with_class(ErrorClass::NonTransient));
    let non_transient_manager = manager(
        Arc::clone(&non_transient_connector),
        Arc::clone(&clock),
        5,
        Duration::ZERO,
    );
    let non_transient = match non_transient_manager.acquire(&ctx, &server()).await {
        Ok(_) => panic!("non-transient connect failure must fail"),
        Err(error) => error,
    };
    assert_eq!(non_transient.error_class(), ErrorClass::NonTransient);
    assert_eq!(non_transient_connector.calls().len(), 1);

    let business_connector = Arc::new(MockConnector::new());
    let (connection, handle) = successful_connection();
    handle.queue_list_result(Err(
        ConnectionError::new("explicit business failure").with_class(ErrorClass::Business)
    ));
    handle.queue_list_result(Ok(Vec::new()));
    business_connector.queue_connection(connection);
    let business_manager = manager(business_connector, Arc::clone(&clock), 5, Duration::ZERO);
    let connection = business_manager
        .acquire(&ctx, &server())
        .await
        .expect("connect");
    let error = connection
        .list_tools(&ctx)
        .await
        .expect_err("business failure");
    assert_eq!(error.class(), ErrorClass::Business);
    assert_eq!(
        handle
            .calls()
            .iter()
            .filter(|call| matches!(call, support::ConnectionCall::ListTools))
            .count(),
        1
    );
    connection.close(&ctx).await.expect("close");
}

struct PendingConnection {
    list_attempts: Arc<AtomicUsize>,
}

impl McpConnection for PendingConnection {
    fn list_tools<'a>(
        &'a self,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
        self.list_attempts.fetch_add(1, Ordering::SeqCst);
        Box::pin(pending())
    }

    fn call_tool<'a>(
        &'a self,
        _ctx: &'a CommandContext,
        _name: &'a str,
        _args: JsonObject,
    ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
        Box::pin(pending())
    }

    fn instructions(&self) -> Option<&str> {
        None
    }

    fn close<'a>(
        self: Box<Self>,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<(), ConnectionError>> {
        Box::pin(async { Ok(()) })
    }

    fn mode(&self) -> ConnectionMode {
        ConnectionMode::Direct
    }
}

async fn wait_for_attempt(counter: &AtomicUsize) {
    while counter.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn total_deadline_aborts_an_unfinished_request_without_starting_another_attempt() {
    let clock = Arc::new(FakeClock::new(Instant::now()));
    let ctx = context(
        clock.as_ref(),
        Arc::new(CancellationFlag::default()),
        Arc::new(RecordingDiagnosticSink::default()),
        Duration::from_secs(1),
    );
    let attempts = Arc::new(AtomicUsize::new(0));
    let connector = Arc::new(MockConnector::new());
    connector.queue_connection(PendingConnection {
        list_attempts: Arc::clone(&attempts),
    });
    let manager = manager(connector, Arc::clone(&clock), 5, Duration::ZERO);
    let connection = manager.acquire(&ctx, &server()).await.expect("connect");

    let (result, ()) = tokio::join!(connection.list_tools(&ctx), async {
        wait_for_attempt(&attempts).await;
        clock.advance(Duration::from_secs(1));
    });

    let error = result.expect_err("deadline must abort pending request");
    assert!(error.is_timeout());
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    connection.close(&ctx).await.expect("close");
}

#[tokio::test]
async fn cancellation_aborts_an_unfinished_request_without_retrying_it() {
    let clock = Arc::new(FakeClock::new(Instant::now()));
    let cancellation = Arc::new(CancellationFlag::default());
    let ctx = context(
        clock.as_ref(),
        Arc::clone(&cancellation),
        Arc::new(RecordingDiagnosticSink::default()),
        Duration::from_secs(30),
    );
    let attempts = Arc::new(AtomicUsize::new(0));
    let connector = Arc::new(MockConnector::new());
    connector.queue_connection(PendingConnection {
        list_attempts: Arc::clone(&attempts),
    });
    let manager = manager(connector, Arc::clone(&clock), 5, Duration::ZERO);
    let connection = manager.acquire(&ctx, &server()).await.expect("connect");

    let (result, ()) = tokio::join!(connection.list_tools(&ctx), async {
        wait_for_attempt(&attempts).await;
        cancellation.cancel();
    });

    let error = result.expect_err("cancellation must abort pending request");
    assert!(error.is_cancelled());
    assert_eq!(error.class(), ErrorClass::Cancelled);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    connection.close(&ctx).await.expect("close");
}

#[tokio::test]
async fn cancellation_aborts_backoff_and_does_not_start_the_next_connect_attempt() {
    let clock = Arc::new(FakeClock::new(Instant::now()));
    let cancellation = Arc::new(CancellationFlag::default());
    let diagnostics = Arc::new(RecordingDiagnosticSink::default());
    let ctx = context(
        clock.as_ref(),
        Arc::clone(&cancellation),
        Arc::clone(&diagnostics),
        Duration::from_secs(30),
    );
    let connector = Arc::new(MockConnector::new());
    connector.queue_error(
        ConnectionError::new("configured-secret-must-not-appear").with_http_status(503),
    );
    let (unused, _) = successful_connection();
    connector.queue_connection(unused);
    let manager = manager(
        Arc::clone(&connector),
        Arc::clone(&clock),
        3,
        Duration::from_secs(1),
    );

    let target = server();
    let (result, ()) = tokio::join!(manager.acquire(&ctx, &target), async {
        while diagnostics.events().is_empty() {
            tokio::task::yield_now().await;
        }
        cancellation.cancel();
    });

    let error = match result {
        Ok(_) => panic!("cancelled backoff must fail"),
        Err(error) => error,
    };
    assert_eq!(error.kind, ErrorKind::NetworkError);
    assert_eq!(error.error_class(), ErrorClass::Cancelled);
    assert_eq!(connector.calls().len(), 1);

    let debug = diagnostics
        .events()
        .into_iter()
        .filter_map(|event| match event {
            DiagnosticEvent::Debug(message) => Some(message),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(debug.contains("next_attempt=1"));
    assert!(debug.contains("error_class=transient"));
    assert!(debug.contains("delay_ns=1000000000"));
    assert!(!debug.contains("configured-secret-must-not-appear"));
}
