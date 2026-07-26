#[path = "../support/mod.rs"]
mod support;

use std::{io, sync::Arc, time::Duration, time::Instant};

use mcp_cli::{
    CliError, CommandContext, Deadline, ErrorClass, ErrorKind, RetryError, RetryPolicy,
    RuntimeConfig, classify_errno, classify_http_status, classify_io_error, retry,
};
use support::{FakeClock, FixedJitter, RecordingDiagnosticSink, TestCancellationToken};

fn parse_one(name: &str, value: impl Into<String>) -> Result<RuntimeConfig, CliError> {
    RuntimeConfig::parse([(name.to_owned(), value.into())])
}

fn assert_invalid(name: &str, value: impl Into<String>) {
    let error = parse_one(name, value).expect_err("invalid runtime value must be rejected");
    assert_eq!(error.kind, ErrorKind::InvalidRuntimeConfig);
    assert!(
        error
            .details
            .as_deref()
            .is_some_and(|details| details.contains(name)),
        "error details must identify {name}: {error:?}"
    );
}

#[test]
fn reference_client_runtime_defaults_and_overrides_follow_the_rust_contract() {
    let defaults = RuntimeConfig::parse(Vec::<(String, String)>::new()).expect("runtime defaults");
    assert_eq!(defaults.timeout, Duration::from_secs(1_800));
    assert_eq!(defaults.concurrency.get(), 5);
    assert_eq!(defaults.max_retries, 3);
    assert_eq!(defaults.retry_base_delay, Duration::from_millis(1_000));

    let configured = RuntimeConfig::parse([
        ("MCP_TIMEOUT", "60"),
        ("MCP_CONCURRENCY", "10"),
        ("MCP_MAX_RETRIES", "0"),
        ("MCP_RETRY_DELAY", "2000"),
    ])
    .expect("documented runtime overrides");
    assert_eq!(configured.timeout, Duration::from_secs(60));
    assert_eq!(configured.concurrency.get(), 10);
    assert_eq!(configured.max_retries, 0);
    assert_eq!(configured.retry_base_delay, Duration::from_millis(2_000));

    // Unlike the reference implementation's silent fallback, the Rust spec
    // requires invalid runtime values to fail with a typed client error.
    for (name, value) in [
        ("MCP_TIMEOUT", "invalid"),
        ("MCP_TIMEOUT", "-5"),
        ("MCP_TIMEOUT", "0"),
        ("MCP_CONCURRENCY", "many"),
        ("MCP_CONCURRENCY", "-3"),
        ("MCP_CONCURRENCY", "0"),
        ("MCP_MAX_RETRIES", "-1"),
        ("MCP_RETRY_DELAY", "-500"),
        ("MCP_RETRY_DELAY", "0"),
    ] {
        assert_invalid(name, value);
    }
}

#[test]
fn classifies_every_required_transient_errno_exactly() {
    for errno in [
        "ECONNREFUSED",
        "ECONNRESET",
        "ETIMEDOUT",
        "EPIPE",
        "ENETUNREACH",
        "EHOSTUNREACH",
        "EAI_AGAIN",
    ] {
        assert_eq!(classify_errno(errno), ErrorClass::Transient, "{errno}");
    }

    for kind in [
        io::ErrorKind::ConnectionRefused,
        io::ErrorKind::ConnectionReset,
        io::ErrorKind::TimedOut,
        io::ErrorKind::BrokenPipe,
        io::ErrorKind::NetworkUnreachable,
        io::ErrorKind::HostUnreachable,
    ] {
        assert_eq!(
            classify_io_error(&io::Error::from(kind)),
            ErrorClass::Transient,
            "{kind:?}"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    assert_eq!(
        classify_io_error(&io::Error::from_raw_os_error(-3)),
        ErrorClass::Transient
    );
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    assert_eq!(
        classify_io_error(&io::Error::from_raw_os_error(2)),
        ErrorClass::Transient
    );
    #[cfg(windows)]
    assert_eq!(
        classify_io_error(&io::Error::from_raw_os_error(11_002)),
        ErrorClass::Transient
    );
}

#[test]
fn classifies_http_transient_and_auth_statuses_without_substring_matches() {
    for status in [429, 502, 503, 504] {
        assert_eq!(classify_http_status(status), ErrorClass::Transient);
        assert_eq!(
            CliError::http_status("remote", status).error_class(),
            ErrorClass::Transient
        );
    }
    for status in [401, 403] {
        assert_eq!(classify_http_status(status), ErrorClass::Auth);
        assert_eq!(
            CliError::http_status("remote", status).error_class(),
            ErrorClass::Auth
        );
    }

    assert_eq!(classify_http_status(5029), ErrorClass::NonTransient);
    for ordinary_text in [
        "port 5029 refused",
        "ordinary text containing HTTP 502",
        "ordinary text containing ECONNRESET",
    ] {
        assert_eq!(
            classify_errno(ordinary_text),
            ErrorClass::NonTransient,
            "text must not be classified by substring: {ordinary_text}"
        );
    }
}

#[test]
fn configuration_json_argument_and_business_errors_are_not_retryable() {
    for kind in [
        ErrorKind::InvalidConfig,
        ErrorKind::InvalidRuntimeConfig,
        ErrorKind::InvalidServerConfig,
        ErrorKind::InvalidJson,
        ErrorKind::InvalidArguments,
    ] {
        assert_eq!(kind.error_class(), ErrorClass::NonTransient, "{kind}");
        assert!(!kind.error_class().is_retryable(), "{kind}");
    }

    let business = CliError::tool_execution_failed("server", "tool", "rejected");
    assert_eq!(business.error_class(), ErrorClass::Business);
    assert!(!business.error_class().is_retryable());
}

#[test]
fn numeric_runtime_variables_accept_all_legal_boundaries() {
    let timeout_min = parse_one("MCP_TIMEOUT", "1").expect("minimum timeout");
    assert_eq!(timeout_min.timeout, Duration::from_secs(1));
    let timeout_max = parse_one("MCP_TIMEOUT", u64::MAX.to_string()).expect("maximum timeout");
    assert_eq!(timeout_max.timeout, Duration::from_secs(u64::MAX));

    let concurrency_min = parse_one("MCP_CONCURRENCY", "1").expect("minimum concurrency");
    assert_eq!(concurrency_min.concurrency.get(), 1);
    let concurrency_max =
        parse_one("MCP_CONCURRENCY", usize::MAX.to_string()).expect("maximum concurrency");
    assert_eq!(concurrency_max.concurrency.get(), usize::MAX);

    let retries_min = parse_one("MCP_MAX_RETRIES", "0").expect("zero retries is valid");
    assert_eq!(retries_min.max_retries, 0);
    let retries_max = parse_one("MCP_MAX_RETRIES", u32::MAX.to_string()).expect("maximum retries");
    assert_eq!(retries_max.max_retries, u32::MAX);

    let retry_delay_min = parse_one("MCP_RETRY_DELAY", "1").expect("minimum retry delay");
    assert_eq!(retry_delay_min.retry_base_delay, Duration::from_millis(1));
    let retry_delay_max =
        parse_one("MCP_RETRY_DELAY", u64::MAX.to_string()).expect("maximum retry delay");
    assert_eq!(
        retry_delay_max.retry_base_delay,
        Duration::from_millis(u64::MAX)
    );

    let daemon_timeout_min = parse_one("MCP_DAEMON_TIMEOUT", "1").expect("minimum daemon timeout");
    assert_eq!(
        daemon_timeout_min.daemon_idle_timeout,
        Duration::from_secs(1)
    );
    let daemon_timeout_max =
        parse_one("MCP_DAEMON_TIMEOUT", u64::MAX.to_string()).expect("maximum daemon timeout");
    assert_eq!(
        daemon_timeout_max.daemon_idle_timeout,
        Duration::from_secs(u64::MAX)
    );
}

#[test]
fn numeric_runtime_variables_reject_zero_negative_nondigit_trailing_and_overflow() {
    let u64_overflow = format!("{}0", u64::MAX);
    for name in ["MCP_TIMEOUT", "MCP_RETRY_DELAY", "MCP_DAEMON_TIMEOUT"] {
        for value in ["0", "-1", "not-a-number", "1x", "1 "] {
            assert_invalid(name, value);
        }
        assert_invalid(name, u64_overflow.clone());
    }

    for value in ["0", "-1", "not-a-number", "1x", "1 "] {
        assert_invalid("MCP_CONCURRENCY", value);
    }
    assert_invalid("MCP_CONCURRENCY", (usize::MAX as u128 + 1).to_string());

    for value in ["-1", "not-a-number", "1x", "1 "] {
        assert_invalid("MCP_MAX_RETRIES", value);
    }
    assert_invalid("MCP_MAX_RETRIES", (u64::from(u32::MAX) + 1).to_string());

    assert_eq!(
        parse_one("MCP_MAX_RETRIES", "0")
            .expect("zero max retries must remain valid")
            .max_retries,
        0
    );
}

#[test]
fn deadline_reports_remaining_expiry_and_local_caps() {
    let start = Instant::now();
    let clock = FakeClock::new(start);
    let deadline = Deadline::new(start + Duration::from_secs(10));

    assert_eq!(deadline.remaining(&clock), Duration::from_secs(10));
    assert!(!deadline.is_expired(&clock));
    assert_eq!(
        deadline.remaining_capped(&clock, Duration::from_secs(3)),
        Duration::from_secs(3)
    );
    assert_eq!(
        deadline.local_deadline(&clock, Duration::from_secs(3)),
        start + Duration::from_secs(3)
    );

    clock.set(start + Duration::from_secs(8));
    assert_eq!(deadline.remaining(&clock), Duration::from_secs(2));
    assert_eq!(
        deadline.remaining_capped(&clock, Duration::from_secs(5)),
        Duration::from_secs(2)
    );
    assert_eq!(
        deadline.local_deadline(&clock, Duration::from_secs(5)),
        deadline.expires_at()
    );

    clock.set(deadline.expires_at());
    assert!(deadline.is_expired(&clock));
    assert_eq!(deadline.remaining(&clock), Duration::ZERO);
    assert_eq!(
        deadline.remaining_capped(&clock, Duration::MAX),
        Duration::ZERO
    );
}

#[test]
fn duration_max_deadlines_saturate_without_panicking() {
    let start = Instant::now();
    let clock = FakeClock::new(start);
    let deadline = Deadline::after(&clock, Duration::MAX);

    assert!(deadline.expires_at() >= start);
    assert_eq!(
        deadline.local_deadline(&clock, Duration::MAX),
        deadline.expires_at()
    );
    assert!(deadline.remaining(&clock) <= Duration::MAX);
}

#[tokio::test]
async fn expired_total_budget_does_not_start_an_attempt() {
    let start = Instant::now();
    let clock = FakeClock::new(start);
    let context = CommandContext {
        deadline: Deadline::new(start),
        cancellation: Arc::new(TestCancellationToken::default()),
        diagnostics: Arc::new(RecordingDiagnosticSink::default()),
    };
    let mut jitter = FixedJitter::new(10_000);
    let mut calls = 0_u32;

    let result: Result<(), RetryError<ErrorClass>> = retry(
        &context,
        &RetryPolicy::default(),
        &clock,
        &mut jitter,
        |_| {
            calls += 1;
            async { Ok::<(), ErrorClass>(()) }
        },
    )
    .await;

    assert_eq!(result, Err(RetryError::Timeout));
    assert_eq!(calls, 0, "an expired total budget must prevent attempt 0");
}
