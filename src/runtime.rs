//! Runtime configuration, deadlines, cancellation, and injectable time sources.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    future::Future,
    num::NonZeroUsize,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{
    error::{CliError, ErrorKind, ExitCode},
    output::DiagnosticSink,
};

/// A sendable future returned by object-safe asynchronous boundaries.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Runtime policy derived from the supported `MCP_*` environment variables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub timeout: Duration,
    pub concurrency: NonZeroUsize,
    pub max_retries: u32,
    pub retry_base_delay: Duration,
    pub strict_env: bool,
    pub daemon_enabled: bool,
    pub daemon_idle_timeout: Duration,
    pub debug: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(1_800),
            concurrency: NonZeroUsize::new(5).expect("the default concurrency is non-zero"),
            max_retries: 3,
            retry_base_delay: Duration::from_millis(1_000),
            strict_env: true,
            daemon_enabled: cfg!(unix),
            daemon_idle_timeout: Duration::from_secs(60),
            debug: false,
        }
    }
}

impl RuntimeConfig {
    /// Parses a deterministic environment snapshot. Numeric values must be
    /// unsigned base-10 integers with no signs, whitespace, or suffixes.
    pub fn parse<I, K, V>(environment: I) -> Result<Self, CliError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let environment = environment
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect::<BTreeMap<_, _>>();
        Self::parse_map(&environment)
    }

    /// Reads only the supported variables from the current process. Keeping
    /// parsing in `parse` makes all policy independently testable without
    /// mutating process-global environment state.
    pub fn from_current_env() -> Result<Self, CliError> {
        const KEYS: [&str; 8] = [
            "MCP_TIMEOUT",
            "MCP_CONCURRENCY",
            "MCP_MAX_RETRIES",
            "MCP_RETRY_DELAY",
            "MCP_STRICT_ENV",
            "MCP_NO_DAEMON",
            "MCP_DAEMON_TIMEOUT",
            "MCP_DEBUG",
        ];

        let mut environment = BTreeMap::new();
        for key in KEYS {
            if let Some(value) = std::env::var_os(key) {
                let value = os_value_to_string(key, value)?;
                environment.insert(key.to_owned(), value);
            }
        }
        Self::parse_map(&environment)
    }

    /// Creates the one absolute command deadline for this runtime policy.
    pub fn deadline(&self, clock: &dyn Clock) -> Deadline {
        Deadline::after(clock, self.timeout)
    }

    fn parse_map(environment: &BTreeMap<String, String>) -> Result<Self, CliError> {
        let defaults = Self::default();
        let timeout = parse_positive_u64(environment, "MCP_TIMEOUT")?
            .map(Duration::from_secs)
            .unwrap_or(defaults.timeout);
        let concurrency = parse_positive_usize(environment, "MCP_CONCURRENCY")?
            .map(|value| NonZeroUsize::new(value).expect("validated as positive"))
            .unwrap_or(defaults.concurrency);
        let max_retries =
            parse_non_negative_u32(environment, "MCP_MAX_RETRIES")?.unwrap_or(defaults.max_retries);
        let retry_base_delay = parse_positive_u64(environment, "MCP_RETRY_DELAY")?
            .map(Duration::from_millis)
            .unwrap_or(defaults.retry_base_delay);
        let daemon_idle_timeout = parse_positive_u64(environment, "MCP_DAEMON_TIMEOUT")?
            .map(Duration::from_secs)
            .unwrap_or(defaults.daemon_idle_timeout);

        let strict_env = environment
            .get("MCP_STRICT_ENV")
            .is_none_or(|value| !value.eq_ignore_ascii_case("false") && value != "0");
        let daemon_disabled = environment
            .get("MCP_NO_DAEMON")
            .is_some_and(|value| value == "1");
        let debug = environment
            .get("MCP_DEBUG")
            .is_some_and(|value| !value.is_empty());

        Ok(Self {
            timeout,
            concurrency,
            max_retries,
            retry_base_delay,
            strict_env,
            daemon_enabled: cfg!(unix) && !daemon_disabled,
            daemon_idle_timeout,
            debug,
        })
    }
}

/// Absolute command deadline. Layers can cap individual waits, but cannot
/// extend this deadline or create a fresh total budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadline {
    expires_at: Instant,
}

impl Deadline {
    pub const fn new(expires_at: Instant) -> Self {
        Self { expires_at }
    }

    pub fn after(clock: &dyn Clock, timeout: Duration) -> Self {
        Self::new(saturating_instant_add(clock.now(), timeout))
    }

    pub const fn expires_at(self) -> Instant {
        self.expires_at
    }

    pub fn remaining(self, clock: &dyn Clock) -> Duration {
        self.expires_at
            .checked_duration_since(clock.now())
            .unwrap_or_default()
    }

    pub fn is_expired(self, clock: &dyn Clock) -> bool {
        clock.now() >= self.expires_at
    }

    /// Returns the remaining total budget capped by an operation-local limit.
    pub fn remaining_capped(self, clock: &dyn Clock, local_cap: Duration) -> Duration {
        self.remaining(clock).min(local_cap)
    }

    /// Computes an operation-local absolute deadline without extending the
    /// command deadline, using saturating `Instant` arithmetic.
    pub fn local_deadline(self, clock: &dyn Clock, local_cap: Duration) -> Instant {
        self.expires_at
            .min(saturating_instant_add(clock.now(), local_cap))
    }
}

/// Injectable cancellation observation boundary.
pub trait CancellationToken: Send + Sync {
    fn is_cancelled(&self) -> bool;
}

/// Thread-safe cancellation token used by production signal coordination and
/// directly controllable by deterministic tests.
#[derive(Debug, Default)]
pub struct CancellationFlag {
    cancelled: AtomicBool,
}

impl CancellationFlag {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

impl CancellationToken for CancellationFlag {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// Context shared by every operation belonging to one command.
#[derive(Clone)]
pub struct CommandContext {
    pub deadline: Deadline,
    pub cancellation: Arc<dyn CancellationToken>,
    pub diagnostics: Arc<dyn DiagnosticSink>,
}

impl CommandContext {
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub fn remaining(&self, clock: &dyn Clock) -> Duration {
        self.deadline.remaining(clock)
    }

    pub fn remaining_capped(&self, clock: &dyn Clock, local_cap: Duration) -> Duration {
        self.deadline.remaining_capped(clock, local_cap)
    }

    /// Runs command-owned resource cleanup under a fresh, bounded grace period.
    ///
    /// Cleanup deliberately does not observe the command cancellation token or
    /// the exhausted command deadline: cancellation and timeout are reasons to
    /// start cleanup, not reasons to skip it. The original context is still
    /// passed to the cleanup operation so diagnostics and command identity stay
    /// consistent. Callers decide whether a cleanup failure is returned (an
    /// explicit close) or reduced to a safe diagnostic (a preceding primary
    /// operation already failed).
    pub async fn run_bounded_cleanup<T, F>(
        &self,
        clock: &dyn Clock,
        grace: Duration,
        cleanup: F,
    ) -> Result<T, CleanupTimeout>
    where
        F: Future<Output = T>,
    {
        let cleanup_deadline = saturating_instant_add(clock.now(), grace);
        tokio::select! {
            biased;
            result = cleanup => Ok(result),
            _ = clock.sleep_until(cleanup_deadline) => Err(CleanupTimeout),
        }
    }
}

/// Indicates that a best-effort cleanup exceeded its independent grace period.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CleanupTimeout;

impl std::fmt::Display for CleanupTimeout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("resource cleanup timed out")
    }
}

impl std::error::Error for CleanupTimeout {}

/// Injectable monotonic clock used by deadline and retry policy.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
    fn sleep_until(&self, deadline: Instant) -> BoxFuture<'_, ()>;
}

/// Tokio-backed production monotonic clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep_until(&self, deadline: Instant) -> BoxFuture<'_, ()> {
        Box::pin(async move { tokio::time::sleep_until(deadline.into()).await })
    }
}

/// Injectable integer jitter source. Implementations must produce values in
/// the inclusive 7500..=12500 basis-point range.
pub trait JitterSource: Send + Sync {
    fn factor_basis_points(&mut self) -> u16;
}

fn parse_positive_u64(
    environment: &BTreeMap<String, String>,
    name: &'static str,
) -> Result<Option<u64>, CliError> {
    environment
        .get(name)
        .map(|value| {
            parse_decimal::<u64>(name, value, "a positive decimal integer")
                .and_then(|parsed| require_positive(name, parsed))
        })
        .transpose()
}

fn parse_positive_usize(
    environment: &BTreeMap<String, String>,
    name: &'static str,
) -> Result<Option<usize>, CliError> {
    environment
        .get(name)
        .map(|value| {
            parse_decimal::<usize>(name, value, "a positive decimal integer")
                .and_then(|parsed| require_positive(name, parsed))
        })
        .transpose()
}

fn parse_non_negative_u32(
    environment: &BTreeMap<String, String>,
    name: &'static str,
) -> Result<Option<u32>, CliError> {
    environment
        .get(name)
        .map(|value| parse_decimal::<u32>(name, value, "a non-negative decimal integer"))
        .transpose()
}

fn parse_decimal<T>(name: &'static str, value: &str, expected: &str) -> Result<T, CliError>
where
    T: std::str::FromStr,
{
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_runtime_config(name, expected));
    }
    value
        .parse()
        .map_err(|_| invalid_runtime_config(name, expected))
}

fn require_positive<T>(name: &'static str, value: T) -> Result<T, CliError>
where
    T: Default + PartialEq,
{
    if value == T::default() {
        Err(invalid_runtime_config(name, "a positive decimal integer"))
    } else {
        Ok(value)
    }
}

fn invalid_runtime_config(name: &'static str, expected: &str) -> CliError {
    let mut error = CliError::new(
        ErrorKind::InvalidRuntimeConfig,
        "Invalid runtime configuration",
        ExitCode::Client,
    );
    error.details = Some(format!("{name} must be {expected}"));
    error
}

fn os_value_to_string(name: &'static str, value: OsString) -> Result<String, CliError> {
    value.into_string().map_err(|_| {
        invalid_runtime_config(name, "valid UTF-8 containing the documented value type")
    })
}

fn saturating_instant_add(start: Instant, duration: Duration) -> Instant {
    if let Some(result) = start.checked_add(duration) {
        return result;
    }

    let mut low = 0_u128;
    let mut high = duration.as_nanos();
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if start.checked_add(duration_from_nanos(middle)).is_some() {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    start
        .checked_add(duration_from_nanos(low))
        .expect("binary search only retains representable instants")
}

fn duration_from_nanos(nanos: u128) -> Duration {
    const NANOS_PER_SECOND: u128 = 1_000_000_000;
    Duration::new(
        (nanos / NANOS_PER_SECOND) as u64,
        (nanos % NANOS_PER_SECOND) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Debug)]
    struct FixedClock(Mutex<Instant>);

    impl FixedClock {
        fn new(now: Instant) -> Self {
            Self(Mutex::new(now))
        }

        fn set(&self, now: Instant) {
            *self.0.lock().expect("clock mutex") = now;
        }
    }

    impl Clock for FixedClock {
        fn now(&self) -> Instant {
            *self.0.lock().expect("clock mutex")
        }

        fn sleep_until(&self, _deadline: Instant) -> BoxFuture<'_, ()> {
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

    #[test]
    fn defaults_match_the_runtime_contract() {
        let config = RuntimeConfig::parse(Vec::<(String, String)>::new()).expect("defaults");

        assert_eq!(config.timeout, Duration::from_secs(1_800));
        assert_eq!(config.concurrency.get(), 5);
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.retry_base_delay, Duration::from_millis(1_000));
        assert!(config.strict_env);
        assert_eq!(config.daemon_enabled, cfg!(unix));
        assert_eq!(config.daemon_idle_timeout, Duration::from_secs(60));
        assert!(!config.debug);
    }

    #[test]
    fn parses_all_supported_overrides_with_documented_units() {
        let config = RuntimeConfig::parse([
            ("MCP_TIMEOUT", "42"),
            ("MCP_CONCURRENCY", "7"),
            ("MCP_MAX_RETRIES", "0"),
            ("MCP_RETRY_DELAY", "125"),
            ("MCP_STRICT_ENV", "FALSE"),
            ("MCP_NO_DAEMON", "1"),
            ("MCP_DAEMON_TIMEOUT", "9"),
            ("MCP_DEBUG", "1"),
        ])
        .expect("valid overrides");

        assert_eq!(config.timeout, Duration::from_secs(42));
        assert_eq!(config.concurrency.get(), 7);
        assert_eq!(config.max_retries, 0);
        assert_eq!(config.retry_base_delay, Duration::from_millis(125));
        assert!(!config.strict_env);
        assert!(!config.daemon_enabled);
        assert_eq!(config.daemon_idle_timeout, Duration::from_secs(9));
        assert!(config.debug);
    }

    #[test]
    fn rejects_invalid_numeric_values_without_falling_back() {
        for (name, value) in [
            ("MCP_TIMEOUT", "0"),
            ("MCP_CONCURRENCY", "-1"),
            ("MCP_MAX_RETRIES", "1x"),
            ("MCP_RETRY_DELAY", " 1"),
            ("MCP_DAEMON_TIMEOUT", "18446744073709551616"),
        ] {
            let error = RuntimeConfig::parse([(name, value)]).expect_err("must reject value");
            assert_eq!(error.kind, ErrorKind::InvalidRuntimeConfig);
            assert_eq!(error.exit_code, ExitCode::Client);
            assert!(
                error
                    .details
                    .as_deref()
                    .is_some_and(|details| details.contains(name)),
                "details should identify {name}"
            );
        }
    }

    #[test]
    fn boolean_flags_follow_their_asymmetric_contracts() {
        let config = RuntimeConfig::parse([
            ("MCP_STRICT_ENV", "anything"),
            ("MCP_NO_DAEMON", "true"),
            ("MCP_DEBUG", ""),
        ])
        .expect("flags are not numeric settings");

        assert!(config.strict_env);
        assert_eq!(config.daemon_enabled, cfg!(unix));
        assert!(!config.debug);
    }

    #[test]
    fn deadline_uses_one_absolute_budget_and_caps_local_waits() {
        let start = Instant::now();
        let clock = FixedClock::new(start);
        let deadline = Deadline::after(&clock, Duration::from_secs(10));

        assert_eq!(deadline.expires_at(), start + Duration::from_secs(10));
        assert_eq!(deadline.remaining(&clock), Duration::from_secs(10));
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
            deadline.local_deadline(&clock, Duration::from_secs(5)),
            deadline.expires_at()
        );
        assert!(!deadline.is_expired(&clock));

        clock.set(start + Duration::from_secs(10));
        assert!(deadline.is_expired(&clock));
        assert_eq!(deadline.remaining(&clock), Duration::ZERO);
    }

    #[test]
    fn deadline_construction_saturates_instead_of_panicking() {
        let start = Instant::now();
        let clock = FixedClock::new(start);
        let deadline = Deadline::after(&clock, Duration::MAX);

        assert!(deadline.expires_at() >= start);
    }

    #[test]
    fn command_context_observes_injected_cancellation_and_clock() {
        let start = Instant::now();
        let clock = FixedClock::new(start);
        let cancellation = Arc::new(CancellationFlag::default());
        let context = CommandContext {
            deadline: Deadline::after(&clock, Duration::from_secs(5)),
            cancellation: cancellation.clone(),
            diagnostics: Arc::new(NullDiagnostics),
        };

        assert!(!context.is_cancelled());
        assert_eq!(context.remaining(&clock), Duration::from_secs(5));
        assert_eq!(
            context.remaining_capped(&clock, Duration::from_secs(2)),
            Duration::from_secs(2)
        );

        cancellation.cancel();
        assert!(context.is_cancelled());
    }

    #[tokio::test]
    async fn bounded_cleanup_ignores_cancelled_and_expired_command_state() {
        let start = Instant::now();
        let clock = FixedClock::new(start);
        let cancellation = Arc::new(CancellationFlag::default());
        cancellation.cancel();
        let context = CommandContext {
            deadline: Deadline::new(start),
            cancellation,
            diagnostics: Arc::new(NullDiagnostics),
        };
        let cleanup_ran = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&cleanup_ran);

        let result = context
            .run_bounded_cleanup(&clock, Duration::from_secs(1), async move {
                observed.store(true, Ordering::SeqCst);
                7
            })
            .await;

        assert_eq!(result, Ok(7));
        assert!(cleanup_ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn bounded_cleanup_reports_its_independent_timeout() {
        let start = Instant::now();
        let clock = FixedClock::new(start);
        let context = CommandContext {
            deadline: Deadline::new(start),
            cancellation: Arc::new(CancellationFlag::default()),
            diagnostics: Arc::new(NullDiagnostics),
        };

        let result = context
            .run_bounded_cleanup(&clock, Duration::from_secs(1), std::future::pending::<()>())
            .await;

        assert_eq!(result, Err(CleanupTimeout));
    }
}
