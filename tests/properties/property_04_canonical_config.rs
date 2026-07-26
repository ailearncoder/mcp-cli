use std::collections::BTreeMap;

use mcp_cli::config::{ConfigHash, canonical_json, config_hash, validate_mcp_servers};
use proptest::prelude::*;
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug)]
enum ObjectPermutation {
    Forward,
    Reverse,
    Rotated,
}

fn identifier() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[a-z][a-z0-9_-]{0,11}").expect("identifier regex is valid")
}

fn text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(
            "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 _/:-工具🦀"
                .chars()
                .collect::<Vec<_>>(),
        ),
        0..25,
    )
    .prop_map(|characters| characters.into_iter().collect())
}

fn nested_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        1 => Just(Value::Null),
        1 => any::<bool>().prop_map(Value::Bool),
        2 => any::<i32>().prop_map(|value| Value::Number(Number::from(value))),
        3 => text().prop_map(Value::String),
    ];

    leaf.prop_recursive(4, 48, 4, |inner| {
        prop_oneof![
            1 => prop::collection::vec(inner.clone(), 0..5).prop_map(Value::Array),
            1 => prop::collection::btree_map(identifier(), inner, 0..5)
                .prop_map(|entries| Value::Object(entries.into_iter().collect())),
        ]
    })
}

fn string_map() -> impl Strategy<Value = BTreeMap<String, String>> {
    prop::collection::btree_map(identifier(), text(), 0..5)
}

fn string_array() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(text(), 0..6)
}

fn order_probe() -> impl Strategy<Value = Value> {
    (text(), any::<i32>()).prop_map(|(label, number)| {
        Value::Array(vec![
            Value::String(format!("left:{label}")),
            Value::Object(Map::from_iter([
                ("kind".to_owned(), Value::String("right".to_owned())),
                ("number".to_owned(), Value::Number(Number::from(number))),
            ])),
        ])
    })
}

fn server_config() -> impl Strategy<Value = Value> {
    let stdio = (
        identifier(),
        string_array(),
        string_map(),
        string_array(),
        string_array(),
        nested_json(),
        order_probe(),
    )
        .prop_map(|(command, args, env, allowed, disabled, metadata, probe)| {
            Value::Object(Map::from_iter([
                ("command".to_owned(), Value::String(command)),
                (
                    "args".to_owned(),
                    Value::Array(args.into_iter().map(Value::String).collect()),
                ),
                (
                    "env".to_owned(),
                    Value::Object(
                        env.into_iter()
                            .map(|(key, value)| (key, Value::String(value)))
                            .collect(),
                    ),
                ),
                (
                    "allowedTools".to_owned(),
                    Value::Array(allowed.into_iter().map(Value::String).collect()),
                ),
                (
                    "disabledTools".to_owned(),
                    Value::Array(disabled.into_iter().map(Value::String).collect()),
                ),
                ("metadata".to_owned(), metadata),
                ("_orderProbe".to_owned(), probe),
            ]))
        });

    let http = (
        prop::sample::select(vec!["http", "https"]),
        identifier(),
        identifier(),
        string_map(),
        string_array(),
        string_array(),
        nested_json(),
        order_probe(),
    )
        .prop_map(
            |(scheme, host, path, headers, allowed, disabled, metadata, probe)| {
                Value::Object(Map::from_iter([
                    (
                        "url".to_owned(),
                        Value::String(format!("{scheme}://{host}.example.test/{path}")),
                    ),
                    (
                        "headers".to_owned(),
                        Value::Object(
                            headers
                                .into_iter()
                                .map(|(key, value)| (key, Value::String(value)))
                                .collect(),
                        ),
                    ),
                    (
                        "allowedTools".to_owned(),
                        Value::Array(allowed.into_iter().map(Value::String).collect()),
                    ),
                    (
                        "disabledTools".to_owned(),
                        Value::Array(disabled.into_iter().map(Value::String).collect()),
                    ),
                    ("metadata".to_owned(), metadata),
                    ("_orderProbe".to_owned(), probe),
                ]))
            },
        );

    prop_oneof![1 => stdio, 1 => http]
}

fn configuration() -> impl Strategy<Value = Value> {
    prop::collection::btree_map(identifier(), server_config(), 1..6).prop_map(|servers| {
        Value::Object(Map::from_iter([(
            "mcpServers".to_owned(),
            Value::Object(servers.into_iter().collect()),
        )]))
    })
}

fn ordered_keys(object: &Map<String, Value>, mode: ObjectPermutation, depth: usize) -> Vec<&str> {
    let mut keys = object.keys().map(String::as_str).collect::<Vec<_>>();
    keys.sort_unstable();

    match mode {
        ObjectPermutation::Forward => {}
        ObjectPermutation::Reverse => keys.reverse(),
        ObjectPermutation::Rotated if keys.len() > 1 => {
            let offset = (depth + 1) % keys.len();
            keys.rotate_left(offset);
        }
        ObjectPermutation::Rotated => {}
    }
    keys
}

fn write_permuted_json(value: &Value, mode: ObjectPermutation, depth: usize, output: &mut String) {
    match value {
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_permuted_json(value, mode, depth + 1, output);
            }
            output.push(']');
        }
        Value::Object(object) => {
            output.push('{');
            for (index, key) in ordered_keys(object, mode, depth).into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(
                    &serde_json::to_string(key).expect("serializing an object key cannot fail"),
                );
                output.push(':');
                write_permuted_json(&object[key], mode, depth + 1, output);
            }
            output.push('}');
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => output.push_str(
            &serde_json::to_string(value).expect("serializing an in-memory scalar cannot fail"),
        ),
    }
}

fn permuted_json(value: &Value, mode: ObjectPermutation) -> Vec<u8> {
    let mut output = String::new();
    write_permuted_json(value, mode, 0, &mut output);
    output.into_bytes()
}

fn write_reference_canonical(value: &Value, output: &mut String) {
    match value {
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_reference_canonical(value, output);
            }
            output.push(']');
        }
        Value::Object(object) => {
            output.push('{');
            let sorted = object.iter().collect::<BTreeMap<_, _>>();
            for (index, (key, value)) in sorted.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(
                    &serde_json::to_string(key).expect("serializing an object key cannot fail"),
                );
                output.push(':');
                write_reference_canonical(value, output);
            }
            output.push('}');
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => output.push_str(
            &serde_json::to_string(value).expect("serializing an in-memory scalar cannot fail"),
        ),
    }
}

fn reference_canonical(value: &Value) -> Vec<u8> {
    let mut output = String::new();
    write_reference_canonical(value, &mut output);
    output.into_bytes()
}

fn reference_hash(value: &Value) -> ConfigHash {
    ConfigHash(Sha256::digest(reference_canonical(value)).into())
}

fn server_values(document: &Value) -> &Map<String, Value> {
    document["mcpServers"]
        .as_object()
        .expect("generated mcpServers is an object")
}

fn reverse_first_order_probe(document: &Value) -> Value {
    let mut changed = document.clone();
    let first_server = changed["mcpServers"]
        .as_object_mut()
        .expect("generated mcpServers is an object")
        .values_mut()
        .next()
        .expect("at least one server is generated");
    first_server["_orderProbe"]
        .as_array_mut()
        .expect("generated order probe is an array")
        .reverse();
    changed
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 4: 配置规范化保留语义与顺序
    // **Validates: Requirements 2.8, 2.10**
    #[test]
    fn property_04_canonical_config_preserves_semantics_and_order(
        original in configuration(),
    ) {
        let expected_canonical = reference_canonical(&original);
        let expected_server_names = server_values(&original).keys().cloned().collect::<Vec<_>>();
        let expected_hashes = server_values(&original)
            .iter()
            .map(|(name, config)| (name.clone(), reference_hash(config)))
            .collect::<BTreeMap<_, _>>();
        let original_probe = server_values(&original)
            .values()
            .next()
            .expect("at least one generated server")["_orderProbe"]
            .clone();

        let raw_permutations = [
            permuted_json(&original, ObjectPermutation::Forward),
            permuted_json(&original, ObjectPermutation::Reverse),
            permuted_json(&original, ObjectPermutation::Rotated),
        ];
        prop_assert_ne!(
            &raw_permutations[0],
            &raw_permutations[1],
            "forward and reverse writers must exercise distinct object-key orders",
        );

        let mut canonical_variants = Vec::new();
        for raw in raw_permutations {
            let parsed: Value = serde_json::from_slice(&raw)
                .expect("independently permuted valid JSON must parse");
            prop_assert_eq!(&parsed, &original, "key order must not alter JSON semantics");

            let canonical = canonical_json(&parsed);
            let round_trip: Value = serde_json::from_slice(&canonical)
                .expect("canonical bytes must remain valid JSON");
            prop_assert_eq!(&round_trip, &original, "canonical round trip must preserve semantics");
            prop_assert_eq!(
                &round_trip["mcpServers"]
                    .as_object()
                    .unwrap()
                    .values()
                    .next()
                    .unwrap()["_orderProbe"],
                &original_probe,
                "canonicalization must preserve array element order",
            );
            prop_assert_eq!(
                &canonical,
                &expected_canonical,
                "production bytes must match the independent sorted-key oracle",
            );

            let validated = validate_mcp_servers(server_values(&parsed))
                .expect("generated server configurations are valid");
            prop_assert_eq!(
                validated.keys().cloned().collect::<Vec<_>>(),
                expected_server_names.clone(),
                "validated server map must preserve every name in sorted order",
            );
            let actual_hashes = server_values(&parsed)
                .iter()
                .map(|(name, config)| (name.clone(), config_hash(config)))
                .collect::<BTreeMap<_, _>>();
            prop_assert_eq!(
                actual_hashes,
                expected_hashes.clone(),
                "all object-key permutations must produce the independent expected hashes",
            );
            canonical_variants.push(canonical);
        }
        prop_assert!(canonical_variants.windows(2).all(|pair| pair[0] == pair[1]));

        let reordered = reverse_first_order_probe(&original);
        prop_assert!(
            validate_mcp_servers(server_values(&reordered)).is_ok(),
            "reordering an extension array must retain a valid server configuration",
        );
        let reordered_probe = server_values(&reordered)
            .values()
            .next()
            .unwrap()["_orderProbe"]
            .clone();
        prop_assert_ne!(&reordered_probe, &original_probe, "probe elements are asymmetric");
        prop_assert_ne!(
            canonical_json(&reordered),
            canonical_json(&original),
            "changing asymmetric array order must change canonical bytes",
        );

        let original_first = server_values(&original).values().next().unwrap();
        let reordered_first = server_values(&reordered).values().next().unwrap();
        prop_assert_ne!(
            config_hash(original_first),
            config_hash(reordered_first),
            "changing asymmetric array order must change ConfigHash",
        );
    }
}
