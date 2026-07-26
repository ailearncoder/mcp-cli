mod support;

use std::{io::Write, sync::Arc, time::Duration};

use mcp_cli::{
    Clock, CommandContext, ConnectionMode, Deadline, DiagnosticSink, JitterSource, JsonObject,
    McpConnection,
};
use serde_json::json;
use support::{
    CapturedOutput, ConnectionCall, FakeClock, FixedJitter, MockMcpConnection,
    RecordingDiagnosticSink, SeededJitter, TestCancellationToken,
};

#[tokio::test]
async fn fake_clock_advances_and_releases_sleepers() {
    let start = std::time::Instant::now();
    let clock = FakeClock::new(start);
    let sleeping_clock = clock.clone();
    let deadline = start + Duration::from_secs(5);
    let sleeper = tokio::spawn(async move { sleeping_clock.sleep_until(deadline).await });

    tokio::task::yield_now().await;
    assert!(!sleeper.is_finished());
    assert_eq!(clock.advance(Duration::from_secs(5)), deadline);
    sleeper.await.expect("fake-clock sleeper should finish");
}

#[test]
fn fixed_and_seeded_jitter_are_deterministic_and_bounded() {
    let mut fixed = FixedJitter::new(10_000);
    assert_eq!(fixed.factor_basis_points(), 10_000);

    let mut first = SeededJitter::new(42);
    let mut second = SeededJitter::new(42);
    for _ in 0..16 {
        let left = first.factor_basis_points();
        assert_eq!(left, second.factor_basis_points());
        assert!((7500..=12500).contains(&left));
    }
}

#[tokio::test]
async fn mock_connection_records_calls_and_close() {
    let (connection, handle) = MockMcpConnection::new(ConnectionMode::Direct);
    handle.queue_list_result(Ok(vec![]));
    handle.queue_call_result(Ok(json!({"content": []})));

    let diagnostics = Arc::new(RecordingDiagnosticSink::default());
    diagnostics.warning("recorded warning");
    let context = CommandContext {
        deadline: Deadline::new(std::time::Instant::now() + Duration::from_secs(30)),
        cancellation: Arc::new(TestCancellationToken::default()),
        diagnostics,
    };

    connection
        .list_tools(&context)
        .await
        .expect("scripted list result");
    connection
        .call_tool(&context, "echo", JsonObject::new())
        .await
        .expect("scripted call result");
    Box::new(connection)
        .close(&context)
        .await
        .expect("default close result");

    assert_eq!(
        handle.calls(),
        vec![
            ConnectionCall::ListTools,
            ConnectionCall::CallTool {
                name: "echo".into(),
                args: JsonObject::new(),
            },
            ConnectionCall::Close,
        ]
    );
    assert!(handle.is_closed());
}

#[test]
fn stdout_and_stderr_capture_are_independent() {
    let capture = CapturedOutput::default();
    let mut stdout = capture.stdout.clone();
    let mut stderr = capture.stderr.clone();

    write!(stdout, "business output").expect("stdout capture");
    write!(stderr, "diagnostic output").expect("stderr capture");

    assert_eq!(capture.stdout.string(), "business output");
    assert_eq!(capture.stderr.string(), "diagnostic output");
}
