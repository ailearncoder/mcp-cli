use mcp_cli::{ToolFilterConfig, policy::tool_filter::ToolFilter};
use proptest::prelude::*;

fn scalar_eq_ignore_case(left: char, right: char) -> bool {
    left == right
        || left.to_lowercase().collect::<String>() == right.to_lowercase().collect::<String>()
        || left.to_uppercase().collect::<String>() == right.to_uppercase().collect::<String>()
}

fn reference_glob_match(pattern: &str, name: &str) -> bool {
    fn visit(
        pattern: &[char],
        name: &[char],
        pattern_index: usize,
        name_index: usize,
        memo: &mut [Vec<Option<bool>>],
    ) -> bool {
        if let Some(result) = memo[pattern_index][name_index] {
            return result;
        }

        let result = match pattern.get(pattern_index) {
            None => name_index == name.len(),
            Some('*') => {
                visit(pattern, name, pattern_index + 1, name_index, memo)
                    || (name_index < name.len()
                        && visit(pattern, name, pattern_index, name_index + 1, memo))
            }
            Some('?') => {
                name_index < name.len()
                    && visit(pattern, name, pattern_index + 1, name_index + 1, memo)
            }
            Some(expected) => {
                name.get(name_index)
                    .is_some_and(|actual| scalar_eq_ignore_case(*expected, *actual))
                    && visit(pattern, name, pattern_index + 1, name_index + 1, memo)
            }
        };

        memo[pattern_index][name_index] = Some(result);
        result
    }

    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let mut memo = vec![vec![None; name.len() + 1]; pattern.len() + 1];
    visit(&pattern, &name, 0, 0, &mut memo)
}

fn reference_is_allowed(name: &str, allowed: &[String], disabled: &[String]) -> bool {
    !disabled
        .iter()
        .any(|pattern| reference_glob_match(pattern, name))
        && (allowed.is_empty()
            || allowed
                .iter()
                .any(|pattern| reference_glob_match(pattern, name)))
}

fn filter(allowed_tools: Vec<String>, disabled_tools: Vec<String>) -> ToolFilter {
    ToolFilter::new(&ToolFilterConfig {
        allowed_tools,
        disabled_tools,
    })
}

fn unicode_scalar() -> impl Strategy<Value = char> {
    prop_oneof![
        8 => any::<char>(),
        2 => Just('/'),
        1 => prop::sample::select(vec!['工', '具', '蟹', '🦀', 'é', 'ß', 'Σ', 'Ж']),
    ]
}

fn literal_pattern_scalar() -> impl Strategy<Value = char> {
    unicode_scalar().prop_filter(
        "literal pattern scalar is not a glob operator",
        |character| !matches!(character, '*' | '?'),
    )
}

fn tool_name() -> impl Strategy<Value = String> {
    prop::collection::vec(unicode_scalar(), 0..16)
        .prop_map(|characters| characters.into_iter().collect())
}

fn tool_pattern() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            6 => literal_pattern_scalar(),
            2 => Just('*'),
            2 => Just('?'),
        ],
        0..12,
    )
    .prop_map(|characters| characters.into_iter().collect())
}

fn pattern_set() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(tool_pattern(), 0..7)
}

fn case_equivalent_pair() -> impl Strategy<Value = (char, char)> {
    prop::sample::select(vec![
        ('a', 'A'),
        ('é', 'É'),
        ('ß', 'ẞ'),
        ('σ', 'Σ'),
        ('ж', 'Ж'),
    ])
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 9: Tool_Filter glob 与授权公式
    // **Validates: Requirements 4.1, 4.2, 4.3, 4.4, 4.5, 4.6**
    #[test]
    fn property_09_tool_filter_glob_and_authorization_formula(
        name in tool_name(),
        allowed in pattern_set(),
        disabled in pattern_set(),
        single_scalar in unicode_scalar(),
        (case_pattern, case_name) in case_equivalent_pair(),
    ) {
        let expected = reference_is_allowed(&name, &allowed, &disabled);
        let actual = filter(allowed.clone(), disabled.clone()).is_allowed(&name);

        prop_assert_eq!(
            actual,
            expected,
            "name={:?}, allowed={:?}, disabled={:?}",
            name,
            allowed,
            disabled,
        );

        // These probes make the key glob semantics mandatory in every case,
        // including slash and non-ASCII Unicode scalar values.
        prop_assert!(filter(vec!["*".into()], vec![]).is_allowed(&name));
        prop_assert!(filter(vec!["父*🦀".into()], vec![]).is_allowed("父/工具/🦀"));
        prop_assert!(filter(vec!["父?工具?🦀".into()], vec![]).is_allowed("父/工具/🦀"));

        let one_scalar = single_scalar.to_string();
        let two_scalars = format!("{single_scalar}{single_scalar}");
        let question_filter = filter(vec!["?".into()], vec![]);
        prop_assert!(question_filter.is_allowed(&one_scalar));
        prop_assert!(!question_filter.is_allowed(""));
        prop_assert!(!question_filter.is_allowed(&two_scalars));

        let case_filter = filter(vec![case_pattern.to_string()], vec![]);
        prop_assert!(case_filter.is_allowed(&case_name.to_string()));

        let empty_pattern_filter = filter(vec![String::new()], vec![]);
        prop_assert!(empty_pattern_filter.is_allowed(""));
        prop_assert!(!empty_pattern_filter.is_allowed("/"));
        prop_assert!(!filter(vec![], vec![String::new()]).is_allowed(""));
        prop_assert!(filter(vec![], vec![String::new()]).is_allowed("/"));

        prop_assert!(filter(vec!["read".into()], vec![]).is_allowed("READ"));
        prop_assert!(!filter(vec!["read".into()], vec![]).is_allowed("prefix_read"));
        prop_assert!(!filter(vec!["read".into()], vec![]).is_allowed("read_suffix"));
    }
}
