use mcp_cli::{
    ExitCode, PerServer, SearchHit, ServerSnapshot, ToolInfo, TransportSummary, format_grep_hits,
    format_server_info, format_server_list,
};
use proptest::prelude::*;
use serde_json::{Map, Value, json};

#[derive(Clone, Debug)]
struct ParameterFixture {
    name: String,
    kind: String,
    required: bool,
    description: Option<String>,
}

#[derive(Clone, Debug)]
struct ToolFixture {
    info: ToolInfo,
    parameters: Vec<ParameterFixture>,
}

#[derive(Clone, Debug)]
struct ServerFixture {
    name: String,
    tools: Vec<ToolFixture>,
}

impl ServerFixture {
    fn snapshot(&self) -> ServerSnapshot {
        ServerSnapshot {
            server: self.name.clone(),
            transport_summary: TransportSummary::Stdio {
                command: format!("run-{}", self.name),
            },
            instructions: None,
            tools: self.tools.iter().map(|tool| tool.info.clone()).collect(),
        }
    }
}

#[derive(Clone, Debug)]
struct Fixture {
    servers: Vec<ServerFixture>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ListToolView {
    name: String,
    description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ListServerView {
    name: String,
    tools: Vec<ListToolView>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParameterView {
    name: String,
    kind: String,
    required: bool,
    description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InfoToolView {
    name: String,
    description: Option<String>,
    parameters: Vec<ParameterView>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InfoServerView {
    name: String,
    tools: Vec<InfoToolView>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GrepHitView {
    server: String,
    tool: String,
    description: Option<String>,
}

#[derive(Debug)]
struct SuccessfulRender {
    stdout: String,
    exit_code: ExitCode,
}

fn successful(stdout: String) -> SuccessfulRender {
    SuccessfulRender {
        stdout,
        exit_code: ExitCode::Success,
    }
}

fn server_name() -> impl Strategy<Value = String> {
    proptest::string::string_regex("s[a-z0-9]{1,6}").expect("server regex is valid")
}

fn tool_name() -> impl Strategy<Value = String> {
    proptest::string::string_regex("t[a-z0-9_]{1,8}").expect("tool regex is valid")
}

fn parameter_name() -> impl Strategy<Value = String> {
    proptest::string::string_regex("p[a-z0-9_]{1,7}").expect("parameter regex is valid")
}

fn description_token() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[a-zA-Z0-9_]{1,12}").expect("description regex is valid")
}

fn parameter_data() -> impl Strategy<Value = (String, bool, String)> {
    (
        prop::sample::select(vec![
            "string", "number", "integer", "boolean", "array", "object",
        ]),
        any::<bool>(),
        description_token(),
    )
        .prop_map(|(kind, required, token)| (kind.to_owned(), required, token))
}

fn fixture() -> impl Strategy<Value = Fixture> {
    let parameters = prop::collection::btree_map(parameter_name(), parameter_data(), 2..5);
    let tools = prop::collection::btree_map(tool_name(), (description_token(), parameters), 2..5);

    prop::collection::btree_map(server_name(), tools, 2..5).prop_map(|servers| {
        let servers = servers
            .into_iter()
            .rev()
            .map(|(server_name, tools)| {
                let tools = tools
                    .into_iter()
                    .rev()
                    .enumerate()
                    .map(|(tool_index, (tool_name, (tool_token, parameters)))| {
                        let mut properties = Map::new();
                        let mut required = Vec::new();
                        let parameters = parameters
                            .into_iter()
                            .rev()
                            .enumerate()
                            .map(|(parameter_index, (name, (kind, is_required, token)))| {
                                let description = (parameter_index % 2 == 0)
                                    .then(|| format!("parameter-desc-{token}"));
                                let mut schema = Map::new();
                                schema.insert("type".to_owned(), Value::String(kind.clone()));
                                if let Some(description) = &description {
                                    schema.insert(
                                        "description".to_owned(),
                                        Value::String(description.clone()),
                                    );
                                }
                                properties.insert(name.clone(), Value::Object(schema));
                                if is_required {
                                    required.push(Value::String(name.clone()));
                                }
                                ParameterFixture {
                                    name,
                                    kind,
                                    required: is_required,
                                    description,
                                }
                            })
                            .collect::<Vec<_>>();
                        let description =
                            (tool_index % 2 == 0).then(|| format!("tool-desc-{tool_token}"));
                        ToolFixture {
                            info: ToolInfo {
                                name: tool_name,
                                description,
                                input_schema: json!({
                                    "type": "object",
                                    "properties": properties,
                                    "required": required,
                                }),
                            },
                            parameters,
                        }
                    })
                    .collect();
                ServerFixture {
                    name: server_name,
                    tools,
                }
            })
            .collect();
        Fixture { servers }
    })
}

fn split_optional_description(text: &str) -> (&str, Option<&str>) {
    text.split_once(" - ")
        .map_or((text, None), |(core, description)| {
            (core, Some(description))
        })
}

fn parse_list(output: &str) -> Result<Vec<ListServerView>, String> {
    let body = output
        .strip_suffix('\n')
        .ok_or_else(|| "list output lacks one trailing newline".to_owned())?;
    let lines = body.split('\n').collect::<Vec<_>>();
    let mut servers = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        if lines[index].is_empty() {
            index += 1;
            continue;
        }
        if lines[index].starts_with(' ') {
            return Err(format!("expected server line, got {:?}", lines[index]));
        }
        let name = lines[index].to_owned();
        index += 1;
        let mut tools = Vec::new();
        while index < lines.len() {
            let Some(entry) = lines[index].strip_prefix("  • ") else {
                break;
            };
            let (name, description) = split_optional_description(entry);
            tools.push(ListToolView {
                name: name.to_owned(),
                description: description.map(str::to_owned),
            });
            index += 1;
        }
        servers.push(ListServerView { name, tools });
    }
    Ok(servers)
}

fn parse_parameter(line: &str) -> Result<ParameterView, String> {
    let entry = line
        .strip_prefix("      • ")
        .ok_or_else(|| format!("invalid parameter line {line:?}"))?;
    let (core, description) = split_optional_description(entry);
    let (name, attributes) = core
        .split_once(" (")
        .ok_or_else(|| format!("parameter lacks attributes: {line:?}"))?;
    let attributes = attributes
        .strip_suffix(')')
        .ok_or_else(|| format!("parameter attributes lack closing parenthesis: {line:?}"))?;
    let (kind, necessity) = attributes
        .split_once(", ")
        .ok_or_else(|| format!("parameter attributes are malformed: {line:?}"))?;
    let required = match necessity {
        "required" => true,
        "optional" => false,
        other => return Err(format!("unknown necessity {other:?}")),
    };
    Ok(ParameterView {
        name: name.to_owned(),
        kind: kind.to_owned(),
        required,
        description: description.map(str::to_owned),
    })
}

fn parse_info(output: &str) -> Result<InfoServerView, String> {
    let body = output
        .strip_suffix('\n')
        .ok_or_else(|| "info output lacks one trailing newline".to_owned())?;
    let lines = body.split('\n').collect::<Vec<_>>();
    let name = lines
        .first()
        .and_then(|line| line.strip_prefix("Server: "))
        .ok_or_else(|| "info output lacks server header".to_owned())?
        .to_owned();
    let tools_header = lines
        .iter()
        .position(|line| line.starts_with("Tools (") && line.ends_with("):"))
        .ok_or_else(|| "info output lacks tools header".to_owned())?;
    let declared_count = lines[tools_header]
        .strip_prefix("Tools (")
        .and_then(|value| value.strip_suffix("):"))
        .ok_or_else(|| "invalid tools header".to_owned())?
        .parse::<usize>()
        .map_err(|error| format!("invalid tools count: {error}"))?;

    let mut tools = Vec::new();
    let mut index = tools_header + 1;
    while index < lines.len() {
        let tool_name = lines[index]
            .strip_prefix("  ")
            .filter(|name| !name.starts_with(' '))
            .ok_or_else(|| format!("invalid tool line {:?}", lines[index]))?
            .to_owned();
        index += 1;

        let mut description = None;
        if index < lines.len()
            && lines[index].starts_with("    ")
            && lines[index] != "    Parameters:"
        {
            description = Some(lines[index][4..].to_owned());
            index += 1;
        }

        let mut parameters = Vec::new();
        if index < lines.len() && lines[index] == "    Parameters:" {
            index += 1;
            while index < lines.len() && lines[index].starts_with("      • ") {
                parameters.push(parse_parameter(lines[index])?);
                index += 1;
            }
        }
        tools.push(InfoToolView {
            name: tool_name,
            description,
            parameters,
        });
    }
    if tools.len() != declared_count {
        return Err(format!(
            "declared {declared_count} tools but parsed {}",
            tools.len()
        ));
    }
    Ok(InfoServerView { name, tools })
}

fn parse_grep(output: &str) -> Result<Vec<GrepHitView>, String> {
    let body = output
        .strip_suffix('\n')
        .ok_or_else(|| "grep output lacks one trailing newline".to_owned())?;
    body.split('\n')
        .map(|line| {
            let (server, entry) = line
                .split_once(' ')
                .ok_or_else(|| format!("invalid grep line {line:?}"))?;
            let (tool, description) = split_optional_description(entry);
            Ok(GrepHitView {
                server: server.to_owned(),
                tool: tool.to_owned(),
                description: description.map(str::to_owned),
            })
        })
        .collect()
}

fn expected_list(fixture: &Fixture, with_descriptions: bool) -> Vec<ListServerView> {
    let mut servers = fixture
        .servers
        .iter()
        .map(|server| {
            let mut tools = server
                .tools
                .iter()
                .map(|tool| ListToolView {
                    name: tool.info.name.clone(),
                    description: with_descriptions
                        .then(|| tool.info.description.clone())
                        .flatten(),
                })
                .collect::<Vec<_>>();
            tools.sort_by(|left, right| left.name.cmp(&right.name));
            ListServerView {
                name: server.name.clone(),
                tools,
            }
        })
        .collect::<Vec<_>>();
    servers.sort_by(|left, right| left.name.cmp(&right.name));
    servers
}

fn expected_info(server: &ServerFixture, with_descriptions: bool) -> InfoServerView {
    let mut tools = server
        .tools
        .iter()
        .map(|tool| {
            let mut parameters = tool
                .parameters
                .iter()
                .map(|parameter| ParameterView {
                    name: parameter.name.clone(),
                    kind: parameter.kind.clone(),
                    required: parameter.required,
                    description: with_descriptions
                        .then(|| parameter.description.clone())
                        .flatten(),
                })
                .collect::<Vec<_>>();
            parameters.sort_by(|left, right| left.name.cmp(&right.name));
            InfoToolView {
                name: tool.info.name.clone(),
                description: with_descriptions
                    .then(|| tool.info.description.clone())
                    .flatten(),
                parameters,
            }
        })
        .collect::<Vec<_>>();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    InfoServerView {
        name: server.name.clone(),
        tools,
    }
}

fn grep_hits(fixture: &Fixture) -> Vec<SearchHit> {
    fixture
        .servers
        .iter()
        .flat_map(|server| {
            server.tools.iter().map(|tool| SearchHit {
                server: server.name.clone(),
                tool: tool.info.clone(),
            })
        })
        .collect()
}

fn expected_grep(fixture: &Fixture, with_descriptions: bool) -> Vec<GrepHitView> {
    let mut hits = fixture
        .servers
        .iter()
        .flat_map(|server| {
            server.tools.iter().map(|tool| GrepHitView {
                server: server.name.clone(),
                tool: tool.info.name.clone(),
                description: with_descriptions
                    .then(|| tool.info.description.clone())
                    .flatten(),
            })
        })
        .collect::<Vec<_>>();
    hits.sort_by(|left, right| {
        left.server
            .cmp(&right.server)
            .then_with(|| left.tool.cmp(&right.tool))
    });
    hits
}

fn list_core(view: &[ListServerView]) -> Vec<(String, Vec<String>)> {
    view.iter()
        .map(|server| {
            (
                server.name.clone(),
                server.tools.iter().map(|tool| tool.name.clone()).collect(),
            )
        })
        .collect()
}

type ParameterCore = (String, String, bool);
type ToolCore = (String, Vec<ParameterCore>);
type InfoCore = (String, Vec<ToolCore>);

fn info_core(view: &InfoServerView) -> InfoCore {
    (
        view.name.clone(),
        view.tools
            .iter()
            .map(|tool| {
                (
                    tool.name.clone(),
                    tool.parameters
                        .iter()
                        .map(|parameter| {
                            (
                                parameter.name.clone(),
                                parameter.kind.clone(),
                                parameter.required,
                            )
                        })
                        .collect(),
                )
            })
            .collect(),
    )
}

fn grep_core(view: &[GrepHitView]) -> Vec<(String, String)> {
    view.iter()
        .map(|hit| (hit.server.clone(), hit.tool.clone()))
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 3: 描述开关只控制描述
    // **Validates: Requirements 1.11**
    #[test]
    fn property_03_description_switch_only_controls_descriptions(fixture in fixture()) {
        let snapshots = fixture.servers.iter().map(ServerFixture::snapshot).collect::<Vec<_>>();
        let list_results = snapshots
            .iter()
            .map(|snapshot| PerServer::Success {
                server: snapshot.server.clone(),
                value: snapshot.clone(),
            })
            .collect::<Vec<_>>();
        let hits = grep_hits(&fixture);

        let list_without = successful(format_server_list(&list_results, false));
        let list_with = successful(format_server_list(&list_results, true));
        prop_assert_eq!(list_without.exit_code, ExitCode::Success);
        prop_assert_eq!(list_with.exit_code, list_without.exit_code);
        let parsed_list_without = parse_list(&list_without.stdout).expect("parse list without descriptions");
        let parsed_list_with = parse_list(&list_with.stdout).expect("parse list with descriptions");
        prop_assert_eq!(&parsed_list_without, &expected_list(&fixture, false));
        prop_assert_eq!(&parsed_list_with, &expected_list(&fixture, true));
        prop_assert_eq!(list_core(&parsed_list_without), list_core(&parsed_list_with));

        for (server, snapshot) in fixture.servers.iter().zip(snapshots.iter()) {
            let info_without = successful(format_server_info(snapshot, false));
            let info_with = successful(format_server_info(snapshot, true));
            prop_assert_eq!(info_without.exit_code, ExitCode::Success);
            prop_assert_eq!(info_with.exit_code, info_without.exit_code);
            let parsed_info_without = parse_info(&info_without.stdout).expect("parse info without descriptions");
            let parsed_info_with = parse_info(&info_with.stdout).expect("parse info with descriptions");
            prop_assert_eq!(&parsed_info_without, &expected_info(server, false));
            prop_assert_eq!(&parsed_info_with, &expected_info(server, true));
            prop_assert_eq!(info_core(&parsed_info_without), info_core(&parsed_info_with));
        }

        let grep_without = successful(format_grep_hits(&hits, false));
        let grep_with = successful(format_grep_hits(&hits, true));
        prop_assert_eq!(grep_without.exit_code, ExitCode::Success);
        prop_assert_eq!(grep_with.exit_code, grep_without.exit_code);
        let parsed_grep_without = parse_grep(&grep_without.stdout).expect("parse grep without descriptions");
        let parsed_grep_with = parse_grep(&grep_with.stdout).expect("parse grep with descriptions");
        prop_assert_eq!(&parsed_grep_without, &expected_grep(&fixture, false));
        prop_assert_eq!(&parsed_grep_with, &expected_grep(&fixture, true));
        prop_assert_eq!(grep_core(&parsed_grep_without), grep_core(&parsed_grep_with));
    }
}
