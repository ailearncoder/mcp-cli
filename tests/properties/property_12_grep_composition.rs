#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use mcp_cli::{
    BoxFuture, CancellationFlag, CliError, CommandContext, CommandOutcome, ConfigHash,
    ConnectionError, ConnectionManager, ConnectionMode, Deadline, DiagnosticSink, GrepHandler,
    JsonObject, McpConnection, ServerDefinition, ServerId, ToolFilterConfig, ToolInfo, ToolResult,
    TransportConfig,
};
use proptest::prelude::*;
use serde_json::json;

#[derive(Clone, Debug)]
struct RawTool {
    token: String,
    shape: u8,
    description: Option<String>,
    schema_tag: u16,
}

#[derive(Clone, Debug)]
struct RawServer {
    suffix: String,
    tools: Vec<RawTool>,
    allowed: Vec<String>,
    disabled: Vec<String>,
    reverse_tools: bool,
}

#[derive(Clone, Debug)]
struct ServerCase {
    definition: ServerDefinition,
    tools: Vec<ToolInfo>,
}

#[derive(Clone, Debug)]
struct Scenario {
    servers: Vec<ServerCase>,
    pattern: String,
}

#[derive(Debug, Default)]
struct NullDiagnostics;

impl DiagnosticSink for NullDiagnostics {
    fn warning(&self, _message: &str) {}
    fn debug(&self, _message: &str) {}
    fn server_stderr(&self, _server: &str, _bytes: &[u8]) {}
}

struct MemoryConnection {
    tools: Vec<ToolInfo>,
}

impl McpConnection for MemoryConnection {
    fn list_tools<'a>(
        &'a self,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<Vec<ToolInfo>, ConnectionError>> {
        let tools = self.tools.clone();
        Box::pin(async move { Ok(tools) })
    }

    fn call_tool<'a>(
        &'a self,
        _ctx: &'a CommandContext,
        _name: &'a str,
        _args: JsonObject,
    ) -> BoxFuture<'a, Result<ToolResult, ConnectionError>> {
        Box::pin(async { panic!("grep property must never call a tool") })
    }

    fn instructions(&self) -> Option<&str> {
        None
    }

    fn close<'a>(
        self: Box<Self>,
        _ctx: &'a CommandContext,
    ) -> BoxFuture<'a, Result<(), ConnectionError>> {
        Box::pin(async { Ok(()) })
    }

    fn mode(&self) -> ConnectionMode {
        ConnectionMode::Direct
    }
}

struct MemoryManager {
    tools_by_server: BTreeMap<String, Vec<ToolInfo>>,
}

impl ConnectionManager for MemoryManager {
    fn acquire<'a>(
        &'a self,
        _ctx: &'a CommandContext,
        server: &'a ServerDefinition,
    ) -> BoxFuture<'a, Result<Box<dyn McpConnection>, CliError>> {
        let tools = self
            .tools_by_server
            .get(&server.name)
            .expect("every generated server has an in-memory connection")
            .clone();
        Box::pin(async move { Ok(Box::new(MemoryConnection { tools }) as Box<dyn McpConnection>) })
    }
}

fn scalar_eq_ignore_case(left: char, right: char) -> bool {
    left == right
        || left.to_lowercase().collect::<String>() == right.to_lowercase().collect::<String>()
        || left.to_uppercase().collect::<String>() == right.to_uppercase().collect::<String>()
}

// Independent Tool_Filter oracle: `*` and `?` may both consume `/`.
fn reference_filter_match(pattern: &str, name: &str) -> bool {
    reference_glob_match(pattern, name, false)
}

// Independent Search_Pattern oracle: one `*` and `?` do not consume `/`,
// while a run of two or more stars may consume it.
fn reference_search_match(pattern: &str, name: &str) -> bool {
    reference_glob_match(pattern, name, true)
}

fn reference_glob_match(pattern: &str, name: &str, search_semantics: bool) -> bool {
    fn visit(
        pattern: &[char],
        name: &[char],
        pattern_index: usize,
        name_index: usize,
        search_semantics: bool,
        memo: &mut [Vec<Option<bool>>],
    ) -> bool {
        if let Some(result) = memo[pattern_index][name_index] {
            return result;
        }

        let result = match pattern.get(pattern_index) {
            None => name_index == name.len(),
            Some('*') => {
                let star_count = pattern[pattern_index..]
                    .iter()
                    .take_while(|character| **character == '*')
                    .count();
                let next_pattern = pattern_index + star_count;
                let may_cross_separator = !search_semantics || star_count >= 2;
                visit(
                    pattern,
                    name,
                    next_pattern,
                    name_index,
                    search_semantics,
                    memo,
                ) || (name_index < name.len()
                    && (may_cross_separator || name[name_index] != '/')
                    && visit(
                        pattern,
                        name,
                        pattern_index,
                        name_index + 1,
                        search_semantics,
                        memo,
                    ))
            }
            Some('?') => {
                name.get(name_index)
                    .is_some_and(|character| !search_semantics || *character != '/')
                    && visit(
                        pattern,
                        name,
                        pattern_index + 1,
                        name_index + 1,
                        search_semantics,
                        memo,
                    )
            }
            Some(expected) => {
                name.get(name_index)
                    .is_some_and(|actual| scalar_eq_ignore_case(*expected, *actual))
                    && visit(
                        pattern,
                        name,
                        pattern_index + 1,
                        name_index + 1,
                        search_semantics,
                        memo,
                    )
            }
        };

        memo[pattern_index][name_index] = Some(result);
        result
    }

    let pattern = pattern.chars().collect::<Vec<_>>();
    let name = name.chars().collect::<Vec<_>>();
    let mut memo = vec![vec![None; name.len() + 1]; pattern.len() + 1];
    visit(&pattern, &name, 0, 0, search_semantics, &mut memo)
}

fn reference_is_allowed(name: &str, filter: &ToolFilterConfig) -> bool {
    !filter
        .disabled_tools
        .iter()
        .any(|pattern| reference_filter_match(pattern, name))
        && (filter.allowed_tools.is_empty()
            || filter
                .allowed_tools
                .iter()
                .any(|pattern| reference_filter_match(pattern, name)))
}

fn safe_token() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9]{1,6}"
}

fn raw_tool() -> impl Strategy<Value = RawTool> {
    (
        safe_token(),
        0_u8..5,
        prop::option::of("[a-zA-Z0-9_./?* -]{0,28}"),
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
        Just("*/*".to_owned()),
        Just("*SECRET*".to_owned()),
        Just("GROUP?/*".to_owned()),
        safe_token(),
        safe_token().prop_map(|token| format!("*{token}*")),
        safe_token().prop_map(|token| format!("{token}?")),
        safe_token().prop_map(|token| format!("root/{token}*")),
    ]
}

fn raw_server() -> impl Strategy<Value = RawServer> {
    (
        safe_token(),
        prop::collection::vec(raw_tool(), 2..=8),
        prop::collection::vec(filter_pattern(), 0..=4),
        prop::collection::vec(filter_pattern(), 0..=4),
        any::<bool>(),
        0_u8..5,
    )
        .prop_map(
            |(suffix, tools, mut allowed, mut disabled, reverse_tools, filter_mode)| {
                match filter_mode {
                    1 => {
                        allowed.push("*".to_owned());
                        disabled.push("*SECRET*".to_owned());
                    }
                    2 => {
                        allowed.extend(["*read*".to_owned(), "plain-*".to_owned()]);
                        disabled.push("*secret*".to_owned());
                    }
                    3 => {
                        allowed.extend(["GROUP?/*".to_owned(), "root*".to_owned()]);
                        disabled.push("*SECRET*".to_owned());
                    }
                    4 => disabled.push("*secret*".to_owned()),
                    _ => {}
                }
                RawServer {
                    suffix,
                    tools,
                    allowed,
                    disabled,
                    reverse_tools,
                }
            },
        )
}

fn arbitrary_search_pattern() -> impl Strategy<Value = String> {
    let atom = prop_oneof![
        5 => safe_token(),
        2 => Just("*".to_owned()),
        2 => Just("?".to_owned()),
        2 => Just("**".to_owned()),
        2 => Just("/".to_owned()),
        1 => Just(".".to_owned()),
        1 => Just("-".to_owned()),
        1 => Just("_".to_owned()),
    ];
    prop::collection::vec(atom, 1..=8).prop_map(|atoms| atoms.concat())
}

fn build_servers(raw_servers: Vec<RawServer>) -> Vec<ServerCase> {
    raw_servers
        .into_iter()
        .enumerate()
        .map(|(server_index, raw)| {
            let name = format!("srv-{server_index}-{}", raw.suffix);
            let mut tools = raw
                .tools
                .into_iter()
                .enumerate()
                .map(|(tool_index, tool)| {
                    let tool_name = match tool.shape {
                        0 => format!("group{server_index}/read_{tool_index}_{}", tool.token),
                        1 => format!("GROUP{server_index}/SECRET_{tool_index}_{}", tool.token),
                        2 => format!("plain-{server_index}-{tool_index}-{}", tool.token),
                        3 => format!("root/{server_index}/deep/{tool_index}-{}", tool.token),
                        _ => format!("x{server_index}{tool_index}{}", tool.token),
                    };
                    ToolInfo {
                        name: tool_name,
                        description: tool.description,
                        input_schema: json!({
                            "type": "object",
                            "properties": {
                                format!("field_{}", tool.schema_tag): {
                                    "type": if tool.schema_tag % 2 == 0 { "string" } else { "integer" },
                                    "tag": tool.schema_tag,
                                }
                            }
                        }),
                    }
                })
                .collect::<Vec<_>>();
            if raw.reverse_tools {
                tools.reverse();
            }

            ServerCase {
                definition: ServerDefinition {
                    name: name.clone(),
                    id: ServerId(format!("{server_index:064x}")),
                    config_hash: ConfigHash([server_index as u8; 32]),
                    transport: TransportConfig::Stdio {
                        command: "in-memory-property-server".to_owned(),
                        args: Vec::new(),
                        env: BTreeMap::new(),
                        cwd: None,
                    },
                    filter: ToolFilterConfig {
                        allowed_tools: raw.allowed,
                        disabled_tools: raw.disabled,
                    },
                },
                tools,
            }
        })
        .collect()
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

fn replace_first_scalar_with_question(name: &str) -> String {
    let mut characters = name.chars();
    characters.next();
    format!("?{}", characters.collect::<String>())
}

fn search_pattern_for_mode(target: &str, mode: u8, fallback: String) -> String {
    let final_segment = target
        .rsplit('/')
        .next()
        .expect("generated name is non-empty");
    match mode {
        0 => target.to_owned(),
        1 => toggle_ascii_case(target),
        2 => "*".to_owned(),
        3 => "**".to_owned(),
        4 => replace_first_scalar_with_question(target),
        5 => target
            .rsplit_once('/')
            .map_or_else(|| "*".to_owned(), |(prefix, _)| format!("{prefix}/*")),
        6 => format!("**/{final_segment}"),
        7 => format!("**/?{}", final_segment.chars().skip(1).collect::<String>()),
        8 => "plain-?-*-*".to_owned(),
        _ => fallback,
    }
}

fn scenario() -> impl Strategy<Value = Scenario> {
    prop::collection::vec(raw_server(), 2..=5).prop_flat_map(|raw_servers| {
        let servers = build_servers(raw_servers);
        let target = servers[0].tools[0].name.clone();
        (Just(servers), 0_u8..10, arbitrary_search_pattern()).prop_map(
            move |(servers, mode, fallback)| Scenario {
                servers,
                pattern: search_pattern_for_mode(&target, mode, fallback),
            },
        )
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

fn definitions(servers: &[ServerCase]) -> BTreeMap<String, ServerDefinition> {
    servers
        .iter()
        .map(|server| (server.definition.name.clone(), server.definition.clone()))
        .collect()
}

fn manager(servers: &[ServerCase], poison_descriptions: Option<&str>) -> Arc<MemoryManager> {
    let tools_by_server = servers
        .iter()
        .map(|server| {
            let mut tools = server.tools.clone();
            if let Some(description) = poison_descriptions {
                for tool in &mut tools {
                    tool.description = Some(format!(
                        "description-only bait containing search pattern {description}"
                    ));
                }
            }
            (server.definition.name.clone(), tools)
        })
        .collect();
    Arc::new(MemoryManager { tools_by_server })
}

fn expected_hits(scenario: &Scenario) -> Vec<(String, String)> {
    let mut hits = scenario
        .servers
        .iter()
        .flat_map(|server| {
            server
                .tools
                .iter()
                .filter(|tool| {
                    reference_is_allowed(&tool.name, &server.definition.filter)
                        && reference_search_match(&scenario.pattern, &tool.name)
                })
                .map(|tool| (server.definition.name.clone(), tool.name.clone()))
        })
        .collect::<Vec<_>>();
    hits.sort();
    hits
}

fn disabled_hits(scenario: &Scenario) -> BTreeSet<(String, String)> {
    scenario
        .servers
        .iter()
        .flat_map(|server| {
            server
                .tools
                .iter()
                .filter(|tool| {
                    server
                        .definition
                        .filter
                        .disabled_tools
                        .iter()
                        .any(|pattern| reference_filter_match(pattern, &tool.name))
                })
                .map(|tool| (server.definition.name.clone(), tool.name.clone()))
        })
        .collect()
}

fn parse_hits(outcome: CommandOutcome) -> Result<(String, Vec<(String, String)>), TestCaseError> {
    let CommandOutcome::HumanText(text) = outcome else {
        return Err(TestCaseError::fail("grep did not return human text"));
    };
    if text == "No matching tools found.\n" {
        return Ok((text, Vec::new()));
    }

    let mut hits = Vec::new();
    for line in text.lines() {
        let Some((server, tool)) = line.split_once(' ') else {
            return Err(TestCaseError::fail(format!(
                "grep line did not contain stable server/tool columns: {line:?}"
            )));
        };
        hits.push((server.to_owned(), tool.to_owned()));
    }
    Ok((text, hits))
}

async fn execute_scenario(
    scenario: &Scenario,
    poison_descriptions: Option<&str>,
) -> Result<(String, Vec<(String, String)>), TestCaseError> {
    let server_definitions = definitions(&scenario.servers);
    let concurrency = NonZeroUsize::new(scenario.servers.len())
        .expect("generated scenarios always contain multiple servers");
    let handler = GrepHandler::new(manager(&scenario.servers, poison_descriptions), concurrency);
    let outcome = handler
        .execute(&context(), &server_definitions, &scenario.pattern, false)
        .await
        .map_err(|error| TestCaseError::fail(format!("grep failed: {error}")))?;
    parse_hits(outcome)
}

async fn check_scenario(scenario: Scenario) -> Result<(), TestCaseError> {
    let expected = expected_hits(&scenario);
    let disabled = disabled_hits(&scenario);
    let (plain_output, actual) = execute_scenario(&scenario, None).await?;
    let (poisoned_output, poisoned_actual) =
        execute_scenario(&scenario, Some(&scenario.pattern)).await?;

    prop_assert_eq!(
        &actual,
        &expected,
        "grep was not exactly authorization followed by name search; pattern={:?}",
        scenario.pattern
    );
    prop_assert_eq!(
        actual.iter().cloned().collect::<BTreeSet<_>>(),
        expected.iter().cloned().collect::<BTreeSet<_>>(),
        "grep hit set had omissions or extras"
    );
    prop_assert!(
        actual.iter().all(|hit| !disabled.contains(hit)),
        "a disabled tool appeared in grep output"
    );
    prop_assert!(
        actual.windows(2).all(|pair| pair[0] <= pair[1]),
        "grep output was not stably sorted by (server, tool)"
    );
    prop_assert_eq!(
        poisoned_actual,
        expected,
        "descriptions affected name-only matching"
    );
    prop_assert_eq!(
        poisoned_output,
        plain_output,
        "changing only descriptions changed grep output with descriptions disabled"
    );

    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 192,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 12: grep 是过滤与搜索的精确组合
    // **Validates: Requirements 1.6**
    #[test]
    fn property_12_grep_is_exact_filter_then_search_composition(
        scenario in scenario()
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("property runtime");
        runtime.block_on(check_scenario(scenario))?;
    }
}
