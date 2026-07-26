#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    io::Cursor,
    num::NonZeroUsize,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use mcp_cli::{
    BoxFuture, CallHandler, CallInput, CancellationFlag, CliError, CommandContext, CommandOutcome,
    ConfigHash, ConnectionError, ConnectionManager, ConnectionMode, Deadline, DiagnosticSink,
    ErrorKind, GrepHandler, InfoHandler, JsonObject, ListHandler, McpConnection, ServerDefinition,
    ServerId, ToolFilterConfig, ToolInfo, ToolResult, TransportConfig,
    policy::tool_filter::ToolFilter,
};
use proptest::{prelude::*, test_runner::RngSeed};
use serde_json::json;

const SERVER_NAME: &str = "memory";

#[derive(Clone, Debug)]
struct RawTool {
    token: String,
    shape: u8,
    description: Option<String>,
    schema_tag: u16,
}

#[derive(Clone, Debug)]
struct Scenario {
    tools: Vec<ToolInfo>,
    filter: ToolFilterConfig,
    candidate_index: usize,
}

#[derive(Debug, Default)]
struct NullDiagnostics;

impl DiagnosticSink for NullDiagnostics {
    fn warning(&self, _message: &str) {}
    fn debug(&self, _message: &str) {}
    fn server_stderr(&self, _server: &str, _bytes: &[u8]) {}
}

#[derive(Debug, Default)]
struct Trace {
    acquired: AtomicUsize,
    listed: AtomicUsize,
    called: AtomicUsize,
    closed: AtomicUsize,
    call_names: Mutex<Vec<String>>,
}

struct MemoryConnection {
    tools: Vec<ToolInfo>,
    trace: Arc<Trace>,
}

impl McpConnection for MemoryConnection {
    fn list_tools<'a>(
        &'a self,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
        self.trace.listed.fetch_add(1, Ordering::SeqCst);
        let tools = self.tools.clone();
        Box::pin(async move { Ok(tools) })
    }

    fn call_tool<'a>(
        &'a self,
        _ctx: &'a CommandContext,
        name: &'a str,
        _args: JsonObject,
    ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
        self.trace.called.fetch_add(1, Ordering::SeqCst);
        self.trace
            .call_names
            .lock()
            .expect("call name lock")
            .push(name.to_owned());
        let result = json!({"called": name});
        Box::pin(async move { Ok(result) })
    }

    fn instructions(&self) -> Option<&str> {
        None
    }

    fn close<'a>(
        self: Box<Self>,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<(), ConnectionError>> {
        self.trace.closed.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn mode(&self) -> ConnectionMode {
        ConnectionMode::Direct
    }
}

struct MemoryManager {
    tools: Vec<ToolInfo>,
    trace: Arc<Trace>,
}

impl MemoryManager {
    fn new(tools: Vec<ToolInfo>) -> (Arc<Self>, Arc<Trace>) {
        let trace = Arc::new(Trace::default());
        (
            Arc::new(Self {
                tools,
                trace: Arc::clone(&trace),
            }),
            trace,
        )
    }
}

impl ConnectionManager for MemoryManager {
    fn acquire<'a>(
        &'a self,
        _ctx: &'a CommandContext,
        _server: &'a ServerDefinition,
    ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, CliError>> {
        self.trace.acquired.fetch_add(1, Ordering::SeqCst);
        let connection = MemoryConnection {
            tools: self.tools.clone(),
            trace: Arc::clone(&self.trace),
        };
        Box::pin(async move { Ok(Box::new(connection) as Box<dyn McpConnection>) })
    }
}

fn scalar_eq_ignore_case(left: char, right: char) -> bool {
    left == right
        || left.to_lowercase().collect::<String>() == right.to_lowercase().collect::<String>()
        || left.to_uppercase().collect::<String>() == right.to_uppercase().collect::<String>()
}

// Independent formal Tool_Filter oracle. This deliberately does not call the
// production ToolFilter, ToolPattern, or is_allowed implementation.
fn oracle_glob_match(pattern: &str, name: &str) -> bool {
    fn visit(
        pattern: &[char],
        name: &[char],
        pattern_index: usize,
        name_index: usize,
        memo: &mut [Vec<Option<bool>>],
    ) -> bool {
        if let Some(result) = memo[pattern_index][name_index] {
            return result;
        }

        let result = match pattern.get(pattern_index) {
            None => name_index == name.len(),
            Some('*') => {
                visit(pattern, name, pattern_index + 1, name_index, memo)
                    || (name_index < name.len()
                        && visit(pattern, name, pattern_index, name_index + 1, memo))
            }
            Some('?') => {
                name_index < name.len()
                    && visit(pattern, name, pattern_index + 1, name_index + 1, memo)
            }
            Some(expected) => {
                name.get(name_index)
                    .is_some_and(|actual| scalar_eq_ignore_case(*expected, *actual))
                    && visit(pattern, name, pattern_index + 1, name_index + 1, memo)
            }
        };

        memo[pattern_index][name_index] = Some(result);
        result
    }

    let pattern = pattern.chars().collect::<Vec<_>>();
    let name = name.chars().collect::<Vec<_>>();
    let mut memo = vec![vec![None; name.len() + 1]; pattern.len() + 1];
    visit(&pattern, &name, 0, 0, &mut memo)
}

fn oracle_is_allowed(name: &str, filter: &ToolFilterConfig) -> bool {
    !filter
        .disabled_tools
        .iter()
        .any(|pattern| oracle_glob_match(pattern, name))
        && (filter.allowed_tools.is_empty()
            || filter
                .allowed_tools
                .iter()
                .any(|pattern| oracle_glob_match(pattern, name)))
}

fn safe_token() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9]{1,6}"
}

fn raw_tool() -> impl Strategy<Value = RawTool> {
    (
        safe_token(),
        0_u8..5,
        prop::option::of("[a-zA-Z0-9_./?* -]{0,24}"),
        any::<u16>(),
    )
        .prop_map(|(token, shape, description, schema_tag)| RawTool {
            token,
            shape,
            description,
            schema_tag,
        })
}

fn filter_pattern() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("*".to_owned()),
        Just("?".to_owned()),
        Just("read/*".to_owned()),
        Just("WRITE_?_*".to_owned()),
        Just("*secret*".to_owned()),
        Just("x/*/deep/*".to_owned()),
        Just("misc-*-*".to_owned()),
        safe_token(),
        safe_token().prop_map(|token| format!("*{token}*")),
        safe_token().prop_map(|token| format!("{token}?")),
    ]
}

fn toggle_ascii_case(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_lowercase() {
                character.to_ascii_uppercase()
            } else if character.is_ascii_uppercase() {
                character.to_ascii_lowercase()
            } else {
                character
            }
        })
        .collect()
}

fn build_tool(index: usize, raw: RawTool) -> ToolInfo {
    let name = match raw.shape {
        0 => format!("read/{index}_{}", raw.token),
        1 => format!("WRITE_{index}_{}", raw.token),
        2 => format!("secret/{index}_{}", raw.token),
        3 => format!("misc-{index}-{}", raw.token),
        _ => format!("x/{index}/deep/{}", raw.token),
    };
    let field = format!("field_{}_{}", index, raw.schema_tag);
    ToolInfo {
        name,
        description: raw
            .description
            .map(|description| format!("description-{index}-{description}")),
        input_schema: json!({
            "type": "object",
            "properties": {
                field.clone(): {
                    "type": if raw.schema_tag.is_multiple_of(2) { "string" } else { "integer" },
                    "description": format!("schema-description-{index}-{}", raw.schema_tag),
                    "x-tag": raw.schema_tag,
                }
            },
            "required": if raw.schema_tag.is_multiple_of(3) { vec![field] } else { Vec::<String>::new() },
            "x-tool-index": index,
        }),
    }
}

fn scenario() -> impl Strategy<Value = Scenario> {
    (
        prop::collection::vec(raw_tool(), 1..=9),
        prop::collection::vec(filter_pattern(), 0..=4),
        prop::collection::vec(filter_pattern(), 0..=4),
        any::<usize>(),
        any::<bool>(),
        0_u8..8,
    )
        .prop_map(
            |(raw_tools, mut allowed, mut disabled, selector, reverse, mode)| {
                let mut tools = raw_tools
                    .into_iter()
                    .enumerate()
                    .map(|(index, raw)| build_tool(index, raw))
                    .collect::<Vec<_>>();
                if reverse {
                    tools.reverse();
                }
                let candidate_index = selector % tools.len();
                let candidate = tools[candidate_index].name.clone();

                match mode {
                    1 => {
                        allowed = vec!["*".to_owned()];
                        disabled = vec![candidate.clone()];
                    }
                    2 => {
                        allowed = vec![toggle_ascii_case(&candidate)];
                        disabled.clear();
                    }
                    3 => {
                        allowed.clear();
                        disabled = vec![toggle_ascii_case(&candidate)];
                    }
                    4 => {
                        allowed.clear();
                        disabled.clear();
                    }
                    5 => {
                        allowed = vec![
                            "read/*".to_owned(),
                            "WRITE_?_*".to_owned(),
                            "x/*/deep/*".to_owned(),
                            "misc-*-*".to_owned(),
                        ];
                        disabled = vec!["*secret*".to_owned()];
                    }
                    6 => {
                        allowed.push("*".to_owned());
                        disabled.push("*secret*".to_owned());
                    }
                    7 => {
                        allowed = vec![candidate];
                        disabled = vec!["*".to_owned()];
                    }
                    _ => {}
                }

                Scenario {
                    tools,
                    filter: ToolFilterConfig {
                        allowed_tools: allowed,
                        disabled_tools: disabled,
                    },
                    candidate_index,
                }
            },
        )
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

fn server(filter: ToolFilterConfig) -> ServerDefinition {
    ServerDefinition {
        name: SERVER_NAME.to_owned(),
        id: ServerId("0".repeat(64)),
        config_hash: ConfigHash([0; 32]),
        transport: TransportConfig::Stdio {
            command: "in-memory-property-server".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        },
        filter,
    }
}

fn configured_servers(filter: ToolFilterConfig) -> BTreeMap<String, ServerDefinition> {
    BTreeMap::from([(SERVER_NAME.to_owned(), server(filter))])
}

fn human_text(outcome: CommandOutcome, command: &str) -> Result<String, TestCaseError> {
    match outcome {
        CommandOutcome::HumanText(text) => Ok(text),
        other => Err(TestCaseError::fail(format!(
            "{command} returned non-text outcome: {other:?}"
        ))),
    }
}

fn list_tool_names(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.strip_prefix("  • ").map(str::to_owned))
        .collect()
}

fn info_tool_names(text: &str) -> Vec<String> {
    let Some((_, tools_section)) = text.split_once("Tools (") else {
        return Vec::new();
    };
    tools_section
        .lines()
        .filter_map(|line| {
            line.strip_prefix("  ")
                .filter(|name| !name.starts_with(' ') && *name != "(none)")
                .map(str::to_owned)
        })
        .collect()
}

fn grep_tool_names(text: &str) -> Vec<String> {
    if text == "No matching tools found.\n" {
        return Vec::new();
    }
    text.lines()
        .filter_map(|line| {
            line.split_once(' ')
                .filter(|(server, _)| *server == SERVER_NAME)
                .map(|(_, tool)| tool.to_owned())
        })
        .collect()
}

async fn display_names(
    scenario: &Scenario,
) -> Result<(Vec<String>, Vec<String>, Vec<String>, Arc<Trace>), TestCaseError> {
    let servers = configured_servers(scenario.filter.clone());
    let (manager, trace) = MemoryManager::new(scenario.tools.clone());
    let manager_boundary: Arc<dyn ConnectionManager> = manager;
    let concurrency = NonZeroUsize::new(1).expect("one is non-zero");

    let list = ListHandler::new(Arc::clone(&manager_boundary), concurrency)
        .execute(&context(), &servers, false)
        .await
        .map_err(|error| TestCaseError::fail(format!("list failed: {error}")))?;
    let info = InfoHandler::new(Arc::clone(&manager_boundary))
        .execute(&context(), &servers, SERVER_NAME, None, false)
        .await
        .map_err(|error| TestCaseError::fail(format!("info failed: {error}")))?;
    let grep = GrepHandler::new(manager_boundary, concurrency)
        .execute(&context(), &servers, "**", false)
        .await
        .map_err(|error| TestCaseError::fail(format!("grep failed: {error}")))?;

    Ok((
        list_tool_names(&human_text(list, "list")?),
        info_tool_names(&human_text(info, "info")?),
        grep_tool_names(&human_text(grep, "grep")?),
        trace,
    ))
}

async fn check_scenario(scenario: Scenario) -> Result<(), TestCaseError> {
    let expected_stable = scenario
        .tools
        .iter()
        .filter(|tool| oracle_is_allowed(&tool.name, &scenario.filter))
        .cloned()
        .collect::<Vec<_>>();

    // `filter` itself must preserve complete description/schema-bearing values
    // in original order; command-specific display sorting is checked separately.
    let actual_stable = ToolFilter::new(&scenario.filter).filter(scenario.tools.clone());
    prop_assert_eq!(
        &actual_stable,
        &expected_stable,
        "production display filter was not the oracle-selected stable subsequence"
    );

    let mut expected_display = expected_stable
        .iter()
        .map(|tool| tool.name.clone())
        .collect::<Vec<_>>();
    expected_display.sort();

    let (list_names, info_names, grep_names, display_trace) = display_names(&scenario).await?;
    prop_assert_eq!(
        &list_names,
        &expected_display,
        "list visibility/order diverged"
    );
    prop_assert_eq!(
        &info_names,
        &expected_display,
        "info visibility/order diverged"
    );
    prop_assert_eq!(
        &grep_names,
        &expected_display,
        "grep visibility/order diverged"
    );
    prop_assert_eq!(display_trace.acquired.load(Ordering::SeqCst), 3);
    prop_assert_eq!(display_trace.listed.load(Ordering::SeqCst), 3);
    prop_assert_eq!(display_trace.called.load(Ordering::SeqCst), 0);
    prop_assert_eq!(display_trace.closed.load(Ordering::SeqCst), 3);

    let candidate = &scenario.tools[scenario.candidate_index];
    let expected_allowed = oracle_is_allowed(&candidate.name, &scenario.filter);
    prop_assert_eq!(
        expected_display.iter().any(|name| name == &candidate.name),
        expected_allowed,
        "display authorization and independent call decision disagreed"
    );
    prop_assert!(
        [&list_names, &info_names, &grep_names]
            .into_iter()
            .all(|names| names.iter().any(|name| name == &candidate.name) == expected_allowed),
        "one display command disagreed with the shared candidate authorization"
    );

    let servers = configured_servers(scenario.filter.clone());
    let (call_manager, call_trace) = MemoryManager::new(scenario.tools.clone());
    let call_boundary: Arc<dyn ConnectionManager> = call_manager;
    let handler = CallHandler::new(call_boundary);
    let mut input = CallInput::new(Cursor::new(Vec::<u8>::new()), true);
    let result = handler
        .execute(
            &context(),
            &servers,
            SERVER_NAME,
            &candidate.name,
            Some("{}"),
            &mut input,
        )
        .await;

    if expected_allowed {
        let outcome = result.map_err(|error| {
            TestCaseError::fail(format!(
                "oracle-authorized visible tool was rejected by call: {error}"
            ))
        })?;
        prop_assert_eq!(
            outcome,
            CommandOutcome::Json(json!({"called": candidate.name}))
        );
        prop_assert_eq!(call_trace.acquired.load(Ordering::SeqCst), 1);
        prop_assert_eq!(call_trace.listed.load(Ordering::SeqCst), 1);
        prop_assert_eq!(call_trace.called.load(Ordering::SeqCst), 1);
        prop_assert_eq!(call_trace.closed.load(Ordering::SeqCst), 1);
        let call_names = call_trace.call_names.lock().expect("call name lock");
        prop_assert_eq!(call_names.as_slice(), [candidate.name.as_str()]);
    } else {
        let error = match result {
            Ok(outcome) => {
                return Err(TestCaseError::fail(format!(
                    "oracle-rejected hidden tool was called successfully: {outcome:?}"
                )));
            }
            Err(error) => error,
        };
        prop_assert_eq!(error.kind, ErrorKind::ToolDisabled);
        prop_assert_eq!(error.machine_kind(), "TOOL_DISABLED");
        prop_assert_eq!(call_trace.acquired.load(Ordering::SeqCst), 0);
        prop_assert_eq!(call_trace.listed.load(Ordering::SeqCst), 0);
        prop_assert_eq!(call_trace.called.load(Ordering::SeqCst), 0);
        prop_assert_eq!(call_trace.closed.load(Ordering::SeqCst), 0);
        prop_assert!(
            call_trace
                .call_names
                .lock()
                .expect("call name lock")
                .is_empty()
        );
    }

    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 192,
        rng_seed: RngSeed::Fixed(0x10f1_17e2_2025),
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 10: 展示过滤与调用授权同源
    // **Validates: Requirements 4.7, 4.8, 4.9**
    #[test]
    fn property_10_display_filter_and_call_authorization_share_one_policy(
        scenario in scenario()
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("property runtime");
        runtime.block_on(check_scenario(scenario))?;
    }
}
