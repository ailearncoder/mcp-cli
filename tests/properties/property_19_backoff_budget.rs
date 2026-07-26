#[path = "../support/mod.rs"]
mod support;

use std::{
    error::Error,
    fmt,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use mcp_cli::{
    Attempt, BoxFuture, ClassifyError, Clock, CommandContext, Deadline, ErrorClass, RetryError,
    RetryPolicy, retry,
};
use proptest::prelude::*;
use support::{FixedJitter, RecordingDiagnosticSink, TestCancellationToken};

const MAX_DELAY_NANOS: u128 = 10_000_000_000;

#[derive(Debug)]
struct RecordingFakeClock {
    now: Mutex<Instant>,
    sleep_targets: Mutex<Vec<Instant>>,
}

impl RecordingFakeClock {
    fn new(start: Instant) -> Self {
        Self {
            now: Mutex::new(start),
            sleep_targets: Mutex::new(Vec::new()),
        }
    }

    fn current(&self) -> Instant {
        *self.now.lock().expect("fake clock mutex")
    }

    fn sleep_targets(&self) -> Vec<Instant> {
        self.sleep_targets
            .lock()
            .expect("sleep target mutex")
            .clone()
    }
}

impl Clock for RecordingFakeClock {
    fn now(&self) -> Instant {
        self.current()
    }

    fn sleep_until(&self, deadline: Instant) -> BoxFuture<'_, ()> {
        self.sleep_targets
            .lock()
            .expect("sleep target mutex")
            .push(deadline);
        *self.now.lock().expect("fake clock mutex") = deadline;
        Box::pin(async {})
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TransientError;

impl fmt::Display for TransientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("transient test failure")
    }
}

impl Error for TransientError {}

impl ClassifyError for TransientError {
    fn class(&self) -> ErrorClass {
        ErrorClass::Transient
    }
}

#[derive(Debug)]
struct Observation {
    result: Result<u8, RetryError<TransientError>>,
    attempts: Vec<Attempt>,
    sleep_targets: Vec<Instant>,
    start: Instant,
    final_time: Instant,
}

fn reference_backoff(base: Duration, attempt: u32) -> Duration {
    let factor = 1_u128.checked_shl(attempt).unwrap_or(u128::MAX);
    let nanos = base.as_nanos().saturating_mul(factor).min(MAX_DELAY_NANOS);
    duration_from_nanos(nanos)
}

fn reference_jittered(base: Duration, attempt: u32, jitter: u16) -> Duration {
    let nanos = reference_backoff(base, attempt)
        .as_nanos()
        .saturating_mul(u128::from(jitter))
        / 10_000;
    duration_from_nanos(nanos)
}

fn duration_from_nanos(nanos: u128) -> Duration {
    Duration::new(
        (nanos / 1_000_000_000) as u64,
        (nanos % 1_000_000_000) as u32,
    )
}

async fn execute_budget_case(
    base: Duration,
    target_attempt: u32,
    jitter_basis_points: u16,
    target_remaining: Duration,
) -> Observation {
    let start = Instant::now();
    let clock = RecordingFakeClock::new(start);
    let elapsed_before_target = (0..target_attempt).fold(Duration::ZERO, |elapsed, attempt| {
        elapsed + reference_jittered(base, attempt, jitter_basis_points)
    });
    let total_budget = elapsed_before_target + target_remaining;
    let context = CommandContext {
        deadline: Deadline::after(&clock, total_budget),
        cancellation: Arc::new(TestCancellationToken::default()),
        diagnostics: Arc::new(RecordingDiagnosticSink::default()),
    };
    let policy = RetryPolicy::new(target_attempt + 1, base);
    let mut jitter = FixedJitter::new(jitter_basis_points);
    let mut attempts = Vec::new();

    let result = retry(&context, &policy, &clock, &mut jitter, |attempt| {
        attempts.push(attempt);
        async move {
            if attempt.index <= target_attempt {
                Err(TransientError)
            } else {
                Ok(7)
            }
        }
    })
    .await;

    Observation {
        result,
        attempts,
        sleep_targets: clock.sleep_targets(),
        start,
        final_time: clock.current(),
    }
}

fn expected_sleep_targets(
    start: Instant,
    base: Duration,
    target_attempt: u32,
    jitter_basis_points: u16,
    include_target: bool,
) -> Vec<Instant> {
    let sleep_count = target_attempt + u32::from(include_target);
    let mut elapsed = Duration::ZERO;
    (0..sleep_count)
        .map(|attempt| {
            elapsed += reference_jittered(base, attempt, jitter_basis_points);
            start + elapsed
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 19: 退避、抖动与预算边界
    // **Validates: Requirements 8.3, 8.4, 8.5**
    #[test]
    fn property_19_backoff_jitter_and_budget_boundaries(
        base_millis in 1_u64..=60_000,
        attempt in 0_u32..=255,
        jitter_basis_points in 7_500_u16..=12_500,
        enough_budget in any::<bool>(),
        boundary_basis_points in 1_u16..=10_000,
        positive_margin_nanos in 1_u64..=5_000_000_000,
    ) {
        let base = Duration::from_millis(base_millis);
        let policy = RetryPolicy::new(attempt + 1, base);
        let expected_pre_jitter = reference_backoff(base, attempt);
        let actual_delay = policy.jittered_delay(attempt, jitter_basis_points);
        let lower_bound = duration_from_nanos(
            expected_pre_jitter.as_nanos().saturating_mul(7_500) / 10_000,
        );
        let upper_bound = duration_from_nanos(
            expected_pre_jitter.as_nanos().saturating_mul(12_500) / 10_000,
        );

        prop_assert_eq!(policy.backoff_delay(attempt), expected_pre_jitter);
        prop_assert_eq!(actual_delay, reference_jittered(base, attempt, jitter_basis_points));
        prop_assert!(actual_delay >= lower_bound);
        prop_assert!(actual_delay <= upper_bound);

        let target_remaining = if enough_budget {
            actual_delay + Duration::from_nanos(positive_margin_nanos)
        } else {
            duration_from_nanos(
                actual_delay
                    .as_nanos()
                    .saturating_mul(u128::from(boundary_basis_points))
                    / 10_000,
            )
        };
        prop_assert!(target_remaining > Duration::ZERO);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("property runtime");
        let observed = runtime.block_on(execute_budget_case(
            base,
            attempt,
            jitter_basis_points,
            target_remaining,
        ));
        let start = observed.start;

        if actual_delay >= target_remaining {
            prop_assert_eq!(observed.result, Err(RetryError::Timeout));
            prop_assert_eq!(observed.attempts.len(), attempt as usize + 1);
            prop_assert_eq!(observed.sleep_targets.len(), attempt as usize);
            prop_assert_eq!(
                &observed.sleep_targets,
                &expected_sleep_targets(start, base, attempt, jitter_basis_points, false),
            );
        } else {
            prop_assert_eq!(observed.result, Ok(7));
            prop_assert_eq!(observed.attempts.len(), attempt as usize + 2);
            prop_assert_eq!(observed.sleep_targets.len(), attempt as usize + 1);
            prop_assert_eq!(
                &observed.sleep_targets,
                &expected_sleep_targets(start, base, attempt, jitter_basis_points, true),
            );
            prop_assert_eq!(
                observed.final_time,
                *observed.sleep_targets.last().expect("target sleep recorded"),
            );
        }
    }
}
