use mcp_cli::format_tool_result;
use proptest::prelude::*;
use serde_json::{Map, Number, Value};

fn json_string() -> BoxedStrategy<String> {
    prop::collection::vec(any::<char>(), 0..=48)
        .prop_map(|characters| characters.into_iter().collect())
        .boxed()
}

fn json_number() -> BoxedStrategy<Value> {
    prop_oneof![
        3 => any::<i64>().prop_map(|value| Value::Number(Number::from(value))),
        2 => any::<u64>().prop_map(|value| Value::Number(Number::from(value))),
        1 => (any::<i32>(), 0_u32..1_000_000).prop_map(|(whole, fraction)| {
            let literal = format!("{whole}.{fraction:06}");
            serde_json::from_str(&literal).expect("generated decimal is valid JSON")
        }),
    ]
    .boxed()
}

fn json_scalar() -> BoxedStrategy<Value> {
    prop_oneof![
        1 => Just(Value::Null),
        1 => any::<bool>().prop_map(Value::Bool),
        3 => json_number(),
        3 => json_string().prop_map(Value::String),
    ]
    .boxed()
}

fn arbitrary_json_value() -> BoxedStrategy<Value> {
    json_scalar()
        .prop_recursive(6, 256, 10, |inner| {
            let array = prop::collection::vec(inner.clone(), 0..=8).prop_map(Value::Array);
            let object = prop::collection::btree_map(json_string(), inner, 0..=8)
                .prop_map(|entries| Value::Object(entries.into_iter().collect()));

            prop_oneof![3 => array, 4 => object]
        })
        .boxed()
}

fn extension_key() -> BoxedStrategy<String> {
    proptest::string::string_regex("x-[a-z][a-z0-9-]{0,20}")
        .expect("extension field regex is valid")
        .boxed()
}

fn content_entry() -> BoxedStrategy<Value> {
    prop_oneof![
        4 => (json_string(), extension_key(), arbitrary_json_value()).prop_map(
            |(text, extension, extension_value)| {
                let mut entry = Map::new();
                entry.insert("type".to_owned(), Value::String("text".to_owned()));
                entry.insert("text".to_owned(), Value::String(text));
                entry.insert(extension, extension_value);
                Value::Object(entry)
            },
        ),
        2 => (json_string(), json_string(), extension_key(), arbitrary_json_value()).prop_map(
            |(data, mime_type, extension, extension_value)| {
                let mut entry = Map::new();
                entry.insert("type".to_owned(), Value::String("image".to_owned()));
                entry.insert("data".to_owned(), Value::String(data));
                entry.insert("mimeType".to_owned(), Value::String(mime_type));
                entry.insert(extension, extension_value);
                Value::Object(entry)
            },
        ),
        2 => (arbitrary_json_value(), extension_key(), arbitrary_json_value()).prop_map(
            |(resource, extension, extension_value)| {
                let mut entry = Map::new();
                entry.insert("type".to_owned(), Value::String("resource".to_owned()));
                entry.insert("resource".to_owned(), resource);
                entry.insert(extension, extension_value);
                Value::Object(entry)
            },
        ),
    ]
    .boxed()
}

fn typical_mcp_result() -> BoxedStrategy<Value> {
    (
        json_string(),
        prop::collection::vec(content_entry(), 0..=4),
        any::<bool>(),
        arbitrary_json_value(),
        prop::collection::btree_map(extension_key(), arbitrary_json_value(), 1..=4),
    )
        .prop_map(
            |(text, mut additional_content, is_error, structured_content, extensions)| {
                let mut text_entry = Map::new();
                text_entry.insert("type".to_owned(), Value::String("text".to_owned()));
                text_entry.insert("text".to_owned(), Value::String(text));

                let mut content = vec![Value::Object(text_entry)];
                content.append(&mut additional_content);

                let mut result = extensions.into_iter().collect::<Map<_, _>>();
                result.insert("content".to_owned(), Value::Array(content));
                result.insert("isError".to_owned(), Value::Bool(is_error));
                result.insert("structuredContent".to_owned(), structured_content);
                Value::Object(result)
            },
        )
        .boxed()
}

fn tool_result_value() -> BoxedStrategy<Value> {
    prop_oneof![
        5 => arbitrary_json_value(),
        5 => typical_mcp_result(),
        2 => prop::collection::vec(arbitrary_json_value(), 0..=8).prop_map(Value::Array),
        2 => json_scalar(),
        1 => Just(Value::Null),
    ]
    .boxed()
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

    // Feature: mcp-cli, Property 26: Tool_Result 完整 JSON round trip
    // **Validates: Requirements 11.9, 11.10**
    #[test]
    fn property_26_tool_result_stdout_round_trips(result in tool_result_value()) {
        let stdout = format_tool_result(&result).expect("serde_json Value must serialize");

        let mut oracle = serde_json::to_vec(&result)
            .expect("independent serde_json oracle must serialize Value");
        oracle.push(b'\n');
        prop_assert_eq!(
            &stdout,
            &oracle,
            "stdout must be exactly the serde_json document plus one trailing newline, with no title or diagnostic",
        );

        prop_assert_eq!(stdout.last(), Some(&b'\n'), "stdout must end in one newline");
        let document = stdout
            .strip_suffix(b"\n")
            .expect("the required trailing newline was checked above");
        prop_assert!(!document.is_empty(), "the JSON document must not be empty");
        prop_assert!(
            !document.contains(&b'\n'),
            "the trailing newline must be the only literal newline in stdout",
        );
        prop_assert!(
            !stdout.contains(&0x1b),
            "JSON stdout must contain no raw ANSI escape sequence",
        );

        let parsed = parse_exactly_one_json_value(&stdout).map_err(TestCaseError::fail)?;
        prop_assert_eq!(
            &parsed,
            &result,
            "parsed stdout must preserve the complete Tool_Result rather than only text content",
        );

        if let (Some(original), Some(round_tripped)) = (result.as_object(), parsed.as_object())
            && original.get("content").is_some_and(Value::is_array)
            && original.contains_key("isError")
            && original.contains_key("structuredContent")
            && original.keys().any(|key| key.starts_with("x-"))
        {
            for field in ["content", "isError", "structuredContent"] {
                prop_assert_eq!(
                    round_tripped.get(field),
                    original.get(field),
                    "typical MCP result field {} must be preserved",
                    field,
                );
            }
            for (key, value) in original.iter().filter(|(key, _)| key.starts_with("x-")) {
                prop_assert_eq!(
                    round_tripped.get(key),
                    Some(value),
                    "unknown extension field {} must be preserved",
                    key,
                );
            }
        }

        let independently_reserialized = serde_json::to_vec(&parsed)
            .expect("a parsed serde_json Value must serialize");
        let parsed_again = parse_exactly_one_json_value(&independently_reserialized)
            .map_err(TestCaseError::fail)?;
        prop_assert_eq!(
            &parsed_again,
            &result,
            "double serde_json round trip must preserve Tool_Result semantics",
        );

        let stdout_again = format_tool_result(&parsed_again)
            .expect("double-round-tripped Value must serialize");
        prop_assert_eq!(
            stdout_again,
            stdout,
            "formatting must be stable after the double round trip",
        );
    }
}
