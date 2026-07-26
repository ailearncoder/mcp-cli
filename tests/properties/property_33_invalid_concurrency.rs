use mcp_cli::{ErrorKind, RuntimeConfig};
use proptest::prelude::*;

const CONCURRENCY_VARIABLE: &str = "MCP_CONCURRENCY";
const DEFAULT_CONCURRENCY: usize = 5;

fn positive_decimal() -> impl Strategy<Value = String> {
    (1_usize..=usize::MAX).prop_map(|value| value.to_string())
}

fn zero_decimal() -> impl Strategy<Value = String> {
    prop::collection::vec(Just('0'), 1..=16).prop_map(|digits| digits.into_iter().collect())
}

fn signed_negative() -> impl Strategy<Value = String> {
    positive_decimal().prop_map(|digits| format!("-{digits}"))
}

fn non_numeric() -> impl Strategy<Value = String> {
    "[A-Za-z_!@#]{1,32}"
}

fn mixed_or_trailing_characters() -> impl Strategy<Value = String> {
    prop_oneof![
        (positive_decimal(), "[A-Za-z_!@#]{1,12}")
            .prop_map(|(digits, suffix)| format!("{digits}{suffix}")),
        (positive_decimal(), "[A-Za-z_!@#]{1,12}", "[0-9]{1,12}")
            .prop_map(|(prefix, middle, suffix)| format!("{prefix}{middle}{suffix}")),
    ]
}

fn whitespace_contaminated() -> impl Strategy<Value = String> {
    prop_oneof![
        "[ \\t\\r\\n]{1,16}".prop_map(|whitespace| whitespace),
        ("[ \\t\\r\\n]{1,8}", positive_decimal())
            .prop_map(|(whitespace, digits)| format!("{whitespace}{digits}")),
        (positive_decimal(), "[ \\t\\r\\n]{1,8}")
            .prop_map(|(digits, whitespace)| format!("{digits}{whitespace}")),
    ]
}

fn overflowing_decimal() -> impl Strategy<Value = String> {
    "[0-9]{1,32}".prop_map(|suffix| format!("{}{suffix}", usize::MAX))
}

fn assert_invalid_concurrency(value: &str) -> Result<(), TestCaseError> {
    match RuntimeConfig::parse([(CONCURRENCY_VARIABLE, value)]) {
        Err(error) => {
            prop_assert_eq!(error.kind, ErrorKind::InvalidRuntimeConfig);
            prop_assert!(
                error
                    .details
                    .as_deref()
                    .is_some_and(|details| details.contains(CONCURRENCY_VARIABLE)),
                "error details did not identify {CONCURRENCY_VARIABLE}: {:?}",
                error.details
            );
            Ok(())
        }
        Ok(config) => Err(TestCaseError::fail(format!(
            "invalid {CONCURRENCY_VARIABLE}={value:?} was accepted with concurrency={} (default={DEFAULT_CONCURRENCY})",
            config.concurrency.get()
        ))),
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 33: 非法并发配置总被拒绝
    // **Validates: Requirements 14.2**
    #[test]
    fn property_33_invalid_concurrency_is_always_rejected(
        zero in zero_decimal(),
        negative in signed_negative(),
        non_digit in non_numeric(),
        mixed_or_trailing in mixed_or_trailing_characters(),
        whitespace in whitespace_contaminated(),
        overflow in overflowing_decimal(),
        valid in 1_usize..=usize::MAX,
    ) {
        for invalid in [
            zero,
            negative,
            non_digit,
            mixed_or_trailing,
            whitespace,
            overflow,
        ] {
            assert_invalid_concurrency(&invalid)?;
        }

        let valid_config = RuntimeConfig::parse([(
            CONCURRENCY_VARIABLE,
            valid.to_string(),
        )])
        .map_err(|error| TestCaseError::fail(format!(
            "valid positive concurrency {valid} was rejected: {error:?}"
        )))?;
        prop_assert_eq!(valid_config.concurrency.get(), valid);
    }
}
