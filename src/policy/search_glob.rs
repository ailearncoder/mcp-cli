//! Search glob matching policy.

use crate::error::CliError;

/// A compiled, case-insensitive glob matched against a complete tool name.
///
/// A single `*` consumes zero or more Unicode scalar values other than `/`.
/// A run of two or more `*` characters is normalized to one globstar, which
/// may also consume `/`. `?` consumes exactly one non-`/` Unicode scalar, and
/// every other character is matched literally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchMatcher {
    tokens: Vec<SearchToken>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchToken {
    SegmentSequence,
    Globstar,
    AnyScalar,
    Literal(char),
}

impl SearchMatcher {
    /// Compiles a search glob into an anchored matcher.
    ///
    /// Compilation is currently infallible because every non-glob character
    /// is treated literally. The `Result` is retained as the stable public API
    /// for callers that precompile patterns before starting search work.
    pub fn compile(pattern: &str) -> Result<Self, CliError> {
        let mut tokens = Vec::with_capacity(pattern.chars().count());
        let mut characters = pattern.chars().peekable();

        while let Some(character) = characters.next() {
            let token = match character {
                '*' => {
                    let mut star_count = 1;
                    while characters.next_if_eq(&'*').is_some() {
                        star_count += 1;
                    }

                    if star_count == 1 {
                        SearchToken::SegmentSequence
                    } else {
                        SearchToken::Globstar
                    }
                }
                '?' => SearchToken::AnyScalar,
                literal => SearchToken::Literal(literal),
            };
            tokens.push(token);
        }

        Ok(Self { tokens })
    }

    /// Returns whether this pattern matches the complete tool name.
    pub fn is_match(&self, tool_name: &str) -> bool {
        let name: Vec<char> = tool_name.chars().collect();
        let mut previous = vec![false; name.len() + 1];
        previous[0] = true;

        for token in &self.tokens {
            let mut current = vec![false; name.len() + 1];

            match token {
                SearchToken::SegmentSequence => {
                    current[0] = previous[0];
                    for index in 1..=name.len() {
                        current[index] =
                            previous[index] || (current[index - 1] && name[index - 1] != '/');
                    }
                }
                SearchToken::Globstar => {
                    current[0] = previous[0];
                    for index in 1..=name.len() {
                        current[index] = previous[index] || current[index - 1];
                    }
                }
                SearchToken::AnyScalar => {
                    for index in 1..=name.len() {
                        current[index] = previous[index - 1] && name[index - 1] != '/';
                    }
                }
                SearchToken::Literal(expected) => {
                    for index in 1..=name.len() {
                        current[index] = previous[index - 1]
                            && scalar_eq_ignore_case(*expected, name[index - 1]);
                    }
                }
            }

            previous = current;
        }

        previous[name.len()]
    }
}

fn scalar_eq_ignore_case(left: char, right: char) -> bool {
    left == right
        || left.to_lowercase().eq(right.to_lowercase())
        || left.to_uppercase().eq(right.to_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(pattern: &str) -> SearchMatcher {
        SearchMatcher::compile(pattern).expect("search glob compilation is infallible")
    }

    #[test]
    fn empty_patterns_and_matches_are_anchored() {
        assert!(matcher("").is_match(""));
        assert!(!matcher("").is_match("tool"));
        assert!(matcher("read").is_match("read"));
        assert!(!matcher("read").is_match("read_file"));
        assert!(!matcher("read").is_match("prefix_read"));
        assert!(matcher("*").is_match(""));
        assert!(matcher("**").is_match(""));
    }

    #[test]
    fn matching_is_case_insensitive_for_unicode_scalars() {
        assert!(matcher("READ/CAFÉ").is_match("read/café"));
        assert!(matcher("工具/ẞ").is_match("工具/ß"));
        assert!(!matcher("工具").is_match("工貝"));
    }

    #[test]
    fn single_star_matches_only_within_a_path_segment() {
        assert!(matcher("server/*/tool").is_match("server/alpha/tool"));
        assert!(matcher("server/*/tool").is_match("server//tool"));
        assert!(!matcher("server/*/tool").is_match("server/a/b/tool"));
        assert!(!matcher("*").is_match("alpha/beta"));
        assert!(matcher("a*b").is_match("ab"));
        assert!(matcher("a*b").is_match("a工具b"));
    }

    #[test]
    fn two_or_more_stars_form_one_globstar_that_crosses_slashes() {
        for pattern in ["**", "***", "****"] {
            assert!(matcher(pattern).is_match("alpha/beta/gamma"));
            assert!(matcher(pattern).is_match(""));
        }

        assert!(matcher("server/**/tool").is_match("server/a/b/tool"));
        assert!(matcher("server/**tool").is_match("server/a/b/tool"));
        assert!(!matcher("server/**/tool").is_match("server/tool"));
    }

    #[test]
    fn question_mark_matches_exactly_one_non_slash_unicode_scalar() {
        let pattern = matcher("工具/?");

        assert!(pattern.is_match("工具/蟹"));
        assert!(pattern.is_match("工具/🦀"));
        assert!(!pattern.is_match("工具/"));
        assert!(!pattern.is_match("工具//"));
        assert!(!pattern.is_match("工具/🦀蟹"));
    }

    #[test]
    fn regular_expression_metacharacters_are_all_literal() {
        let literal = r".^$+()[]{}|\\";

        assert!(matcher(literal).is_match(literal));
        assert!(matcher("read.+(file)[0]{1}|end").is_match("READ.+(FILE)[0]{1}|END"));
        assert!(!matcher("read.+").is_match("read_anything"));
        assert!(!matcher("[ab]").is_match("a"));
        assert!(!matcher("a|b").is_match("a"));
    }

    #[test]
    fn mixed_wildcards_preserve_slash_boundaries_and_full_anchoring() {
        let pattern = matcher("root/**/item-?.*");

        assert!(pattern.is_match("ROOT/a/b/item-蟹.json"));
        assert!(!pattern.is_match("prefix/root/a/b/item-x.json"));
        assert!(!pattern.is_match("root/a/b/item-/json"));
        assert!(!pattern.is_match("root/a/b/item-xy.json"));
    }
}
