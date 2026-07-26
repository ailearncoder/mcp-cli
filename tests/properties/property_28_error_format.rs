use mcp_cli::{CliError, ErrorKind, render_structured_error};
use proptest::prelude::*;

const TEXT_CHARACTERS: &[char] = &[
    'a', 'b', 'c', 'D', 'E', 'F', '0', '1', '2', ' ', '\t', '-', '_', '/', '.', ',', '!', '?', '[',
    ']', '(', ')', '中', '文', 'é', 'ß', 'λ', '🙂',
];

fn label_safe_text() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(TEXT_CHARACTERS.to_vec()), 0..=64)
        .prop_map(|characters| characters.into_iter().collect())
}

fn expected_bytes(
    kind: ErrorKind,
    message: &str,
    details: Option<&str>,
    suggestion: Option<&str>,
) -> Vec<u8> {
    let mut expected = Vec::new();
    expected.extend_from_slice(b"Error [");
    expected.extend_from_slice(kind.as_str().as_bytes());
    expected.extend_from_slice(b"]: ");
    expected.extend_from_slice(message.as_bytes());
    if let Some(details) = details {
        expected.extend_from_slice(b"\n  Details: ");
        expected.extend_from_slice(details.as_bytes());
    }
    if let Some(suggestion) = suggestion {
        expected.extend_from_slice(b"\n  Suggestion: ");
        expected.extend_from_slice(suggestion.as_bytes());
    }
    expected.push(b'\n');
    expected
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 28: Structured_Error 格式
    // **Validates: Requirements 12.1, 12.2, 12.3**
    #[test]
    fn property_28_structured_error_has_exact_stable_format(
        message in label_safe_text(),
        details in prop::option::of(label_safe_text()),
        suggestion in prop::option::of(label_safe_text()),
    ) {
        prop_assert!(!message.contains(['\n', '\r', ':']));
        prop_assert!(details.as_deref().is_none_or(|value| !value.contains(['\n', '\r', ':'])));
        prop_assert!(suggestion.as_deref().is_none_or(|value| !value.contains(['\n', '\r', ':'])));

        for kind in ErrorKind::ALL {
            let mut error = CliError::from_kind(kind, message.clone());
            if let Some(details) = &details {
                error = error.with_details(details.clone());
            }
            if let Some(suggestion) = &suggestion {
                error = error.with_suggestion(suggestion.clone());
            }

            let mut actual = Vec::new();
            render_structured_error(&mut actual, &error).expect("Vec writes cannot fail");

            let expected = expected_bytes(
                kind,
                &message,
                details.as_deref(),
                suggestion.as_deref(),
            );
            prop_assert_eq!(&actual, &expected, "wrong bytes for {}", kind);

            prop_assert_eq!(actual.last(), Some(&b'\n'));
            prop_assert!(!actual.ends_with(b"\n\n"));
            prop_assert_eq!(
                actual.iter().filter(|&&byte| byte == b'\n').count(),
                1 + usize::from(details.is_some()) + usize::from(suggestion.is_some()),
            );

            let rendered = std::str::from_utf8(&actual).expect("renderer preserves UTF-8 inputs");
            let lines = rendered.strip_suffix('\n').expect("one trailing newline");
            let lines = lines.split('\n').collect::<Vec<_>>();
            let expected_first_line = format!("Error [{}]: {message}", kind.as_str());
            prop_assert_eq!(lines.first().copied(), Some(expected_first_line.as_str()));
            prop_assert_eq!(
                rendered.matches("  Details: ").count(),
                usize::from(details.is_some()),
            );
            prop_assert_eq!(
                rendered.matches("  Suggestion: ").count(),
                usize::from(suggestion.is_some()),
            );

            let mut next_line = 1;
            if let Some(details) = &details {
                let expected_line = format!("  Details: {details}");
                prop_assert_eq!(lines.get(next_line).copied(), Some(expected_line.as_str()));
                next_line += 1;
            }
            if let Some(suggestion) = &suggestion {
                let expected_line = format!("  Suggestion: {suggestion}");
                prop_assert_eq!(lines.get(next_line).copied(), Some(expected_line.as_str()));
                next_line += 1;
            }
            prop_assert_eq!(lines.len(), next_line);
        }
    }
}
