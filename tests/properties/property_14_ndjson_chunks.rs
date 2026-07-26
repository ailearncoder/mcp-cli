#![cfg(unix)]
#![forbid(unsafe_code)]

use mcp_cli::daemon::{IPC_MAX_FRAME_SIZE, IpcOperation, IpcRequest, NdjsonCodec, encode_message};
use proptest::{prelude::*, test_runner::TestCaseError};
use serde_json::{Map, Value, json};

const CASES: u32 = 128;

fn text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(
            "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 _-é界工具🦀Жم"
                .chars()
                .collect::<Vec<_>>(),
        ),
        0..=24,
    )
    .prop_map(|characters| characters.into_iter().collect())
}

fn object_key() -> impl Strategy<Value = String> {
    text().prop_map(|key| {
        if key.is_empty() {
            "empty".to_owned()
        } else {
            key
        }
    })
}

fn recursive_json() -> BoxedStrategy<Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|number| json!(number)),
        text().prop_map(Value::String),
    ];

    leaf.prop_recursive(4, 64, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..=5).prop_map(Value::Array),
            prop::collection::btree_map(object_key(), inner, 0..=5)
                .prop_map(|entries| Value::Object(entries.into_iter().collect())),
        ]
    })
    .boxed()
}

fn recursive_args() -> impl Strategy<Value = Map<String, Value>> {
    prop::collection::btree_map(object_key(), recursive_json(), 0..=5)
        .prop_map(|entries| entries.into_iter().collect())
}

fn request_id() -> impl Strategy<Value = String> {
    let generated = prop::collection::vec(
        prop::sample::select(
            "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-é界工具🦀Жم"
                .chars()
                .collect::<Vec<_>>(),
        ),
        1..=32,
    )
    .prop_map(|characters| characters.into_iter().collect::<String>());

    prop_oneof![
        8 => generated,
        1 => Just("a".repeat(128)),
        1 => Just("界".repeat(42)),
        1 => Just("🦀".repeat(32)),
    ]
}

fn tool_name() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(
            "abcdefghijklmnopqrstuvwxyz-工具🦀"
                .chars()
                .collect::<Vec<_>>(),
        ),
        1..=24,
    )
    .prop_map(|characters| characters.into_iter().collect())
}

fn operation() -> impl Strategy<Value = IpcOperation> {
    prop_oneof![
        Just(IpcOperation::Ping),
        Just(IpcOperation::ListTools),
        (tool_name(), recursive_args())
            .prop_map(|(tool_name, args)| IpcOperation::CallTool { tool_name, args }),
        Just(IpcOperation::GetInstructions),
        Just(IpcOperation::Close),
    ]
}

fn request() -> impl Strategy<Value = IpcRequest> {
    (request_id(), operation()).prop_map(|(id, operation)| {
        IpcRequest::new(id, operation).expect("generator only creates valid IPC requests")
    })
}

fn mandatory_requests() -> Vec<IpcRequest> {
    let recursive = json!({
        "level1": {
            "level2": {
                "array": [1, {"level3": [true, null, "终点🦀"]}]
            }
        }
    })
    .as_object()
    .expect("literal object")
    .clone();

    vec![
        IpcRequest::new("p", IpcOperation::Ping).expect("valid lower-bound ID"),
        IpcRequest::new("a".repeat(128), IpcOperation::ListTools).expect("valid 128-byte ASCII ID"),
        IpcRequest::new(
            "界".repeat(42),
            IpcOperation::CallTool {
                tool_name: "递归工具🦀".to_owned(),
                args: recursive,
            },
        )
        .expect("valid Unicode ID and recursive arguments"),
        IpcRequest::new("🦀".repeat(32), IpcOperation::GetInstructions)
            .expect("valid 128-byte Unicode ID"),
        IpcRequest::new("结束", IpcOperation::Close).expect("valid Unicode ID"),
    ]
}

fn build_wire(
    requests: &[IpcRequest],
    generated_crlf: &[bool],
) -> Result<(Vec<u8>, Vec<usize>), TestCaseError> {
    let mut wire = Vec::new();
    let mut frame_lengths = Vec::with_capacity(requests.len());

    for (index, request) in requests.iter().enumerate() {
        let mut frame = encode_message(request).map_err(|error| {
            TestCaseError::fail(format!("encoder rejected valid request: {error}"))
        })?;
        prop_assert_eq!(frame.last(), Some(&b'\n'));
        prop_assert!(frame.len() - 1 <= IPC_MAX_FRAME_SIZE);

        // Every generated stream contains both terminator forms. Remaining
        // frames use generated choices, so LF/CRLF placement still varies.
        let use_crlf = match index {
            0 => false,
            1 => true,
            _ => generated_crlf[index % generated_crlf.len()],
        };
        if use_crlf {
            frame.insert(frame.len() - 1, b'\r');
        }
        frame_lengths.push(frame.len());
        wire.extend(frame);
    }

    Ok((wire, frame_lengths))
}

fn arbitrary_chunk_lengths(total: usize, seeds: &[usize]) -> Vec<usize> {
    assert!(total > 0);
    assert!(!seeds.is_empty());

    let mut lengths = Vec::new();
    let mut remaining = total;
    let mut index = 0;
    while remaining > 0 {
        let length = 1 + seeds[index % seeds.len()] % remaining;
        lengths.push(length);
        remaining -= length;
        index += 1;
    }
    lengths
}

fn lengths_at_offsets(total: usize, offsets: impl IntoIterator<Item = usize>) -> Vec<usize> {
    let mut offsets = offsets
        .into_iter()
        .filter(|offset| *offset > 0 && *offset < total)
        .collect::<Vec<_>>();
    offsets.sort_unstable();
    offsets.dedup();

    let mut previous = 0;
    let mut lengths = offsets
        .into_iter()
        .map(|offset| {
            let length = offset - previous;
            previous = offset;
            length
        })
        .collect::<Vec<_>>();
    lengths.push(total - previous);
    lengths
}

/// Independent partition oracle: it only applies positive byte lengths and
/// proves exact reconstruction. It never asks the production codec where a
/// frame or message boundary lies.
fn partition_by_lengths<'a>(
    wire: &'a [u8],
    lengths: &[usize],
) -> Result<Vec<&'a [u8]>, TestCaseError> {
    prop_assert!(!wire.is_empty());
    prop_assert!(!lengths.is_empty());
    prop_assert!(lengths.iter().all(|length| *length > 0));
    prop_assert_eq!(lengths.iter().sum::<usize>(), wire.len());

    let mut offset = 0;
    let chunks = lengths
        .iter()
        .map(|length| {
            let start = offset;
            offset += length;
            &wire[start..offset]
        })
        .collect::<Vec<_>>();
    prop_assert_eq!(chunks.concat(), wire);
    Ok(chunks)
}

fn assert_partition_round_trip(
    label: &str,
    wire: &[u8],
    lengths: &[usize],
    expected: &[IpcRequest],
) -> Result<(), TestCaseError> {
    let chunks = partition_by_lengths(wire, lengths)?;
    let mut codec = NdjsonCodec::new();
    let mut decoded = Vec::new();

    for chunk in chunks {
        decoded.extend(codec.push_messages::<IpcRequest>(chunk).map_err(|error| {
            TestCaseError::fail(format!("{label} partition failed to decode: {error}"))
        })?);
    }

    let finished = codec
        .finish()
        .map_err(|error| TestCaseError::fail(format!("{label} finish failed: {error}")))?;
    prop_assert_eq!(finished, None, "{} left a partial frame", label);
    prop_assert_eq!(codec.buffered_len(), 0, "{} retained bytes", label);
    prop_assert!(!codec.is_failed(), "{} poisoned the codec", label);
    prop_assert_eq!(&decoded, expected, "{} changed order or duplicates", label);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: CASES,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 14: NDJSON 任意分块 round trip
    // **Validates: Requirements 7.6**
    #[test]
    fn property_14_ndjson_arbitrary_chunk_round_trip(
        random_requests in prop::collection::vec(request(), 0..=8),
        duplicate_index in any::<usize>(),
        partition_seeds in prop::collection::vec(any::<usize>(), 1..=48),
        generated_crlf in prop::collection::vec(any::<bool>(), 1..=16),
    ) {
        let mut requests = mandatory_requests();
        requests.extend(random_requests);
        let duplicate = requests[duplicate_index % requests.len()].clone();
        requests.push(duplicate);

        prop_assert!(!requests.is_empty());
        prop_assert!(
            requests.iter().any(|request| matches!(request.operation(), IpcOperation::Ping)),
            "generated sequence omitted ping"
        );
        prop_assert!(
            requests.iter().any(|request| matches!(request.operation(), IpcOperation::ListTools)),
            "generated sequence omitted listTools"
        );
        let has_call_tool = requests
            .iter()
            .any(|request| matches!(request.operation(), IpcOperation::CallTool { .. }));
        prop_assert!(has_call_tool, "generated sequence omitted callTool");
        prop_assert!(
            requests.iter().any(|request| matches!(request.operation(), IpcOperation::GetInstructions)),
            "generated sequence omitted getInstructions"
        );
        prop_assert!(
            requests.iter().any(|request| matches!(request.operation(), IpcOperation::Close)),
            "generated sequence omitted close"
        );
        let last = requests.last().expect("nonempty");
        let preserves_duplicate = requests[..requests.len() - 1].contains(last);
        prop_assert!(preserves_duplicate, "generated sequence omitted a duplicate");

        let (wire, frame_lengths) = build_wire(&requests, &generated_crlf)?;
        prop_assert!(wire.windows(2).any(|bytes| bytes == b"\r\n"));
        let has_lf = wire.iter().enumerate().any(|(index, byte)| {
            *byte == b'\n' && (index == 0 || wire[index - 1] != b'\r')
        });
        prop_assert!(has_lf, "generated stream omitted an LF terminator");

        // Arbitrary nonempty generated byte partition.
        let arbitrary = arbitrary_chunk_lengths(wire.len(), &partition_seeds);
        assert_partition_round_trip("generated arbitrary", &wire, &arbitrary, &requests)?;

        // Byte-at-a-time forces every possible intra-frame split, including
        // UTF-8 code units and both bytes around every CRLF terminator.
        assert_partition_round_trip(
            "byte-at-a-time",
            &wire,
            &vec![1; wire.len()],
            &requests,
        )?;

        // One chunk is the maximal sticky-packet case: all frames coalesced.
        assert_partition_round_trip("all frames coalesced", &wire, &[wire.len()], &requests)?;

        // Frame-sized chunks make every chunk end exactly at an independently
        // known encoder boundary.
        assert_partition_round_trip("frame boundaries", &wire, &frame_lengths, &requests)?;

        // Split specifically between CR and LF without consulting the decoder.
        let crlf_offsets = wire
            .windows(2)
            .enumerate()
            .filter_map(|(index, bytes)| (bytes == b"\r\n").then_some(index + 1));
        let across_crlf = lengths_at_offsets(wire.len(), crlf_offsets);
        prop_assert!(across_crlf.len() > 1);
        assert_partition_round_trip("across CRLF", &wire, &across_crlf, &requests)?;

        // Mix an intra-frame split with selected whole-frame boundaries so one
        // run simultaneously exercises split and coalesced frames.
        let mut mixed_offsets = vec![frame_lengths[0] / 2];
        let mut boundary = 0;
        for (index, frame_length) in frame_lengths.iter().enumerate() {
            boundary += frame_length;
            if index % 2 == 0 {
                mixed_offsets.push(boundary);
            }
        }
        let mixed = lengths_at_offsets(wire.len(), mixed_offsets);
        assert_partition_round_trip("split and coalesced", &wire, &mixed, &requests)?;
    }
}
