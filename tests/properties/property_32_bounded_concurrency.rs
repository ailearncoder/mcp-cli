#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use mcp_cli::{
    BoxFuture, CancellationFlag, CliError, CommandContext, ConfigHash, Deadline, ErrorKind,
    ExitCode, PerServer, ServerDefinition, ServerId, ToolFilterConfig, TransportConfig,
    commands::execute_bounded_server_batch, output::DiagnosticSink,
};
use proptest::prelude::*;

#[derive(Clone, Debug)]
struct TaskSpec {
    schedule_steps: u8,
    should_fail: bool,
    value: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TaskValue {
    task_index: usize,
    value: i64,
}

#[derive(Debug, Default)]
struct NullDiagnostics;

impl DiagnosticSink for NullDiagnostics {
    fn warning(&self, _message: &str) {}
    fn debug(&self, _message: &str) {}
    fn server_stderr(&self, _server: &str, _bytes: &[u8]) {}
}

struct ActiveTaskGuard {
    active: Arc<AtomicUsize>,
}

impl ActiveTaskGuard {
    fn enter(active: Arc<AtomicUsize>, peak: &AtomicUsize) -> Self {
        let current = active.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(current, Ordering::SeqCst);
        Self { active }
    }
}

impl Drop for ActiveTaskGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

fn test_epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn server_name(index: usize) -> String {
    format!("server-{index:04}")
}

fn servers(task_count: usize) -> BTreeMap<String, ServerDefinition> {
    (0..task_count)
        .map(|index| {
            let name = server_name(index);
            (
                name.clone(),
                ServerDefinition {
                    name,
                    id: ServerId(format!("{index:064x}")),
                    config_hash: ConfigHash([index as u8; 32]),
                    transport: TransportConfig::Stdio {
                        command: "unused-property-fixture".to_owned(),
                        args: Vec::new(),
                        env: BTreeMap::new(),
                        cwd: None,
                    },
                    filter: ToolFilterConfig::default(),
                },
            )
        })
        .collect()
}

fn scenario() -> impl Strategy<Value = (Vec<TaskSpec>, usize)> {
    prop::collection::vec(
        (0_u8..=12, any::<bool>(), any::<i64>()).prop_map(
            |(schedule_steps, should_fail, value)| TaskSpec {
                schedule_steps,
                should_fail,
                value,
            },
        ),
        1..=20,
    )
    .prop_flat_map(|tasks| {
        let largest_useful_limit = tasks.len() + 5;
        (Just(tasks), 1_usize..=largest_useful_limit)
    })
}

async fn run_scenario(tasks: Vec<TaskSpec>, limit: usize) -> Result<(), TestCaseError> {
    let task_count = tasks.len();
    let servers = servers(task_count);
    let deadline = Deadline::new(test_epoch() + Duration::from_secs(3_600));
    let context = CommandContext {
        deadline,
        cancellation: Arc::new(CancellationFlag::default()),
        diagnostics: Arc::new(NullDiagnostics),
    };
    let expected_context_address = (&context as *const CommandContext) as usize;

    let starts = Arc::new(
        (0..task_count)
            .map(|_| AtomicUsize::new(0))
            .collect::<Vec<_>>(),
    );
    let finishes = Arc::new(
        (0..task_count)
            .map(|_| AtomicUsize::new(0))
            .collect::<Vec<_>>(),
    );
    let successful_completions = Arc::new(
        (0..task_count)
            .map(|_| AtomicUsize::new(0))
            .collect::<Vec<_>>(),
    );
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let context_observations = Arc::new(Mutex::new(Vec::with_capacity(task_count)));
    let task_specs = Arc::new(
        tasks
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, spec)| (server_name(index), (index, spec)))
            .collect::<BTreeMap<_, _>>(),
    );

    let starts_for_batch = Arc::clone(&starts);
    let finishes_for_batch = Arc::clone(&finishes);
    let successful_completions_for_batch = Arc::clone(&successful_completions);
    let active_for_batch = Arc::clone(&active);
    let peak_for_batch = Arc::clone(&peak);
    let observations_for_batch = Arc::clone(&context_observations);
    let specs_for_batch = Arc::clone(&task_specs);

    let results = execute_bounded_server_batch(
        &context,
        &servers,
        NonZeroUsize::new(limit).expect("generated concurrency limit is positive"),
        move |task_context, server| {
            let (task_index, spec) = specs_for_batch
                .get(&server.name)
                .expect("every generated server has an independent task specification")
                .clone();
            let starts = Arc::clone(&starts_for_batch);
            let finishes = Arc::clone(&finishes_for_batch);
            let successful_completions = Arc::clone(&successful_completions_for_batch);
            let active = Arc::clone(&active_for_batch);
            let peak = Arc::clone(&peak_for_batch);
            let observations = Arc::clone(&observations_for_batch);
            let context_address = (task_context as *const CommandContext) as usize;
            let observed_deadline = task_context.deadline;

            Box::pin(async move {
                starts[task_index].fetch_add(1, Ordering::SeqCst);
                observations.lock().expect("observation mutex").push((
                    task_index,
                    context_address,
                    observed_deadline,
                ));
                let _active_guard = ActiveTaskGuard::enter(active, peak.as_ref());

                // Generated cooperative yields model controllable scheduling
                // delay without network access, timers, or wall-clock sleeps.
                for _ in 0..spec.schedule_steps {
                    tokio::task::yield_now().await;
                }

                finishes[task_index].fetch_add(1, Ordering::SeqCst);
                if spec.should_fail {
                    Err(CliError::new(
                        ErrorKind::NetworkError,
                        format!("isolated failure for task {task_index}"),
                        ExitCode::Network,
                    ))
                } else {
                    successful_completions[task_index].fetch_add(1, Ordering::SeqCst);
                    Ok(TaskValue {
                        task_index,
                        value: spec.value,
                    })
                }
            }) as BoxFuture<'_, Result<TaskValue, CliError>>
        },
    )
    .await;

    // This oracle is derived only from generated task specifications. It does
    // not call the production executor or reuse its sorting implementation.
    let expected_names = (0..task_count).map(server_name).collect::<Vec<_>>();
    prop_assert_eq!(results.len(), task_count);
    prop_assert_eq!(
        results.iter().map(PerServer::server).collect::<Vec<_>>(),
        expected_names
    );
    prop_assert_eq!(active.load(Ordering::SeqCst), 0);
    let observed_peak = peak.load(Ordering::SeqCst);
    prop_assert!(observed_peak > 0);
    prop_assert!(
        observed_peak <= limit.min(task_count),
        "observed peak {observed_peak} exceeded min(limit={limit}, task_count={task_count})"
    );

    for (task_index, (spec, result)) in tasks.iter().zip(results.iter()).enumerate() {
        prop_assert_eq!(starts[task_index].load(Ordering::SeqCst), 1);
        prop_assert_eq!(finishes[task_index].load(Ordering::SeqCst), 1);
        let expected_name = server_name(task_index);

        if spec.should_fail {
            prop_assert_eq!(successful_completions[task_index].load(Ordering::SeqCst), 0);
            match result {
                PerServer::Failure { server, error } => {
                    prop_assert_eq!(server, &expected_name);
                    prop_assert_eq!(error.kind, ErrorKind::NetworkError);
                    prop_assert_eq!(
                        error.message.as_str(),
                        format!("isolated failure for task {task_index}")
                    );
                }
                PerServer::Success { .. } => {
                    return Err(TestCaseError::fail(format!(
                        "failed task {task_index} was rewritten as a success"
                    )));
                }
            }
        } else {
            prop_assert_eq!(successful_completions[task_index].load(Ordering::SeqCst), 1);
            match result {
                PerServer::Success { server, value } => {
                    prop_assert_eq!(server, &expected_name);
                    prop_assert_eq!(
                        value,
                        &TaskValue {
                            task_index,
                            value: spec.value,
                        },
                        "successful task value was rewritten"
                    );
                }
                PerServer::Failure { .. } => {
                    return Err(TestCaseError::fail(format!(
                        "successful task {task_index} was replaced by a sibling failure"
                    )));
                }
            }
        }
    }

    let mut observations = context_observations
        .lock()
        .expect("observation mutex")
        .clone();
    observations.sort_by_key(|(task_index, _, _)| *task_index);
    prop_assert_eq!(observations.len(), task_count);
    for (expected_index, (task_index, context_address, observed_deadline)) in
        observations.into_iter().enumerate()
    {
        prop_assert_eq!(task_index, expected_index);
        prop_assert_eq!(context_address, expected_context_address);
        prop_assert_eq!(observed_deadline, deadline);
    }

    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 32: 有界并发与失败隔离
    // **Validates: Requirements 14.1, 14.3**
    #[test]
    fn property_32_bounded_concurrency_and_failure_isolation(
        (tasks, limit) in scenario()
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("property runtime");
        runtime.block_on(run_scenario(tasks, limit))?;
    }
}
