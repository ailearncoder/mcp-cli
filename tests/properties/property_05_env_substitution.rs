use std::collections::{BTreeMap, BTreeSet};

use mcp_cli::{DiagnosticSink, config::substitute::substitute_environment};
use proptest::prelude::*;
use serde_json::{Map, Number, Value};

struct NullDiagnostics;

impl DiagnosticSink for NullDiagnostics {
    fn warning(&self, _message: &str) {}

    fn debug(&self, _message: &str) {}

    fn server_stderr(&self, _server: &str, _bytes: &[u8]) {}
}

#[derive(Clone, Debug)]
struct SubstitutionCase {
    input: Value,
    env: BTreeMap<String, String>,
}

fn plain_text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(
            "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 _/-é工具🦀"
                .chars()
                .collect::<Vec<_>>(),
        ),
        0..24,
    )
    .prop_map(|characters| characters.into_iter().collect())
}

fn object_key() -> impl Strategy<Value = String> {
    prop_oneof![
        9 => plain_text(),
        1 => Just("${KEY_ONLY}".to_owned()),
    ]
}

fn bounded_json(variable_names: Vec<String>) -> BoxedStrategy<Value> {
    let placeholder_name = prop::sample::select(variable_names);
    let string_value = prop_oneof![
        4 => plain_text().prop_map(Value::String),
        3 => placeholder_name
            .clone()
            .prop_map(|name| Value::String(format!("prefix-${{{name}}}-suffix"))),
        2 => placeholder_name
            .clone()
            .prop_map(|name| Value::String(format!("${{{name}}}|${{{name}}}"))),
        1 => (placeholder_name.clone(), placeholder_name).prop_map(|(first, second)| {
            Value::String(format!("before-${{{first}}}-between-${{{second}}}-after"))
        }),
    ];

    let leaf = prop_oneof![
        5 => string_value,
        1 => Just(Value::Null),
        1 => any::<bool>().prop_map(Value::Bool),
        2 => any::<i32>().prop_map(|number| Value::Number(Number::from(number))),
    ];

    leaf.prop_recursive(4, 48, 4, |inner| {
        prop_oneof![
            1 => prop::collection::vec(inner.clone(), 0..5).prop_map(Value::Array),
            1 => prop::collection::btree_map(object_key(), inner, 0..5).prop_map(|entries| {
                Value::Object(entries.into_iter().collect::<Map<String, Value>>())
            }),
        ]
    })
    .boxed()
}

fn substitution_case() -> impl Strategy<Value = SubstitutionCase> {
    prop::collection::btree_set(0_u16..10_000, 1..7).prop_flat_map(|suffixes| {
        let generated_names = suffixes
            .into_iter()
            .map(|suffix| format!("VAR_{suffix}"))
            .collect::<Vec<_>>();
        let mut all_names = vec!["OUTER".to_owned(), "OTHER".to_owned()];
        all_names.extend(generated_names.iter().cloned());
        let generated_json = bounded_json(all_names);
        let value_count = generated_names.len();

        (
            Just(generated_names),
            prop::collection::vec(plain_text(), value_count),
            generated_json,
        )
            .prop_map(|(generated_names, generated_values, generated_json)| {
                let mut env = BTreeMap::from([
                    ("OUTER".to_owned(), "injected-${OTHER}-literal".to_owned()),
                    ("OTHER".to_owned(), "expanded-only-from-source".to_owned()),
                ]);
                env.extend(generated_names.into_iter().zip(generated_values));

                let input = Value::Object(Map::from_iter([
                    (
                        "${KEY_ONLY}".to_owned(),
                        Value::Object(Map::from_iter([
                            ("generated".to_owned(), generated_json),
                            (
                                "repeated".to_owned(),
                                Value::String("before-${OUTER}-middle-${OUTER}-after".to_owned()),
                            ),
                            (
                                "single_other".to_owned(),
                                Value::String("${OTHER}".to_owned()),
                            ),
                            (
                                "plain".to_owned(),
                                Value::String("placeholder-free text".to_owned()),
                            ),
                            (
                                "scalars".to_owned(),
                                Value::Array(vec![
                                    Value::Null,
                                    Value::Bool(true),
                                    Value::Number(Number::from(-17)),
                                ]),
                            ),
                        ])),
                    ),
                    ("top_level_array".to_owned(), Value::Array(vec![])),
                ]));

                SubstitutionCase { input, env }
            })
    })
}

fn valid_variable_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn reference_substitute_string(input: &str, env: &BTreeMap<String, String>) -> String {
    let mut output = String::with_capacity(input.len());
    let mut remaining = input;

    while let Some(start) = remaining.find("${") {
        output.push_str(&remaining[..start]);
        let candidate = &remaining[start + 2..];
        let Some(end) = candidate.find('}') else {
            output.push_str(&remaining[start..]);
            return output;
        };
        let name = &candidate[..end];
        if valid_variable_name(name) {
            output.push_str(env.get(name).expect("generated env covers every reference"));
            remaining = &candidate[end + 1..];
        } else {
            output.push_str("${");
            remaining = candidate;
        }
    }

    output.push_str(remaining);
    output
}

fn reference_substitute(value: &Value, env: &BTreeMap<String, String>) -> Value {
    match value {
        Value::String(text) => Value::String(reference_substitute_string(text, env)),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| reference_substitute(value, env))
                .collect(),
        ),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), reference_substitute(value, env)))
                .collect(),
        ),
        Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
    }
}

fn referenced_variables(value: &Value, names: &mut BTreeSet<String>) {
    match value {
        Value::String(text) => {
            let mut remaining = text.as_str();
            while let Some(start) = remaining.find("${") {
                let candidate = &remaining[start + 2..];
                let Some(end) = candidate.find('}') else {
                    break;
                };
                let name = &candidate[..end];
                if valid_variable_name(name) {
                    names.insert(name.to_owned());
                }
                remaining = &candidate[end + 1..];
            }
        }
        Value::Array(values) => {
            for value in values {
                referenced_variables(value, names);
            }
        }
        Value::Object(object) => {
            for value in object.values() {
                referenced_variables(value, names);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn preserves_non_substituted_structure(
    original: &Value,
    actual: &Value,
    env: &BTreeMap<String, String>,
) -> bool {
    match (original, actual) {
        (Value::String(before), Value::String(after)) => {
            after == &reference_substitute_string(before, env)
                && (before.contains("${") || before == after)
        }
        (Value::Array(before), Value::Array(after)) => {
            before.len() == after.len()
                && before
                    .iter()
                    .zip(after)
                    .all(|(before, after)| preserves_non_substituted_structure(before, after, env))
        }
        (Value::Object(before), Value::Object(after)) => {
            before.keys().eq(after.keys())
                && before.iter().all(|(key, before_value)| {
                    after.get(key).is_some_and(|after_value| {
                        preserves_non_substituted_structure(before_value, after_value, env)
                    })
                })
        }
        (Value::Null, Value::Null)
        | (Value::Bool(_), Value::Bool(_))
        | (Value::Number(_), Value::Number(_)) => original == actual,
        _ => false,
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 5: 已定义环境变量的一次递归替换
    // **Validates: Requirements 3.1**
    #[test]
    fn property_05_defined_environment_variables_are_substituted_once(
        case in substitution_case(),
    ) {
        let mut references = BTreeSet::new();
        referenced_variables(&case.input, &mut references);
        prop_assert!(
            references.iter().all(|name| case.env.contains_key(name)),
            "env must cover all generated references: references={references:?}, env={:?}",
            case.env.keys().collect::<Vec<_>>(),
        );
        prop_assert!(case.env.keys().all(|name| valid_variable_name(name)));
        prop_assert_eq!(case.env.len(), case.env.keys().collect::<BTreeSet<_>>().len());

        let expected = reference_substitute(&case.input, &case.env);
        let outcome = substitute_environment(
            &case.input,
            true,
            |name| case.env.get(name).cloned(),
            &NullDiagnostics,
        ).expect("all referenced variables are defined");

        prop_assert!(outcome.missing.is_empty());
        prop_assert_eq!(&outcome.value, &expected);
        prop_assert!(preserves_non_substituted_structure(
            &case.input,
            &outcome.value,
            &case.env,
        ));

        let root = outcome.value["${KEY_ONLY}"]
            .as_object()
            .expect("the invariant probe object remains present");
        prop_assert_eq!(
            root["repeated"].as_str(),
            Some(
                "before-injected-${OTHER}-literal-middle-\
                 injected-${OTHER}-literal-after",
            ),
            "both original OUTER placeholders are replaced without rescanning inserted text",
        );
        prop_assert_eq!(
            root["single_other"].as_str(),
            Some("expanded-only-from-source"),
            "an OTHER placeholder present in the original source is still replaced",
        );
        let key_only = ["$", "{KEY_ONLY}"].concat();
        prop_assert!(
            outcome.value.as_object().unwrap().contains_key(&key_only),
            "placeholder-shaped object key must remain unchanged",
        );
        prop_assert_eq!(
            &root["plain"],
            &Value::String("placeholder-free text".to_owned())
        );
        prop_assert_eq!(
            &root["scalars"],
            &Value::Array(vec![
                Value::Null,
                Value::Bool(true),
                Value::Number(Number::from(-17)),
            ]),
        );
    }
}
