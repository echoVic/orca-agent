use std::collections::HashMap;
use std::path::PathBuf;

use clap::ValueEnum;
use serde::{Deserialize, Deserializer, Serialize};

use crate::approval_rules::PermissionRules;
use crate::approval_types::ApprovalMode;
use crate::external_config::ExternalToolConfig;
use crate::hook_types::HookConfig;
use crate::mcp_types::McpServerConfig;
use crate::model::ModelSelection;
use crate::subagent_config::SubagentConfig;
use crate::tool_types::ToolOutputTruncation;

pub mod file;
pub mod folder_trust;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VimInsertEscapeSequence {
    value: String,
    first: char,
    second: char,
}

impl VimInsertEscapeSequence {
    pub fn parse(value: &str) -> Result<Self, String> {
        const ERROR: &str =
            "vim_insert_escape must contain exactly two non-whitespace, non-control characters";

        let mut characters = value.chars();
        let Some(first) = characters.next() else {
            return Err(ERROR.to_string());
        };
        let Some(second) = characters.next() else {
            return Err(ERROR.to_string());
        };
        if characters.next().is_some()
            || [first, second]
                .into_iter()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err(ERROR.to_string());
        }
        Ok(Self {
            value: value.to_string(),
            first,
            second,
        })
    }

    pub fn first(&self) -> char {
        self.first
    }

    pub fn second(&self) -> char {
        self.second
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl<'de> Deserialize<'de> for VimInsertEscapeSequence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeName {
    #[default]
    Auto,
    Dark,
    Light,
    Solarized,
    Catppuccin,
}

impl ThemeName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Solarized => "solarized",
            Self::Catppuccin => "catppuccin",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputFormat {
    Jsonl,
    Text,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    #[default]
    Mock,
    #[value(name = "deepseek-fixture")]
    DeepSeekFixture,
    #[value(name = "deepseek")]
    DeepSeek,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mock => "mock",
            Self::DeepSeekFixture => "deepseek-fixture",
            Self::DeepSeek => "deepseek",
        }
    }
}

#[derive(Clone, Debug)]
pub enum HistoryMode {
    Record,
    Disabled,
    Resume(String),
    /// Continue a saved conversation but restore only the message log up to a
    /// durable message boundary (`resume_at` is a persisted conversation item
    /// id). Messages after the boundary are not replayed to the model.
    ResumeAt {
        selector: String,
        resume_at: String,
    },
    Fork(String),
}

pub const DEFAULT_MAX_READ_PARALLEL_TOOLS: usize = 8;
pub const DEFAULT_MAX_WORKFLOW_CONCURRENT_AGENTS: usize = 16;
pub const DEFAULT_MAX_WORKFLOW_AGENTS_PER_RUN: u32 = 1000;
pub const DEFAULT_MAX_WORKFLOW_AGENT_RETRIES: u32 = 1;
pub const MAX_WORKFLOW_AGENT_RETRIES: u32 = 5;
pub const DEFAULT_PERMISSION_PROFILE_GLOB_SCAN_MAX_DEPTH: usize = 32;
pub const MAX_PERMISSION_PROFILE_GLOB_SCAN_MAX_DEPTH: usize = 256;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelRuntimeConfig {
    #[serde(default)]
    pub context_window: Option<usize>,
    #[serde(default)]
    pub auto_compact_token_limit: Option<usize>,
    #[serde(default)]
    pub soft_compact_token_limit: Option<usize>,
}

impl ModelRuntimeConfig {
    pub fn normalized(self) -> Self {
        Self {
            context_window: self.context_window.map(|value| value.max(1)),
            auto_compact_token_limit: self.auto_compact_token_limit.map(|value| value.max(1)),
            soft_compact_token_limit: self.soft_compact_token_limit.map(|value| value.max(1)),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    High,
    #[default]
    Max,
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::High => "high",
            Self::Max => "max",
        }
    }
}

/// Explicit execution budget from `[budget]` configuration or CLI options.
/// Every dimension is independently optional; all default to `None` (unlimited).
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct BudgetConfig {
    pub max_turns: Option<u32>,
    pub max_tool_calls: Option<u32>,
    pub max_cost_usd_micros: Option<u64>,
    pub max_wall_time_ms: Option<u64>,
}

impl BudgetConfig {
    pub fn to_spec(self) -> crate::budget::BudgetSpec {
        crate::budget::BudgetSpec {
            max_turns: self.max_turns,
            max_tool_calls: self.max_tool_calls,
            max_cost_usd_micros: self.max_cost_usd_micros,
            max_wall_time_ms: self.max_wall_time_ms,
        }
    }

    pub fn from_spec(spec: crate::budget::BudgetSpec) -> Self {
        Self {
            max_turns: spec.max_turns,
            max_tool_calls: spec.max_tool_calls,
            max_cost_usd_micros: spec.max_cost_usd_micros,
            max_wall_time_ms: spec.max_wall_time_ms,
        }
    }

    /// Validates every present dimension is a positive value. Called after
    /// CLI/file decoding so an explicit `0` (or a negative/NaN cost) is
    /// rejected with a clear error instead of being silently dropped or
    /// panicking in the runtime worker.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(max_turns) = self.max_turns
            && max_turns == 0
        {
            return Err("max_turns must be a positive integer".to_string());
        }
        if let Some(max_tool_calls) = self.max_tool_calls
            && max_tool_calls == 0
        {
            return Err("max_tool_calls must be a positive integer".to_string());
        }
        if let Some(max_cost_usd_micros) = self.max_cost_usd_micros
            && max_cost_usd_micros == 0
        {
            return Err("max_cost_usd must be a positive amount".to_string());
        }
        if let Some(max_wall_time_ms) = self.max_wall_time_ms
            && max_wall_time_ms == 0
        {
            return Err("max_wall_time_secs must be a positive amount".to_string());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolConfig {
    #[serde(default = "default_max_read_parallel")]
    pub max_read_parallel: usize,
    #[serde(default)]
    pub output_truncation: ToolOutputTruncation,
    #[serde(default = "default_shell_timeout_secs")]
    pub shell_timeout_secs: u64,
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            max_read_parallel: DEFAULT_MAX_READ_PARALLEL_TOOLS,
            output_truncation: ToolOutputTruncation::default(),
            shell_timeout_secs: default_shell_timeout_secs(),
        }
    }
}

impl ToolConfig {
    const MAX_READ_PARALLEL_UPPER: usize = 32;
    const MAX_SHELL_TIMEOUT_SECS: u64 = 3600;

    pub fn normalized(mut self) -> Self {
        if self.max_read_parallel == 0 {
            self.max_read_parallel = 1;
        } else if self.max_read_parallel > Self::MAX_READ_PARALLEL_UPPER {
            self.max_read_parallel = Self::MAX_READ_PARALLEL_UPPER;
        }
        if self.shell_timeout_secs == 0 {
            self.shell_timeout_secs = 1;
        } else if self.shell_timeout_secs > Self::MAX_SHELL_TIMEOUT_SECS {
            self.shell_timeout_secs = Self::MAX_SHELL_TIMEOUT_SECS;
        }
        self.output_truncation = self.output_truncation.normalized();
        self
    }
}

fn default_max_read_parallel() -> usize {
    DEFAULT_MAX_READ_PARALLEL_TOOLS
}

fn default_shell_timeout_secs() -> u64 {
    120
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkflowTeamConfig {
    #[serde(default)]
    pub max_agent_retries: Option<u32>,
    #[serde(default)]
    pub max_agent_tokens: Option<u64>,
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
}

impl WorkflowTeamConfig {
    pub fn normalized(mut self) -> Self {
        if let Some(max_agent_retries) = self.max_agent_retries {
            self.max_agent_retries = Some(max_agent_retries.min(MAX_WORKFLOW_AGENT_RETRIES));
        }
        if let Some(max_agent_tokens) = self.max_agent_tokens {
            self.max_agent_tokens = Some(max_agent_tokens.max(1));
        }
        self.allowed_tools = self.allowed_tools.map(|tools| {
            tools
                .into_iter()
                .map(|tool| tool.trim().to_string())
                .filter(|tool| !tool.is_empty())
                .collect::<Vec<_>>()
        });
        if self
            .allowed_tools
            .as_ref()
            .is_some_and(|tools| tools.is_empty())
        {
            self.allowed_tools = Some(Vec::new());
        }
        self
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkflowConfig {
    #[serde(default = "default_workflows_enabled")]
    pub enabled: bool,
    #[serde(default = "default_max_workflow_concurrent_agents")]
    pub max_concurrent_agents: usize,
    #[serde(default = "default_max_workflow_agents_per_run")]
    pub max_agents_per_run: u32,
    #[serde(default = "default_max_workflow_agent_retries")]
    pub max_agent_retries: u32,
    #[serde(default)]
    pub max_agent_tokens: Option<u64>,
    #[serde(default = "default_workflow_keyword_trigger_enabled")]
    pub keyword_trigger_enabled: bool,
    #[serde(default)]
    pub teams: HashMap<String, WorkflowTeamConfig>,
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_concurrent_agents: DEFAULT_MAX_WORKFLOW_CONCURRENT_AGENTS,
            max_agents_per_run: DEFAULT_MAX_WORKFLOW_AGENTS_PER_RUN,
            max_agent_retries: DEFAULT_MAX_WORKFLOW_AGENT_RETRIES,
            max_agent_tokens: None,
            keyword_trigger_enabled: true,
            teams: HashMap::new(),
        }
    }
}

fn default_workflows_enabled() -> bool {
    true
}

fn default_max_workflow_concurrent_agents() -> usize {
    DEFAULT_MAX_WORKFLOW_CONCURRENT_AGENTS
}

fn default_max_workflow_agents_per_run() -> u32 {
    DEFAULT_MAX_WORKFLOW_AGENTS_PER_RUN
}

fn default_max_workflow_agent_retries() -> u32 {
    DEFAULT_MAX_WORKFLOW_AGENT_RETRIES
}

fn default_workflow_keyword_trigger_enabled() -> bool {
    true
}

#[derive(Clone, Debug)]
pub struct RunConfig {
    pub app_version: String,
    pub prompt: String,
    pub cwd: Option<PathBuf>,
    pub output_format: OutputFormat,
    pub approval_mode: ApprovalMode,
    pub provider: ProviderKind,
    pub verifier: Option<String>,
    pub model: ModelSelection,
    pub model_runtime: ModelRuntimeConfig,
    pub reasoning_effort: ReasoningEffort,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub mcp_servers: Vec<McpServerConfig>,
    pub hooks: Vec<HookConfig>,
    pub external_tools: Vec<ExternalToolConfig>,
    pub history_mode: HistoryMode,
    pub show_session_picker: bool,
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub permission_profiles: HashMap<String, PermissionProfileConfig>,
    pub runtime_workspace_roots: Option<Vec<PathBuf>>,
    pub permission_rules: PermissionRules,
    pub additional_working_directories: Vec<AdditionalWorkingDirectory>,
    pub budget: BudgetConfig,
    pub subagents: SubagentConfig,
    pub tools: ToolConfig,
    pub workflows: WorkflowConfig,
    pub theme: ThemeName,
    pub vim_mode: bool,
    pub vim_insert_escape: Option<VimInsertEscapeSequence>,
    pub update_check: bool,
    pub desktop_notifications: bool,
    pub terminal_notifications: bool,
    pub auto_memory: bool,
}

/// Immutable execution policy captured when work is delegated to another
/// runtime owner. The snapshot keeps child execution stable even if the
/// parent process later changes its interactive settings.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegationSnapshot {
    pub approval_mode: ApprovalMode,
    pub active_permission_profile: Option<ActivePermissionProfile>,
    #[serde(default)]
    pub permission_profiles: HashMap<String, PermissionProfileConfig>,
    pub runtime_workspace_roots: Option<Vec<PathBuf>>,
    pub permission_rules: PermissionRules,
    #[serde(default)]
    pub additional_working_directories: Vec<AdditionalWorkingDirectory>,
    pub model: Option<String>,
}

impl DelegationSnapshot {
    pub fn from_config(config: &RunConfig) -> Self {
        Self {
            approval_mode: config.approval_mode,
            active_permission_profile: config.active_permission_profile.clone(),
            permission_profiles: config.permission_profiles.clone(),
            runtime_workspace_roots: config.runtime_workspace_roots.clone(),
            permission_rules: config.permission_rules.clone(),
            additional_working_directories: config.additional_working_directories.clone(),
            model: config.model.as_option(),
        }
    }

    pub fn apply_to(&self, config: &mut RunConfig, child_model_override: Option<String>) {
        config.approval_mode = self.approval_mode;
        config.active_permission_profile = self.active_permission_profile.clone();
        config.permission_profiles = self.permission_profiles.clone();
        config.runtime_workspace_roots = self.runtime_workspace_roots.clone();
        config.permission_rules = self.permission_rules.clone();
        config.additional_working_directories = self.additional_working_directories.clone();

        let model = child_model_override.or_else(|| self.model.clone());
        if let Ok(model) = ModelSelection::parse(model) {
            config.model = model;
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdditionalWorkingDirectory {
    pub path: PathBuf,
    pub source: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActivePermissionProfile {
    pub id: String,
    pub extends: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PermissionProfileConfig {
    #[serde(default)]
    pub extends: Option<String>,
    #[serde(default)]
    pub filesystem: PermissionProfileFilesystemConfig,
    #[serde(default)]
    pub network: PermissionProfileNetworkConfig,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct PermissionProfileFilesystemConfig {
    #[serde(default, alias = "globScanMaxDepth")]
    glob_scan_max_depth: Option<usize>,
    entries: HashMap<PathBuf, PermissionProfileFileAccess>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum PermissionProfileFilesystemEntry {
    Access(PermissionProfileFileAccess),
    Scoped(HashMap<PathBuf, PermissionProfileFileAccess>),
}

impl<'de> Deserialize<'de> for PermissionProfileFilesystemConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawPermissionProfileFilesystemConfig {
            #[serde(default, alias = "globScanMaxDepth")]
            glob_scan_max_depth: Option<usize>,
            #[serde(flatten)]
            entries: HashMap<PathBuf, PermissionProfileFilesystemEntry>,
        }

        let raw = RawPermissionProfileFilesystemConfig::deserialize(deserializer)?;
        let mut entries = HashMap::new();
        for (path, entry) in raw.entries {
            let path = normalize_permission_profile_filesystem_path(path);
            match entry {
                PermissionProfileFilesystemEntry::Access(access) => {
                    entries.insert(path, access);
                }
                PermissionProfileFilesystemEntry::Scoped(scoped) => {
                    for (subpath, access) in scoped {
                        entries.insert(
                            normalize_permission_profile_filesystem_path(path.join(subpath)),
                            access,
                        );
                    }
                }
            }
        }
        Ok(Self {
            glob_scan_max_depth: raw.glob_scan_max_depth.map(normalize_glob_scan_max_depth),
            entries,
        })
    }
}

fn normalize_glob_scan_max_depth(depth: usize) -> usize {
    depth.clamp(1, MAX_PERMISSION_PROFILE_GLOB_SCAN_MAX_DEPTH)
}

fn normalize_permission_profile_filesystem_path(path: PathBuf) -> PathBuf {
    let Some(path_str) = path.to_str() else {
        return path;
    };
    let Some(stripped) = path_str.strip_suffix("/**") else {
        return path;
    };
    if stripped.is_empty() {
        return path;
    }
    PathBuf::from(stripped)
}

impl PermissionProfileFilesystemConfig {
    pub fn glob_scan_max_depth(&self) -> Option<usize> {
        self.glob_scan_max_depth
    }

    pub fn get(&self, path: &std::path::Path) -> Option<&PermissionProfileFileAccess> {
        self.entries.get(path)
    }

    pub fn entries(&self) -> impl Iterator<Item = (&PathBuf, &PermissionProfileFileAccess)> {
        self.entries.iter()
    }

    pub fn from_parts(
        glob_scan_max_depth: Option<usize>,
        entries: HashMap<PathBuf, PermissionProfileFileAccess>,
    ) -> Self {
        Self {
            glob_scan_max_depth: glob_scan_max_depth.map(normalize_glob_scan_max_depth),
            entries,
        }
    }
}

impl From<HashMap<PathBuf, PermissionProfileFileAccess>> for PermissionProfileFilesystemConfig {
    fn from(entries: HashMap<PathBuf, PermissionProfileFileAccess>) -> Self {
        Self {
            glob_scan_max_depth: None,
            entries,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PermissionProfileNetworkConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub domains: PermissionProfileNetworkDomainsConfig,
    #[serde(default)]
    pub unix_sockets: PermissionProfileNetworkUnixSocketsConfig,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PermissionProfileNetworkDomainsConfig {
    #[serde(flatten)]
    entries: HashMap<String, PermissionProfileNetworkAccess>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PermissionProfileNetworkUnixSocketsConfig {
    #[serde(flatten)]
    entries: HashMap<PathBuf, PermissionProfileNetworkAccess>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionProfileNetworkAccess {
    Allow,
    Deny,
}

impl PermissionProfileNetworkDomainsConfig {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, domain: &str) -> Option<&PermissionProfileNetworkAccess> {
        self.entries.get(domain)
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &PermissionProfileNetworkAccess)> {
        self.entries
            .iter()
            .map(|(domain, access)| (domain.as_str(), access))
    }
}

impl PermissionProfileNetworkUnixSocketsConfig {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(
        &self,
    ) -> impl Iterator<Item = (&std::path::Path, &PermissionProfileNetworkAccess)> {
        self.entries
            .iter()
            .map(|(path, access)| (path.as_path(), access))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionProfileFileAccess {
    Read,
    Write,
    ReadWrite,
    Deny,
}

impl PermissionProfileFileAccess {
    pub fn allows_read(self) -> bool {
        matches!(self, Self::Read | Self::ReadWrite)
    }

    pub fn allows_write(self) -> bool {
        matches!(self, Self::Write | Self::ReadWrite)
    }

    pub fn denies_write(self) -> bool {
        matches!(self, Self::Deny)
    }
}

impl ActivePermissionProfile {
    pub fn new(id: impl Into<String>, extends: Option<impl Into<String>>) -> Self {
        Self {
            id: id.into(),
            extends: extends.map(Into::into),
        }
    }
}

impl AdditionalWorkingDirectory {
    pub fn new(path: impl Into<PathBuf>, source: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            source: source.into(),
        }
    }
}

pub fn format_config_show(config: &RunConfig) -> String {
    let api_key = if config.api_key.is_some() {
        "<redacted>"
    } else {
        "<unset>"
    };
    let base_url = config.base_url.as_deref().unwrap_or("<default>");
    let cwd = config
        .cwd
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<current>".to_string());
    let verifier = config.verifier.as_deref().unwrap_or("<unset>");
    let budget = budget_summary(&config.budget);
    let runtime = runtime_summary(config);
    let vim_insert_escape = config
        .vim_insert_escape
        .as_ref()
        .map(|sequence| toml::Value::String(sequence.as_str().to_string()).to_string())
        .unwrap_or_else(|| "\"<unset>\"".to_string());

    format!(
        concat!(
            "model = \"{}\"\n",
            "mode = \"{}\"\n",
            "api_key = \"{}\"\n",
            "base_url = \"{}\"\n",
            "provider = \"{}\"\n",
            "reasoning_effort = \"{}\"\n",
            "model_context_window = \"{}\"\n",
            "model_auto_compact_token_limit = \"{}\"\n",
            "model_soft_compact_token_limit = \"{}\"\n",
            "cwd = \"{}\"\n",
            "verifier = \"{}\"\n",
            "theme = \"{}\"\n",
            "vim_mode = {}\n",
            "vim_insert_escape = {}\n",
            "update_check = {}\n",
            "desktop_notifications = {}\n",
            "terminal_notifications = {}\n",
            "auto_memory = {}\n",
            "\n",
            "[budget]\n",
            "max_turns = {}\n",
            "max_tool_calls = {}\n",
            "max_cost_usd_micros = {}\n",
            "max_wall_time_ms = {}\n",
            "\n",
            "[runtime]\n",
            "approval = \"{}\"\n",
            "filesystem = \"{}\"\n",
            "network = \"{}\"\n",
            "history = \"{}\"\n",
            "context_window = \"{}\"\n",
            "auto_compact_token_limit = \"{}\"\n",
            "soft_compact_token_limit = \"{}\"\n",
            "tool_output_truncation = \"{}\"\n",
            "workflow_agents = \"{}\"\n",
            "\n",
            "[tools]\n",
            "max_read_parallel = {}\n",
            "output_truncation = \"{}\"\n",
            "shell_timeout_secs = {}\n",
            "\n",
            "[subagents]\n",
            "max_depth = {}\n",
            "max_parallel = {}\n",
            "\n",
            "[counts]\n",
            "mcp_servers = {}\n",
            "external_tools = {}\n",
            "hooks = {}\n",
            "permission_rules = {}\n",
            "additional_working_directories = {}"
        ),
        config.model.display_name(),
        config.approval_mode.as_str(),
        api_key,
        base_url,
        config.provider.as_str(),
        config.reasoning_effort.as_str(),
        config
            .model_runtime
            .context_window
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<default>".to_string()),
        config
            .model_runtime
            .auto_compact_token_limit
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<default>".to_string()),
        config
            .model_runtime
            .soft_compact_token_limit
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<default>".to_string()),
        cwd,
        verifier,
        config.theme.as_str(),
        config.vim_mode,
        vim_insert_escape,
        config.update_check,
        config.desktop_notifications,
        config.terminal_notifications,
        config.auto_memory,
        budget.max_turns,
        budget.max_tool_calls,
        budget.max_cost_usd_micros,
        budget.max_wall_time_ms,
        runtime.approval,
        runtime.filesystem,
        runtime.network,
        runtime.history,
        runtime.context_window,
        runtime.auto_compact_token_limit,
        runtime.soft_compact_token_limit,
        runtime.tool_output_truncation,
        runtime.workflow_agents,
        config.tools.max_read_parallel,
        config.tools.output_truncation,
        config.tools.shell_timeout_secs,
        config.subagents.max_depth,
        config.subagents.max_parallel,
        config.mcp_servers.len(),
        config.external_tools.len(),
        config.hooks.len(),
        config.permission_rules.rules.len(),
        config.additional_working_directories.len()
    )
}

struct RuntimeSummary {
    approval: &'static str,
    filesystem: &'static str,
    network: &'static str,
    history: &'static str,
    context_window: String,
    auto_compact_token_limit: String,
    soft_compact_token_limit: String,
    tool_output_truncation: String,
    workflow_agents: String,
}

fn runtime_summary(config: &RunConfig) -> RuntimeSummary {
    RuntimeSummary {
        approval: config.approval_mode.as_str(),
        filesystem: filesystem_posture(config.approval_mode),
        network: network_posture(config),
        history: history_posture(&config.history_mode),
        context_window: config
            .model_runtime
            .context_window
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<model-default>".to_string()),
        auto_compact_token_limit: config
            .model_runtime
            .auto_compact_token_limit
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<model-default>".to_string()),
        soft_compact_token_limit: config
            .model_runtime
            .soft_compact_token_limit
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<model-default>".to_string()),
        tool_output_truncation: config.tools.output_truncation.to_string(),
        workflow_agents: format!(
            "max_parallel={}, max_per_run={}, max_agent_tokens={}",
            config.workflows.max_concurrent_agents,
            config.workflows.max_agents_per_run,
            config
                .workflows
                .max_agent_tokens
                .map(|value| value.to_string())
                .unwrap_or_else(|| "<unset>".to_string())
        ),
    }
}

fn option_or_unset(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "<unset>".to_string())
}

fn budget_summary(budget: &BudgetConfig) -> BudgetSummary {
    BudgetSummary {
        max_turns: budget
            .max_turns
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<unset>".to_string()),
        max_tool_calls: budget
            .max_tool_calls
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<unset>".to_string()),
        max_cost_usd_micros: option_or_unset(budget.max_cost_usd_micros),
        max_wall_time_ms: option_or_unset(budget.max_wall_time_ms),
    }
}

struct BudgetSummary {
    max_turns: String,
    max_tool_calls: String,
    max_cost_usd_micros: String,
    max_wall_time_ms: String,
}

fn filesystem_posture(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Plan => "read-only",
        ApprovalMode::Suggest | ApprovalMode::AutoEdit => "workspace-write",
        ApprovalMode::FullAuto => "danger-full-access",
    }
}

fn network_posture(config: &RunConfig) -> &'static str {
    if config.provider == ProviderKind::Mock && config.mcp_servers.is_empty() {
        "not-configured"
    } else {
        "allowed"
    }
}

fn history_posture(history_mode: &HistoryMode) -> &'static str {
    match history_mode {
        HistoryMode::Record => "recording",
        HistoryMode::Disabled => "disabled",
        HistoryMode::Resume(_) => "resume",
        HistoryMode::ResumeAt { .. } => "resume-at",
        HistoryMode::Fork(_) => "fork",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval_rules::PermissionRule;
    use crate::approval_types::ApprovalMode;
    use crate::approval_types::Decision;
    use crate::model::{AUTO_MODEL, FLASH_MODEL, ModelSelection};

    #[test]
    fn theme_name_defaults_to_auto_and_round_trips_all_values() {
        assert_eq!(ThemeName::default(), ThemeName::Auto);

        for (wire, theme) in [
            ("\"auto\"", ThemeName::Auto),
            ("\"dark\"", ThemeName::Dark),
            ("\"light\"", ThemeName::Light),
            ("\"solarized\"", ThemeName::Solarized),
            ("\"catppuccin\"", ThemeName::Catppuccin),
        ] {
            assert_eq!(serde_json::from_str::<ThemeName>(wire).unwrap(), theme);
            assert_eq!(serde_json::to_string(&theme).unwrap(), wire);
        }
    }

    #[test]
    fn vim_insert_escape_validates_exactly_two_printable_non_whitespace_scalars() {
        for (value, first, second) in [("jj", 'j', 'j'), ("jk", 'j', 'k'), ("你好", '你', '好')]
        {
            let sequence = VimInsertEscapeSequence::parse(value).unwrap();
            assert_eq!(sequence.first(), first);
            assert_eq!(sequence.second(), second);
            assert_eq!(sequence.as_str(), value);
        }

        for value in ["", "j", "jjj", "j ", " j", "\nj", "j\u{7f}"] {
            let error = VimInsertEscapeSequence::parse(value).unwrap_err();
            assert!(error.contains("exactly two"), "{value:?}: {error}");
        }
    }

    #[test]
    fn format_config_show_redacts_api_key_and_includes_effective_values() {
        let config = RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::FullAuto,
            provider: ProviderKind::DeepSeekFixture,
            verifier: None,
            model: ModelSelection::from_unchecked(Some("deepseek-v4-flash".to_string())),
            model_runtime: ModelRuntimeConfig {
                context_window: Some(128_000),
                auto_compact_token_limit: Some(96_000),
                soft_compact_token_limit: Some(64_000),
            },
            reasoning_effort: ReasoningEffort::Max,
            api_key: Some("sk-secret".to_string()),
            base_url: Some("https://api.example".to_string()),
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: PermissionRules::default(),
            additional_working_directories: Vec::new(),
            budget: BudgetConfig {
                max_cost_usd_micros: Some(1_250_000),
                ..BudgetConfig::default()
            },
            subagents: SubagentConfig::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::Auto,
            vim_mode: true,
            vim_insert_escape: Some(VimInsertEscapeSequence::parse("j\\").unwrap()),
            update_check: false,
            desktop_notifications: true,
            terminal_notifications: false,
            auto_memory: true,
        };

        let shown = format_config_show(&config);

        assert!(shown.contains("model = \"deepseek-v4-flash\""));
        assert!(shown.contains("reasoning_effort = \"max\""));
        assert!(shown.contains("model_context_window = \"128000\""));
        assert!(shown.contains("model_auto_compact_token_limit = \"96000\""));
        assert!(shown.contains("model_soft_compact_token_limit = \"64000\""));
        assert!(shown.contains("mode = \"full-auto\""));
        assert!(shown.contains("[runtime]"));
        assert!(shown.contains("filesystem = \"danger-full-access\""));
        assert!(shown.contains("network = \"allowed\""));
        assert!(shown.contains("approval = \"full-auto\""));
        assert!(shown.contains("history = \"disabled\""));
        assert!(shown.contains("theme = \"auto\""));
        let vim_insert_escape_line = shown
            .lines()
            .find(|line| line.starts_with("vim_insert_escape = "))
            .expect("vim insert escape line");
        let parsed: toml::Value = vim_insert_escape_line.parse().unwrap();
        assert_eq!(
            parsed
                .get("vim_insert_escape")
                .and_then(toml::Value::as_str),
            Some("j\\")
        );
        assert!(shown.contains("desktop_notifications = true"));
        assert!(shown.contains("terminal_notifications = false"));
        assert!(shown.contains("api_key = \"<redacted>\""));
        assert!(!shown.contains("sk-secret"));
    }

    #[test]
    fn delegation_snapshot_round_trips_execution_policy_and_applies_child_model() {
        let mut parent = RunConfig {
            app_version: "test".to_string(),
            prompt: String::new(),
            cwd: Some(PathBuf::from("/workspace")),
            output_format: OutputFormat::Jsonl,
            approval_mode: ApprovalMode::Plan,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(Some(AUTO_MODEL.to_string())).unwrap(),
            model_runtime: ModelRuntimeConfig::default(),
            reasoning_effort: ReasoningEffort::default(),
            api_key: None,
            base_url: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: Some(ActivePermissionProfile::new("strict", Some("base"))),
            permission_profiles: HashMap::new(),
            runtime_workspace_roots: Some(vec![PathBuf::from("/workspace"), PathBuf::from("job")]),
            permission_rules: PermissionRules {
                rules: vec![PermissionRule::new("bash", "cargo *", Decision::Allow)],
            },
            additional_working_directories: vec![AdditionalWorkingDirectory::new(
                "job",
                "delegation",
            )],
            budget: BudgetConfig::default(),
            subagents: SubagentConfig::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::default(),
            vim_mode: false,
            vim_insert_escape: None,
            update_check: false,
            desktop_notifications: false,
            terminal_notifications: false,
            auto_memory: false,
        };

        let snapshot = DelegationSnapshot::from_config(&parent);
        let encoded = serde_json::to_string(&snapshot).expect("serialize snapshot");
        let decoded: DelegationSnapshot =
            serde_json::from_str(&encoded).expect("deserialize snapshot");
        decoded.apply_to(&mut parent, Some(FLASH_MODEL.to_string()));

        assert_eq!(decoded.approval_mode, ApprovalMode::Plan);
        assert_eq!(
            decoded.active_permission_profile,
            parent.active_permission_profile
        );
        assert_eq!(
            decoded.runtime_workspace_roots,
            parent.runtime_workspace_roots
        );
        assert_eq!(decoded.permission_rules, parent.permission_rules);
        assert_eq!(
            decoded.additional_working_directories,
            parent.additional_working_directories
        );
        assert_eq!(parent.approval_mode, ApprovalMode::Plan);
        assert_eq!(parent.model.as_deref(), Some(FLASH_MODEL));
    }
}
