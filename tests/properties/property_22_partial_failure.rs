#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use mcp_cli::{
    BoxFuture, CancellationFlag, CliError, CommandContext, CommandOutcome, ConfigHash,
    ConnectionError, ConnectionManager, ConnectionMode, Deadline, DiagnosticSink, JsonObject,
    ListHandler, McpConnection, ServerDefinition, ServerId, ToolFilterConfig, ToolInfo, ToolResult,
    TransportConfig,
};
use proptest::prelude::*;
use serde_json::json;
use tokio::sync::{Semaphore, mpsc};

#[derive(Clone, Debug)]
struct ToolSpec {
    name: String,
    description: String,
    schema_tag: u16,
}

#[derive(Clone, Debug)]
struct ServerSpec {
    name: String,
    tools: Vec<ToolSpec>,
    connect_failure: bool,
    list_failure: bool,
    close_failure: bool,
    completion_rank: u16,
}

impl ServerSpec {
    fn succeeds(&self) -> bool {
        !self.connect_failure && !self.list_failure && !self.close_failure
    }
}

#[derive(Clone, Debug)]
struct Scenario {
    servers: Vec<ServerSpec>,
    concurrency: usize,
}

#[derive(Debug, Default)]
struct NullDiagnostics;

impl DiagnosticSink for NullDiagnostics {
    fn warning(&self, _message: &str) {}
    fn debug(&self, _message: &str) {}
    fn server_stderr(&self, _server: &str, _bytes: &[u8]) {}
}

#[derive(Debug, Default)]
struct ServerTrace {
    acquired: AtomicUsize,
    listed: AtomicUsize,
    closed: AtomicUsize,
}

struct ScriptedConnection {
    server: String,
    tools: Vec<ToolInfo>,
    list_failure: bool,
    close_failure: bool,
    trace: Arc<ServerTrace>,
    finished: mpsc::UnboundedSender<String>,
}

impl McpConnection for ScriptedConnection {
    fn list_tools<'a>(
        &'a self,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
        self.trace.listed.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if self.list_failure {
                Err(ConnectionError::new(format!(
                    "generated list failure for {}",
                    self.server
                )))
            } else {
                Ok(self.tools.clone())
            }
        })
    }

    fn call_tool<'a>(
        &'a self,
        _ctx: &'a CommandContext,
        _name: &'a str,
        _args: JsonObject,
    ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
        Box::pin(async { panic!("the list property must not call tools") })
    }

    fn instructions(&self) -> Option<&str> {
        None
    }

    fn close<'a>(
        self: Box<Self>,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<(), ConnectionError>> {
        Box::pin(async move {
            self.trace.closed.fetch_add(1, Ordering::SeqCst);
            self.finished
                .send(self.server.clone())
                .expect("completion driver remains available");
            if self.close_failure {
                Err(ConnectionError::new(format!(
                    "generated close failure for {}",
                    self.server
                )))
            } else {
                Ok(())
            }
        })
    }

    fn mode(&self) -> ConnectionMode {
        ConnectionMode::Direct
    }
}

struct GatedManager {
    specs: BTreeMap<String, ServerSpec>,
    gates: BTreeMap<String, Arc<Semaphore>>,
    traces: BTreeMap<String, Arc<ServerTrace>>,
    started: mpsc::UnboundedSender<String>,
    finished: mpsc::UnboundedSender<String>,
}

impl ConnectionManager for GatedManager {
    fn acquire<'a>(
        &'a self,
        _ctx: &'a CommandContext,
        server: &'a ServerDefinition,
    ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, CliError>> {
        let spec = self
            .specs
            .get(&server.name)
            .expect("every definition has a generated specification")
            .clone();
        let gate = Arc::clone(
            self.gates
                .get(&server.name)
                .expect("every generated server has a completion gate"),
        );
        let trace = Arc::clone(
            self.traces
                .get(&server.name)
                .expect("every generated server has a trace"),
        );
        let started = self.started.clone();
        let finished = self.finished.clone();

        Box::pin(async move {
            trace.acquired.fetch_add(1, Ordering::SeqCst);
            started
                .send(spec.name.clone())
                .expect("completion driver remains available");
            let permit = gate
                .acquire()
                .await
                .expect("property completion gate remains open");
            permit.forget();

            if spec.connect_failure {
                finished
                    .send(spec.name.clone())
                    .expect("completion driver remains available");
                return Err(CliError::network_error(
                    &spec.name,
                    format!("generated connect failure for {}", spec.name),
                ));
            }

            let tools = spec
                .tools
                .iter()
                .map(|tool| ToolInfo {
                    name: tool.name.clone(),
                    description: Some(tool.description.clone()),
                    input_schema: json!({
                        "type": "object",
                        "x-property-tag": tool.schema_tag,
                    }),
                })
                .collect();
            Ok(Box::new(ScriptedConnection {
                server: spec.name,
                tools,
                list_failure: spec.list_failure,
                close_failure: spec.close_failure,
                trace,
                finished,
            }) as Box<dyn McpConnection>)
        })
    }
}

#[derive(Clone, Copy)]
enum CompletionDirection {
    Ascending,
    Descending,
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedServer {
    server: String,
    tools: Vec<(String, String)>,
    errors: Vec<String>,
}

struct Execution {
    output: String,
    completion_order: Vec<String>,
    traces: BTreeMap<String, Arc<ServerTrace>>,
}

fn scenario_strategy() -> impl Strategy<Value = Scenario> {
    let raw_server = (
        prop::collection::vec("[a-z]{1,6}", 1..=4),
        prop::collection::vec(any::<bool>(), 3),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<u16>(),
        any::<u8>(),
    );

    prop::collection::vec(raw_server, 4..=8)
        .prop_flat_map(|raw_servers| {
            let server_count = raw_servers.len();
            (Just(raw_servers), 2_usize..=server_count)
        })
        .prop_map(|(raw_servers, concurrency)| {
            let mut servers = raw_servers
                .into_iter()
                .enumerate()
                .map(
                    |(
                        server_index,
                        (
                            local_tokens,
                            shared_mask,
                            connect_failure,
                            list_failure,
                            close_failure,
                            force_success,
                            completion_rank,
                            permutation,
                        ),
                    )| {
                        let mut tools = vec![ToolSpec {
                            name: "shared-0".to_owned(),
                            description: format!("shared-description-{server_index}-0"),
                            schema_tag: (server_index as u16).wrapping_mul(257),
                        }];
                        for (shared_index, included) in shared_mask.into_iter().enumerate() {
                            if included {
                                tools.push(ToolSpec {
                                    name: format!("shared-{}", shared_index + 1),
                                    description: format!(
                                        "shared-description-{server_index}-{}",
                                        shared_index + 1
                                    ),
                                    schema_tag: (server_index as u16)
                                        .wrapping_mul(257)
                                        .wrapping_add(shared_index as u16 + 1),
                                });
                            }
                        }
                        for (tool_index, token) in local_tokens.into_iter().enumerate() {
                            tools.push(ToolSpec {
                                name: format!("local-{server_index}-{tool_index}-{token}"),
                                description: format!(
                                    "local-description-{server_index}-{tool_index}"
                                ),
                                schema_tag: (server_index as u16)
                                    .wrapping_mul(257)
                                    .wrapping_add(tool_index as u16 + 16),
                            });
                        }
                        let rotation = usize::from(permutation) % tools.len();
                        tools.rotate_left(rotation);

                        let (connect_failure, list_failure, close_failure) = if force_success {
                            (false, false, false)
                        } else {
                            (connect_failure, list_failure, close_failure)
                        };
                        ServerSpec {
                            name: format!("server-{server_index:02}"),
                            tools,
                            connect_failure,
                            list_failure,
                            close_failure,
                            completion_rank,
                        }
                    },
                )
                .collect::<Vec<_>>();

            // Every generated case is genuinely partial: it has at least one
            // successful server and at least one independently failing server.
            servers[0].connect_failure = false;
            servers[0].list_failure = false;
            servers[0].close_failure = false;
            servers[1].connect_failure = false;
            servers[1].list_failure = false;
            servers[1].close_failure = false;
            match servers[1].completion_rank % 3 {
                0 => servers[1].connect_failure = true,
                1 => servers[1].list_failure = true,
                _ => servers[1].close_failure = true,
            }

            Scenario {
                servers,
                concurrency,
            }
        })
}

fn context() -> CommandContext {
    CommandContext {
        deadline: Deadline::new(Instant::now() + Duration::from_secs(3_600)),
        cancellation: Arc::new(CancellationFlag::default()),
        diagnostics: Arc::new(NullDiagnostics),
    }
}

fn definitions(scenario: &Scenario) -> BTreeMap<String, ServerDefinition> {
    scenario
        .servers
        .iter()
        .enumerate()
        .map(|(index, server)| {
            (
                server.name.clone(),
                ServerDefinition {
                    name: server.name.clone(),
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

type GatedFixture = (
    Arc<GatedManager>,
    mpsc::UnboundedReceiver<String>,
    mpsc::UnboundedReceiver<String>,
    BTreeMap<String, Arc<ServerTrace>>,
);

fn fixture(scenario: &Scenario) -> GatedFixture {
    let specs = scenario
        .servers
        .iter()
        .map(|server| (server.name.clone(), server.clone()))
        .collect::<BTreeMap<_, _>>();
    let gates = scenario
        .servers
        .iter()
        .map(|server| (server.name.clone(), Arc::new(Semaphore::new(0))))
        .collect::<BTreeMap<_, _>>();
    let traces = scenario
        .servers
        .iter()
        .map(|server| (server.name.clone(), Arc::new(ServerTrace::default())))
        .collect::<BTreeMap<_, _>>();
    let (started, started_rx) = mpsc::unbounded_channel();
    let (finished, finished_rx) = mpsc::unbounded_channel();
    let manager = Arc::new(GatedManager {
        specs,
        gates,
        traces: traces.clone(),
        started,
        finished,
    });
    (manager, started_rx, finished_rx, traces)
}

async fn drive_completion_order(
    scenario: &Scenario,
    manager: &GatedManager,
    started: &mut mpsc::UnboundedReceiver<String>,
    finished: &mut mpsc::UnboundedReceiver<String>,
    direction: CompletionDirection,
) -> Result<Vec<String>, TestCaseError> {
    let total = scenario.servers.len();
    let mut started_count = 0;
    let mut active = BTreeSet::new();

    while active.len() < scenario.concurrency.min(total) {
        let server = started
            .recv()
            .await
            .expect("the bounded batch starts the initial server tasks");
        prop_assert!(active.insert(server));
        started_count += 1;
    }
    prop_assert!(
        started.try_recv().is_err(),
        "more than the generated concurrency limit entered acquire"
    );

    let rank = scenario
        .servers
        .iter()
        .map(|server| (server.name.as_str(), server.completion_rank))
        .collect::<BTreeMap<_, _>>();
    let mut completion_order = Vec::with_capacity(total);

    while completion_order.len() < total {
        let selected = active
            .iter()
            .min_by(|left, right| {
                let left_key = (rank[left.as_str()], left.as_str());
                let right_key = (rank[right.as_str()], right.as_str());
                match direction {
                    CompletionDirection::Ascending => left_key.cmp(&right_key),
                    CompletionDirection::Descending => right_key.cmp(&left_key),
                }
            })
            .expect("at least one bounded task is active")
            .clone();
        manager
            .gates
            .get(&selected)
            .expect("selected server has a gate")
            .add_permits(1);
        let completed = finished
            .recv()
            .await
            .expect("every generated task reports completion");
        prop_assert_eq!(&completed, &selected);
        prop_assert!(active.remove(&completed));
        completion_order.push(completed);

        if started_count < total {
            let next = started
                .recv()
                .await
                .expect("a permit release starts the next waiting task");
            prop_assert!(active.insert(next));
            started_count += 1;
            prop_assert!(active.len() <= scenario.concurrency);
        }
    }

    Ok(completion_order)
}

async fn run_execution(
    scenario: &Scenario,
    direction: CompletionDirection,
) -> Result<Execution, TestCaseError> {
    let servers = definitions(scenario);
    let (manager, mut started, mut finished, traces) = fixture(scenario);
    let handler = ListHandler::new(
        manager.clone(),
        NonZeroUsize::new(scenario.concurrency).expect("generated limit is positive"),
    );
    let ctx = context();

    let (outcome, completion_order) = tokio::join!(
        handler.execute(&ctx, &servers, true),
        drive_completion_order(
            scenario,
            manager.as_ref(),
            &mut started,
            &mut finished,
            direction,
        ),
    );
    let outcome = outcome.map_err(|error| {
        TestCaseError::fail(format!("generated list execution failed: {error}"))
    })?;
    let output = match outcome {
        CommandOutcome::HumanText(output) => output,
        other => {
            return Err(TestCaseError::fail(format!(
                "list returned a non-text outcome: {other:?}"
            )));
        }
    };

    Ok(Execution {
        output,
        completion_order: completion_order?,
        traces,
    })
}

// Independent parser/oracle boundary: expected values below are computed only
// from generated input and never call the production formatter or sort helper.
fn parse_list_output(output: &str) -> Result<Vec<ParsedServer>, TestCaseError> {
    prop_assert!(output.ends_with('\n'));
    prop_assert!(!output.ends_with("\n\n"));
    let body = output
        .strip_suffix('\n')
        .expect("the trailing newline was asserted");
    let mut parsed = Vec::new();

    for block in body.split("\n\n") {
        let mut lines = block.lines();
        let server = lines
            .next()
            .ok_or_else(|| TestCaseError::fail("list output contained an empty server block"))?
            .to_owned();
        let mut tools = Vec::new();
        let mut errors = Vec::new();
        for line in lines {
            if let Some(tool) = line.strip_prefix("  • ") {
                let (name, description) = tool.split_once(" - ").ok_or_else(|| {
                    TestCaseError::fail(format!(
                        "described tool entry had no stable separator: {line}"
                    ))
                })?;
                tools.push((name.to_owned(), description.to_owned()));
            } else if let Some(error) = line
                .strip_prefix("  <error: ")
                .and_then(|line| line.strip_suffix('>'))
            {
                errors.push(error.to_owned());
            } else {
                return Err(TestCaseError::fail(format!(
                    "unrecognized list output line: {line}"
                )));
            }
        }
        parsed.push(ParsedServer {
            server,
            tools,
            errors,
        });
    }

    Ok(parsed)
}

fn verify_execution(scenario: &Scenario, execution: &Execution) -> Result<(), TestCaseError> {
    let parsed = parse_list_output(&execution.output)?;
    let expected_server_order = scenario
        .servers
        .iter()
        .map(|server| server.name.clone())
        .collect::<Vec<_>>();
    prop_assert_eq!(
        parsed
            .iter()
            .map(|server| server.server.clone())
            .collect::<Vec<_>>(),
        expected_server_order
    );
    prop_assert_eq!(parsed.len(), scenario.servers.len());

    let mut expected_success_tools = BTreeSet::new();
    let mut actual_success_tools = BTreeSet::new();

    for (spec, actual) in scenario.servers.iter().zip(parsed.iter()) {
        prop_assert_eq!(&actual.server, &spec.name);
        let trace = execution
            .traces
            .get(&spec.name)
            .expect("every generated server has a trace");
        prop_assert_eq!(trace.acquired.load(Ordering::SeqCst), 1);

        if spec.connect_failure {
            prop_assert_eq!(trace.listed.load(Ordering::SeqCst), 0);
            prop_assert_eq!(trace.closed.load(Ordering::SeqCst), 0);
        } else {
            prop_assert_eq!(trace.listed.load(Ordering::SeqCst), 1);
            prop_assert_eq!(trace.closed.load(Ordering::SeqCst), 1);
        }

        if spec.succeeds() {
            prop_assert!(actual.errors.is_empty());
            let mut expected_tools = spec
                .tools
                .iter()
                .map(|tool| (tool.name.clone(), tool.description.clone()))
                .collect::<Vec<_>>();
            expected_tools.sort();
            prop_assert_eq!(&actual.tools, &expected_tools);

            for tool in expected_tools {
                expected_success_tools.insert((spec.name.clone(), tool.clone()));
                actual_success_tools.insert((actual.server.clone(), tool));
            }
        } else {
            prop_assert!(actual.tools.is_empty());
            prop_assert_eq!(actual.errors.len(), 1);
            prop_assert!(!actual.errors[0].trim().is_empty());
            prop_assert!(
                actual.errors[0].contains(&spec.name),
                "failure item must identify only its own server"
            );
        }
    }

    prop_assert_eq!(actual_success_tools, expected_success_tools);
    Ok(())
}

async fn verify_scenario(scenario: Scenario) -> Result<(), TestCaseError> {
    let ascending = run_execution(&scenario, CompletionDirection::Ascending).await?;
    let descending = run_execution(&scenario, CompletionDirection::Descending).await?;

    prop_assert_ne!(
        &ascending.completion_order,
        &descending.completion_order,
        "the fixture must exercise distinct asynchronous completion orders"
    );
    prop_assert_eq!(&ascending.output, &descending.output);
    verify_execution(&scenario, &ascending)?;
    verify_execution(&scenario, &descending)?;
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 22: 部分失败保留全部成功
    // **Validates: Requirements 9.4**
    #[test]
    fn property_22_partial_failures_preserve_every_success(
        scenario in scenario_strategy()
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("property runtime");
        runtime.block_on(verify_scenario(scenario))?;
    }
}
