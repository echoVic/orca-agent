// Subagent 专用代理类型
// Phase 4: 高级特性 - 专用代理

use serde::{Deserialize, Serialize};

#[path = "agent_definition.rs"]
pub mod agent_definition;

/// 专用子代理类型
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubagentType {
    /// 通用代理（默认）
    General,
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

impl SubagentType {
    pub fn is_builtin_name(name: &str) -> bool {
        !matches!(Self::from_str(name), Self::Custom(_))
    }

    pub fn identifier(&self) -> &str {
        match self {
            Self::General => "general",
            Self::CodeReviewer => "code_reviewer",
            Self::TestWriter => "test_writer",
            Self::Debugger => "debugger",
            Self::Documenter => "documenter",
            Self::Custom(name) => name,
        }
    }

    /// 获取该类型的系统提示后缀
    pub fn system_prompt_suffix(&self) -> &'static str {
        match self {
            Self::General => "",
            Self::CodeReviewer => CODE_REVIEWER_PROMPT,
            Self::TestWriter => TEST_WRITER_PROMPT,
            Self::Debugger => DEBUGGER_PROMPT,
            Self::Documenter => DOCUMENTER_PROMPT,
            Self::Custom(_) => "",
        }
    }

    /// 获取该类型允许的工具集
    pub fn allowed_tools(&self) -> Vec<&'static str> {
        match self {
            Self::General => vec![
                "read_file",
                "list_files",
                "grep",
                "bash",
                "edit",
                "write_file",
                "git_status",
                "web_search",
            ],
            Self::CodeReviewer => vec!["read_file", "list_files", "grep", "git_status"],
            Self::TestWriter => vec![
                "read_file",
                "list_files",
                "grep",
                "bash",
                "edit",
                "write_file",
            ],
            Self::Debugger => vec!["read_file", "list_files", "grep", "bash", "write_file"],
            Self::Documenter => vec!["read_file", "list_files", "grep", "edit", "write_file"],
            // Custom agents require a resolved definition and an explicit runtime policy.
            Self::Custom(_) => vec![],
        }
    }

    /// 从字符串解析
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "general" => Self::General,
            "code_reviewer" | "codereview" | "reviewer" => Self::CodeReviewer,
            "test_writer" | "testwriter" | "tester" => Self::TestWriter,
            "debugger" | "debug" => Self::Debugger,
            "documenter" | "doc" | "docs" => Self::Documenter,
            _ => Self::Custom(s.to_string()),
        }
    }
}

// ============================================
// 系统提示模板
// ============================================

const CODE_REVIEWER_PROMPT: &str = r#"

## Code Reviewer Role

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
"#;

const TEST_WRITER_PROMPT: &str = r#"

## Test Writer Role

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
"#;

const DEBUGGER_PROMPT: &str = r#"

## Debugger Role

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

**Output**:
- Clear problem description
- Root cause analysis
- Step-by-step reproduction
- Proposed fix with explanation
- Preventive measures
"#;

const DOCUMENTER_PROMPT: &str = r#"

## Documenter Role

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
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subagent_type_from_str() {
        assert_eq!(SubagentType::from_str("general"), SubagentType::General);
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
    fn test_allowed_tools() {
        let reviewer = SubagentType::CodeReviewer;
        let tools = reviewer.allowed_tools();
        assert!(tools.contains(&"read_file"));
        assert!(tools.contains(&"grep"));
        assert!(!tools.contains(&"edit")); // Reviewer 不能编辑
    }

    #[test]
    fn test_system_prompt_suffix() {
        let reviewer = SubagentType::CodeReviewer;
        let prompt = reviewer.system_prompt_suffix();
        assert!(prompt.contains("Code Reviewer Role"));
        assert!(prompt.contains("code review expert"));
    }

    #[test]
    fn test_default_is_general() {
        let default_type = SubagentType::default();
        assert_eq!(default_type, SubagentType::General);
    }
}
