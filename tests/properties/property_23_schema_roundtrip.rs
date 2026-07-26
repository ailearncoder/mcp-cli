use mcp_cli::format_json_schema;
use proptest::prelude::*;
use serde_json::{Map, Number, Value};

fn json_text() -> impl Strategy<Value = String> {
    prop::collection::vec(any::<char>(), 0..=40)
        .prop_map(|characters| characters.into_iter().collect())
}

fn json_decimal() -> impl Strategy<Value = Value> {
    (any::<i32>(), 0_u32..1_000_000).prop_map(|(whole, fraction)| {
        let literal = format!("{whole}.{fraction:06}");
        serde_json::from_str(&literal).expect("generated decimal is valid JSON")
    })
}

fn ordinary_key() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[A-Za-z_][A-Za-z0-9_.-]{0,23}")
        .expect("ordinary JSON key regex is valid")
}

fn extension_key() -> impl Strategy<Value = String> {
    proptest::string::string_regex("x-[a-z][a-z0-9-]{0,20}")
        .expect("extension keyword regex is valid")
}

fn schema_key() -> impl Strategy<Value = String> {
    prop_oneof![
        5 => ordinary_key(),
        2 => extension_key(),
        1 => prop::sample::select(vec![
            "$schema".to_owned(),
            "$defs".to_owned(),
            "unevaluatedProperties".to_owned(),
            "dependentSchemas".to_owned(),
        ]),
    ]
}

fn json_schema_value() -> impl Strategy<Value = Value> {
    let scalar = prop_oneof![
        1 => Just(Value::Null),
        1 => any::<bool>().prop_map(Value::Bool),
        2 => any::<i64>().prop_map(|value| Value::Number(Number::from(value))),
        1 => json_decimal(),
        3 => json_text().prop_map(Value::String),
    ];

    scalar.prop_recursive(5, 160, 8, |inner| {
        let array = prop::collection::vec(inner.clone(), 0..=8).prop_map(Value::Array);
        let object = prop::collection::btree_map(schema_key(), inner.clone(), 0..=8)
            .prop_map(|entries| Value::Object(entries.into_iter().collect()));
        let extension_object = (
            extension_key(),
            inner.clone(),
            prop::collection::btree_map(schema_key(), inner, 0..=6),
        )
            .prop_map(|(extension, extension_value, entries)| {
                let mut object = entries.into_iter().collect::<Map<_, _>>();
                object.insert(extension, extension_value);
                Value::Object(object)
            });

        prop_oneof![3 => array, 3 => object, 2 => extension_object]
    })
}

fn parse_exactly_one_json_value(bytes: &[u8]) -> Result<Value, String> {
    let mut values = serde_json::Deserializer::from_slice(bytes).into_iter::<Value>();
    let value = values
        .next()
        .ok_or_else(|| "stdout did not contain a JSON value".to_owned())?
        .map_err(|error| error.to_string())?;
    match values.next() {
        None => Ok(value),
        Some(Ok(_)) => Err("stdout contained more than one JSON value".to_owned()),
        Some(Err(error)) => Err(format!("stdout had a non-JSON suffix: {error}")),
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 23: Tool_Schema 输出 round trip
    // **Validates: Requirements 9.8**
    #[test]
    fn property_23_tool_schema_stdout_round_trips(schema in json_schema_value()) {
        let stdout = format_json_schema(&schema).expect("serde_json Value must serialize");

        prop_assert_eq!(stdout.last(), Some(&b'\n'), "stdout must end with one newline");
        prop_assert_eq!(
            stdout.iter().filter(|&&byte| byte == b'\n').count(),
            1,
            "stdout must contain exactly one literal newline",
        );
        let document = stdout
            .strip_suffix(b"\n")
            .expect("the required trailing newline was checked above");
        prop_assert!(!document.is_empty(), "the JSON document must not be empty");

        prop_assert!(!document.contains(&0x1b), "JSON stdout must not contain ANSI escapes");
        for forbidden in [
            b"Schema:".as_slice(),
            b"Tool schema:".as_slice(),
            b"[mcp-cli]".as_slice(),
            b"Error [".as_slice(),
            b"Details:".as_slice(),
            b"Suggestion:".as_slice(),
        ] {
            prop_assert!(
                !document.starts_with(forbidden),
                "JSON stdout must not have a title or diagnostic prefix",
            );
            prop_assert!(
                !document.ends_with(forbidden),
                "JSON stdout must not have a title or diagnostic suffix",
            );
        }

        let parsed = parse_exactly_one_json_value(document)
            .map_err(TestCaseError::fail)?;
        prop_assert_eq!(&parsed, &schema, "parsed schema must be semantically equivalent");

        let independently_reserialized = serde_json::to_vec(&parsed)
            .expect("a parsed serde_json Value must serialize");
        let parsed_again = parse_exactly_one_json_value(&independently_reserialized)
            .map_err(TestCaseError::fail)?;
        prop_assert_eq!(
            &parsed_again,
            &schema,
            "double round trip must preserve schema semantics",
        );
    }
}
