//! Tool visibility and authorization policy.

use crate::{config::ToolFilterConfig, domain::ToolInfo};

/// A compiled, anchored glob pattern for a complete tool name.
///
/// Only `*` and `?` have special meaning. `*` consumes zero or more Unicode
/// scalar values (including `/`), while `?` consumes exactly one Unicode
/// scalar value. Every other character is matched literally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolPattern {
    tokens: Vec<PatternToken>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PatternToken {
    AnySequence,
    AnyScalar,
    Literal(char),
}

impl ToolPattern {
    /// Compiles a tool glob. Compilation is infallible because non-glob
    /// characters never acquire regular-expression semantics.
    pub fn new(pattern: &str) -> Self {
        let mut tokens = Vec::with_capacity(pattern.chars().count());

        for character in pattern.chars() {
            let token = match character {
                '*' => PatternToken::AnySequence,
                '?' => PatternToken::AnyScalar,
                literal => PatternToken::Literal(literal),
            };

            // Adjacent stars are equivalent to one star and needlessly expand
            // the matcher state, so normalize them while compiling.
            if token != PatternToken::AnySequence
                || tokens.last() != Some(&PatternToken::AnySequence)
            {
                tokens.push(token);
            }
        }

        Self { tokens }
    }

    /// Returns whether this pattern matches the complete tool name.
    pub fn is_match(&self, tool_name: &str) -> bool {
        let name: Vec<char> = tool_name.chars().collect();
        let mut pattern_index = 0;
        let mut name_index = 0;
        let mut last_star = None;
        let mut star_match_end = 0;

        while name_index < name.len() {
            match self.tokens.get(pattern_index) {
                Some(PatternToken::AnyScalar) => {
                    pattern_index += 1;
                    name_index += 1;
                }
                Some(PatternToken::Literal(expected))
                    if scalar_eq_ignore_case(*expected, name[name_index]) =>
                {
                    pattern_index += 1;
                    name_index += 1;
                }
                Some(PatternToken::AnySequence) => {
                    last_star = Some(pattern_index);
                    pattern_index += 1;
                    star_match_end = name_index;
                }
                _ => {
                    let Some(star_index) = last_star else {
                        return false;
                    };

                    // Retry the suffix after allowing the most recent `*` to
                    // consume one additional Unicode scalar value.
                    star_match_end += 1;
                    name_index = star_match_end;
                    pattern_index = star_index + 1;
                }
            }
        }

        while self.tokens.get(pattern_index) == Some(&PatternToken::AnySequence) {
            pattern_index += 1;
        }

        pattern_index == self.tokens.len()
    }
}

impl From<&str> for ToolPattern {
    fn from(pattern: &str) -> Self {
        Self::new(pattern)
    }
}

impl From<String> for ToolPattern {
    fn from(pattern: String) -> Self {
        Self::new(&pattern)
    }
}

/// A value that exposes the complete MCP tool name used for authorization.
pub trait ToolNamed {
    fn tool_name(&self) -> &str;
}

impl ToolNamed for ToolInfo {
    fn tool_name(&self) -> &str {
        &self.name
    }
}

/// Compiled visibility and invocation policy for one server.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolFilter {
    allowed: Vec<ToolPattern>,
    disabled: Vec<ToolPattern>,
}

impl ToolFilter {
    /// Compiles the existing configuration model into a reusable policy.
    pub fn new(config: &ToolFilterConfig) -> Self {
        Self {
            allowed: config
                .allowed_tools
                .iter()
                .map(|pattern| ToolPattern::new(pattern))
                .collect(),
            disabled: config
                .disabled_tools
                .iter()
                .map(|pattern| ToolPattern::new(pattern))
                .collect(),
        }
    }

    /// Returns the single authorization decision shared by discovery and call.
    pub fn is_allowed(&self, tool_name: &str) -> bool {
        if self
            .disabled
            .iter()
            .any(|pattern| pattern.is_match(tool_name))
        {
            return false;
        }

        self.allowed.is_empty()
            || self
                .allowed
                .iter()
                .any(|pattern| pattern.is_match(tool_name))
    }

    /// Removes unauthorized tools without changing the order of the survivors.
    pub fn filter<T: ToolNamed>(&self, tools: Vec<T>) -> Vec<T> {
        tools
            .into_iter()
            .filter(|tool| self.is_allowed(tool.tool_name()))
            .collect()
    }
}

impl From<&ToolFilterConfig> for ToolFilter {
    fn from(config: &ToolFilterConfig) -> Self {
        Self::new(config)
    }
}

impl From<ToolFilterConfig> for ToolFilter {
    fn from(config: ToolFilterConfig) -> Self {
        Self::new(&config)
    }
}

fn scalar_eq_ignore_case(left: char, right: char) -> bool {
    left == right
        || left.to_lowercase().eq(right.to_lowercase())
        || left.to_uppercase().eq(right.to_uppercase())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn config(allowed: &[&str], disabled: &[&str]) -> ToolFilterConfig {
        ToolFilterConfig {
            allowed_tools: allowed
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect(),
            disabled_tools: disabled
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect(),
        }
    }

    fn tool(name: &str) -> ToolInfo {
        ToolInfo {
            name: name.to_owned(),
            description: None,
            input_schema: json!({}),
        }
    }

    #[test]
    fn empty_configuration_allows_every_name_and_empty_pattern_is_anchored() {
        let filter = ToolFilter::new(&ToolFilterConfig::default());

        assert!(filter.is_allowed(""));
        assert!(filter.is_allowed("any/tool"));
        assert!(ToolPattern::new("").is_match(""));
        assert!(!ToolPattern::new("").is_match("tool"));
        assert!(ToolPattern::new("*").is_match(""));
        assert!(ToolPattern::new("read").is_match("read"));
        assert!(!ToolPattern::new("read").is_match("read_file"));
        assert!(!ToolPattern::new("read").is_match("prefix_read"));
    }

    #[test]
    fn matching_is_case_insensitive_and_regex_characters_are_literal() {
        assert!(ToolPattern::new("READ.+(FILE)[0]").is_match("read.+(file)[0]"));
        assert!(ToolPattern::new("CAFÉ").is_match("café"));
        assert!(!ToolPattern::new("read.+").is_match("read_anything"));
        assert!(!ToolPattern::new("[ab]").is_match("a"));
    }

    #[test]
    fn question_mark_consumes_exactly_one_unicode_scalar() {
        let pattern = ToolPattern::new("工具/?");

        assert!(pattern.is_match("工具/蟹"));
        assert!(pattern.is_match("工具/🦀"));
        assert!(!pattern.is_match("工具/"));
        assert!(!pattern.is_match("工具/🦀蟹"));
    }

    #[test]
    fn star_can_span_slashes_and_question_mark_can_match_one_slash() {
        assert!(ToolPattern::new("server*tool").is_match("server/a/b/tool"));
        assert!(ToolPattern::new("server?tool").is_match("server/tool"));
        assert!(!ToolPattern::new("server?tool").is_match("server/a/tool"));
    }

    #[test]
    fn disabled_patterns_take_precedence_over_allowed_patterns() {
        let filter = ToolFilter::new(&config(&["READ_*", "write_*"], &["*/secret", "read_*"]));

        assert!(!filter.is_allowed("read_file"));
        assert!(!filter.is_allowed("READ_SECRET"));
        assert!(filter.is_allowed("write_file"));
        assert!(!filter.is_allowed("delete_file"));
    }

    #[test]
    fn empty_allowed_list_defaults_to_allow_unless_disabled() {
        let filter = ToolFilter::new(&config(&[], &["danger*"]));

        assert!(filter.is_allowed("safe/tool"));
        assert!(!filter.is_allowed("DANGER/tool"));
    }

    #[test]
    fn filter_is_the_stable_subsequence_selected_by_is_allowed() {
        let filter = ToolFilter::new(&config(&["*file*"], &["delete_*", "private/*"]));
        let tools = vec![
            tool("write_file"),
            tool("delete_file"),
            tool("search_files"),
            tool("private/file"),
            tool("read_file"),
        ];
        let expected: Vec<String> = tools
            .iter()
            .filter(|tool| filter.is_allowed(&tool.name))
            .map(|tool| tool.name.clone())
            .collect();

        let actual: Vec<String> = filter
            .filter(tools)
            .into_iter()
            .map(|tool| tool.name)
            .collect();

        assert_eq!(actual, expected);
        assert_eq!(actual, ["write_file", "search_files", "read_file"]);
    }
}
