//! Environment-variable substitution for configuration values.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::{error::CliError, output::DiagnosticSink, policy::redact::SecretSet};

/// Result of one recursive environment-substitution pass.
///
/// `missing` is always sorted and deduplicated. In strict mode a non-empty
/// missing set is returned as [`CliError`] instead of an outcome.
#[derive(Clone, Debug, PartialEq)]
pub struct SubstitutionOutcome {
    pub value: Value,
    pub missing: BTreeSet<String>,
    pub secrets: SecretSet,
}

/// Recursively substitutes valid `${VAR_NAME}` placeholders in JSON string
/// values.
///
/// Object keys and non-string values are never changed. Environment values are
/// appended directly to the output and are therefore not scanned for further
/// placeholders. A lookup is performed at most once per unique variable name.
pub fn substitute_environment<F>(
    value: &Value,
    strict: bool,
    mut lookup: F,
    diagnostics: &dyn DiagnosticSink,
) -> Result<SubstitutionOutcome, CliError>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut substituted = value.clone();
    let mut resolved = BTreeMap::<String, Option<String>>::new();
    let mut missing = BTreeSet::new();
    let mut secrets = SecretSet::new();

    substitute_value(
        &mut substituted,
        &mut lookup,
        &mut resolved,
        &mut missing,
        &mut secrets,
    );

    if strict && !missing.is_empty() {
        return Err(CliError::missing_env_vars(&missing));
    }

    if !strict {
        for name in &missing {
            diagnostics.warning(&format!(
                "Environment variable {name} is not set; substituting an empty string"
            ));
        }
    }

    Ok(SubstitutionOutcome {
        value: substituted,
        missing,
        secrets,
    })
}

fn substitute_value<F>(
    value: &mut Value,
    lookup: &mut F,
    resolved: &mut BTreeMap<String, Option<String>>,
    missing: &mut BTreeSet<String>,
    secrets: &mut SecretSet,
) where
    F: FnMut(&str) -> Option<String>,
{
    match value {
        Value::String(text) => {
            *text = substitute_string(text, lookup, resolved, missing, secrets);
        }
        Value::Array(values) => {
            for value in values {
                substitute_value(value, lookup, resolved, missing, secrets);
            }
        }
        Value::Object(object) => {
            // Iterating values only is intentional: configuration object keys
            // are data identifiers and must not undergo substitution.
            for value in object.values_mut() {
                substitute_value(value, lookup, resolved, missing, secrets);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn substitute_string<F>(
    input: &str,
    lookup: &mut F,
    resolved: &mut BTreeMap<String, Option<String>>,
    missing: &mut BTreeSet<String>,
    secrets: &mut SecretSet,
) -> String
where
    F: FnMut(&str) -> Option<String>,
{
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len());
    let mut copied_until = 0;
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index..].starts_with(b"${") {
            let name_start = index + 2;
            if name_start < bytes.len() && is_name_start(bytes[name_start]) {
                let mut cursor = name_start + 1;
                while cursor < bytes.len() && is_name_continue(bytes[cursor]) {
                    cursor += 1;
                }

                if cursor < bytes.len() && bytes[cursor] == b'}' {
                    let name = &input[name_start..cursor];
                    output.push_str(&input[copied_until..index]);

                    if !resolved.contains_key(name) {
                        let value = lookup(name);
                        if let Some(value) = &value {
                            secrets.register_env(name, value.as_bytes());
                        } else {
                            missing.insert(name.to_owned());
                        }
                        resolved.insert(name.to_owned(), value);
                    }

                    if let Some(value) = resolved.get(name).and_then(Option::as_deref) {
                        output.push_str(value);
                    }
                    index = cursor + 1;
                    copied_until = index;
                    continue;
                }
            }
        }

        index += input[index..]
            .chars()
            .next()
            .expect("index is within the string")
            .len_utf8();
    }

    output.push_str(&input[copied_until..]);
    output
}

const fn is_name_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

const fn is_name_continue(byte: u8) -> bool {
    is_name_start(byte) || byte.is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;
    use crate::error::ErrorKind;

    #[derive(Default)]
    struct RecordingDiagnostics {
        warnings: Mutex<Vec<String>>,
    }

    impl RecordingDiagnostics {
        fn warnings(&self) -> Vec<String> {
            self.warnings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

    impl DiagnosticSink for RecordingDiagnostics {
        fn warning(&self, message: &str) {
            self.warnings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(message.to_owned());
        }

        fn debug(&self, _message: &str) {}

        fn server_stderr(&self, _server: &str, _bytes: &[u8]) {}
    }

    #[test]
    fn recursively_replaces_string_values_but_not_keys_or_other_nodes() {
        let input = json!({
            "${UNCHANGED_KEY}": "${FIRST}/${SECOND}/${FIRST}",
            "nested": [
                "prefix-${SECOND}-suffix",
                {"value": "${FIRST}"},
                17,
                true,
                null
            ]
        });
        let diagnostics = RecordingDiagnostics::default();
        let env = BTreeMap::from([
            ("FIRST", "one"),
            ("SECOND", "two"),
            ("UNCHANGED_KEY", "changed"),
        ]);

        let outcome = substitute_environment(
            &input,
            true,
            |name| env.get(name).map(ToString::to_string),
            &diagnostics,
        )
        .expect("all referenced values are defined");

        assert_eq!(
            outcome.value,
            json!({
                "${UNCHANGED_KEY}": "one/two/one",
                "nested": ["prefix-two-suffix", {"value": "one"}, 17, true, null]
            })
        );
        assert!(outcome.missing.is_empty());
        assert_eq!(outcome.secrets.len(), 2);
        assert!(diagnostics.warnings().is_empty());
    }

    #[test]
    fn uses_explicit_name_grammar_and_never_expands_inserted_values_again() {
        let input = json!([
            "${OUTER}",
            "${_OK1}",
            "${1BAD}",
            "${BAD-NAME}",
            "${}",
            "${UNFINISHED"
        ]);
        let diagnostics = RecordingDiagnostics::default();
        let env = BTreeMap::from([
            ("OUTER", "${INNER}"),
            ("INNER", "must-not-expand"),
            ("_OK1", "valid"),
        ]);

        let outcome = substitute_environment(
            &input,
            true,
            |name| env.get(name).map(ToString::to_string),
            &diagnostics,
        )
        .expect("valid references are defined");

        assert_eq!(
            outcome.value,
            json!([
                "${INNER}",
                "valid",
                "${1BAD}",
                "${BAD-NAME}",
                "${}",
                "${UNFINISHED"
            ])
        );
        assert_eq!(outcome.secrets.len(), 2);
    }

    #[test]
    fn strict_mode_reports_every_sorted_unique_missing_name_without_values() {
        let input = json!({
            "first": "${ZED}:${ALPHA}:${ZED}",
            "defined": "${TOKEN}"
        });
        let diagnostics = RecordingDiagnostics::default();
        let secret = "defined-secret-value";

        let error = substitute_environment(
            &input,
            true,
            |name| (name == "TOKEN").then(|| secret.to_owned()),
            &diagnostics,
        )
        .expect_err("strict substitution must reject missing variables");

        assert_eq!(error.kind, ErrorKind::MissingEnvVar);
        assert_eq!(
            error.details.as_deref(),
            Some("Missing variables: ALPHA, ZED")
        );
        for visible in [
            error.message,
            error.details.unwrap_or_default(),
            error.suggestion.unwrap_or_default(),
        ] {
            assert!(!visible.contains(secret));
        }
        assert!(diagnostics.warnings().is_empty());
    }

    #[test]
    fn non_strict_mode_empties_missing_values_warns_once_and_registers_secrets() {
        let input = json!({
            "value": "a${MISSING}b${TOKEN}c${MISSING}d${EMPTY}",
            "again": "${OTHER}"
        });
        let diagnostics = RecordingDiagnostics::default();

        let outcome = substitute_environment(
            &input,
            false,
            |name| match name {
                "TOKEN" => Some("top-secret".to_owned()),
                "EMPTY" => Some(String::new()),
                _ => None,
            },
            &diagnostics,
        )
        .expect("non-strict substitution succeeds");

        assert_eq!(
            outcome.value,
            json!({"value": "abtop-secretcd", "again": ""})
        );
        assert_eq!(
            outcome.missing,
            BTreeSet::from(["MISSING".to_owned(), "OTHER".to_owned()])
        );
        assert_eq!(
            diagnostics.warnings(),
            vec![
                "Environment variable MISSING is not set; substituting an empty string",
                "Environment variable OTHER is not set; substituting an empty string",
            ]
        );
        assert_eq!(outcome.secrets.len(), 1, "empty values are not secrets");
        assert_eq!(
            outcome.secrets.redact("token=top-secret"),
            "token=[REDACTED]"
        );
    }
}
