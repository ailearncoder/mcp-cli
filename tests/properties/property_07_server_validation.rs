use std::collections::BTreeMap;

use mcp_cli::{
    ErrorKind,
    config::{TransportConfig, validate_mcp_servers},
};
use proptest::prelude::*;
use serde_json::{Map, Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExpectedTransport {
    Stdio,
    Http,
}

#[derive(Clone, Debug)]
enum Mutation {
    CommandWrongType,
    CommandEmpty,
    UrlWrongType,
    UrlInvalidScheme,
    ArrayWrongType { field: &'static str },
    ArrayElementWrongType { field: &'static str, index: usize },
    MapWrongType { field: &'static str },
    MapValueWrongType { field: &'static str, key: String },
    CwdWrongType,
}

impl Mutation {
    fn apply(&self, config: &mut Map<String, Value>) {
        match self {
            Self::CommandWrongType => {
                config.insert("command".to_owned(), Value::Bool(false));
            }
            Self::CommandEmpty => {
                config.insert("command".to_owned(), Value::String(String::new()));
            }
            Self::UrlWrongType => {
                config.insert("url".to_owned(), json!(17));
            }
            Self::UrlInvalidScheme => {
                config.insert(
                    "url".to_owned(),
                    Value::String("ftp://invalid.example.test/mcp".to_owned()),
                );
            }
            Self::ArrayWrongType { field } => {
                config.insert((*field).to_owned(), json!({"not": "an array"}));
            }
            Self::ArrayElementWrongType { field, index } => {
                config
                    .get_mut(*field)
                    .and_then(Value::as_array_mut)
                    .expect("generated array field")[*index] = json!(17);
            }
            Self::MapWrongType { field } => {
                config.insert((*field).to_owned(), json!(["not", "an", "object"]));
            }
            Self::MapValueWrongType { field, key } => {
                config
                    .get_mut(*field)
                    .and_then(Value::as_object_mut)
                    .expect("generated map field")
                    .insert(key.clone(), json!(17));
            }
            Self::CwdWrongType => {
                config.insert("cwd".to_owned(), Value::Null);
            }
        }
    }

    fn relative_path(&self) -> String {
        match self {
            Self::CommandWrongType | Self::CommandEmpty => ".command".to_owned(),
            Self::UrlWrongType | Self::UrlInvalidScheme => ".url".to_owned(),
            Self::ArrayWrongType { field } | Self::MapWrongType { field } => {
                format!(".{field}")
            }
            Self::ArrayElementWrongType { field, index } => format!(".{field}[{index}]"),
            Self::MapValueWrongType { field, key } => format!(
                ".{field}[{}]",
                serde_json::to_string(key).expect("string encoding cannot fail")
            ),
            Self::CwdWrongType => ".cwd".to_owned(),
        }
    }

    fn reason(&self) -> &'static str {
        match self {
            Self::CommandWrongType | Self::UrlWrongType | Self::CwdWrongType => "must be a string",
            Self::CommandEmpty => "must be a non-empty string",
            Self::UrlInvalidScheme => "must be a valid HTTP or HTTPS URL",
            Self::ArrayWrongType { .. } => "must be an array of strings",
            Self::ArrayElementWrongType { .. } | Self::MapValueWrongType { .. } => {
                "must be a string"
            }
            Self::MapWrongType { .. } => "must be an object with string values",
        }
    }
}

#[derive(Clone, Debug)]
struct ValidationCase {
    server: String,
    config: Map<String, Value>,
    expected_transport: ExpectedTransport,
    mutation: Mutation,
}

fn identifier() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[A-Za-z][A-Za-z0-9_-]{0,15}")
        .expect("identifier regex is valid")
}

fn text() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[A-Za-z0-9_ ./:-]{0,24}").expect("text regex is valid")
}

fn string_array() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(text(), 1..5)
}

fn string_map() -> impl Strategy<Value = BTreeMap<String, String>> {
    prop::collection::btree_map(identifier(), text(), 1..5)
}

fn stdio_case() -> impl Strategy<Value = ValidationCase> {
    (
        identifier(),
        identifier(),
        string_array(),
        string_map(),
        identifier(),
        string_array(),
        string_array(),
        0_u8..11,
    )
        .prop_map(
            |(server, command, args, env, cwd_suffix, allowed, disabled, mutation_index)| {
                let env_key = env.keys().next().expect("non-empty generated env").clone();
                let mutation = match mutation_index {
                    0 => Mutation::CommandWrongType,
                    1 => Mutation::CommandEmpty,
                    2 => Mutation::ArrayWrongType { field: "args" },
                    3 => Mutation::ArrayElementWrongType {
                        field: "args",
                        index: 0,
                    },
                    4 => Mutation::MapWrongType { field: "env" },
                    5 => Mutation::MapValueWrongType {
                        field: "env",
                        key: env_key,
                    },
                    6 => Mutation::CwdWrongType,
                    7 => Mutation::ArrayWrongType {
                        field: "allowedTools",
                    },
                    8 => Mutation::ArrayElementWrongType {
                        field: "allowedTools",
                        index: 0,
                    },
                    9 => Mutation::ArrayWrongType {
                        field: "disabledTools",
                    },
                    _ => Mutation::ArrayElementWrongType {
                        field: "disabledTools",
                        index: 0,
                    },
                };
                let config = json!({
                    "command": command,
                    "args": args,
                    "env": env,
                    "cwd": format!("/tmp/{cwd_suffix}"),
                    "allowedTools": allowed,
                    "disabledTools": disabled,
                })
                .as_object()
                .expect("object literal")
                .clone();

                ValidationCase {
                    server,
                    config,
                    expected_transport: ExpectedTransport::Stdio,
                    mutation,
                }
            },
        )
}

fn http_case() -> impl Strategy<Value = ValidationCase> {
    (
        identifier(),
        prop::sample::select(vec!["http", "https"]),
        identifier(),
        identifier(),
        string_map(),
        string_array(),
        string_array(),
        0_u8..8,
    )
        .prop_map(
            |(server, scheme, host, path, headers, allowed, disabled, mutation_index)| {
                let header_key = headers
                    .keys()
                    .next()
                    .expect("non-empty generated headers")
                    .clone();
                let mutation = match mutation_index {
                    0 => Mutation::UrlWrongType,
                    1 => Mutation::UrlInvalidScheme,
                    2 => Mutation::MapWrongType { field: "headers" },
                    3 => Mutation::MapValueWrongType {
                        field: "headers",
                        key: header_key,
                    },
                    4 => Mutation::ArrayWrongType {
                        field: "allowedTools",
                    },
                    5 => Mutation::ArrayElementWrongType {
                        field: "allowedTools",
                        index: 0,
                    },
                    6 => Mutation::ArrayWrongType {
                        field: "disabledTools",
                    },
                    _ => Mutation::ArrayElementWrongType {
                        field: "disabledTools",
                        index: 0,
                    },
                };
                let config = json!({
                    "url": format!("{scheme}://{host}.example.test/{path}"),
                    "headers": headers,
                    "allowedTools": allowed,
                    "disabledTools": disabled,
                })
                .as_object()
                .expect("object literal")
                .clone();

                ValidationCase {
                    server,
                    config,
                    expected_transport: ExpectedTransport::Http,
                    mutation,
                }
            },
        )
}

fn validation_case() -> impl Strategy<Value = ValidationCase> {
    prop_oneof![1 => stdio_case(), 1 => http_case()]
}

fn singleton_servers(case: &ValidationCase) -> Map<String, Value> {
    Map::from_iter([(case.server.clone(), Value::Object(case.config.clone()))])
}

fn expected_details(case: &ValidationCase) -> String {
    let encoded_server = serde_json::to_string(&case.server).expect("string encoding cannot fail");
    format!(
        "Field mcpServers[{encoded_server}]{}: {}",
        case.mutation.relative_path(),
        case.mutation.reason(),
    )
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 7: 服务器配置分类与字段错误定位
    // **Validates: Requirements 3.5, 3.6, 3.9**
    #[test]
    fn property_07_server_configs_have_unique_transports_and_precise_field_errors(
        case in validation_case(),
    ) {
        let has_command = case.config.contains_key("command");
        let has_url = case.config.contains_key("url");
        prop_assert_ne!(has_command, has_url, "valid oracle input has exactly one selector");

        let valid_servers = singleton_servers(&case);
        let validated = validate_mcp_servers(&valid_servers)
            .expect("the independently generated baseline must be valid");
        prop_assert_eq!(validated.len(), 1);
        let definition = validated
            .get(&case.server)
            .expect("validator preserves the generated server name");
        prop_assert_eq!(&definition.name, &case.server);

        match (&case.expected_transport, &definition.transport) {
            (ExpectedTransport::Stdio, TransportConfig::Stdio { .. }) => {
                prop_assert!(has_command);
                prop_assert!(!has_url);
            }
            (ExpectedTransport::Http, TransportConfig::Http { .. }) => {
                prop_assert!(!has_command);
                prop_assert!(has_url);
            }
            (expected, actual) => prop_assert!(
                false,
                "independent transport oracle expected {expected:?}, validator returned {actual:?}",
            ),
        }

        let mut invalid_case = case.clone();
        invalid_case.mutation.apply(&mut invalid_case.config);
        let error = validate_mcp_servers(&singleton_servers(&invalid_case))
            .expect_err("a single invalid field mutation must be rejected");

        prop_assert_eq!(error.kind, ErrorKind::InvalidServerConfig);
        prop_assert_eq!(error.machine_kind(), "INVALID_SERVER_CONFIG");
        prop_assert!(error.message.contains(&case.server));
        let expected = expected_details(&case);
        prop_assert_eq!(error.details.as_deref(), Some(expected.as_str()));
    }
}
