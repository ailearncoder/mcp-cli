use std::collections::{BTreeMap, BTreeSet};

use mcp_cli::connection::direct::merge_stdio_environment;
use proptest::prelude::*;

fn environment_key() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_"
                .chars()
                .collect::<Vec<_>>(),
        ),
        1..13,
    )
    .prop_map(|characters| characters.into_iter().collect())
}

fn environment_value() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789 _/:-=工具🦀${}"
                .chars()
                .collect::<Vec<_>>(),
        ),
        0..33,
    )
    .prop_map(|characters| characters.into_iter().collect())
}

fn environment() -> impl Strategy<Value = BTreeMap<String, String>> {
    prop::collection::btree_map(environment_key(), environment_value(), 0..25)
}

fn reference_merge(
    parent: &BTreeMap<String, String>,
    configured: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    parent
        .keys()
        .chain(configured.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|key| {
            let value = configured
                .get(&key)
                .or_else(|| parent.get(&key))
                .expect("each union key is present in at least one input")
                .clone();
            (key, value)
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 8: stdio 环境合并右侧覆盖
    // **Validates: Requirements 3.10**
    #[test]
    fn property_08_stdio_environment_merge_uses_right_hand_precedence(
        parent in environment(),
        configured in environment(),
    ) {
        let original_parent = parent.clone();
        let original_configured = configured.clone();
        let expected = reference_merge(&parent, &configured);
        let expected_keys = parent
            .keys()
            .chain(configured.keys())
            .cloned()
            .collect::<BTreeSet<_>>();

        let merged = merge_stdio_environment(&parent, &configured);
        let repeated = merge_stdio_environment(&parent, &configured);
        let actual_keys = merged.keys().cloned().collect::<BTreeSet<_>>();

        prop_assert_eq!(&actual_keys, &expected_keys, "result keys must be exactly the input union");
        prop_assert_eq!(&merged, &expected, "result must match the independent right-biased oracle");

        for (key, parent_value) in &parent {
            if !configured.contains_key(key) {
                prop_assert_eq!(
                    merged.get(key),
                    Some(parent_value),
                    "a parent-only key must retain its parent value",
                );
            }
        }

        for (key, configured_value) in &configured {
            prop_assert_eq!(
                merged.get(key),
                Some(configured_value),
                "every configured key must use the configured value",
            );
        }

        prop_assert_eq!(&parent, &original_parent, "the parent input must not be modified");
        prop_assert_eq!(
            &configured,
            &original_configured,
            "the configured input must not be modified",
        );
        prop_assert_eq!(&merged, &repeated, "identical inputs must produce an identical result");
    }
}
