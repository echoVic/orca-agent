// Subagent 专用代理类型
// Phase 4: 高级特性 - 专用代理
//
// Every built-in role is described exactly once in `BuiltinAgentDescriptor` and
// that single source feeds the model-visible catalog, the tool schema enum, the
// runtime tool ceiling, and the child system prompt. Custom agents keep their
// own frozen definition and only reuse the rendering helpers.

use serde::{Deserialize, Serialize};

#[path = "agent_definition.rs"]
pub mod agent_definition;

/// 专用子代理类型
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubagentType {
    /// 通用代理（默认）
    General,
    /// 只读代码探索代理
    Explorer,
    /// 代码审查专家
    CodeReviewer,
    /// 测试编写专家
    TestWriter,
    /// 调试专家
    Debugger,
    /// 文档编写专家
    Documenter,
    /// 自定义类型
    Custom(String),
}

impl Default for SubagentType {
    fn default() -> Self {
        Self::General
    }
}

/// Which filesystem/process capabilities a role may use.
///
/// This is reported to the model and used by tests; the enforced ceiling is
/// always the explicit tool list from [`BuiltinAgentDescriptor::tools`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoleCapability {
    /// Filesystem and process mutation through edit/write tools.
    Write,
    /// Shell or process execution.
    Execute,
}

/// How much conversation context the role receives. Phase one ships only
/// `Fresh`; `Fork` is reserved for the bounded-context work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoleContextPolicy {
    /// A standalone brief with the current system prompt only.
    Fresh,
}

/// Everything the runtime knows about one built-in role.
#[derive(Clone, Copy, Debug)]
pub struct BuiltinAgentDescriptor {
    /// Canonical role kind.
    pub kind: BuiltinAgentKind,
    /// Canonical identifier used in the tool schema enum.
    pub name: &'static str,
    /// Accepted aliases; the canonical name is always accepted.
    pub aliases: &'static [&'static str],
    /// Short selection hint shown in the tool description.
    pub when_to_use: &'static str,
    /// Short negative hint shown alongside the selection hint.
    pub avoid_when: &'static str,
    /// Explicit capability ceiling. This is the runtime-enforced tool list.
    pub tools: &'static [&'static str],
    /// Capabilities the role must never use.
    pub forbidden: &'static [RoleCapability],
    /// Context assembly policy.
    pub context: RoleContextPolicy,
    /// Required items in the role's final report.
    pub deliverables: &'static [&'static str],
    /// Role instructions appended to the child system prompt.
    pub prompt: &'static str,
}

/// The copyable subset of [`SubagentType`] that names a built-in role.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuiltinAgentKind {
    General,
    Explorer,
    CodeReviewer,
    TestWriter,
    Debugger,
    Documenter,
}

impl BuiltinAgentKind {
    pub fn subagent_type(self) -> SubagentType {
        match self {
            Self::General => SubagentType::General,
            Self::Explorer => SubagentType::Explorer,
            Self::CodeReviewer => SubagentType::CodeReviewer,
            Self::TestWriter => SubagentType::TestWriter,
            Self::Debugger => SubagentType::Debugger,
            Self::Documenter => SubagentType::Documenter,
        }
    }
}

impl BuiltinAgentDescriptor {
    /// A role is read-only when it may neither write nor execute.
    pub fn is_read_only(&self) -> bool {
        self.forbidden.contains(&RoleCapability::Write)
            && self.forbidden.contains(&RoleCapability::Execute)
    }

    /// Whether the role may modify files.
    pub fn may_write(&self) -> bool {
        !self.forbidden.contains(&RoleCapability::Write)
    }

    /// Whether the role may run processes or shell commands.
    pub fn may_execute(&self) -> bool {
        !self.forbidden.contains(&RoleCapability::Execute)
    }
}

impl SubagentType {
    pub fn is_builtin_name(name: &str) -> bool {
        !matches!(Self::from_str(name), Self::Custom(_))
    }

    pub fn identifier(&self) -> &str {
        self.builtin().map_or_else(
            || match self {
                Self::Custom(name) => name.as_str(),
                _ => unreachable!("every built-in role has a descriptor"),
            },
            |descriptor| descriptor.name,
        )
    }

    /// The single built-in descriptor for this role, or `None` for custom agents.
    pub fn builtin(&self) -> Option<&'static BuiltinAgentDescriptor> {
        let kind = self.builtin_kind()?;
        builtin_agents()
            .iter()
            .find(|descriptor| descriptor.kind == kind)
    }

    /// The copyable kind for a built-in role, or `None` for custom agents.
    pub fn builtin_kind(&self) -> Option<BuiltinAgentKind> {
        Some(match self {
            Self::General => BuiltinAgentKind::General,
            Self::Explorer => BuiltinAgentKind::Explorer,
            Self::CodeReviewer => BuiltinAgentKind::CodeReviewer,
            Self::TestWriter => BuiltinAgentKind::TestWriter,
            Self::Debugger => BuiltinAgentKind::Debugger,
            Self::Documenter => BuiltinAgentKind::Documenter,
            Self::Custom(_) => return None,
        })
    }

    /// Role instructions appended to the child system prompt.
    pub fn system_prompt_suffix(&self) -> &'static str {
        self.builtin().map_or("", |descriptor| descriptor.prompt)
    }

    /// The role's enforced tool ceiling.
    ///
    /// Custom agents carry an explicit frozen definition instead, so they have
    /// no implicit ceiling and must never inherit the general role's tools.
    pub fn allowed_tools(&self) -> Vec<&'static str> {
        self.builtin()
            .map(|descriptor| descriptor.tools.to_vec())
            .unwrap_or_default()
    }

    /// 从字符串解析
    pub fn from_str(s: &str) -> Self {
        let normalized = s.trim().to_lowercase();
        for descriptor in builtin_agents() {
            if descriptor.name == normalized || descriptor.aliases.contains(&normalized.as_str()) {
                return descriptor.kind.subagent_type();
            }
        }
        Self::Custom(s.to_string())
    }
}

/// The built-in role catalog: the single source of truth for names, aliases,
/// tool ceilings, and prompts.
pub fn builtin_agents() -> &'static [BuiltinAgentDescriptor] {
    BUILTIN_AGENTS
}

/// Resolves a built-in role by any accepted name or alias.
pub fn resolve_builtin_agent(name: &str) -> Option<&'static BuiltinAgentDescriptor> {
    SubagentType::from_str(name).builtin()
}

const READ_ONLY_FORBIDDEN: &[RoleCapability] = &[RoleCapability::Write, RoleCapability::Execute];

const EXPLORER_PROMPT: &str = r#"## Explorer Role

You locate and explain code; you do not change it.

**Focus**
1. Find the files, symbols, and call chains that answer the delegated question.
2. Trace how data and control actually flow, including the edge cases you can prove from source.
3. Separate what you verified by reading from what you are inferring.

**Constraints**
- Read-only: no file edits, no writes, no shell or process execution, no network.
- Do not propose or attempt a fix unless the task explicitly asks for one.
- Never report a path or line you did not actually read.

**Report**
- Direct answer to the delegated question.
- Evidence as `path:line` (or `path:line-line`) for every claim that matters.
- Relevant symbols by exact name.
- Open questions, contradictions, and the places you could not resolve."#;

const GENERAL_PROMPT: &str = r#"## General Role

You own one bounded, multi-step piece of work and report it back.

**Working rules**
- Stay inside the delegated scope; do not widen it into unrelated cleanup or refactors.
- Read before you change, and keep changes consistent with the surrounding code.
- When you modify files, list every changed file with a one-line reason.
- Never claim a command or test passed unless you actually ran it in this task.

**Report**
- Outcome: what is now true that was not true before.
- Changes: files touched and why.
- Verification: the exact commands you ran and their result, or an explicit statement that nothing was run.
- Open questions and anything that needs a parent decision."#;

const CODE_REVIEWER_PROMPT: &str = r#"## Code Reviewer Role

You are a specialized code review expert. Your task is to analyze code for:

**Focus Areas**:
1. **Code Quality**: Style, readability, maintainability
2. **Potential Bugs**: Logic errors, edge cases, error handling
3. **Performance**: Inefficiencies, optimization opportunities
4. **Security**: Vulnerabilities, unsafe patterns
5. **Best Practices**: Language idioms, design patterns

**Review Format**:
Return a structured review with:
- Summary of overall code quality
- Specific issues (with file:line references)
- Severity levels (critical, major, minor, suggestion)
- Recommendations for improvement

**Constraints**:
- Read-only access (no editing)
- Focus on analysis, not implementation
- Provide actionable feedback
- Report only defects you can point at with a path and line; if you found none, say so instead of padding the list
"#;

const TEST_WRITER_PROMPT: &str = r#"## Test Writer Role

You are a specialized test writing expert. Your task is to create comprehensive tests.

**Focus Areas**:
1. **Test Coverage**: Unit, integration, edge cases
2. **Test Quality**: Clear descriptions, proper assertions
3. **Test Organization**: Logical grouping, naming conventions
4. **Error Cases**: Exception handling, boundary conditions

**Test Creation Approach**:
- Analyze existing code structure
- Identify critical paths and edge cases
- Write tests following project conventions
- Include setup/teardown as needed
- Add meaningful test descriptions

**Best Practices**:
- Follow AAA pattern (Arrange, Act, Assert)
- One assertion per test when possible
- Use descriptive test names
- Mock external dependencies

**Report**:
- Test files added or changed
- The exact command used to run them and its observed result
- Cases you deliberately did not cover and why
"#;

const DEBUGGER_PROMPT: &str = r#"## Debugger Role

You are a specialized debugging expert. Your task is to identify and diagnose issues.

**Focus Areas**:
1. **Root Cause Analysis**: Find the underlying problem
2. **Error Investigation**: Analyze stack traces and logs
3. **Reproduction**: Identify steps to reproduce
4. **Fix Proposals**: Suggest concrete solutions

**Debugging Approach**:
- Examine error messages and logs
- Trace execution flow
- Check variable states and data flow
- Identify timing or race conditions
- Verify assumptions and preconditions

**Evidence Rules**:
- Mark every conclusion as either *reproduced* (you ran it and saw it) or *inferred* (read from source only).
- Never present a static reading as a confirmed reproduction.

**Output**:
- Clear problem description
- Root cause analysis
- Step-by-step reproduction
- Proposed fix with explanation
- Preventive measures
"#;

const DOCUMENTER_PROMPT: &str = r#"## Documenter Role

You are a specialized documentation expert. Your task is to create clear, comprehensive documentation.

**Focus Areas**:
1. **API Documentation**: Functions, parameters, return values
2. **Usage Examples**: Practical code samples
3. **Architecture**: System design and structure
4. **User Guides**: How-to instructions

**Documentation Approach**:
- Analyze code structure and APIs
- Write clear, concise descriptions
- Include practical examples
- Follow project documentation style
- Add diagrams where helpful

**Best Practices**:
- Start with high-level overview
- Document public APIs thoroughly
- Include edge cases and error conditions
- Keep examples up-to-date
- Use consistent terminology

**Scope**: Document only what the task asks for. Do not create new documentation files for a task that did not request documentation.

**Report**:
- Documentation files added or changed
- Which source of truth each claim was taken from
"#;

const BUILTIN_AGENTS: &[BuiltinAgentDescriptor] = &[
    BuiltinAgentDescriptor {
        kind: BuiltinAgentKind::Explorer,
        name: "explorer",
        aliases: &["explore", "scout"],
        when_to_use: "read-only multi-file exploration, locating symbols, tracing a call chain or dependency through source",
        avoid_when: "single file lookups, a one-line answer, or any task that must modify files or run commands",
        tools: &["read_file", "glob", "grep", "git_status"],
        forbidden: READ_ONLY_FORBIDDEN,
        context: RoleContextPolicy::Fresh,
        deliverables: &["direct answer", "path:line evidence", "open questions"],
        prompt: EXPLORER_PROMPT,
    },
    BuiltinAgentDescriptor {
        kind: BuiltinAgentKind::General,
        name: "general",
        aliases: &["general-purpose"],
        when_to_use: "bounded multi-step implementation or an investigation that needs execution, plus any task that does not match a narrower role",
        avoid_when: "a single file edit or a question you can answer with one or two direct tool calls",
        tools: &[
            "read_file",
            "list_files",
            "grep",
            "bash",
            "edit",
            "write_file",
            "git_status",
            "web_search",
        ],
        forbidden: &[],
        context: RoleContextPolicy::Fresh,
        deliverables: &["outcome", "changed files", "verification actually run"],
        prompt: GENERAL_PROMPT,
    },
    BuiltinAgentDescriptor {
        kind: BuiltinAgentKind::CodeReviewer,
        name: "code_reviewer",
        aliases: &["codereview", "reviewer"],
        when_to_use: "independent review of an existing change, looking for defects, regressions, and missing coverage",
        avoid_when: "writing the fix, or reviewing code that has not been written yet",
        tools: &["read_file", "list_files", "grep", "git_status"],
        forbidden: READ_ONLY_FORBIDDEN,
        context: RoleContextPolicy::Fresh,
        deliverables: &[
            "defects by severity with path:line evidence",
            "explicit statement when no defect was found",
        ],
        prompt: CODE_REVIEWER_PROMPT,
    },
    BuiltinAgentDescriptor {
        kind: BuiltinAgentKind::TestWriter,
        name: "test_writer",
        aliases: &["testwriter", "tester"],
        when_to_use: "writing or extending tests inside an explicitly named scope, including running them",
        avoid_when: "production-code changes, or tasks where the test scope is still undecided",
        tools: &[
            "read_file",
            "list_files",
            "grep",
            "bash",
            "edit",
            "write_file",
        ],
        forbidden: &[],
        context: RoleContextPolicy::Fresh,
        deliverables: &[
            "test files changed",
            "test command and observed result",
            "uncovered cases",
        ],
        prompt: TEST_WRITER_PROMPT,
    },
    BuiltinAgentDescriptor {
        kind: BuiltinAgentKind::Debugger,
        name: "debugger",
        aliases: &["debug"],
        when_to_use: "reproducing and root-causing a failure, including instrumented runs and fault injection",
        avoid_when: "reviewing code that already works, or broad exploratory reading with no failure to explain",
        tools: &["read_file", "list_files", "grep", "bash", "write_file"],
        forbidden: &[],
        context: RoleContextPolicy::Fresh,
        deliverables: &[
            "reproduction steps",
            "root cause",
            "whether the failure was reproduced or only inferred",
        ],
        prompt: DEBUGGER_PROMPT,
    },
    BuiltinAgentDescriptor {
        kind: BuiltinAgentKind::Documenter,
        name: "documenter",
        aliases: &["doc", "docs"],
        when_to_use: "docs the task explicitly asked for: API references, usage examples, architecture notes, guides",
        avoid_when: "a normal code change; do not create documentation nobody requested",
        tools: &["read_file", "list_files", "grep", "edit", "write_file"],
        forbidden: &[],
        context: RoleContextPolicy::Fresh,
        deliverables: &[
            "documentation files changed",
            "source of truth for each claim",
        ],
        prompt: DOCUMENTER_PROMPT,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn test_subagent_type_from_str() {
        assert_eq!(SubagentType::from_str("general"), SubagentType::General);
        assert_eq!(SubagentType::from_str("explorer"), SubagentType::Explorer);
        assert_eq!(
            SubagentType::from_str("code_reviewer"),
            SubagentType::CodeReviewer
        );
        assert_eq!(
            SubagentType::from_str("test_writer"),
            SubagentType::TestWriter
        );
        assert_eq!(SubagentType::from_str("debugger"), SubagentType::Debugger);
        assert_eq!(
            SubagentType::from_str("documenter"),
            SubagentType::Documenter
        );
    }

    #[test]
    fn aliases_resolve_to_the_canonical_kind() {
        assert_eq!(SubagentType::from_str("Explore"), SubagentType::Explorer);
        assert_eq!(SubagentType::from_str("scout"), SubagentType::Explorer);
        assert_eq!(
            SubagentType::from_str("reviewer"),
            SubagentType::CodeReviewer
        );
        assert_eq!(
            SubagentType::from_str("CODEREVIEW"),
            SubagentType::CodeReviewer
        );
        assert_eq!(SubagentType::from_str(" tester "), SubagentType::TestWriter);
        assert_eq!(SubagentType::from_str("docs"), SubagentType::Documenter);
        assert_eq!(SubagentType::from_str("debug"), SubagentType::Debugger);
        assert_eq!(
            SubagentType::from_str("general-purpose"),
            SubagentType::General
        );
    }

    #[test]
    fn aliases_are_reserved_so_custom_agents_cannot_shadow_them() {
        for alias in ["explore", "scout", "reviewer", "tester", "docs", "debug"] {
            assert!(
                SubagentType::is_builtin_name(alias),
                "{alias} must stay reserved"
            );
        }
        assert!(!SubagentType::is_builtin_name("my-auditor"));
    }

    #[test]
    fn test_allowed_tools() {
        let reviewer = SubagentType::CodeReviewer;
        let tools = reviewer.allowed_tools();
        assert!(tools.contains(&"read_file"));
        assert!(tools.contains(&"grep"));
        assert!(!tools.contains(&"edit")); // Reviewer 不能编辑
    }

    #[test]
    fn explorer_is_strictly_read_only() {
        let explorer = &SubagentType::Explorer;
        let tools = explorer.allowed_tools();

        assert!(tools.contains(&"read_file"));
        assert!(tools.contains(&"glob"));
        assert!(tools.contains(&"grep"));
        assert!(tools.contains(&"git_status"));
        for forbidden in ["bash", "edit", "write_file", "web_search", "subagent"] {
            assert!(
                !tools.contains(&forbidden),
                "explorer must not get {forbidden}"
            );
        }
        let descriptor = explorer.builtin().expect("explorer descriptor");
        assert!(descriptor.is_read_only());
        assert_eq!(descriptor.forbidden, READ_ONLY_FORBIDDEN);
    }

    #[test]
    fn custom_agents_have_no_implicit_tool_ceiling() {
        let custom = SubagentType::Custom("audit".to_string());
        assert!(custom.allowed_tools().is_empty());
        assert!(custom.builtin().is_none());
        assert_eq!(custom.system_prompt_suffix(), "");
        assert_eq!(custom.identifier(), "audit");
    }

    #[test]
    fn every_role_declares_the_full_selection_contract() {
        for descriptor in builtin_agents() {
            assert!(!descriptor.when_to_use.is_empty(), "{}", descriptor.name);
            assert!(!descriptor.avoid_when.is_empty(), "{}", descriptor.name);
            assert!(!descriptor.prompt.is_empty(), "{}", descriptor.name);
            assert!(
                !descriptor.deliverables.is_empty(),
                "{} must declare deliverables",
                descriptor.name
            );
            assert_eq!(
                descriptor.kind.subagent_type().identifier(),
                descriptor.name
            );
            assert!(
                !descriptor.tools.is_empty(),
                "{} must declare a tool ceiling",
                descriptor.name
            );
        }
    }

    #[test]
    fn catalog_names_aliases_and_tools_are_unique() {
        let mut names = BTreeSet::new();
        let mut words = BTreeSet::new();
        for descriptor in builtin_agents() {
            assert!(
                names.insert(descriptor.name),
                "duplicate {}",
                descriptor.name
            );
            assert!(
                words.insert(descriptor.name),
                "duplicate word {}",
                descriptor.name
            );
        }
        for descriptor in builtin_agents() {
            for alias in descriptor.aliases {
                assert!(words.insert(alias), "duplicate alias word {alias}");
                assert_ne!(
                    SubagentType::from_str(alias).identifier(),
                    *alias,
                    "alias {alias} must not be its own canonical name"
                );
            }
            let mut tools = BTreeSet::new();
            for tool in descriptor.tools {
                assert!(tools.insert(*tool), "duplicate tool {tool}");
            }
        }
    }

    #[test]
    fn read_only_roles_never_list_write_or_execute_tools() {
        for descriptor in builtin_agents().iter().filter(|d| d.is_read_only()) {
            for tool in descriptor.tools {
                assert!(
                    !matches!(*tool, "bash" | "edit" | "write_file" | "exec_command"),
                    "{} claims read-only but declares {tool}",
                    descriptor.name
                );
            }
        }
    }

    #[test]
    fn test_system_prompt_suffix() {
        let reviewer = SubagentType::CodeReviewer;
        let prompt = reviewer.system_prompt_suffix();
        assert!(prompt.contains("Code Reviewer Role"));
        assert!(prompt.contains("code review expert"));
    }

    #[test]
    fn explorer_prompt_states_read_only_limits_and_report_shape() {
        let prompt = SubagentType::Explorer.system_prompt_suffix();

        assert!(prompt.contains("Explorer Role"));
        assert!(prompt.contains("Read-only"));
        assert!(prompt.contains("path:line"));
        assert!(prompt.contains("Open questions"));
    }

    #[test]
    fn test_default_is_general() {
        let default_type = SubagentType::default();
        assert_eq!(default_type, SubagentType::General);
    }
}
