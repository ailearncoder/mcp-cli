#[path = "../support/mod.rs"]
mod support;

use std::{
    collections::VecDeque,
    error::Error,
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use mcp_cli::{
    Attempt, ClassifyError, CommandContext, Deadline, ErrorClass, RetryError, RetryPolicy, retry,
};
use proptest::prelude::*;
use support::{FakeClock, FixedJitter, RecordingDiagnosticSink, TestCancellationToken};

const SUCCESS_SENTINEL: u16 = u16::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScriptedClass {
    Transient,
    NonTransient,
    Auth,
    Business,
}

impl ScriptedClass {
    const TERMINAL: [Self; 3] = [Self::NonTransient, Self::Auth, Self::Business];

    const fn error_class(self) -> ErrorClass {
        match self {
            Self::Transient => ErrorClass::Transient,
            Self::NonTransient => ErrorClass::NonTransient,
            Self::Auth => ErrorClass::Auth,
            Self::Business => ErrorClass::Business,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ScriptedResult {
    Success(u16),
    Failure(ScriptedClass),
}

impl ScriptedResult {
    fn into_operation_result(self) -> Result<u16, TestError> {
        match self {
            Self::Success(value) => Ok(value),
            Self::Failure(class) => Err(TestError(class)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TestError(ScriptedClass);

impl fmt::Display for TestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}", self.0)
    }
}

impl Error for TestError {}

impl ClassifyError for TestError {
    fn class(&self) -> ErrorClass {
        self.0.error_class()
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Observation {
    result: Result<u16, RetryError<TestError>>,
    attempts: Vec<Attempt>,
    fake_clock_unchanged: bool,
}

async fn execute(script: Vec<ScriptedResult>, retry_limit: u32) -> Observation {
    let start = Instant::now();
    let clock = FakeClock::new(start);
    let context = CommandContext {
        deadline: Deadline::after(&clock, Duration::from_secs(1)),
        cancellation: Arc::new(TestCancellationToken::default()),
        diagnostics: Arc::new(RecordingDiagnosticSink::default()),
    };
    let policy = RetryPolicy::new(retry_limit, Duration::ZERO);
    let mut jitter = FixedJitter::new(10_000);
    let mut scripted = VecDeque::from(script);
    let mut attempts = Vec::new();

    let result = retry(&context, &policy, &clock, &mut jitter, |attempt| {
        attempts.push(attempt);
        let result = scripted
            .pop_front()
            .expect("script must contain one result for every permitted attempt")
            .into_operation_result();
        async move { result }
    })
    .await;

    Observation {
        result,
        attempts,
        fake_clock_unchanged: clock.current() == start,
    }
}

fn reference_result(
    script: &[ScriptedResult],
    retry_limit: u32,
) -> (Result<u16, RetryError<TestError>>, usize) {
    for (attempt_index, outcome) in script.iter().enumerate() {
        let call_count = attempt_index + 1;
        match outcome {
            ScriptedResult::Success(value) => return (Ok(*value), call_count),
            ScriptedResult::Failure(class) => {
                if *class != ScriptedClass::Transient || attempt_index as u32 >= retry_limit {
                    return (Err(RetryError::Operation(TestError(*class))), call_count);
                }
            }
        }
    }

    panic!("generated script must terminate with success or an exhausted retry limit");
}

fn expected_attempts(call_count: usize) -> Vec<Attempt> {
    (0..call_count)
        .map(|index| Attempt {
            index: index as u32,
            retry_index: (index > 0).then(|| index as u32 - 1),
        })
        .collect()
}

fn scripted_result() -> impl Strategy<Value = ScriptedResult> {
    prop_oneof![
        any::<u16>().prop_map(ScriptedResult::Success),
        Just(ScriptedResult::Failure(ScriptedClass::Transient)),
        Just(ScriptedResult::Failure(ScriptedClass::NonTransient)),
        Just(ScriptedResult::Failure(ScriptedClass::Auth)),
        Just(ScriptedResult::Failure(ScriptedClass::Business)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 18: 重试分类与次数
    // **Validates: Requirements 8.1, 8.2, 8.7**
    #[test]
    fn property_18_retry_classification_and_count(
        generated_prefix in prop::collection::vec(scripted_result(), 0..20),
        transient_failures in 1_usize..20,
        retry_limit in 0_u32..8,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("property runtime");

        let mut generated_script = generated_prefix.clone();
        generated_script.push(ScriptedResult::Success(SUCCESS_SENTINEL));
        let (expected_result, expected_calls) =
            reference_result(&generated_script, retry_limit);
        let observed = runtime.block_on(execute(generated_script, retry_limit));

        prop_assert_eq!(observed.result, expected_result);
        prop_assert_eq!(observed.attempts, expected_attempts(expected_calls));
        prop_assert!(observed.fake_clock_unchanged);
        prop_assert!(expected_calls <= 1 + retry_limit as usize);

        for class in ScriptedClass::TERMINAL {
            let mut script = vec![ScriptedResult::Failure(class)];
            script.extend(generated_prefix.iter().cloned());
            script.push(ScriptedResult::Success(SUCCESS_SENTINEL));
            let observed = runtime.block_on(execute(script, retry_limit));

            prop_assert_eq!(
                observed.result,
                Err(RetryError::Operation(TestError(class)))
            );
            prop_assert_eq!(observed.attempts, expected_attempts(1));
            prop_assert!(observed.fake_clock_unchanged);
        }

        let mut transient_script =
            vec![ScriptedResult::Failure(ScriptedClass::Transient); transient_failures];
        transient_script.push(ScriptedResult::Success(SUCCESS_SENTINEL));
        let observed = runtime.block_on(execute(transient_script, retry_limit));
        let expected_transient_calls =
            (transient_failures + 1).min(1 + retry_limit as usize);
        let expected_transient_result = if retry_limit as usize >= transient_failures {
            Ok(SUCCESS_SENTINEL)
        } else {
            Err(RetryError::Operation(TestError(ScriptedClass::Transient)))
        };

        prop_assert_eq!(observed.result, expected_transient_result);
        prop_assert_eq!(
            observed.attempts,
            expected_attempts(expected_transient_calls)
        );
        prop_assert!(observed.fake_clock_unchanged);
        prop_assert!(expected_transient_calls <= 1 + retry_limit as usize);

        for class in [ScriptedClass::Transient]
            .into_iter()
            .chain(ScriptedClass::TERMINAL)
        {
            let script = vec![
                ScriptedResult::Failure(class),
                ScriptedResult::Success(SUCCESS_SENTINEL),
            ];
            let observed = runtime.block_on(execute(script, 0));

            prop_assert_eq!(
                observed.result,
                Err(RetryError::Operation(TestError(class)))
            );
            prop_assert_eq!(observed.attempts, expected_attempts(1));
            prop_assert!(observed.fake_clock_unchanged);
        }
    }
}
