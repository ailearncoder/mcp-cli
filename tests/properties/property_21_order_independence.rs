#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use mcp_cli::{
    BoxFuture, CancellationFlag, CommandContext, CommandOutcome, ConfigHash, ConnectionError,
    ConnectionManager, ConnectionMode, Deadline, DiagnosticSink, GrepHandler, JsonObject,
    ListHandler, McpConnection, PlainTextPresenter, Presenter, ServerDefinition, ServerId,
    StylePolicy, ToolFilterConfig, ToolInfo, ToolResult, TransportConfig,
};
use proptest::prelude::*;
use serde_json::json;
use tokio::sync::{Semaphore, mpsc};

#[derive(Clone, Debug)]
struct ToolSpec {
    name: String,
    description: Option<String>,
    schema_tag: u16,
}

#[derive(Clone, Debug)]
struct ServerSpec {
    name: String,
    tools: Vec<ToolSpec>,
    schedule_key: u64,
}

#[derive(Clone, Debug)]
struct Scenario {
    servers: Vec<ServerSpec>,
}

#[derive(Debug, Default)]
struct NullDiagnostics;

impl DiagnosticSink for NullDiagnostics {
    fn warning(&self, _message: &str) {}
    fn debug(&self, _message: &str) {}
    fn server_stderr(&self, _server: &str, _bytes: &[u8]) {}
}

struct GatedConnection {
    server: String,
    tools: Vec<ToolInfo>,
    gate: Arc<Semaphore>,
    completed: mpsc::UnboundedSender<String>,
}

impl McpConnection for GatedConnection {
    fn list_tools<'a>(
        &'a self,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
        Box::pin(async move {
            let permit = self
                .gate
                .acquire()
                .await
                .expect("property completion gate remains open");
            permit.forget();
            Ok(self.tools.clone())
        })
    }

    fn call_tool<'a>(
        &'a self,
        _ctx: &'a CommandContext,
        _name: &'a str,
        _args: JsonObject,
    ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
        Box::pin(async { panic!("list and grep handlers must not call tools") })
    }

    fn instructions(&self) -> Option<&str> {
        None
    }

    fn close<'a>(
        self: Box<Self>,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<(), ConnectionError>> {
        Box::pin(async move {
            self.completed
                .send(self.server.clone())
                .expect("completion driver remains available");
            Ok(())
        })
    }

    fn mode(&self) -> ConnectionMode {
        ConnectionMode::Direct
    }
}

struct GatedManager {
    tools: BTreeMap<String, Vec<ToolInfo>>,
    gates: BTreeMap<String, Arc<Semaphore>>,
    completed: mpsc::UnboundedSender<String>,
}

impl ConnectionManager for GatedManager {
    fn acquire<'a>(
        &'a self,
        _ctx: &'a CommandContext,
        server: &'a ServerDefinition,
    ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, mcp_cli::CliError>> {
        let connection = GatedConnection {
            server: server.name.clone(),
            tools: self
                .tools
                .get(&server.name)
                .expect("every generated server has tools")
                .clone(),
            gate: Arc::clone(
                self.gates
                    .get(&server.name)
                    .expect("every generated server has a completion gate"),
            ),
            completed: self.completed.clone(),
        };
        Box::pin(async move { Ok(Box::new(connection) as Box<dyn McpConnection>) })
    }
}

fn scenario_strategy() -> impl Strategy<Value = Scenario> {
    let token = "[a-z]{1,8}";
    let description = prop::option::of("[A-Za-z0-9 ]{0,24}");
    let raw_tool = (token, description, any::<bool>(), any::<u16>());
    let raw_server = (token, prop::collection::vec(raw_tool, 1..=5), any::<u64>());

    prop::collection::vec(raw_server, 2..=6).prop_map(|servers| Scenario {
        servers: servers
            .into_iter()
            .enumerate()
            .map(
                |(server_index, (server_token, tools, schedule_key))| ServerSpec {
                    name: format!("server-{server_index}-{server_token}"),
                    tools: tools
                        .into_iter()
                        .enumerate()
                        .map(
                            |(tool_index, (tool_token, description, generated_hit, schema_tag))| {
                                // Every server has at least one grep result while the rest of the
                                // generated set can independently match or miss.
                                let is_hit = tool_index == 0 || generated_hit;
                                ToolSpec {
                                    name: format!(
                                        "{}-{tool_index}-{tool_token}",
                                        if is_hit { "hit" } else { "miss" }
                                    ),
                                    description,
                                    schema_tag,
                                }
                            },
                        )
                        .collect(),
                    schedule_key,
                },
            )
            .collect(),
    })
}

fn test_epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn context() -> CommandContext {
    CommandContext {
        deadline: Deadline::new(test_epoch() + Duration::from_secs(3_600)),
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

fn tool_map(scenario: &Scenario) -> BTreeMap<String, Vec<ToolInfo>> {
    scenario
        .servers
        .iter()
        .map(|server| {
            (
                server.name.clone(),
                server
                    .tools
                    .iter()
                    .map(|tool| ToolInfo {
                        name: tool.name.clone(),
                        description: tool.description.clone(),
                        input_schema: json!({"type": "object", "x-property-tag": tool.schema_tag}),
                    })
                    .collect(),
            )
        })
        .collect()
}

fn schedules(scenario: &Scenario) -> (Vec<String>, Vec<String>) {
    let mut keyed = scenario
        .servers
        .iter()
        .enumerate()
        .map(|(index, server)| (server.schedule_key, index, server.name.clone()))
        .collect::<Vec<_>>();
    keyed.sort_by_key(|(key, index, _)| (*key, *index));
    let first = keyed
        .iter()
        .map(|(_, _, server)| server.clone())
        .collect::<Vec<_>>();
    let second = first.iter().rev().cloned().collect::<Vec<_>>();
    (first, second)
}

fn completion_fixture(
    scenario: &Scenario,
) -> (
    Arc<GatedManager>,
    BTreeMap<String, Arc<Semaphore>>,
    mpsc::UnboundedReceiver<String>,
) {
    let gates = scenario
        .servers
        .iter()
        .map(|server| (server.name.clone(), Arc::new(Semaphore::new(0))))
        .collect::<BTreeMap<_, _>>();
    let (completed, receiver) = mpsc::unbounded_channel();
    let manager = Arc::new(GatedManager {
        tools: tool_map(scenario),
        gates: gates.clone(),
        completed,
    });
    (manager, gates, receiver)
}

async fn force_completion_order(
    schedule: &[String],
    gates: &BTreeMap<String, Arc<Semaphore>>,
    receiver: &mut mpsc::UnboundedReceiver<String>,
) -> Result<(), TestCaseError> {
    for expected_server in schedule {
        gates
            .get(expected_server)
            .expect("scheduled server has a gate")
            .add_permits(1);
        let completed_server = receiver
            .recv()
            .await
            .expect("handler closes every generated connection");
        prop_assert_eq!(&completed_server, expected_server);
    }
    Ok(())
}

fn render_plain(outcome: CommandOutcome) -> Result<Vec<u8>, TestCaseError> {
    PlainTextPresenter
        .render(outcome, StylePolicy::new(false, true))
        .map_err(|error| TestCaseError::fail(format!("plain stdout rendering failed: {error}")))
}

async fn run_list(
    scenario: &Scenario,
    schedule: &[String],
    with_descriptions: bool,
) -> Result<Vec<u8>, TestCaseError> {
    let servers = definitions(scenario);
    let (manager, gates, mut receiver) = completion_fixture(scenario);
    let handler = ListHandler::new(
        manager,
        NonZeroUsize::new(servers.len()).expect("scenario has multiple servers"),
    );
    let ctx = context();

    let (outcome, driven) = tokio::join!(
        handler.execute(&ctx, &servers, with_descriptions),
        force_completion_order(schedule, &gates, &mut receiver),
    );
    driven?;
    let outcome = outcome
        .map_err(|error| TestCaseError::fail(format!("generated list scenario failed: {error}")))?;
    render_plain(outcome)
}

async fn run_grep(
    scenario: &Scenario,
    schedule: &[String],
    with_descriptions: bool,
) -> Result<Vec<u8>, TestCaseError> {
    let servers = definitions(scenario);
    let (manager, gates, mut receiver) = completion_fixture(scenario);
    let handler = GrepHandler::new(
        manager,
        NonZeroUsize::new(servers.len()).expect("scenario has multiple servers"),
    );
    let ctx = context();

    let (outcome, driven) = tokio::join!(
        handler.execute(&ctx, &servers, "hit-*", with_descriptions),
        force_completion_order(schedule, &gates, &mut receiver),
    );
    driven?;
    let outcome = outcome
        .map_err(|error| TestCaseError::fail(format!("generated grep scenario failed: {error}")))?;
    render_plain(outcome)
}

// Independent output oracle: this deliberately does not call any production
// formatter, presenter sorting helper, or command sorting function.
fn expected_list_stdout(scenario: &Scenario, with_descriptions: bool) -> Vec<u8> {
    let mut servers = scenario.servers.iter().collect::<Vec<_>>();
    servers.sort_by(|left, right| left.name.cmp(&right.name));
    let mut lines = Vec::new();

    for (server_index, server) in servers.into_iter().enumerate() {
        if server_index != 0 {
            lines.push(String::new());
        }
        lines.push(server.name.clone());
        let mut tools = server.tools.iter().collect::<Vec<_>>();
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        for tool in tools {
            let mut line = format!("  • {}", tool.name);
            if with_descriptions && let Some(description) = &tool.description {
                line.push_str(" - ");
                line.push_str(description);
            }
            lines.push(line);
        }
    }

    format!("{}\n", lines.join("\n")).into_bytes()
}

// Independent grep oracle sorted only from generated domain values.
fn expected_grep_stdout(scenario: &Scenario, with_descriptions: bool) -> Vec<u8> {
    let mut hits = scenario
        .servers
        .iter()
        .flat_map(|server| {
            server
                .tools
                .iter()
                .filter(|tool| tool.name.starts_with("hit-"))
                .map(move |tool| (server.name.as_str(), tool))
        })
        .collect::<Vec<_>>();
    hits.sort_by(|(left_server, left_tool), (right_server, right_tool)| {
        left_server
            .cmp(right_server)
            .then_with(|| left_tool.name.cmp(&right_tool.name))
    });

    let lines = hits
        .into_iter()
        .map(|(server, tool)| {
            let mut line = format!("{server} {}", tool.name);
            if with_descriptions && let Some(description) = &tool.description {
                line.push_str(" - ");
                line.push_str(description);
            }
            line
        })
        .collect::<Vec<_>>();
    format!("{}\n", lines.join("\n")).into_bytes()
}

async fn verify_scenario(scenario: Scenario) -> Result<(), TestCaseError> {
    let (first_schedule, second_schedule) = schedules(&scenario);
    prop_assert_ne!(&first_schedule, &second_schedule);

    for with_descriptions in [false, true] {
        let first_list = run_list(&scenario, &first_schedule, with_descriptions).await?;
        let second_list = run_list(&scenario, &second_schedule, with_descriptions).await?;
        prop_assert_eq!(&first_list, &second_list);
        prop_assert_eq!(
            first_list,
            expected_list_stdout(&scenario, with_descriptions)
        );

        let first_grep = run_grep(&scenario, &first_schedule, with_descriptions).await?;
        let second_grep = run_grep(&scenario, &second_schedule, with_descriptions).await?;
        prop_assert_eq!(&first_grep, &second_grep);
        prop_assert_eq!(
            first_grep,
            expected_grep_stdout(&scenario, with_descriptions)
        );
    }

    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 21: 批处理输出与完成顺序无关
    // **Validates: Requirements 9.1, 9.2, 10.6, 14.4, 17.8**
    #[test]
    fn property_21_batch_output_is_independent_of_completion_order(
        scenario in scenario_strategy()
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("property runtime");
        runtime.block_on(verify_scenario(scenario))?;
    }
}
