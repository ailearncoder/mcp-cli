use std::ffi::OsString;

use mcp_cli::{CommandSpec, parse_cli};
use proptest::prelude::*;
use serde_json::{Map, Number, Value};

fn name_scalar() -> impl Strategy<Value = char> {
    prop::sample::select(
        "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_.:@+~= 工具服务🦀éΣЖ"
            .chars()
            .collect::<Vec<_>>(),
    )
}

fn target_name() -> impl Strategy<Value = String> {
    prop::collection::vec(name_scalar(), 1..25)
        .prop_filter(
            "target names cannot begin with an option prefix",
            |characters| {
                characters
                    .first()
                    .is_some_and(|character| *character != '-')
            },
        )
        .prop_map(|characters| characters.into_iter().collect())
}

fn json_string() -> impl Strategy<Value = String> {
    prop::collection::vec(any::<char>(), 0..32)
        .prop_map(|characters| characters.into_iter().collect())
}

fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|number| Value::Number(Number::from(number))),
        json_string().prop_map(Value::String),
    ];

    leaf.prop_recursive(4, 64, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::btree_map(json_string(), inner, 0..6)
                .prop_map(|entries| Value::Object(entries.into_iter().collect())),
        ]
    })
}

fn json_object_text() -> impl Strategy<Value = String> {
    prop::collection::btree_map(json_string(), json_value(), 0..8).prop_map(|entries| {
        serde_json::to_string(&Value::Object(Map::from_iter(entries)))
            .expect("generated JSON objects are serializable")
    })
}

fn parse(arguments: Vec<String>) -> CommandSpec {
    parse_cli(arguments.into_iter().map(OsString::from))
        .expect("generated command syntax is valid")
        .command
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 1: 目标语法等价
    // **Validates: Requirements 1.5, 1.8**
    #[test]
    fn property_01_target_syntax_is_equivalent(
        server in target_name(),
        tool in target_name(),
        inline_json in json_object_text(),
    ) {
        prop_assert!(!server.is_empty());
        prop_assert!(!tool.is_empty());
        prop_assert!(!server.contains('/'));
        prop_assert!(!tool.contains('/'));
        prop_assert!(serde_json::from_str::<Map<String, Value>>(&inline_json).is_ok());

        let expected_info = CommandSpec::Info {
            server: server.clone(),
            tool: Some(tool.clone()),
            with_descriptions: false,
        };
        let split_info = parse(vec!["info".into(), server.clone(), tool.clone()]);
        let slash_info = parse(vec!["info".into(), format!("{server}/{tool}")]);

        prop_assert_eq!(&split_info, &expected_info);
        prop_assert_eq!(&slash_info, &expected_info);
        prop_assert_eq!(split_info, slash_info);

        let expected_call = CommandSpec::Call {
            server: server.clone(),
            tool: tool.clone(),
            inline_json: Some(inline_json.clone()),
        };
        let split_call = parse(vec![
            "call".into(),
            server.clone(),
            tool.clone(),
            inline_json.clone(),
        ]);
        let slash_call = parse(vec![
            "call".into(),
            format!("{server}/{tool}"),
            inline_json.clone(),
        ]);

        prop_assert_eq!(&split_call, &expected_call);
        prop_assert_eq!(&slash_call, &expected_call);
        prop_assert_eq!(split_call, slash_call);

        match expected_call {
            CommandSpec::Call {
                server: actual_server,
                tool: actual_tool,
                inline_json: Some(actual_json),
            } => {
                prop_assert_eq!(actual_server, server);
                prop_assert_eq!(actual_tool, tool);
                prop_assert_eq!(actual_json, inline_json);
            }
            _ => prop_assert!(false, "independent call oracle has the wrong variant"),
        }
    }
}
