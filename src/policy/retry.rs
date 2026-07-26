//! Retry classification, backoff, jitter, and total-budget execution.

use std::{error::Error, fmt, future::Future, io, time::Duration};

use crate::runtime::{Clock, CommandContext, JitterSource, RuntimeConfig};

/// Stable retry categories derived from structured adapter errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorClass {
    Transient,
    NonTransient,
    Auth,
    Business,
    Cancelled,
}

impl ErrorClass {
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::Transient)
    }
}

/// Error types used with [`retry`] expose their structured retry category.
pub trait ClassifyError {
    fn class(&self) -> ErrorClass;
}

impl ClassifyError for ErrorClass {
    fn class(&self) -> ErrorClass {
        *self
    }
}

/// Classifies an errno symbolic name without substring matching.
///
/// Exact matching prevents unrelated text such as a port number from being
/// interpreted as a retryable status or errno.
pub fn classify_errno(errno: &str) -> ErrorClass {
    match errno {
        "ECONNREFUSED" | "ECONNRESET" | "ETIMEDOUT" | "EPIPE" | "ENETUNREACH" | "EHOSTUNREACH"
        | "EAI_AGAIN" => ErrorClass::Transient,
        _ => ErrorClass::NonTransient,
    }
}

/// Classifies a structured standard-library I/O error.
pub fn classify_io_error(error: &io::Error) -> ErrorClass {
    use io::ErrorKind;

    if matches!(
        error.kind(),
        ErrorKind::ConnectionRefused
            | ErrorKind::ConnectionReset
            | ErrorKind::TimedOut
            | ErrorKind::BrokenPipe
            | ErrorKind::NetworkUnreachable
            | ErrorKind::HostUnreachable
    ) {
        return ErrorClass::Transient;
    }

    match error.raw_os_error() {
        Some(code) if is_eai_again(code) => ErrorClass::Transient,
        _ => ErrorClass::NonTransient,
    }
}

/// Classifies an HTTP response status according to the retry contract.
pub const fn classify_http_status(status: u16) -> ErrorClass {
    match status {
        401 | 403 => ErrorClass::Auth,
        429 | 502 | 503 | 504 => ErrorClass::Transient,
        _ => ErrorClass::NonTransient,
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const fn is_eai_again(code: i32) -> bool {
    code == -3
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
const fn is_eai_again(code: i32) -> bool {
    code == 2
}

#[cfg(windows)]
const fn is_eai_again(code: i32) -> bool {
    code == 11_002
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    windows
)))]
const fn is_eai_again(_code: i32) -> bool {
    false
}

/// Retry limits and delay parameters for one logical operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Maximum retries after the initial attempt.
    pub retry_limit: u32,
    pub base_delay: Duration,
    /// Backoff cap before jitter. The runtime policy fixes this at ten seconds.
    pub max_delay: Duration,
}

impl RetryPolicy {
    pub const MAX_DELAY: Duration = Duration::from_secs(10);

    pub const fn new(retry_limit: u32, base_delay: Duration) -> Self {
        Self {
            retry_limit,
            base_delay,
            max_delay: Self::MAX_DELAY,
        }
    }

    pub const fn from_runtime_config(config: &RuntimeConfig) -> Self {
        Self::new(config.max_retries, config.retry_base_delay)
    }

    /// Returns `min(base_delay * 2^retry_index, max_delay)` using saturating
    /// integer arithmetic for every input.
    pub fn backoff_delay(&self, retry_index: u32) -> Duration {
        let factor = 1_u128.checked_shl(retry_index).unwrap_or(u128::MAX);
        let nanos = self.base_delay.as_nanos().saturating_mul(factor);
        duration_from_nanos(nanos.min(self.max_delay.as_nanos()))
    }

    /// Applies integer basis-point jitter. Out-of-contract sources are clamped
    /// so the executor itself always preserves the required 75%..=125% range.
    pub fn jittered_delay(&self, retry_index: u32, factor_basis_points: u16) -> Duration {
        let factor = u128::from(factor_basis_points.clamp(7_500, 12_500));
        let nanos = self
            .backoff_delay(retry_index)
            .as_nanos()
            .saturating_mul(factor)
            / 10_000;
        duration_from_nanos(nanos)
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::new(3, Duration::from_secs(1))
    }
}

/// Attempt metadata passed to the operation exactly once per invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Attempt {
    /// Initial attempt is zero; each retry increments this value by one.
    pub index: u32,
    /// `None` for the initial attempt and `Some(index - 1)` for retries.
    pub retry_index: Option<u32>,
}

impl Attempt {
    pub const fn initial() -> Self {
        Self {
            index: 0,
            retry_index: None,
        }
    }

    const fn retry(retry_index: u32) -> Self {
        Self {
            index: retry_index.saturating_add(1),
            retry_index: Some(retry_index),
        }
    }
}

/// Executor-level outcomes that are independent of adapter error types.
#[derive(Debug, PartialEq, Eq)]
pub enum RetryError<E> {
    Operation(E),
    Timeout,
    Cancelled,
}

impl<E: fmt::Display> fmt::Display for RetryError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Operation(error) => error.fmt(formatter),
            Self::Timeout => formatter.write_str("total timeout budget exhausted"),
            Self::Cancelled => formatter.write_str("operation cancelled"),
        }
    }
}

impl<E> Error for RetryError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Operation(error) => Some(error),
            Self::Timeout | Self::Cancelled => None,
        }
    }
}

/// Executes one logical operation under a shared absolute command deadline.
///
/// The operation is invoked once for each [`Attempt`]. Only transient errors
/// are retried. If the jittered wait is greater than or equal to the remaining
/// budget, no sleep occurs and no next attempt is started.
pub async fn retry<T, E, F, Fut>(
    ctx: &CommandContext,
    policy: &RetryPolicy,
    clock: &dyn Clock,
    jitter: &mut dyn JitterSource,
    mut operation: F,
) -> Result<T, RetryError<E>>
where
    E: ClassifyError,
    F: FnMut(Attempt) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempt = Attempt::initial();

    loop {
        if ctx.is_cancelled() {
            return Err(RetryError::Cancelled);
        }
        if ctx.deadline.is_expired(clock) {
            return Err(RetryError::Timeout);
        }

        let error = match operation(attempt).await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };

        let retry_index = attempt.index;
        let error_class = error.class();
        if !error_class.is_retryable() || retry_index >= policy.retry_limit {
            return Err(RetryError::Operation(error));
        }
        if ctx.is_cancelled() {
            return Err(RetryError::Cancelled);
        }

        let delay = policy.jittered_delay(retry_index, jitter.factor_basis_points());
        let now = clock.now();
        let remaining = ctx
            .deadline
            .expires_at()
            .checked_duration_since(now)
            .unwrap_or_default();
        if delay >= remaining {
            return Err(RetryError::Timeout);
        }

        let wake_at = now
            .checked_add(delay)
            .expect("a delay below the deadline's remaining budget is representable");
        ctx.diagnostics.debug(&format!(
            "retry scheduled next_attempt={} error_class={} delay_ns={}",
            attempt.index.saturating_add(1),
            diagnostic_error_class(error_class),
            delay.as_nanos()
        ));
        tokio::select! {
            _ = clock.sleep_until(wake_at) => {}
            _ = wait_for_cancellation(ctx) => return Err(RetryError::Cancelled),
        }

        if ctx.is_cancelled() {
            return Err(RetryError::Cancelled);
        }
        if ctx.deadline.is_expired(clock) {
            return Err(RetryError::Timeout);
        }

        attempt = Attempt::retry(retry_index);
    }
}

async fn wait_for_cancellation(ctx: &CommandContext) {
    if ctx.is_cancelled() {
        return;
    }

    let mut poll = tokio::time::interval(Duration::from_millis(5));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        poll.tick().await;
        if ctx.is_cancelled() {
            return;
        }
    }
}

fn diagnostic_error_class(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::Transient => "transient",
        ErrorClass::NonTransient => "non_transient",
        ErrorClass::Auth => "auth",
        ErrorClass::Business => "business",
        ErrorClass::Cancelled => "cancelled",
    }
}

fn duration_from_nanos(nanos: u128) -> Duration {
    const NANOS_PER_SECOND: u128 = 1_000_000_000;
    let seconds = nanos / NANOS_PER_SECOND;
    if seconds > u128::from(u64::MAX) {
        return Duration::MAX;
    }
    Duration::new(seconds as u64, (nanos % NANOS_PER_SECOND) as u32)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        time::Instant,
    };

    use super::*;
    use crate::{
        output::DiagnosticSink,
        runtime::{BoxFuture, CancellationFlag, Deadline},
    };

    #[derive(Debug)]
    struct AdvancingClock {
        now: Mutex<Instant>,
        sleeps: Mutex<Vec<Instant>>,
    }

    impl AdvancingClock {
        fn new(now: Instant) -> Self {
            Self {
                now: Mutex::new(now),
                sleeps: Mutex::new(Vec::new()),
            }
        }

        fn advance(&self, duration: Duration) {
            let mut now = self.now.lock().expect("clock mutex");
            *now = now.checked_add(duration).expect("test clock overflow");
        }

        fn sleeps(&self) -> Vec<Instant> {
            self.sleeps.lock().expect("sleep mutex").clone()
        }
    }

    impl Clock for AdvancingClock {
        fn now(&self) -> Instant {
            *self.now.lock().expect("clock mutex")
        }

        fn sleep_until(&self, deadline: Instant) -> BoxFuture<'_, ()> {
            self.sleeps.lock().expect("sleep mutex").push(deadline);
            *self.now.lock().expect("clock mutex") = deadline;
            Box::pin(async {})
        }
    }

    #[derive(Debug, Default)]
    struct NullDiagnostics;

    impl DiagnosticSink for NullDiagnostics {
        fn warning(&self, _message: &str) {}
        fn debug(&self, _message: &str) {}
        fn server_stderr(&self, _server: &str, _bytes: &[u8]) {}
    }

    #[derive(Debug)]
    struct FixedJitter(u16);

    impl JitterSource for FixedJitter {
        fn factor_basis_points(&mut self) -> u16 {
            self.0
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct TestError(ErrorClass);

    impl fmt::Display for TestError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "{:?}", self.0)
        }
    }

    impl Error for TestError {}

    impl ClassifyError for TestError {
        fn class(&self) -> ErrorClass {
            self.0
        }
    }

    fn context(clock: &dyn Clock, budget: Duration) -> CommandContext {
        CommandContext {
            deadline: Deadline::after(clock, budget),
            cancellation: Arc::new(CancellationFlag::default()),
            diagnostics: Arc::new(NullDiagnostics),
        }
    }

    #[test]
    fn diagnostic_error_classes_have_stable_names() {
        assert_eq!(diagnostic_error_class(ErrorClass::Transient), "transient");
        assert_eq!(
            diagnostic_error_class(ErrorClass::NonTransient),
            "non_transient"
        );
        assert_eq!(diagnostic_error_class(ErrorClass::Auth), "auth");
        assert_eq!(diagnostic_error_class(ErrorClass::Business), "business");
        assert_eq!(diagnostic_error_class(ErrorClass::Cancelled), "cancelled");
    }

    #[test]
    fn classifies_every_required_errno_and_no_similar_names() {
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

        for errno in ["ECONNREFUSED_EXTRA", "EAI_AGAINLY", "EINVAL", ""] {
            assert_eq!(classify_errno(errno), ErrorClass::NonTransient, "{errno}");
        }
    }

    #[test]
    fn classifies_structured_io_error_kinds() {
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
        assert_eq!(
            classify_io_error(&io::Error::from(io::ErrorKind::InvalidInput)),
            ErrorClass::NonTransient
        );
    }

    #[test]
    fn classifies_http_statuses_without_numeric_substring_fallback() {
        for status in [429, 502, 503, 504] {
            assert_eq!(classify_http_status(status), ErrorClass::Transient);
        }
        for status in [401, 403] {
            assert_eq!(classify_http_status(status), ErrorClass::Auth);
        }
        for status in [200, 400, 404, 500, 501, 505, 5029] {
            assert_eq!(classify_http_status(status), ErrorClass::NonTransient);
        }
    }

    #[test]
    fn backoff_saturates_multiplication_and_caps_before_jitter() {
        let policy = RetryPolicy::new(100, Duration::from_nanos(1));
        assert_eq!(policy.backoff_delay(0), Duration::from_nanos(1));
        assert_eq!(
            policy.backoff_delay(33),
            Duration::from_nanos(8_589_934_592)
        );
        assert_eq!(policy.backoff_delay(34), Duration::from_secs(10));
        assert_eq!(policy.backoff_delay(u32::MAX), Duration::from_secs(10));

        let huge = RetryPolicy::new(1, Duration::MAX);
        assert_eq!(huge.backoff_delay(0), Duration::from_secs(10));
        assert_eq!(huge.backoff_delay(1), Duration::from_secs(10));
    }

    #[test]
    fn jitter_uses_integer_basis_points_and_enforces_boundaries() {
        let policy = RetryPolicy::new(1, Duration::from_secs(4));
        assert_eq!(policy.jittered_delay(0, 7_500), Duration::from_secs(3));
        assert_eq!(policy.jittered_delay(0, 10_000), Duration::from_secs(4));
        assert_eq!(policy.jittered_delay(0, 12_500), Duration::from_secs(5));
        assert_eq!(policy.jittered_delay(0, 0), Duration::from_secs(3));
        assert_eq!(policy.jittered_delay(0, u16::MAX), Duration::from_secs(5));
    }

    #[tokio::test]
    async fn retries_transient_failures_once_per_attempt_then_succeeds() {
        let start = Instant::now();
        let clock = AdvancingClock::new(start);
        let ctx = context(&clock, Duration::from_secs(30));
        let policy = RetryPolicy::new(3, Duration::from_secs(1));
        let mut jitter = FixedJitter(10_000);
        let mut results = VecDeque::from([
            Err(TestError(ErrorClass::Transient)),
            Err(TestError(ErrorClass::Transient)),
            Ok(42),
        ]);
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let observed = attempts.clone();

        let value = retry(&ctx, &policy, &clock, &mut jitter, move |attempt| {
            observed.lock().expect("attempt mutex").push(attempt);
            let result = results.pop_front().expect("one result per attempt");
            async move { result }
        })
        .await
        .expect("third attempt succeeds");

        assert_eq!(value, 42);
        assert_eq!(
            *attempts.lock().expect("attempt mutex"),
            [
                Attempt {
                    index: 0,
                    retry_index: None
                },
                Attempt {
                    index: 1,
                    retry_index: Some(0)
                },
                Attempt {
                    index: 2,
                    retry_index: Some(1)
                },
            ]
        );
        assert_eq!(
            clock.sleeps(),
            [
                start + Duration::from_secs(1),
                start + Duration::from_secs(3)
            ]
        );
    }

    #[tokio::test]
    async fn non_retryable_classes_return_after_the_first_attempt() {
        for class in [
            ErrorClass::NonTransient,
            ErrorClass::Auth,
            ErrorClass::Business,
            ErrorClass::Cancelled,
        ] {
            let clock = AdvancingClock::new(Instant::now());
            let ctx = context(&clock, Duration::from_secs(30));
            let mut jitter = FixedJitter(10_000);
            let mut calls = 0;

            let result: Result<(), _> = retry(
                &ctx,
                &RetryPolicy::new(10, Duration::from_secs(1)),
                &clock,
                &mut jitter,
                |_| {
                    calls += 1;
                    let error = TestError(class);
                    async move { Err(error) }
                },
            )
            .await;

            assert_eq!(result, Err(RetryError::Operation(TestError(class))));
            assert_eq!(calls, 1);
            assert!(clock.sleeps().is_empty());
        }
    }

    #[tokio::test]
    async fn zero_retry_limit_never_starts_a_second_attempt() {
        let clock = AdvancingClock::new(Instant::now());
        let ctx = context(&clock, Duration::from_secs(30));
        let mut jitter = FixedJitter(10_000);
        let mut calls = 0;

        let result: Result<(), _> = retry(
            &ctx,
            &RetryPolicy::new(0, Duration::from_secs(1)),
            &clock,
            &mut jitter,
            |_| {
                calls += 1;
                async { Err(TestError(ErrorClass::Transient)) }
            },
        )
        .await;

        assert_eq!(
            result,
            Err(RetryError::Operation(TestError(ErrorClass::Transient)))
        );
        assert_eq!(calls, 1);
        assert!(clock.sleeps().is_empty());
    }

    #[tokio::test]
    async fn insufficient_budget_neither_sleeps_nor_starts_another_attempt() {
        let clock = AdvancingClock::new(Instant::now());
        let ctx = context(&clock, Duration::from_secs(5));
        let policy = RetryPolicy::new(3, Duration::from_secs(4));
        let mut jitter = FixedJitter(12_500);
        let mut calls = 0;

        let result: Result<(), _> = retry(&ctx, &policy, &clock, &mut jitter, |_| {
            calls += 1;
            async { Err(TestError(ErrorClass::Transient)) }
        })
        .await;

        assert_eq!(result, Err(RetryError::Timeout));
        assert_eq!(calls, 1);
        assert!(clock.sleeps().is_empty());
    }

    #[tokio::test]
    async fn operation_time_consumes_the_same_absolute_retry_budget() {
        let clock = AdvancingClock::new(Instant::now());
        let ctx = context(&clock, Duration::from_secs(5));
        let policy = RetryPolicy::new(3, Duration::from_secs(2));
        let mut jitter = FixedJitter(10_000);
        let mut calls = 0;

        let result: Result<(), _> = retry(&ctx, &policy, &clock, &mut jitter, |_| {
            calls += 1;
            clock.advance(Duration::from_secs(3));
            async { Err(TestError(ErrorClass::Transient)) }
        })
        .await;

        assert_eq!(result, Err(RetryError::Timeout));
        assert_eq!(calls, 1);
        assert!(clock.sleeps().is_empty());
    }

    #[tokio::test]
    async fn expired_or_cancelled_context_does_not_start_an_attempt() {
        let clock = AdvancingClock::new(Instant::now());
        let expired = context(&clock, Duration::ZERO);
        let mut jitter = FixedJitter(10_000);
        let mut calls = 0;
        let result: Result<(), RetryError<TestError>> = retry(
            &expired,
            &RetryPolicy::default(),
            &clock,
            &mut jitter,
            |_| {
                calls += 1;
                async { Ok(()) }
            },
        )
        .await;
        assert_eq!(result, Err(RetryError::Timeout));
        assert_eq!(calls, 0);

        let cancellation = Arc::new(CancellationFlag::default());
        cancellation.cancel();
        let cancelled = CommandContext {
            deadline: Deadline::after(&clock, Duration::from_secs(10)),
            cancellation,
            diagnostics: Arc::new(NullDiagnostics),
        };
        let result: Result<(), RetryError<TestError>> = retry(
            &cancelled,
            &RetryPolicy::default(),
            &clock,
            &mut jitter,
            |_| {
                calls += 1;
                async { Ok(()) }
            },
        )
        .await;
        assert_eq!(result, Err(RetryError::Cancelled));
        assert_eq!(calls, 0);
    }
}
