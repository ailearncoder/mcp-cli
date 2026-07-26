#[path = "../support/mod.rs"]
mod support;

use std::{
    error::Error,
    fmt,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use mcp_cli::{
    BoxFuture, ClassifyError, Clock, CommandContext, Deadline, ErrorClass, RetryError, RetryPolicy,
    SecretSet, WriterDiagnosticSink, retry,
};
use proptest::prelude::*;
use support::{FixedJitter, MemoryWriter, TestCancellationToken};

#[derive(Debug)]
struct FakeClock(Mutex<Instant>);

impl FakeClock {
    fn new(start: Instant) -> Self {
        Self(Mutex::new(start))
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        *self.0.lock().expect("fake clock mutex")
    }

    fn sleep_until(&self, deadline: Instant) -> BoxFuture<'_, ()> {
        *self.0.lock().expect("fake clock mutex") = deadline;
        Box::pin(async {})
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GeneratedClass {
    Transient,
    NonTransient,
    Auth,
    Business,
    Cancelled,
}

impl GeneratedClass {
    const fn error_class(self) -> ErrorClass {
        match self {
            Self::Transient => ErrorClass::Transient,
            Self::NonTransient => ErrorClass::NonTransient,
            Self::Auth => ErrorClass::Auth,
            Self::Business => ErrorClass::Business,
            Self::Cancelled => ErrorClass::Cancelled,
        }
    }
}

#[derive(Debug)]
struct SecretSource(String);

impl fmt::Display for SecretSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "source payload={}", self.0)
    }
}

impl Error for SecretSource {}

#[derive(Debug)]
struct SecretError {
    class: GeneratedClass,
    payload: String,
    source: SecretSource,
}

impl fmt::Display for SecretError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "display payload={}", self.payload)
    }
}

impl Error for SecretError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

impl ClassifyError for SecretError {
    fn class(&self) -> ErrorClass {
        self.class.error_class()
    }
}

#[derive(Debug)]
struct Observation {
    result: Result<(), RetryError<SecretError>>,
    output: String,
}

fn execute(
    class: GeneratedClass,
    delay: Duration,
    secrets: &[String],
    debug_enabled: bool,
    schedule_retry: bool,
) -> Observation {
    let start = Instant::now();
    let clock = FakeClock::new(start);
    let budget = if schedule_retry {
        delay + Duration::from_secs(1)
    } else {
        delay
    };
    let writer = MemoryWriter::default();
    let mut secret_set = SecretSet::new();
    for secret in secrets {
        secret_set.insert(secret);
    }
    let diagnostics = Arc::new(WriterDiagnosticSink::new(
        writer.clone(),
        debug_enabled,
        secret_set,
    ));
    let context = CommandContext {
        deadline: Deadline::after(&clock, budget),
        cancellation: Arc::new(TestCancellationToken::default()),
        diagnostics,
    };
    context
        .diagnostics
        .warning(&format!("warning {}", secrets.join(" ")));

    let payload = secrets.join("|");
    let mut first_error = Some(SecretError {
        class,
        payload: payload.clone(),
        source: SecretSource(payload),
    });
    let mut jitter = FixedJitter::new(10_000);
    let policy = RetryPolicy::new(1, delay);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("property runtime");
    let result = runtime.block_on(retry(&context, &policy, &clock, &mut jitter, |_| {
        let result = first_error.take().map_or(Ok(()), Err);
        async move { result }
    }));

    Observation {
        result,
        output: writer.string(),
    }
}

fn generated_class() -> impl Strategy<Value = GeneratedClass> {
    prop_oneof![
        Just(GeneratedClass::Transient),
        Just(GeneratedClass::NonTransient),
        Just(GeneratedClass::Auth),
        Just(GeneratedClass::Business),
        Just(GeneratedClass::Cancelled),
    ]
}

fn generated_secrets() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec("[A-Za-z0-9]{1,24}", 1..5).prop_map(|tokens| {
        tokens
            .into_iter()
            .enumerate()
            .map(|(index, token)| format!("credential_{index}_{token}"))
            .collect()
    })
}

fn warning_lines(output: &str) -> Vec<String> {
    output
        .lines()
        .filter(|line| line.starts_with("[mcp-cli] warning: "))
        .map(str::to_owned)
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 20: 重试诊断契约
    // **Validates: Requirements 8.8, 8.9**
    #[test]
    fn property_20_retry_diagnostics_contract(
        class in generated_class(),
        delay_nanos in 1_u64..=1_000_000_000,
        secrets in generated_secrets(),
        schedule_retry in any::<bool>(),
    ) {
        let delay = Duration::from_nanos(delay_nanos);
        let enabled = execute(class, delay, &secrets, true, schedule_retry);
        let disabled = execute(class, delay, &secrets, false, schedule_retry);
        let enabled_debug = enabled
            .output
            .lines()
            .filter(|line| line.starts_with("[mcp-cli] debug: "))
            .collect::<Vec<_>>();

        prop_assert!(disabled.output.lines().all(|line| !line.starts_with("[mcp-cli] debug: ")));
        prop_assert_eq!(warning_lines(&enabled.output), warning_lines(&disabled.output));
        prop_assert_eq!(warning_lines(&enabled.output).len(), 1);

        for secret in &secrets {
            prop_assert!(!enabled.output.contains(secret));
            prop_assert!(!disabled.output.contains(secret));
        }
        prop_assert!(!enabled.output.contains("display payload="));
        prop_assert!(!enabled.output.contains("source payload="));

        if class == GeneratedClass::Transient && schedule_retry {
            prop_assert!(enabled.result.is_ok());
            prop_assert!(disabled.result.is_ok());
            prop_assert_eq!(enabled_debug.len(), 1);
            prop_assert!(enabled_debug[0].contains("retry scheduled"));
            prop_assert!(enabled_debug[0].contains("next_attempt=1"));
            prop_assert!(enabled_debug[0].contains("error_class=transient"));
            let expected_delay = format!("delay_ns={}", delay_nanos);
            prop_assert!(enabled_debug[0].contains(&expected_delay));
        } else {
            prop_assert!(enabled_debug.is_empty());
            prop_assert!(!enabled.output.contains("retry scheduled"));

            if class == GeneratedClass::Transient {
                prop_assert!(matches!(enabled.result, Err(RetryError::Timeout)));
                prop_assert!(matches!(disabled.result, Err(RetryError::Timeout)));
            } else {
                prop_assert!(matches!(
                    enabled.result,
                    Err(RetryError::Operation(ref error)) if error.class == class
                ));
                prop_assert!(matches!(
                    disabled.result,
                    Err(RetryError::Operation(ref error)) if error.class == class
                ));
            }
        }
    }
}
