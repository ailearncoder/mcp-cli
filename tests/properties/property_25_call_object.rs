#![forbid(unsafe_code)]

use std::io::Cursor;

use mcp_cli::{CallInput, ErrorKind};
use proptest::{prelude::*, test_runner::RngSeed};
use serde_json::{Map, Number, Value};

fn unicode_string(max_len: usize) -> BoxedStrategy<String> {
    let arbitrary = prop::collection::vec(any::<char>(), 0..=max_len)
        .prop_map(|characters| characters.into_iter().collect::<String>());
    let explicitly_unicode = (
        prop::collection::vec(any::<char>(), 0..=max_len / 2),
        any::<char>().prop_filter("non-ASCII Unicode scalar", |character| {
            !character.is_ascii()
        }),
        prop::collection::vec(any::<char>(), 0..=max_len / 2),
    )
        .prop_map(|(prefix, unicode, suffix)| {
            prefix
                .into_iter()
                .chain([unicode])
                .chain(suffix)
                .collect::<String>()
        });

    prop_oneof![arbitrary, explicitly_unicode].boxed()
}

fn json_scalar() -> BoxedStrategy<Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|number| Value::Number(Number::from(number))),
        unicode_string(32).prop_map(Value::String),
    ]
    .boxed()
}

fn arbitrary_json_value() -> BoxedStrategy<Value> {
    json_scalar()
        .prop_recursive(5, 192, 8, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..=6).prop_map(Value::Array),
                prop::collection::btree_map(unicode_string(20), inner, 0..=6)
                    .prop_map(|entries| Value::Object(entries.into_iter().collect())),
            ]
        })
        .boxed()
}

fn json_object() -> BoxedStrategy<Map<String, Value>> {
    prop::collection::btree_map(unicode_string(20), arbitrary_json_value(), 0..=8)
        .prop_map(|entries| entries.into_iter().collect())
        .boxed()
}

fn non_object_json() -> BoxedStrategy<(Value, &'static str)> {
    prop_oneof![
        Just((Value::Null, "null")),
        any::<bool>().prop_map(|value| (Value::Bool(value), "boolean")),
        any::<i64>().prop_map(|value| (Value::Number(Number::from(value)), "number")),
        unicode_string(48).prop_map(|value| (Value::String(value), "string")),
        prop::collection::vec(arbitrary_json_value(), 0..=8)
            .prop_map(|value| (Value::Array(value), "array")),
    ]
    .boxed()
}

fn json_whitespace() -> BoxedStrategy<(String, String)> {
    (
        prop::collection::vec(prop::sample::select(vec![' ', '\t', '\r', '\n']), 0..=8),
        prop::collection::vec(prop::sample::select(vec![' ', '\t', '\r', '\n']), 0..=8),
    )
        .prop_map(|(prefix, suffix)| {
            (
                prefix.into_iter().collect::<String>(),
                suffix.into_iter().collect::<String>(),
            )
        })
        .boxed()
}

fn encode_with_whitespace(value: &Value, prefix: &str, suffix: &str) -> String {
    format!(
        "{prefix}{}{suffix}",
        serde_json::to_string(value).expect("generated serde_json::Value must serialize")
    )
}

fn visible_error(error: &mcp_cli::CliError) -> (&ErrorKind, &str, Option<&str>, Option<&str>) {
    (
        &error.kind,
        error.message.as_str(),
        error.details.as_deref(),
        error.suggestion.as_deref(),
    )
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        rng_seed: RngSeed::Fixed(0x25ca_1106_2025),
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 25: call 只接受顶层 object
    // **Validates: Requirements 11.6**
    #[test]
    fn property_25_call_accepts_only_top_level_objects(
        object in json_object(),
        (non_object, expected_type) in non_object_json(),
        (prefix, suffix) in json_whitespace(),
    ) {
        let object_value = Value::Object(object.clone());
        let object_text = encode_with_whitespace(&object_value, &prefix, &suffix);

        // The independent oracle is the generated Value::Object itself. The
        // production parser is used only by CallInput, never to build expected data.
        let mut ignored_stdin = Cursor::new(b"not consulted for inline input".to_vec());
        let inline_object = CallInput::new(&mut ignored_stdin, false)
            .read(Some(&object_text))
            .expect("every generated top-level object must be accepted inline");
        prop_assert_eq!(Value::Object(inline_object), object_value.clone());
        prop_assert_eq!(ignored_stdin.position(), 0, "inline input must not read stdin");

        let mut object_stdin = Cursor::new(object_text.as_bytes());
        let stdin_object = CallInput::new(&mut object_stdin, false)
            .read(None)
            .expect("every generated top-level object must be accepted from non-TTY stdin");
        prop_assert_eq!(Value::Object(stdin_object), object_value);
        prop_assert_eq!(object_stdin.position(), object_text.len() as u64);

        let non_object_text = encode_with_whitespace(&non_object, &prefix, &suffix);
        let expected_details = format!(
            "Top-level JSON value is {expected_type}; expected object"
        );

        let mut ignored_stdin = Cursor::new(b"not consulted for inline input".to_vec());
        let inline_error = CallInput::new(&mut ignored_stdin, false)
            .read(Some(&non_object_text))
            .expect_err("every generated non-object must be rejected inline");
        prop_assert_eq!(inline_error.kind, ErrorKind::InvalidArguments);
        prop_assert_eq!(inline_error.message.as_str(), "Tool arguments must be a JSON object");
        prop_assert_eq!(inline_error.details.as_deref(), Some(expected_details.as_str()));
        prop_assert_eq!(ignored_stdin.position(), 0, "inline rejection must not read stdin");

        let mut non_object_stdin = Cursor::new(non_object_text.as_bytes());
        let stdin_error = CallInput::new(&mut non_object_stdin, false)
            .read(None)
            .expect_err("every generated non-object must be rejected from non-TTY stdin");
        prop_assert_eq!(stdin_error.kind, ErrorKind::InvalidArguments);
        prop_assert_eq!(stdin_error.message.as_str(), "Tool arguments must be a JSON object");
        prop_assert_eq!(stdin_error.details.as_deref(), Some(expected_details.as_str()));
        prop_assert_eq!(non_object_stdin.position(), non_object_text.len() as u64);

        // Both paths must reject deterministically with the same typed, safe
        // diagnostic. Exact fixed text identifies only the JSON type and cannot
        // expose generated strings, keys, numbers, or nested array contents.
        prop_assert_eq!(visible_error(&inline_error), visible_error(&stdin_error));
    }
}
