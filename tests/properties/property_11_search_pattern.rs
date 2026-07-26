use mcp_cli::policy::search_glob::SearchMatcher;
use proptest::prelude::*;

fn scalar_eq_ignore_case(left: char, right: char) -> bool {
    left == right
        || left.to_lowercase().collect::<String>() == right.to_lowercase().collect::<String>()
        || left.to_uppercase().collect::<String>() == right.to_uppercase().collect::<String>()
}

fn reference_search_match(pattern: &str, name: &str) -> bool {
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
                let star_count = pattern[pattern_index..]
                    .iter()
                    .take_while(|character| **character == '*')
                    .count();
                let next_pattern_index = pattern_index + star_count;
                let may_cross_slash = star_count >= 2;

                visit(pattern, name, next_pattern_index, name_index, memo)
                    || (name_index < name.len()
                        && (may_cross_slash || name[name_index] != '/')
                        && visit(pattern, name, pattern_index, name_index + 1, memo))
            }
            Some('?') => {
                name.get(name_index)
                    .is_some_and(|character| *character != '/')
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

fn production_search_match(pattern: &str, name: &str) -> bool {
    SearchMatcher::compile(pattern)
        .expect("search glob compilation is infallible")
        .is_match(name)
}

fn unicode_scalar() -> impl Strategy<Value = char> {
    prop_oneof![
        7 => any::<char>(),
        2 => prop::sample::select(vec!['工', '具', '蟹', '🦀', 'é', 'ß', 'Σ', 'Ж']),
        1 => Just('/'),
    ]
}

fn literal_pattern_scalar() -> impl Strategy<Value = char> {
    prop_oneof![
        7 => unicode_scalar().prop_filter(
            "literal pattern scalar is not a glob operator",
            |character| !matches!(character, '*' | '?'),
        ),
        3 => prop::sample::select(r#".^$+()[]{}|\\"#.chars().collect::<Vec<_>>()),
    ]
}

fn tool_name() -> impl Strategy<Value = String> {
    prop_oneof![
        8 => prop::collection::vec(unicode_scalar(), 0..20)
            .prop_map(|characters| characters.into_iter().collect()),
        1 => Just(String::new()),
        1 => Just("父/工具/🦀".to_owned()),
    ]
}

fn search_pattern() -> impl Strategy<Value = String> {
    let atom = prop_oneof![
        6 => literal_pattern_scalar().prop_map(|character| character.to_string()),
        2 => Just("?".to_owned()),
        2 => (1_usize..=6).prop_map(|length| "*".repeat(length)),
    ];

    prop_oneof![
        8 => prop::collection::vec(atom, 0..14).prop_map(|atoms| atoms.concat()),
        1 => Just(String::new()),
        1 => (1_usize..=6).prop_map(|length| "*".repeat(length)),
    ]
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

fn regex_metacharacter() -> impl Strategy<Value = char> {
    prop::sample::select(r#".^$+()[]{}|\\"#.chars().collect::<Vec<_>>())
}

fn non_slash_unicode_scalar() -> impl Strategy<Value = char> {
    prop::sample::select(vec!['工', '蟹', '🦀', 'é', 'ß', 'Σ', 'Ж'])
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    // Feature: mcp-cli, Property 11: Search_Pattern 语义
    // **Validates: Requirements 10.1, 10.2, 10.3, 10.4, 10.5**
    #[test]
    fn property_11_search_pattern_semantics(
        pattern in search_pattern(),
        name in tool_name(),
        (case_pattern, case_name) in case_equivalent_pair(),
        regex_literal in regex_metacharacter(),
        unicode_character in non_slash_unicode_scalar(),
        star_run_length in 1_usize..=6,
    ) {
        let expected = reference_search_match(&pattern, &name);
        let actual = production_search_match(&pattern, &name);

        prop_assert_eq!(
            actual,
            expected,
            "pattern={:?}, name={:?}",
            pattern,
            name,
        );

        // Full matching and empty inputs remain anchored.
        prop_assert!(production_search_match("", ""));
        prop_assert!(!production_search_match("", "工具"));
        prop_assert!(production_search_match("工具", "工具"));
        prop_assert!(!production_search_match("工具", "前工具"));
        prop_assert!(!production_search_match("工具", "工具后"));

        // A single star cannot cross slash, while every run of two or more can.
        prop_assert!(production_search_match("*", ""));
        prop_assert!(!production_search_match("*", "父/工具"));
        prop_assert!(production_search_match("**", "父/工具"));
        let star_run_pattern = format!("父{}蟹", "*".repeat(star_run_length));
        let slash_name = "父/工具/蟹";
        prop_assert_eq!(
            production_search_match(&star_run_pattern, slash_name),
            star_run_length >= 2,
        );
        prop_assert!(production_search_match(&star_run_pattern, "父工具蟹"));

        // Question mark consumes one Unicode scalar, but never slash.
        let unicode_name = unicode_character.to_string();
        prop_assert!(production_search_match("?", &unicode_name));
        prop_assert!(!production_search_match("?", ""));
        prop_assert!(!production_search_match("?", "/"));
        prop_assert!(!production_search_match("?", "工具"));

        // Matching is case-insensitive for both ASCII and non-ASCII scalars.
        prop_assert!(production_search_match(
            &case_pattern.to_string(),
            &case_name.to_string(),
        ));

        // Non-glob regular-expression metacharacters are ordinary literals.
        let literal_pattern = regex_literal.to_string();
        prop_assert!(production_search_match(&literal_pattern, &literal_pattern));
        prop_assert!(!production_search_match(&literal_pattern, "x"));
        let all_regex_literals = r#".^$+()[]{}|\\"#;
        prop_assert!(production_search_match(all_regex_literals, all_regex_literals));
    }
}
