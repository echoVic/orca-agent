use serde::{Deserialize, Serialize};

use crate::approval_types::Decision;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PermissionRule {
    pub tool: String,
    pub pattern: String,
    pub decision: Decision,
}

impl PermissionRule {
    pub fn new(tool: impl Into<String>, pattern: impl Into<String>, decision: Decision) -> Self {
        Self {
            tool: tool.into(),
            pattern: pattern.into(),
            decision,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PermissionRules {
    #[serde(default)]
    pub rules: Vec<PermissionRule>,
}

#[derive(Clone, Debug, Default)]
pub struct CompiledPermissionRules {
    rules: Vec<CompiledPermissionRule>,
}

#[derive(Clone, Debug)]
struct CompiledPermissionRule {
    tool: String,
    pattern: CompiledGlob,
    decision: Decision,
}

#[derive(Clone, Debug)]
struct CompiledGlob {
    pattern: Vec<u8>,
}

impl CompiledPermissionRules {
    pub fn from_rules(rules: PermissionRules) -> Self {
        Self {
            rules: rules
                .rules
                .into_iter()
                .map(CompiledPermissionRule::new)
                .collect(),
        }
    }

    pub fn matching_decision(&self, tool: &str, target: Option<&str>) -> Option<Decision> {
        self.rules
            .iter()
            .filter(|rule| rule.matches(tool, target))
            .map(|rule| rule.decision)
            .max()
    }
}

impl CompiledPermissionRule {
    fn new(rule: PermissionRule) -> Self {
        Self {
            tool: rule.tool,
            pattern: CompiledGlob::new(rule.pattern),
            decision: rule.decision,
        }
    }

    fn matches(&self, tool: &str, target: Option<&str>) -> bool {
        if self.tool != tool {
            return false;
        }
        target.is_some_and(|target| self.pattern.matches(target))
    }
}

impl CompiledGlob {
    fn new(pattern: String) -> Self {
        Self {
            pattern: pattern.into_bytes(),
        }
    }

    fn matches(&self, value: &str) -> bool {
        #[cfg(windows)]
        {
            // Windows command names and filesystem paths are case-insensitive.
            // Fold ASCII here because permission patterns are command-line
            // syntax, while preserving byte-level glob semantics elsewhere.
            let pattern = self
                .pattern
                .iter()
                .map(u8::to_ascii_lowercase)
                .collect::<Vec<_>>();
            let value = value
                .as_bytes()
                .iter()
                .map(u8::to_ascii_lowercase)
                .collect::<Vec<_>>();
            return glob_matches(&pattern, &value);
        }
        #[cfg(not(windows))]
        {
            glob_matches(&self.pattern, value.as_bytes())
        }
    }
}

fn glob_matches(pattern: &[u8], value: &[u8]) -> bool {
    glob_match(pattern, 0, value, 0)
}

fn glob_match(pattern: &[u8], mut p: usize, value: &[u8], mut v: usize) -> bool {
    while p < pattern.len() && v < value.len() {
        match pattern[p] {
            b'*' => {
                // Check for ** (matches across directory separators)
                if p + 1 < pattern.len() && pattern[p + 1] == b'*' {
                    let next_p = if p + 2 < pattern.len() && pattern[p + 2] == b'/' {
                        p + 3
                    } else {
                        p + 2
                    };
                    // ** matches zero or more path segments
                    for i in v..=value.len() {
                        if glob_match(pattern, next_p, value, i) {
                            return true;
                        }
                    }
                    return false;
                }
                // Single * does not match /
                p += 1;
                for i in v..=value.len() {
                    if i > v && value[i - 1] == b'/' {
                        break;
                    }
                    if glob_match(pattern, p, value, i) {
                        return true;
                    }
                }
                return false;
            }
            b'?' => {
                if value[v] == b'/' {
                    return false;
                }
                let char_len = match value[v] {
                    b if b < 0x80 => 1,
                    b if b < 0xE0 => 2,
                    b if b < 0xF0 => 3,
                    _ => 4,
                };
                p += 1;
                v += char_len.min(value.len() - v);
            }
            c => {
                if c != value[v] {
                    return false;
                }
                p += 1;
                v += 1;
            }
        }
    }

    while p < pattern.len() && pattern[p] == b'*' {
        if p + 1 < pattern.len() && pattern[p + 1] == b'*' {
            p += 2;
            if p < pattern.len() && pattern[p] == b'/' {
                p += 1;
            }
        } else {
            p += 1;
        }
    }

    p == pattern.len() && v == value.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval_types::Decision;

    #[test]
    fn compiled_permission_rule_matches_tool_and_glob_pattern() {
        let rule =
            CompiledPermissionRule::new(PermissionRule::new("bash", "cargo *", Decision::Allow));

        assert!(rule.matches("bash", Some("cargo test")));
        assert!(!rule.matches("bash", Some("npm test")));
        assert!(!rule.matches("edit", Some("cargo test")));
        assert!(!rule.matches("bash", None));
    }

    #[test]
    fn compiled_permission_rules_cache_globs_for_runtime_matching() {
        let rules = PermissionRules {
            rules: vec![
                PermissionRule::new("bash", "cargo *", Decision::Allow),
                PermissionRule::new("bash", "rm -rf *", Decision::Deny),
            ],
        };

        let compiled = CompiledPermissionRules::from_rules(rules);

        assert_eq!(
            compiled.matching_decision("bash", Some("cargo test")),
            Some(Decision::Allow)
        );
        assert_eq!(
            compiled.matching_decision("bash", Some("rm -rf target")),
            Some(Decision::Deny)
        );
        assert_eq!(compiled.matching_decision("bash", Some("npm test")), None);
    }

    #[test]
    fn compiled_permission_rules_return_strictest_matching_decision() {
        let rules = PermissionRules {
            rules: vec![
                PermissionRule::new("bash", "cargo *", Decision::Allow),
                PermissionRule::new("bash", "cargo publish *", Decision::Prompt),
                PermissionRule::new("bash", "cargo publish secret*", Decision::Deny),
            ],
        };

        let compiled = CompiledPermissionRules::from_rules(rules);

        assert_eq!(
            compiled.matching_decision("bash", Some("cargo publish secret-crate")),
            Some(Decision::Deny)
        );
        assert_eq!(
            compiled.matching_decision("bash", Some("cargo publish public-crate")),
            Some(Decision::Prompt)
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_compiled_rules_match_command_case_insensitively() {
        let compiled = CompiledPermissionRules::from_rules(PermissionRules {
            rules: vec![PermissionRule::new("bash", "cargo *", Decision::Allow)],
        });

        assert_eq!(
            compiled.matching_decision("bash", Some("Cargo test")),
            Some(Decision::Allow)
        );
    }

    #[test]
    fn glob_single_star_does_not_match_path_separator() {
        assert!(glob_matches(b"src/*", b"src/main.rs"));
        assert!(!glob_matches(b"src/*", b"src/nested/main.rs"));
        assert!(glob_matches(b"cargo *", b"cargo test"));
        assert!(!glob_matches(b"cargo *", b"cargo test/nested"));
    }

    #[test]
    fn glob_double_star_matches_across_directories() {
        assert!(glob_matches(b"/etc/**", b"/etc/passwd"));
        assert!(glob_matches(b"/etc/**", b"/etc/ssh/config"));
        assert!(glob_matches(b"/etc/**", b"/etc/deep/nested/path"));
        assert!(!glob_matches(b"/etc/**", b"/usr/bin/env"));
        assert!(glob_matches(b"src/**/*.rs", b"src/foo/bar.rs"));
        assert!(glob_matches(b"src/**/*.rs", b"src/a/b/c.rs"));
    }
}
