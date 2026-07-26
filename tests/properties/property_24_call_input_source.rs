#![forbid(unsafe_code)]

use std::io::{self, Cursor, Read};

use mcp_cli::{CALL_INPUT_MAX_SIZE, CallInput, JsonObject};
use proptest::{prelude::*, test_runner::RngSeed};
use serde_json::{Number, Value};

#[derive(Debug)]
enum ReaderPayload {
    Bytes(Cursor<Vec<u8>>),
    Oversized { remaining: usize },
    Failure,
}

#[derive(Debug)]
struct CountingReader {
    payload: ReaderPayload,
    reads: usize,
    deny_reads: bool,
}

impl CountingReader {
    fn bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            payload: ReaderPayload::Bytes(Cursor::new(bytes.into())),
            reads: 0,
            deny_reads: false,
        }
    }

    fn oversized() -> Self {
        Self {
            payload: ReaderPayload::Oversized {
                remaining: CALL_INPUT_MAX_SIZE + 1,
            },
            reads: 0,
            deny_reads: false,
        }
    }

    fn failure() -> Self {
        Self {
            payload: ReaderPayload::Failure,
            reads: 0,
            deny_reads: false,
        }
    }

    fn deny_reads(mut self) -> Self {
        self.deny_reads = true;
        self
    }
}

impl Read for CountingReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.reads += 1;
        if self.deny_reads {
            return Err(io::Error::other("property reader must not be read"));
        }

        match &mut self.payload {
            ReaderPayload::Bytes(bytes) => bytes.read(buffer),
            ReaderPayload::Oversized { remaining } => {
                let count = buffer.len().min(*remaining);
                buffer[..count].fill(b'x');
                *remaining -= count;
                Ok(count)
            }
            ReaderPayload::Failure => Err(io::Error::other("generated stdin read failure")),
        }
    }
}

fn json_string() -> BoxedStrategy<String> {
    prop::collection::vec(any::<char>(), 0..=24)
        .prop_map(|characters| characters.into_iter().collect())
        .boxed()
}

fn json_scalar() -> BoxedStrategy<Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|value| Value::Number(Number::from(value))),
        json_string().prop_map(Value::String),
    ]
    .boxed()
}

fn json_value() -> BoxedStrategy<Value> {
    json_scalar()
        .prop_recursive(4, 96, 8, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..=5).prop_map(Value::Array),
                prop::collection::btree_map(json_string(), inner, 0..=5)
                    .prop_map(|entries| Value::Object(entries.into_iter().collect())),
            ]
        })
        .boxed()
}

fn json_object() -> BoxedStrategy<JsonObject> {
    prop::collection::btree_map(json_string(), json_value(), 0..=8)
        .prop_map(|entries| entries.into_iter().collect())
        .boxed()
}

fn mixed_ascii_unicode_whitespace() -> BoxedStrategy<String> {
    let ascii = prop::collection::vec(
        prop::sample::select(vec![' ', '\t', '\r', '\n', '\u{000b}', '\u{000c}']),
        1..=12,
    );
    let unicode = prop::collection::vec(
        prop::sample::select(vec![
            '\u{0085}', '\u{00a0}', '\u{1680}', '\u{2003}', '\u{2028}', '\u{2029}', '\u{202f}',
            '\u{205f}', '\u{3000}',
        ]),
        1..=12,
    );

    (ascii, unicode, any::<bool>())
        .prop_map(|(ascii, unicode, ascii_first)| {
            let characters = if ascii_first {
                ascii.into_iter().chain(unicode).collect::<Vec<_>>()
            } else {
                unicode.into_iter().chain(ascii).collect::<Vec<_>>()
            };
            characters.into_iter().collect()
        })
        .boxed()
}

fn ignored_stdin_variants(object_bytes: &[u8], whitespace: &str) -> Vec<CountingReader> {
    vec![
        CountingReader::bytes(object_bytes.to_vec()).deny_reads(),
        CountingReader::bytes(b"{ invalid json".to_vec()).deny_reads(),
        CountingReader::bytes(whitespace.as_bytes().to_vec()).deny_reads(),
        CountingReader::bytes(Vec::new()).deny_reads(),
        CountingReader::oversized().deny_reads(),
        CountingReader::failure().deny_reads(),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        rng_seed: RngSeed::Fixed(0x24ca_1100_2025),
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 24: call 输入源选择与空白归一化
    // **Validates: Requirements 11.1, 11.4**
    #[test]
    fn property_24_call_input_source_selection_and_whitespace_normalization(
        inline_object in json_object(),
        stdin_object in json_object(),
        whitespace in mixed_ascii_unicode_whitespace(),
        inline_tty_state in any::<bool>(),
    ) {
        let inline_json = serde_json::to_string(&Value::Object(inline_object.clone()))
            .expect("generated inline object serializes");
        let stdin_json = serde_json::to_vec(&Value::Object(stdin_object.clone()))
            .expect("generated stdin object serializes");

        // The oracle is the generated object itself. Every possible competing
        // stdin category (valid but different, invalid, whitespace, EOF,
        // oversized, and read failure) is forbidden from being read.
        for reader in ignored_stdin_variants(&stdin_json, &whitespace) {
            let mut input = CallInput::new(reader, inline_tty_state);
            let actual = input
                .read(Some(&inline_json))
                .expect("valid inline object must determine the result");
            let reader = input.into_inner();

            prop_assert_eq!(&actual, &inline_object);
            prop_assert_eq!(reader.reads, 0, "inline input must not inspect stdin");
        }

        // A TTY without inline input also ignores every stdin category,
        // including readers that would fail or exceed the size limit.
        for reader in ignored_stdin_variants(&stdin_json, &whitespace) {
            let mut input = CallInput::new(reader, true);
            let actual = input.read(None).expect("TTY input defaults to an object");
            let reader = input.into_inner();

            prop_assert_eq!(actual, JsonObject::new());
            prop_assert_eq!(reader.reads, 0, "TTY input must not inspect stdin");
        }

        // With no inline input and non-TTY stdin, both true EOF and every
        // generated ASCII + Unicode whitespace combination normalize to {}.
        for bytes in [Vec::new(), whitespace.as_bytes().to_vec()] {
            let reader = CountingReader::bytes(bytes);
            let mut input = CallInput::new(reader, false);
            let actual = input
                .read(None)
                .expect("EOF and whitespace-only stdin normalize to an object");
            let reader = input.into_inner();

            prop_assert_eq!(actual, JsonObject::new());
            prop_assert!(reader.reads > 0, "non-TTY stdin must be consumed to EOF");
        }

        // The independent oracle remains the generated map; it does not call
        // the production parser to derive the expected object.
        let reader = CountingReader::bytes(stdin_json);
        let mut input = CallInput::new(reader, false);
        let actual = input
            .read(None)
            .expect("generated non-empty stdin object is valid");
        let reader = input.into_inner();

        prop_assert_eq!(actual, stdin_object);
        prop_assert!(reader.reads > 0, "non-TTY object stdin must be consumed");
    }
}
