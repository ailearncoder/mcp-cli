#[path = "../support/mod.rs"]
mod support;

use std::{
    collections::VecDeque,
    error::Error,
    fmt,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use mcp_cli::{
    Attempt, BoxFuture, ClassifyError, Clock, CommandContext, Deadline, DiagnosticSink, ErrorClass,
    RetryError, RetryPolicy, retry,
};
use proptest::prelude::*;
use support::{DiagnosticEvent, RecordingDiagnosticSink, SeededJitter, TestCancellationToken};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScriptedError {
    Transient,
    NonTransient,
    Auth,
    Business,
    Cancelled,
}

impl ScriptedError {
    const fn class(self) -> ErrorClass {
        match self {
            Self::Transient => ErrorClass::Transient,
            Self::NonTransient => ErrorClass::NonTransient,
            Self::Auth => ErrorClass::Auth,
            Self::Business => ErrorClass::Business,
            Self::Cancelled => ErrorClass::Cancelled,
        }
    }
}

impl fmt::Display for ScriptedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl Error for ScriptedError {}

impl ClassifyError for ScriptedError {
    fn class(&self) -> ErrorClass {
        (*self).class()
    }
}

#[derive(Clone, Copy, Debug)]
enum IdleEventKind {
    ValidRequest,
    InvalidRequest,
    Poll,
    CloseRequest,
}

#[derive(Clone, Copy, Debug)]
struct IdleEvent {
    advance_millis: u64,
    kind: IdleEventKind,
}

#[derive(Clone, Debug)]
struct Scenario {
    retry_errors: Vec<ScriptedError>,
    idle_events: Vec<IdleEvent>,
    initial_offset_millis: u64,
    jitter_seed: u64,
    retry_limit: u32,
    retry_base_millis: u64,
    retry_budget_millis: u64,
    idle_timeout_millis: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AttemptTrace {
    attempt: Attempt,
    at: Duration,
    remaining: Duration,
    scripted_error: Option<ScriptedError>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SleepTrace {
    from: Duration,
    delay: Duration,
    target: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeadlineScope {
    Retry,
    Idle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DeadlineTransition {
    scope: DeadlineScope,
    step: usize,
    at: Duration,
    expires_at: Duration,
    remaining: Duration,
    expired: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum IdleDecision {
    ValidRequestReset { deadline: Duration },
    InvalidRequestIgnored { deadline: Duration },
    RemainActive { remaining: Duration },
    ShutdownIdleTimeout,
    ShutdownCloseRequest,
    AlreadyShutdown,
}

#[derive(Debug, PartialEq, Eq)]
struct ExecutionTrace {
    initial_offset: Duration,
    retry_outcome: Result<(), RetryError<ScriptedError>>,
    attempts: Vec<AttemptTrace>,
    sleeps: Vec<SleepTrace>,
    deadline_transitions: Vec<DeadlineTransition>,
    idle_decisions: Vec<IdleDecision>,
    diagnostics: Vec<DiagnosticEvent>,
}

#[derive(Debug)]
struct TracingFakeClock {
    initial: Instant,
    state: Mutex<ClockState>,
    diagnostics: Arc<RecordingDiagnosticSink>,
}

#[derive(Debug)]
struct ClockState {
    now: Instant,
    sleeps: Vec<SleepTrace>,
}

impl TracingFakeClock {
    fn new(initial: Instant, diagnostics: Arc<RecordingDiagnosticSink>) -> Self {
        Self {
            initial,
            state: Mutex::new(ClockState {
                now: initial,
                sleeps: Vec::new(),
            }),
            diagnostics,
        }
    }

    fn elapsed(&self) -> Duration {
        self.now()
            .checked_duration_since(self.initial)
            .expect("the fake clock never moves before its initial instant")
    }

    fn advance(&self, duration: Duration) {
        let elapsed = {
            let mut state = self.state.lock().expect("fake clock lock poisoned");
            state.now = state
                .now
                .checked_add(duration)
                .expect("generated fake-clock advance is bounded");
            state
                .now
                .checked_duration_since(self.initial)
                .expect("the fake clock only advances")
        };
        self.diagnostics.debug(&format!(
            "scenario clock advance_ns={} now_ns={}",
            duration.as_nanos(),
            elapsed.as_nanos()
        ));
    }

    fn sleeps(&self) -> Vec<SleepTrace> {
        self.state
            .lock()
            .expect("fake clock lock poisoned")
            .sleeps
            .clone()
    }
}

impl Clock for TracingFakeClock {
    fn now(&self) -> Instant {
        self.state.lock().expect("fake clock lock poisoned").now
    }

    fn sleep_until(&self, deadline: Instant) -> BoxFuture<'_, ()> {
        let sleep = {
            let mut state = self.state.lock().expect("fake clock lock poisoned");
            let from = state
                .now
                .checked_duration_since(self.initial)
                .expect("the fake clock only advances");
            let delay = deadline
                .checked_duration_since(state.now)
                .expect("retry sleeps never target the fake clock's past");
            state.now = deadline;
            let sleep = SleepTrace {
                from,
                delay,
                target: deadline
                    .checked_duration_since(self.initial)
                    .expect("retry sleep target follows the initial instant"),
            };
            state.sleeps.push(sleep.clone());
            sleep
        };
        self.diagnostics.debug(&format!(
            "scenario retry sleep_ns={} target_ns={}",
            sleep.delay.as_nanos(),
            sleep.target.as_nanos()
        ));
        Box::pin(async {})
    }
}

fn deadline_transition(
    scope: DeadlineScope,
    step: usize,
    deadline: Deadline,
    clock: &TracingFakeClock,
) -> DeadlineTransition {
    DeadlineTransition {
        scope,
        step,
        at: clock.elapsed(),
        expires_at: deadline
            .expires_at()
            .checked_duration_since(clock.initial)
            .expect("generated deadlines follow the initial instant"),
        remaining: deadline.remaining(clock),
        expired: deadline.is_expired(clock),
    }
}

async fn run_scenario(scenario: &Scenario, initial: Instant) -> ExecutionTrace {
    let diagnostics = Arc::new(RecordingDiagnosticSink::default());
    let clock = TracingFakeClock::new(initial, diagnostics.clone());
    let retry_deadline =
        Deadline::after(&clock, Duration::from_millis(scenario.retry_budget_millis));
    let context = CommandContext {
        deadline: retry_deadline,
        cancellation: Arc::new(TestCancellationToken::default()),
        diagnostics: diagnostics.clone(),
    };
    let policy = RetryPolicy::new(
        scenario.retry_limit,
        Duration::from_millis(scenario.retry_base_millis),
    );
    let mut jitter = SeededJitter::new(scenario.jitter_seed);
    let mut scripted_errors = VecDeque::from(scenario.retry_errors.clone());
    let mut attempts = Vec::new();
    let mut deadline_transitions = vec![deadline_transition(
        DeadlineScope::Retry,
        0,
        retry_deadline,
        &clock,
    )];

    let retry_outcome = retry(&context, &policy, &clock, &mut jitter, |attempt| {
        let scripted_error = scripted_errors.pop_front();
        attempts.push(AttemptTrace {
            attempt,
            at: clock.elapsed(),
            remaining: retry_deadline.remaining(&clock),
            scripted_error,
        });
        deadline_transitions.push(deadline_transition(
            DeadlineScope::Retry,
            attempt.index as usize + 1,
            retry_deadline,
            &clock,
        ));
        diagnostics.debug(&format!(
            "scenario retry attempt={} class={:?}",
            attempt.index,
            scripted_error.map(ScriptedError::class)
        ));
        async move {
            match scripted_error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    })
    .await;

    deadline_transitions.push(deadline_transition(
        DeadlineScope::Retry,
        attempts.len() + 1,
        retry_deadline,
        &clock,
    ));

    // This is intentionally a minimal, pure idle lifecycle model. It reuses
    // the production Clock/Deadline semantics but does not cover worker I/O.
    let idle_timeout = Duration::from_millis(scenario.idle_timeout_millis);
    let mut idle_deadline = Deadline::after(&clock, idle_timeout);
    let mut shutdown = false;
    let mut idle_decisions = Vec::with_capacity(scenario.idle_events.len());
    deadline_transitions.push(deadline_transition(
        DeadlineScope::Idle,
        0,
        idle_deadline,
        &clock,
    ));

    for (step, event) in scenario.idle_events.iter().enumerate() {
        clock.advance(Duration::from_millis(event.advance_millis));

        let decision = if shutdown {
            IdleDecision::AlreadyShutdown
        } else if idle_deadline.is_expired(&clock) {
            shutdown = true;
            IdleDecision::ShutdownIdleTimeout
        } else {
            match event.kind {
                IdleEventKind::ValidRequest => {
                    idle_deadline = Deadline::after(&clock, idle_timeout);
                    IdleDecision::ValidRequestReset {
                        deadline: idle_deadline
                            .expires_at()
                            .checked_duration_since(initial)
                            .expect("idle deadline follows the initial instant"),
                    }
                }
                IdleEventKind::InvalidRequest => IdleDecision::InvalidRequestIgnored {
                    deadline: idle_deadline
                        .expires_at()
                        .checked_duration_since(initial)
                        .expect("idle deadline follows the initial instant"),
                },
                IdleEventKind::Poll => IdleDecision::RemainActive {
                    remaining: idle_deadline.remaining(&clock),
                },
                IdleEventKind::CloseRequest => {
                    shutdown = true;
                    IdleDecision::ShutdownCloseRequest
                }
            }
        };

        diagnostics.debug(&format!(
            "scenario idle step={step} event={:?} decision={decision:?}",
            event.kind
        ));
        idle_decisions.push(decision);
        deadline_transitions.push(deadline_transition(
            DeadlineScope::Idle,
            step + 1,
            idle_deadline,
            &clock,
        ));
    }

    ExecutionTrace {
        initial_offset: Duration::from_millis(scenario.initial_offset_millis),
        retry_outcome,
        attempts,
        sleeps: clock.sleeps(),
        deadline_transitions,
        idle_decisions,
        diagnostics: diagnostics.events(),
    }
}

fn scripted_error() -> impl Strategy<Value = ScriptedError> {
    prop_oneof![
        Just(ScriptedError::Transient),
        Just(ScriptedError::NonTransient),
        Just(ScriptedError::Auth),
        Just(ScriptedError::Business),
        Just(ScriptedError::Cancelled),
    ]
}

fn idle_event() -> impl Strategy<Value = IdleEvent> {
    (
        0_u64..=4_000,
        prop_oneof![
            Just(IdleEventKind::ValidRequest),
            Just(IdleEventKind::InvalidRequest),
            Just(IdleEventKind::Poll),
            Just(IdleEventKind::CloseRequest),
        ],
    )
        .prop_map(|(advance_millis, kind)| IdleEvent {
            advance_millis,
            kind,
        })
}

fn scenario() -> impl Strategy<Value = Scenario> {
    (
        prop::collection::vec(scripted_error(), 0..16),
        prop::collection::vec(idle_event(), 0..24),
        0_u64..=60_000,
        any::<u64>(),
        0_u32..8,
        1_u64..=1_000,
        1_u64..=30_000,
        1_u64..=10_000,
    )
        .prop_map(
            |(
                retry_errors,
                idle_events,
                initial_offset_millis,
                jitter_seed,
                retry_limit,
                retry_base_millis,
                retry_budget_millis,
                idle_timeout_millis,
            )| Scenario {
                retry_errors,
                idle_events,
                initial_offset_millis,
                jitter_seed,
                retry_limit,
                retry_base_millis,
                retry_budget_millis,
                idle_timeout_millis,
            },
        )
}

fn opaque_test_epoch() -> Instant {
    // `Instant` has no constructible constant epoch. This single anchor is an
    // opaque token only: no elapsed wall/monotonic time or real sleep affects
    // the scenario, and both fresh executions receive the exact same instant.
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 37: 固定时钟和随机源产生可重复 trace
    // **Validates: Requirements 17.2**
    #[test]
    fn property_37_fixed_clock_and_seeded_jitter_repeat_trace(input in scenario()) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("property runtime");
        let initial = opaque_test_epoch()
            .checked_add(Duration::from_millis(input.initial_offset_millis))
            .expect("generated initial fake-clock offset is bounded");

        let first = runtime.block_on(run_scenario(&input, initial));
        let second = runtime.block_on(run_scenario(&input, initial));

        prop_assert_eq!(&first.attempts, &second.attempts);
        prop_assert_eq!(&first.sleeps, &second.sleeps);
        prop_assert_eq!(
            &first.deadline_transitions,
            &second.deadline_transitions
        );
        prop_assert_eq!(&first.idle_decisions, &second.idle_decisions);
        prop_assert_eq!(&first.diagnostics, &second.diagnostics);
        prop_assert_eq!(first, second);
    }
}
