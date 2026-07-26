//! Canonical JSON, configuration hashes, and server identifiers.

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{ConfigHash, ServerId};

/// Number of lowercase hexadecimal characters in a complete SHA-256 digest.
pub const SHA256_HEX_LENGTH: usize = 64;

/// Serializes JSON deterministically by recursively sorting object keys.
///
/// Arrays retain their original order, and scalar values are emitted with
/// `serde_json`'s normal JSON representation so their JSON types and values
/// are preserved.
pub fn canonical_json(value: &Value) -> Vec<u8> {
    let mut output = Vec::new();
    write_canonical(value, &mut output);
    output
}

/// Computes the complete SHA-256 digest of a canonical server configuration.
///
/// Callers pass the environment-substituted server JSON value after it has
/// successfully passed validation. Hashing the original value preserves
/// compatible extension fields that are not part of the typed transport model.
pub fn config_hash(config: &Value) -> ConfigHash {
    ConfigHash(Sha256::digest(canonical_json(config)).into())
}

/// Derives a filesystem-safe, stable identifier from the complete SHA-256
/// digest of the server name's UTF-8 bytes.
pub fn server_id(server_name: &str) -> ServerId {
    ServerId(lowercase_hex(&Sha256::digest(server_name.as_bytes())))
}

impl ConfigHash {
    /// Returns the complete digest as fixed-length lowercase hexadecimal text.
    pub fn to_hex(self) -> String {
        lowercase_hex(&self.0)
    }
}

fn write_canonical(value: &Value, output: &mut Vec<u8>) {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(value) => output.extend_from_slice(if *value { b"true" } else { b"false" }),
        Value::Number(value) => output.extend_from_slice(value.to_string().as_bytes()),
        Value::String(value) => {
            serde_json::to_writer(output, value)
                .expect("serializing a JSON string to memory cannot fail");
        }
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                write_canonical(value, output);
            }
            output.push(b']');
        }
        Value::Object(object) => {
            let mut entries = object.iter().collect::<Vec<_>>();
            entries.sort_unstable_by_key(|(key, _)| *key);

            output.push(b'{');
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                serde_json::to_writer(&mut *output, key)
                    .expect("serializing a JSON object key to memory cannot fail");
                output.push(b':');
                write_canonical(value, output);
            }
            output.push(b'}');
        }
    }
}

fn lowercase_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";

    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use super::*;

    fn object(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
        Value::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect::<Map<_, _>>(),
        )
    }

    #[test]
    fn canonical_json_sorts_every_object_recursively() {
        let value = object([
            (
                "z",
                object([
                    ("two", json!(2)),
                    ("one", object([("y", json!(true)), ("x", json!(false))])),
                ]),
            ),
            ("a", json!(0)),
        ]);

        assert_eq!(
            canonical_json(&value),
            br#"{"a":0,"z":{"one":{"x":false,"y":true},"two":2}}"#
        );
    }

    #[test]
    fn canonical_json_preserves_array_order_and_scalar_semantics() {
        let value = json!([3, 1, 2, null, true, false, "line\ntext", {"b": 2, "a": 1}]);
        let bytes = canonical_json(&value);

        assert_eq!(
            bytes,
            br#"[3,1,2,null,true,false,"line\ntext",{"a":1,"b":2}]"#
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).expect("canonical JSON parses"),
            value
        );
    }

    #[test]
    fn config_hash_is_full_sha256_stable_for_key_order_and_changes_with_values() {
        let first = object([
            ("command", json!("runner")),
            ("env", object([("Z", json!("last")), ("A", json!("first"))])),
        ]);
        let reordered = object([
            ("env", object([("A", json!("first")), ("Z", json!("last"))])),
            ("command", json!("runner")),
        ]);
        let changed = object([
            ("command", json!("runner")),
            (
                "env",
                object([("A", json!("changed")), ("Z", json!("last"))]),
            ),
        ]);

        let hash = config_hash(&first);
        assert_eq!(hash, config_hash(&reordered));
        assert_ne!(hash, config_hash(&changed));
        assert_eq!(hash.to_hex().len(), SHA256_HEX_LENGTH);
        assert!(
            hash.to_hex()
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert_eq!(
            config_hash(&json!({})).to_hex(),
            "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
    }

    #[test]
    fn server_id_has_safe_fixed_grammar_and_is_stable() {
        for name in ["alpha", "../escape", "dir/server", "控制\n字符", ""] {
            let first = server_id(name);
            let second = server_id(name);

            assert_eq!(first, second);
            assert_eq!(first.0.len(), SHA256_HEX_LENGTH);
            assert!(
                first
                    .0
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            );
        }

        assert_ne!(server_id("alpha"), server_id("Alpha"));
    }
}
