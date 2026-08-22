use std::path::{Path, PathBuf};

use std::fs;

use base64::Engine as _;
use nucleo::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo::{Config as MatcherConfig, Matcher, Utf32Str};
use orca_core::conversation::{ImageDetail, ImageInput, ImageSource};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_MENTION_BYTES: usize = 32 * 1024;
const MAX_MENTION_SOURCE_BYTES: usize = orca_tools::file_admission::MAX_EDIT_FILE_BYTES;
const MAX_PLUGIN_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_INLINE_IMAGE_BYTES: usize = 5 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MentionFileKind {
    File,
    Directory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MentionKind {
    File,
    Skill,
    Plugin,
    Resource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MentionSigil {
    At,
    Dollar,
}

impl MentionSigil {
    pub fn as_char(self) -> char {
        match self {
            Self::At => '@',
            Self::Dollar => '$',
        }
    }
}

impl MentionKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Skill => "skill",
            Self::Plugin => "plugin",
            Self::Resource => "resource",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MentionCandidate {
    pub id: String,
    pub kind: MentionKind,
    pub display: String,
    pub description: String,
    pub score: u32,
    pub indices: Vec<u32>,
    pub target: MentionTarget,
}

impl MentionCandidate {
    pub fn from_file_match(candidate: &orca_file_search::SearchMatch) -> Self {
        let kind = match candidate.kind {
            orca_file_search::MatchKind::File => MentionFileKind::File,
            orca_file_search::MatchKind::Directory => MentionFileKind::Directory,
        };
        let target = MentionTarget::File {
            root: candidate.root.clone(),
            path: candidate.path.clone(),
            kind,
        };
        Self {
            id: target.stable_id(),
            kind: MentionKind::File,
            display: candidate.path.clone(),
            description: candidate.root.display().to_string(),
            score: candidate.score,
            indices: candidate.indices.clone(),
            target,
        }
    }

    pub fn is_directory(&self) -> bool {
        matches!(
            self.target,
            MentionTarget::File {
                kind: MentionFileKind::Directory,
                ..
            }
        )
    }
}

#[derive(Debug, Clone, Default)]
pub struct MentionCatalog {
    candidates: Vec<MentionCandidate>,
    errors: Vec<String>,
}

impl MentionCatalog {
    pub fn discover_skills(roots: &[PathBuf]) -> Self {
        let (orca_home, agents_home) = skill_discovery_dirs_from_env();
        Self::discover_skills_with_dirs(roots, orca_home.as_deref(), agents_home.as_deref())
    }

    pub fn discover(roots: &[PathBuf], mcp_registry: &orca_mcp::McpRegistry) -> Self {
        let (orca_home, agents_home) = skill_discovery_dirs_from_env();
        Self::discover_with_skill_dirs(
            roots,
            mcp_registry,
            orca_home.as_deref(),
            agents_home.as_deref(),
        )
    }

    fn discover_with_skill_dirs(
        roots: &[PathBuf],
        mcp_registry: &orca_mcp::McpRegistry,
        orca_home: Option<&Path>,
        agents_home: Option<&Path>,
    ) -> Self {
        let roots = if roots.is_empty() {
            vec![std::env::current_dir().unwrap_or_default()]
        } else {
            roots.to_vec()
        };
        let mut catalog = Self::discover_skills_with_dirs(&roots, orca_home, agents_home);
        let (plugins, plugin_errors) = discover_plugins(&roots);
        catalog.errors.extend(plugin_errors);
        catalog.candidates.extend(plugins.into_iter().map(|plugin| {
            let target = MentionTarget::Plugin {
                name: plugin.name.clone(),
                manifest_path: plugin.manifest_path,
            };
            MentionCandidate {
                id: target.stable_id(),
                kind: MentionKind::Plugin,
                display: plugin.display_name,
                description: plugin.description,
                score: 0,
                indices: Vec::new(),
                target,
            }
        }));
        let resources = mcp_registry.list_resources_with_errors(None);
        catalog.errors.extend(resources.errors);
        catalog
            .candidates
            .extend(resources.resources.into_iter().map(|resource| {
                let target = MentionTarget::Resource {
                    server: resource.server.clone(),
                    uri: resource.uri.clone(),
                };
                MentionCandidate {
                    id: target.stable_id(),
                    kind: MentionKind::Resource,
                    display: resource.name,
                    description: resource
                        .description
                        .unwrap_or_else(|| format!("{} · {}", resource.server, resource.uri)),
                    score: 0,
                    indices: Vec::new(),
                    target,
                }
            }));
        let templates = mcp_registry.list_resource_templates_with_errors(None);
        catalog.errors.extend(templates.errors);
        catalog
            .candidates
            .extend(templates.resource_templates.into_iter().map(|template| {
                let target = MentionTarget::ResourceTemplate {
                    server: template.server.clone(),
                    uri_template: template.uri_template.clone(),
                };
                MentionCandidate {
                    id: target.stable_id(),
                    kind: MentionKind::Resource,
                    display: template.name,
                    description: template.description.unwrap_or_else(|| {
                        format!("{} · {}", template.server, template.uri_template)
                    }),
                    score: 0,
                    indices: Vec::new(),
                    target,
                }
            }));
        catalog.candidates.sort_by(candidate_identity_order);
        catalog
            .candidates
            .dedup_by(|left, right| left.id == right.id);
        catalog
    }

    fn discover_skills_with_dirs(
        roots: &[PathBuf],
        orca_home: Option<&Path>,
        agents_home: Option<&Path>,
    ) -> Self {
        let roots = if roots.is_empty() {
            vec![std::env::current_dir().unwrap_or_default()]
        } else {
            roots.to_vec()
        };
        let mut catalog = Self::default();
        for root in &roots {
            match orca_tools::skills::discover_metadata(root, orca_home, agents_home) {
                Ok(skills) => {
                    for skill in skills {
                        let target = MentionTarget::Skill {
                            id: skill.id.clone(),
                            path: skill.path.clone(),
                        };
                        catalog.candidates.push(MentionCandidate {
                            id: target.stable_id(),
                            kind: MentionKind::Skill,
                            display: skill.id,
                            description: if skill.description.is_empty() {
                                skill.name
                            } else {
                                format!("{} — {}", skill.name, skill.description)
                            },
                            score: 0,
                            indices: Vec::new(),
                            target,
                        });
                    }
                }
                Err(error) => catalog.errors.push(error),
            }
        }
        catalog.candidates.sort_by(candidate_identity_order);
        catalog
            .candidates
            .dedup_by(|left, right| left.id == right.id);
        catalog
    }

    pub fn candidates(&self) -> &[MentionCandidate] {
        &self.candidates
    }

    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<MentionCandidate> {
        self.search_filtered(query, limit, |_| true)
    }

    pub fn search_kind(
        &self,
        query: &str,
        kind: MentionKind,
        limit: usize,
    ) -> Vec<MentionCandidate> {
        self.search_filtered(query, limit, |candidate| candidate.kind == kind)
    }

    pub fn search_all_kind(&self, query: &str, kind: MentionKind) -> Vec<MentionCandidate> {
        self.search_filtered(query, self.candidates.len().max(1), |candidate| {
            candidate.kind == kind
        })
    }

    fn search_filtered(
        &self,
        query: &str,
        limit: usize,
        include: impl Fn(&MentionCandidate) -> bool,
    ) -> Vec<MentionCandidate> {
        let limit = limit.max(1);
        if query.trim().is_empty() {
            return self
                .candidates
                .iter()
                .filter(|candidate| include(candidate))
                .take(limit)
                .cloned()
                .collect();
        }
        let atom = Atom::new(
            query.trim(),
            CaseMatching::Smart,
            Normalization::Smart,
            AtomKind::Fuzzy,
            false,
        );
        let mut matcher = Matcher::new(MatcherConfig::DEFAULT);
        let mut haystack_buf = Vec::new();
        let mut scored = Vec::new();
        for candidate in &self.candidates {
            if !include(candidate) {
                continue;
            }
            let mut candidate = candidate.clone();
            let haystack = Utf32Str::new(&candidate.display, &mut haystack_buf);
            let mut indices = Vec::new();
            let display_score = atom.indices(haystack, &mut matcher, &mut indices);
            let description_score = if display_score.is_none() {
                let description = Utf32Str::new(&candidate.description, &mut haystack_buf);
                atom.score(description, &mut matcher)
            } else {
                None
            };
            let Some(score) = display_score.or(description_score) else {
                continue;
            };
            candidate.score = u32::from(score)
                + exact_match_bonus(query, &candidate.display)
                + kind_score_bonus(candidate.kind);
            candidate.indices = indices;
            scored.push(candidate);
        }
        scored.sort_by(candidate_score_order);
        scored.truncate(limit);
        scored
    }
}

fn skill_discovery_dirs_from_env() -> (Option<PathBuf>, Option<PathBuf>) {
    let orca_home = std::env::var_os("ORCA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".orca")));
    let agents_home = dirs::home_dir().map(|home| home.join(".agents"));
    (orca_home, agents_home)
}

pub fn merge_candidates(
    query: &str,
    static_candidates: Vec<MentionCandidate>,
    file_candidates: Vec<MentionCandidate>,
    limit: usize,
) -> Vec<MentionCandidate> {
    let mut merged = Vec::new();
    if query.is_empty() {
        let static_head = static_candidates.len().min(4);
        merged.extend(static_candidates.iter().take(static_head).cloned());
        merged.extend(file_candidates);
        merged.extend(static_candidates.into_iter().skip(static_head));
    } else {
        merged.extend(static_candidates);
        merged.extend(file_candidates);
        merged.sort_by(|left, right| right.score.cmp(&left.score));
    }
    let mut seen = std::collections::HashSet::new();
    merged.retain(|candidate| seen.insert(candidate.id.clone()));
    merged.truncate(limit.max(1));
    merged
}

fn candidate_score_order(left: &MentionCandidate, right: &MentionCandidate) -> std::cmp::Ordering {
    right
        .score
        .cmp(&left.score)
        .then_with(|| candidate_identity_order(left, right))
}

fn candidate_identity_order(
    left: &MentionCandidate,
    right: &MentionCandidate,
) -> std::cmp::Ordering {
    mention_kind_order(left.kind)
        .cmp(&mention_kind_order(right.kind))
        .then_with(|| {
            left.display
                .to_lowercase()
                .cmp(&right.display.to_lowercase())
        })
        .then_with(|| left.id.cmp(&right.id))
}

fn mention_kind_order(kind: MentionKind) -> u8 {
    match kind {
        MentionKind::Skill => 0,
        MentionKind::Plugin => 1,
        MentionKind::Resource => 2,
        MentionKind::File => 3,
    }
}

fn exact_match_bonus(query: &str, display: &str) -> u32 {
    if query == display {
        10_000
    } else if query.eq_ignore_ascii_case(display) {
        9_000
    } else if display.to_lowercase().starts_with(&query.to_lowercase()) {
        2_000
    } else {
        0
    }
}

fn kind_score_bonus(kind: MentionKind) -> u32 {
    match kind {
        MentionKind::Skill => 30,
        MentionKind::Plugin => 20,
        MentionKind::Resource => 10,
        MentionKind::File => 0,
    }
}

#[derive(Debug)]
struct DiscoveredPlugin {
    name: String,
    display_name: String,
    description: String,
    manifest_path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct PluginManifest {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    interface: PluginInterface,
}

#[derive(Debug, Default, Deserialize)]
struct PluginInterface {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    #[serde(rename = "shortDescription")]
    short_description: Option<String>,
}

fn discover_plugins(roots: &[PathBuf]) -> (Vec<DiscoveredPlugin>, Vec<String>) {
    let mut plugin_roots = roots
        .iter()
        .flat_map(|root| [root.join(".orca/plugins"), root.join(".codex/plugins")])
        .collect::<Vec<_>>();
    if let Some(home) = std::env::var_os("ORCA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".orca")))
    {
        plugin_roots.push(home.join("plugins"));
    }
    let mut plugins = Vec::new();
    let mut errors = Vec::new();
    for root in plugin_roots {
        if !root.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&root)
            .follow_links(false)
            .max_depth(8)
            .into_iter()
            .filter_map(Result::ok)
        {
            let path = entry.path();
            if !entry.file_type().is_file()
                || path.file_name().and_then(|name| name.to_str()) != Some("plugin.json")
                || path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    != Some(".codex-plugin")
            {
                continue;
            }
            let content = match orca_tools::file_admission::read_text_file_with_limit(
                path,
                MAX_PLUGIN_MANIFEST_BYTES,
                || false,
            ) {
                Ok(content) => content,
                Err(error) => {
                    errors.push(format!("failed to read plugin {}: {error}", path.display()));
                    continue;
                }
            };
            let manifest = match serde_json::from_str::<PluginManifest>(&content) {
                Ok(manifest) if !manifest.name.trim().is_empty() => manifest,
                Ok(_) => {
                    errors.push(format!("plugin {} has an empty name", path.display()));
                    continue;
                }
                Err(error) => {
                    errors.push(format!("invalid plugin {}: {error}", path.display()));
                    continue;
                }
            };
            plugins.push(DiscoveredPlugin {
                display_name: manifest
                    .interface
                    .display_name
                    .unwrap_or_else(|| manifest.name.clone()),
                description: manifest
                    .interface
                    .short_description
                    .filter(|value| !value.is_empty())
                    .unwrap_or(manifest.description),
                name: manifest.name,
                manifest_path: path.canonicalize().unwrap_or_else(|_| path.to_path_buf()),
            });
        }
    }
    plugins.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.manifest_path.cmp(&right.manifest_path))
    });
    plugins.dedup_by(|left, right| left.manifest_path == right.manifest_path);
    (plugins, errors)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MentionTarget {
    File {
        root: PathBuf,
        path: String,
        kind: MentionFileKind,
    },
    Skill {
        id: String,
        path: PathBuf,
    },
    Plugin {
        name: String,
        manifest_path: PathBuf,
    },
    Resource {
        server: String,
        uri: String,
    },
    ResourceTemplate {
        server: String,
        uri_template: String,
    },
}

impl MentionTarget {
    pub fn stable_id(&self) -> String {
        let mut hasher = Sha256::new();
        hash_stable_component(&mut hasher, b"orca-mention-target-v1");
        let kind = match self {
            Self::File { root, path, kind } => {
                hash_stable_component(&mut hasher, b"file");
                hash_stable_component(&mut hasher, root.as_os_str().as_encoded_bytes());
                hash_stable_component(&mut hasher, path.as_bytes());
                hash_stable_component(
                    &mut hasher,
                    match kind {
                        MentionFileKind::File => b"file",
                        MentionFileKind::Directory => b"directory",
                    },
                );
                "file"
            }
            Self::Skill { id, path } => {
                hash_stable_component(&mut hasher, b"skill");
                hash_stable_component(&mut hasher, id.as_bytes());
                hash_stable_component(&mut hasher, path.as_os_str().as_encoded_bytes());
                "skill"
            }
            Self::Plugin {
                name,
                manifest_path,
            } => {
                hash_stable_component(&mut hasher, b"plugin");
                hash_stable_component(&mut hasher, name.as_bytes());
                hash_stable_component(&mut hasher, manifest_path.as_os_str().as_encoded_bytes());
                "plugin"
            }
            Self::Resource { server, uri } => {
                hash_stable_component(&mut hasher, b"resource");
                hash_stable_component(&mut hasher, server.as_bytes());
                hash_stable_component(&mut hasher, uri.as_bytes());
                "resource"
            }
            Self::ResourceTemplate {
                server,
                uri_template,
            } => {
                hash_stable_component(&mut hasher, b"resource_template");
                hash_stable_component(&mut hasher, server.as_bytes());
                hash_stable_component(&mut hasher, uri_template.as_bytes());
                "resource_template"
            }
        };
        format!("{kind}:{:x}", hasher.finalize())
    }
}

fn hash_stable_component(hasher: &mut Sha256, component: &[u8]) {
    hasher.update((component.len() as u64).to_be_bytes());
    hasher.update(component);
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MentionBinding {
    pub start: usize,
    pub end: usize,
    pub visible: String,
    pub target: MentionTarget,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MentionBindings {
    text: String,
    bindings: Vec<MentionBinding>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpandedPrompt {
    pub text: String,
    pub images: Vec<ImageInput>,
}

impl MentionBindings {
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            bindings: Vec::new(),
        }
    }

    pub fn from_bindings(text: &str, bindings: Vec<MentionBinding>) -> Self {
        let mut state = Self {
            text: text.to_string(),
            bindings,
        };
        state.retain_valid();
        state
    }

    pub fn bindings(&self) -> &[MentionBinding] {
        &self.bindings
    }

    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.bindings.clear();
    }

    pub fn reconcile(&mut self, next: &str) {
        if self.text == next {
            return;
        }
        let (old_start, old_end, new_end) = changed_range(&self.text, next);
        let removed = old_end.saturating_sub(old_start);
        let inserted = new_end.saturating_sub(old_start);
        let delta = inserted as isize - removed as isize;
        self.bindings.retain_mut(|binding| {
            if old_end <= binding.start {
                binding.start = binding.start.saturating_add_signed(delta);
                binding.end = binding.end.saturating_add_signed(delta);
                true
            } else {
                old_start >= binding.end
            }
        });
        self.text.clear();
        self.text.push_str(next);
        self.retain_valid();
    }

    pub fn apply_selection(&mut self, previous: &str, edit: &MentionEdit, target: MentionTarget) {
        self.reconcile(previous);
        self.reconcile(&edit.text);
        let visible = edit.text[edit.replacement_start..edit.replacement_end].to_string();
        self.bindings.push(MentionBinding {
            start: edit.replacement_start,
            end: edit.replacement_end,
            visible,
            target,
        });
        self.bindings.sort_by_key(|binding| binding.start);
        self.retain_valid();
    }

    fn retain_valid(&mut self) {
        self.bindings.retain(|binding| {
            binding.start < binding.end
                && binding.end <= self.text.len()
                && self.text.is_char_boundary(binding.start)
                && self.text.is_char_boundary(binding.end)
                && self.text[binding.start..binding.end] == binding.visible
        });
        self.bindings.sort_by_key(|binding| binding.start);
        let mut previous_end = 0;
        self.bindings.retain(|binding| {
            if binding.start < previous_end {
                return false;
            }
            previous_end = binding.end;
            true
        });
    }
}

fn changed_range(previous: &str, next: &str) -> (usize, usize, usize) {
    let prefix = previous
        .bytes()
        .zip(next.bytes())
        .take_while(|(left, right)| left == right)
        .count();
    let mut prefix = prefix;
    while !previous.is_char_boundary(prefix) || !next.is_char_boundary(prefix) {
        prefix = prefix.saturating_sub(1);
    }
    let old_tail = &previous[prefix..];
    let new_tail = &next[prefix..];
    let suffix = old_tail
        .bytes()
        .rev()
        .zip(new_tail.bytes().rev())
        .take_while(|(left, right)| left == right)
        .count();
    let mut old_end = previous.len().saturating_sub(suffix);
    let mut new_end = next.len().saturating_sub(suffix);
    while !previous.is_char_boundary(old_end) || !next.is_char_boundary(new_end) {
        old_end = old_end.saturating_add(1).min(previous.len());
        new_end = new_end.saturating_add(1).min(next.len());
    }
    (prefix, old_end, new_end)
}

pub fn expand_mentions(
    input: &str,
    bindings: &MentionBindings,
    cwd: &Path,
    workspace_roots: &[PathBuf],
    mcp_registry: &orca_mcp::McpRegistry,
) -> Result<String, String> {
    let (orca_home, agents_home) = skill_discovery_dirs_from_env();
    expand_mentions_with_skill_dirs(
        input,
        bindings,
        cwd,
        workspace_roots,
        mcp_registry,
        orca_home.as_deref(),
        agents_home.as_deref(),
    )
}

pub fn expand_mentions_for_model(
    input: &str,
    bindings: &MentionBindings,
    cwd: &Path,
    workspace_roots: &[PathBuf],
    mcp_registry: &orca_mcp::McpRegistry,
) -> Result<ExpandedPrompt, String> {
    let (orca_home, agents_home) = skill_discovery_dirs_from_env();
    let valid_bindings = bindings.bindings().iter().filter(|binding| {
        binding.end <= input.len()
            && input.is_char_boundary(binding.start)
            && input.is_char_boundary(binding.end)
            && input[binding.start..binding.end] == binding.visible
    });
    let mut text_blocks = Vec::new();
    let mut images = Vec::new();
    let mut inline_image_bytes = 0usize;
    let mut seen_targets = std::collections::HashSet::new();
    for binding in valid_bindings {
        if !seen_targets.insert(binding.target.stable_id()) {
            continue;
        }
        if let Some(image) = image_input_for_target(&binding.target, cwd, workspace_roots)? {
            if let ImageSource::Base64 { data, .. } = &image.source {
                inline_image_bytes =
                    inline_image_bytes.saturating_add(data.len().saturating_mul(3) / 4);
                if inline_image_bytes > MAX_INLINE_IMAGE_BYTES {
                    return Err(format!(
                        "attached images exceed Orca's {} MiB inline limit",
                        MAX_INLINE_IMAGE_BYTES / (1024 * 1024)
                    ));
                }
            }
            images.push(image);
            text_blocks.push(format!("[Attached image: {}]", binding.visible));
        } else {
            text_blocks.push(expand_bound_target(
                &binding.target,
                cwd,
                workspace_roots,
                mcp_registry,
                orca_home.as_deref(),
                agents_home.as_deref(),
            )?);
        }
    }
    Ok(ExpandedPrompt {
        text: append_mention_blocks(input, text_blocks)?,
        images,
    })
}

fn image_input_for_target(
    target: &MentionTarget,
    cwd: &Path,
    workspace_roots: &[PathBuf],
) -> Result<Option<ImageInput>, String> {
    let MentionTarget::File { root, path, kind } = target else {
        return Ok(None);
    };
    if *kind != MentionFileKind::File {
        return Ok(None);
    }
    let root = root
        .canonicalize()
        .map_err(|error| format!("failed to resolve mention root: {error}"))?;
    if !allowed_workspace_roots(cwd, workspace_roots).contains(&root) {
        return Err(format!(
            "bound mention root {} is outside the active workspaces",
            root.display()
        ));
    }
    let resolved = root
        .join(path)
        .canonicalize()
        .map_err(|error| format!("failed to resolve bound @{path}: {error}"))?;
    if !resolved.starts_with(&root) || !resolved.is_file() {
        return Err(format!("bound @{path} is not a file inside its workspace"));
    }
    let metadata = fs::metadata(&resolved)
        .map_err(|error| format!("failed to inspect bound @{path}: {error}"))?;
    if metadata.len() > MAX_INLINE_IMAGE_BYTES as u64 {
        return Err(format!(
            "bound image @{path} exceeds Orca's {} MiB inline limit",
            MAX_INLINE_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    let bytes =
        fs::read(&resolved).map_err(|error| format!("failed to read bound @{path}: {error}"))?;
    let Some(media_type) = sniff_image_media_type(&bytes) else {
        return Ok(None);
    };
    Ok(Some(ImageInput {
        source: ImageSource::Base64 {
            media_type: media_type.to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        },
        detail: ImageDetail::High,
    }))
}

fn sniff_image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn expand_mentions_with_skill_dirs(
    input: &str,
    bindings: &MentionBindings,
    cwd: &Path,
    workspace_roots: &[PathBuf],
    mcp_registry: &orca_mcp::McpRegistry,
    orca_home: Option<&Path>,
    agents_home: Option<&Path>,
) -> Result<String, String> {
    let valid_bindings = bindings.bindings().iter().filter(|binding| {
        binding.end <= input.len()
            && input.is_char_boundary(binding.start)
            && input.is_char_boundary(binding.end)
            && input[binding.start..binding.end] == binding.visible
    });
    let mut blocks = Vec::new();
    let mut seen_targets = std::collections::HashSet::new();
    for binding in valid_bindings {
        if seen_targets.insert(binding.target.stable_id()) {
            blocks.push(expand_bound_target(
                &binding.target,
                cwd,
                workspace_roots,
                mcp_registry,
                orca_home,
                agents_home,
            )?);
        }
    }
    append_mention_blocks(input, blocks)
}

fn append_mention_blocks(input: &str, blocks: Vec<String>) -> Result<String, String> {
    if blocks.is_empty() {
        return Ok(input.to_string());
    }
    Ok(format!("{}\n\n{}", input, blocks.join("\n\n")))
}

fn expand_bound_target(
    target: &MentionTarget,
    cwd: &Path,
    workspace_roots: &[PathBuf],
    mcp_registry: &orca_mcp::McpRegistry,
    orca_home: Option<&Path>,
    agents_home: Option<&Path>,
) -> Result<String, String> {
    match target {
        MentionTarget::File { root, path, kind } => {
            if *kind != MentionFileKind::File {
                return Err(format!("@{path} is a directory, not an attachable file"));
            }
            let root = root
                .canonicalize()
                .map_err(|error| format!("failed to resolve mention root: {error}"))?;
            let allowed = allowed_workspace_roots(cwd, workspace_roots);
            if !allowed.contains(&root) {
                return Err(format!(
                    "bound mention root {} is outside the active workspaces",
                    root.display()
                ));
            }
            let resolved = root
                .join(path)
                .canonicalize()
                .map_err(|error| format!("failed to resolve bound @{path}: {error}"))?;
            if !resolved.starts_with(&root) || !resolved.is_file() {
                return Err(format!("bound @{path} is not a file inside its workspace"));
            }
            file_block(path, &resolved, path, Some(&root))
        }
        MentionTarget::Skill { id, path } => {
            let path = path.canonicalize().unwrap_or_else(|_| path.clone());
            let mut selected = None;
            for root in allowed_workspace_roots(cwd, workspace_roots) {
                if let Ok(skills) = orca_tools::skills::discover(&root, orca_home, agents_home)
                    && let Some(skill) = skills.into_iter().find(|skill| {
                        skill.id == *id
                            && skill.path.canonicalize().unwrap_or(skill.path.clone()) == path
                    })
                {
                    selected = Some(skill);
                    break;
                }
            }
            let skill =
                selected.ok_or_else(|| format!("bound skill is no longer available: {id}"))?;
            orca_tools::skills::format_skills_prompt_block(&[skill])
                .ok_or_else(|| format!("bound skill is empty: {id}"))
        }
        MentionTarget::Plugin {
            name,
            manifest_path,
        } => {
            let manifest_path = manifest_path
                .canonicalize()
                .unwrap_or_else(|_| manifest_path.clone());
            if !plugin_manifest_allowed(&manifest_path, cwd, workspace_roots) {
                return Err(format!(
                    "bound plugin manifest is outside configured plugin roots: {}",
                    manifest_path.display()
                ));
            }
            let content = orca_tools::file_admission::read_text_file_with_limit(
                &manifest_path,
                MAX_PLUGIN_MANIFEST_BYTES,
                || false,
            )
            .map_err(|error| {
                format!(
                    "failed to read bound plugin manifest {}: {error}",
                    manifest_path.display()
                )
            })?;
            let manifest: serde_json::Value = serde_json::from_str(&content).map_err(|error| {
                format!(
                    "invalid bound plugin manifest {}: {error}",
                    manifest_path.display()
                )
            })?;
            if manifest.get("name").and_then(serde_json::Value::as_str) != Some(name.as_str()) {
                return Err(format!("bound plugin identity changed: {name}"));
            }
            let rendered = serde_json::to_string_pretty(&manifest)
                .map_err(|error| format!("failed to render plugin manifest: {error}"))?;
            let (rendered, truncated) = truncate_content(&rendered);
            Ok(format!(
                "<plugin name=\"{}\" manifest=\"{}\">\n{}{}\n</plugin>",
                escape_attr(name),
                escape_attr(&manifest_path.display().to_string()),
                rendered,
                if truncated {
                    "\n[... truncated ...]"
                } else {
                    ""
                }
            ))
        }
        MentionTarget::Resource { server, uri } => {
            let resource = mcp_registry.read_resource(server, uri)?;
            let rendered = serde_json::to_string_pretty(&resource)
                .map_err(|error| format!("failed to render MCP resource {uri}: {error}"))?;
            let (rendered, truncated) = truncate_content(&rendered);
            Ok(format!(
                "<mcp_resource server=\"{}\" uri=\"{}\">\n{}{}\n</mcp_resource>",
                escape_attr(server),
                escape_attr(uri),
                rendered,
                if truncated {
                    "\n[... truncated ...]"
                } else {
                    ""
                }
            ))
        }
        MentionTarget::ResourceTemplate {
            server,
            uri_template,
        } => {
            let templates = mcp_registry
                .list_resource_templates(Some(server))
                .map_err(|error| {
                    format!(
                        "failed to revalidate bound MCP resource template {server} {uri_template}: {error}"
                    )
                })?;
            if !templates
                .iter()
                .any(|template| template.uri_template == *uri_template)
            {
                return Err(format!(
                    "bound MCP resource template is no longer available: {server} {uri_template}"
                ));
            }
            Ok(format!(
                "<mcp_resource_template server=\"{}\" uri_template=\"{}\" />",
                escape_attr(server),
                escape_attr(uri_template)
            ))
        }
    }
}

fn allowed_workspace_roots(cwd: &Path, workspace_roots: &[PathBuf]) -> Vec<PathBuf> {
    let roots = if workspace_roots.is_empty() {
        vec![cwd.to_path_buf()]
    } else {
        workspace_roots.to_vec()
    };
    roots
        .into_iter()
        .map(|root| root.canonicalize().unwrap_or(root))
        .fold(Vec::new(), |mut unique, root| {
            if !unique.contains(&root) {
                unique.push(root);
            }
            unique
        })
}

fn plugin_manifest_allowed(path: &Path, cwd: &Path, workspace_roots: &[PathBuf]) -> bool {
    let mut plugin_roots = allowed_workspace_roots(cwd, workspace_roots)
        .into_iter()
        .flat_map(|root| [root.join(".orca/plugins"), root.join(".codex/plugins")])
        .collect::<Vec<_>>();
    if let Some(home) = std::env::var_os("ORCA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".orca")))
    {
        plugin_roots.push(home.join("plugins"));
    }
    plugin_roots.into_iter().any(|root| {
        let root = root.canonicalize().unwrap_or(root);
        path.starts_with(root)
            && path.file_name().and_then(|name| name.to_str()) == Some("plugin.json")
            && path
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                == Some(".codex-plugin")
    })
}

fn file_block(
    display_path: &str,
    resolved: &Path,
    mention: &str,
    root: Option<&Path>,
) -> Result<String, String> {
    let content = orca_tools::file_admission::read_text_file_with_limit(
        resolved,
        MAX_MENTION_SOURCE_BYTES,
        || false,
    )
    .map_err(|error| match error {
        orca_tools::file_admission::FileAdmissionError::InvalidUtf8 => {
            format!("@{mention} appears to be a binary file")
        }
        error => format!("failed to read @{mention}: {error}"),
    })?;
    if content.as_bytes().contains(&0) {
        return Err(format!("@{mention} appears to be a binary file"));
    }
    let (content, truncated) = truncate_content(&content);
    let marker = if truncated {
        "\n[... truncated ...]"
    } else {
        ""
    };
    let root_attr = root
        .map(|root| format!(r#" root="{}""#, escape_attr(&root.display().to_string())))
        .unwrap_or_default();
    Ok(format!(
        r#"<file path="{}"{}>
{}{}</file>"#,
        escape_attr(display_path),
        root_attr,
        content,
        marker
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionToken {
    pub start: usize,
    pub end: usize,
    pub query: String,
    pub quoted: bool,
    pub sigil: MentionSigil,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionEdit {
    pub text: String,
    pub cursor: usize,
    pub replacement_start: usize,
    pub replacement_end: usize,
}

pub fn current_mention_token(input: &str) -> Option<MentionToken> {
    mention_token_at_cursor(input, input.len())
}

pub fn mention_token_at_cursor(input: &str, cursor: usize) -> Option<MentionToken> {
    if cursor > input.len() || !input.is_char_boundary(cursor) {
        return None;
    }

    let mut active = None;
    for (start, ch) in input.char_indices() {
        let sigil = match ch {
            '@' => MentionSigil::At,
            '$' => MentionSigil::Dollar,
            _ => continue,
        };
        if start > 0
            && !input[..start]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace)
        {
            continue;
        }

        let query_start = start + 1;
        if input[query_start..].starts_with('"') {
            let query_start = query_start + 1;
            let closing_quote = input[query_start..]
                .find('"')
                .map(|offset| query_start + offset);
            let query_end = closing_quote.unwrap_or(input.len());
            if (query_start..=query_end).contains(&cursor) {
                active = Some(MentionToken {
                    start,
                    end: closing_quote.map_or(query_end, |end| end + 1),
                    query: input[query_start..query_end].to_string(),
                    quoted: true,
                    sigil,
                });
            }
            continue;
        }

        let end = input[query_start..]
            .find(char::is_whitespace)
            .map(|offset| query_start + offset)
            .unwrap_or(input.len());
        if !(query_start..=end).contains(&cursor) {
            continue;
        }
        let query = &input[query_start..end];
        if query.starts_with('@') || query.starts_with("http://") || query.starts_with("https://") {
            continue;
        }
        active = Some(MentionToken {
            start,
            end,
            query: query.to_string(),
            quoted: false,
            sigil,
        });
    }
    active
}

pub fn complete_file_mention_from_candidates(input: &str, candidates: &[String]) -> Option<String> {
    complete_file_mention_from_candidates_at_cursor(input, input.len(), candidates)
        .map(|edit| edit.text)
}

pub fn complete_file_mention_from_candidates_at_cursor(
    input: &str,
    cursor: usize,
    candidates: &[String],
) -> Option<MentionEdit> {
    let token = mention_token_at_cursor(input, cursor)?;
    let replacement = if candidates.len() == 1 {
        candidates[0].clone()
    } else {
        let common = common_prefix(candidates)?;
        if !common.starts_with(&token.query) {
            return None;
        }
        common
    };
    if replacement == token.query {
        return None;
    }
    let mut completed = String::new();
    completed.push_str(&input[..token.start]);
    if token.quoted {
        completed.push(token.sigil.as_char());
        completed.push('"');
    } else {
        completed.push(token.sigil.as_char());
    }
    completed.push_str(&replacement);
    let completed_cursor = completed.len();
    if token.quoted && input[token.start..token.end].ends_with('"') {
        completed.push('"');
    }
    completed.push_str(&input[token.end..]);
    Some(MentionEdit {
        text: completed,
        cursor: completed_cursor,
        replacement_start: token.start,
        replacement_end: completed_cursor,
    })
}

pub fn apply_mention_selection(input: &str, candidate: &str) -> String {
    apply_mention_selection_at_cursor(input, input.len(), candidate)
        .map_or_else(|| input.to_string(), |edit| edit.text)
}

pub fn apply_mention_selection_at_cursor(
    input: &str,
    cursor: usize,
    candidate: &str,
) -> Option<MentionEdit> {
    let token = mention_token_at_cursor(input, cursor)?;
    let has_space = candidate.contains(' ');
    let mut result = String::new();
    result.push_str(&input[..token.start]);
    if token.quoted || has_space {
        result.push(token.sigil.as_char());
        result.push('"');
        result.push_str(candidate);
        if !candidate.ends_with('/') {
            result.push('"');
        }
    } else {
        result.push(token.sigil.as_char());
        result.push_str(candidate);
    }
    let inserted_end = result.len();
    let suffix = &input[token.end..];
    let mut result_cursor = inserted_end;
    if !candidate.ends_with('/') {
        if let Some(whitespace) = suffix.chars().next().filter(|ch| ch.is_whitespace()) {
            result_cursor += whitespace.len_utf8();
        } else {
            result.push(' ');
            result_cursor += 1;
        }
    }
    result.push_str(suffix);
    Some(MentionEdit {
        text: result,
        cursor: result_cursor,
        replacement_start: token.start,
        replacement_end: inserted_end,
    })
}

fn common_prefix(values: &[String]) -> Option<String> {
    let first = values.first()?.as_str();
    let mut end = first.len();
    for value in values.iter().skip(1) {
        end = end.min(value.len());
        while end > 0 && !value.starts_with(&first[..end]) {
            end -= 1;
            while !first.is_char_boundary(end) {
                end -= 1;
            }
        }
    }
    Some(first[..end].to_string())
}

fn truncate_content(content: &str) -> (&str, bool) {
    if content.len() <= MAX_MENTION_BYTES {
        return (content, false);
    }
    let mut end = MAX_MENTION_BYTES;
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    (&content[..end], true)
}

fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unbound_at_tokens_remain_literal_even_when_they_look_like_paths() {
        let cwd = tempfile::tempdir().unwrap();
        fs::write(cwd.path().join("README.md"), "do not inject").unwrap();
        let roots = vec![cwd.path().to_path_buf()];
        let registry = orca_mcp::McpRegistry::default();

        for input in [
            "@oai/sky还能逆向吗",
            "read @README.md",
            "email foo@example.com",
            "install @oai/sky",
            "visit https://example.com/@oai/sky",
            "punctuation (@README.md), [@oai/sky]!",
        ] {
            let expanded = expand_mentions(
                input,
                &MentionBindings::new(input),
                cwd.path(),
                &roots,
                &registry,
            )
            .unwrap();

            assert_eq!(expanded, input);
        }
    }

    #[test]
    fn completes_unique_file_mention() {
        let completed =
            complete_file_mention_from_candidates("read @no", &["notes.txt".to_string()]).unwrap();

        assert_eq!(completed, "read @notes.txt");
    }

    #[test]
    fn completes_common_prefix_for_multiple_mentions() {
        let completed = complete_file_mention_from_candidates(
            "read @a",
            &["alpha-one.txt".to_string(), "alpha-two.txt".to_string()],
        )
        .unwrap();

        assert_eq!(completed, "read @alpha-");
    }

    #[test]
    fn rejects_binary_files() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("image.png");
        fs::write(&image, b"\x89PNG\r\n\x1a\n\x00\x00").unwrap();

        let err = file_block("image.png", &image, "image.png", None).unwrap_err();

        assert!(err.contains("binary file"));
    }

    #[test]
    fn multimodal_expansion_attaches_recognized_image_without_text_inlining() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("image.png");
        fs::write(&image, b"\x89PNG\r\n\x1a\n\x00\x00").unwrap();
        let input = "inspect @image.png";
        let bindings = MentionBindings::from_bindings(
            input,
            vec![MentionBinding {
                start: 8,
                end: input.len(),
                visible: "@image.png".to_string(),
                target: MentionTarget::File {
                    root: dir.path().to_path_buf(),
                    path: "image.png".to_string(),
                    kind: MentionFileKind::File,
                },
            }],
        );

        let expanded = expand_mentions_for_model(
            input,
            &bindings,
            dir.path(),
            &[dir.path().to_path_buf()],
            &orca_mcp::McpRegistry::default(),
        )
        .unwrap();

        assert!(expanded.text.contains("[Attached image: @image.png]"));
        assert!(matches!(
            expanded.images.as_slice(),
            [ImageInput {
                source: ImageSource::Base64 { media_type, .. },
                detail: ImageDetail::High,
            }] if media_type == "image/png"
        ));
    }

    #[test]
    fn rejects_oversized_and_non_regular_file_mentions_before_reading() {
        let dir = tempfile::tempdir().unwrap();
        let oversized = dir.path().join("oversized.txt");
        fs::File::create(&oversized)
            .unwrap()
            .set_len((MAX_MENTION_SOURCE_BYTES + 1) as u64)
            .unwrap();

        let oversized_error =
            file_block("oversized.txt", &oversized, "oversized.txt", None).unwrap_err();
        assert!(oversized_error.contains("file is too large"));

        let directory_error = file_block("folder", dir.path(), "folder", None).unwrap_err();
        assert!(directory_error.contains("not a regular file"));
    }

    #[test]
    fn completes_quoted_mention() {
        let completed = complete_file_mention_from_candidates(
            r#"read @"my dir/no"#,
            &["my dir/notes.txt".to_string()],
        )
        .unwrap();

        assert!(completed.contains("my dir/notes.txt"));
    }

    #[test]
    fn current_token_exposes_range_query_and_quote_state() {
        let token = current_mention_token(r#"review @"src/ma"#).unwrap();

        assert_eq!(
            token,
            MentionToken {
                start: 7,
                end: 15,
                query: "src/ma".to_string(),
                quoted: true,
                sigil: MentionSigil::At,
            }
        );
    }

    #[test]
    fn dollar_token_selection_preserves_skill_sigil() {
        let token = current_mention_token("$code-rev").unwrap();

        assert_eq!(token.query, "code-rev");
        assert_eq!(token.sigil, MentionSigil::Dollar);

        let edit = apply_mention_selection_at_cursor("$code-rev", "$code-rev".len(), "code-review")
            .unwrap();
        assert_eq!(edit.text, "$code-review ");
        assert_eq!(edit.cursor, edit.text.len());
    }

    #[test]
    fn token_at_cursor_owns_the_earlier_mention_instead_of_the_final_token() {
        let input = "compare @src/lib.rs with @README.md";
        let cursor = input.find("lib.rs").unwrap() + 3;

        let token = mention_token_at_cursor(input, cursor).unwrap();

        assert_eq!(token.start, 8);
        assert_eq!(token.query, "src/lib.rs");
    }

    #[test]
    fn closed_quoted_mention_is_inactive_after_the_closing_quote() {
        let input = r#"review @"my dir/file.rs" "#;

        assert!(mention_token_at_cursor(input, input.len()).is_none());
        assert_eq!(
            mention_token_at_cursor(input, input.find("file.rs").unwrap() + 2)
                .unwrap()
                .query,
            "my dir/file.rs"
        );
    }

    #[test]
    fn selection_at_an_earlier_cursor_preserves_the_remaining_composer_text() {
        let input = "compare @sr with @README.md";
        let edit = apply_mention_selection_at_cursor(input, 11, "src/lib.rs").unwrap();

        assert_eq!(edit.text, "compare @src/lib.rs with @README.md");
        assert_eq!(&edit.text[..edit.cursor], "compare @src/lib.rs ");
    }

    #[test]
    fn completes_from_existing_snapshot_without_searching() {
        let completed = complete_file_mention_from_candidates(
            "review @src/m",
            &["src/main.rs".to_string(), "src/match.rs".to_string()],
        )
        .unwrap();

        assert_eq!(completed, "review @src/ma");
    }

    #[test]
    fn selecting_a_directory_keeps_the_mention_open_for_browsing() {
        assert_eq!(apply_mention_selection("review @s", "src/"), "review @src/");
        assert_eq!(
            apply_mention_selection(r#"review @"my"#, "my dir/"),
            r#"review @"my dir/"#
        );
    }

    #[test]
    fn selecting_a_quoted_file_closes_the_token_and_moves_past_whitespace() {
        let edit = apply_mention_selection_at_cursor(r#"review @"my" later"#, 11, "my dir/file.rs")
            .unwrap();

        assert_eq!(edit.text, r#"review @"my dir/file.rs" later"#);
        assert!(mention_token_at_cursor(&edit.text, edit.cursor).is_none());
    }

    #[test]
    fn atomic_binding_rebases_before_edits_and_invalidates_inner_edits() {
        let root = PathBuf::from("/workspace");
        let input = "review @ma";
        let edit = apply_mention_selection_at_cursor(input, input.len(), "main.rs").unwrap();
        let mut bindings = MentionBindings::new(input);
        bindings.apply_selection(
            input,
            &edit,
            MentionTarget::File {
                root: root.clone(),
                path: "main.rs".to_string(),
                kind: MentionFileKind::File,
            },
        );

        bindings.reconcile("please review @main.rs ");
        assert_eq!(bindings.bindings().len(), 1);
        assert_eq!(bindings.bindings()[0].start, 14);
        assert!(
            bindings.bindings()[0]
                .target
                .stable_id()
                .starts_with("file:")
        );

        bindings.reconcile("please review @main-old.rs ");
        assert!(bindings.bindings().is_empty());
    }

    #[test]
    fn stable_ids_do_not_collide_when_components_contain_separators() {
        let first = MentionTarget::File {
            root: std::env::temp_dir().join("a"),
            path: "b:c".to_string(),
            kind: MentionFileKind::File,
        };
        let second = MentionTarget::File {
            root: std::env::temp_dir().join("a:b"),
            path: "c".to_string(),
            kind: MentionFileKind::File,
        };

        assert_ne!(first.stable_id(), second.stable_id());
    }

    #[test]
    fn bound_file_identity_selects_the_exact_root_when_paths_collide() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        fs::write(first.path().join("same.txt"), "first root").unwrap();
        fs::write(second.path().join("same.txt"), "second root").unwrap();
        let input = "read @same.txt";
        let bindings = MentionBindings::from_bindings(
            input,
            vec![MentionBinding {
                start: 5,
                end: input.len(),
                visible: "@same.txt".to_string(),
                target: MentionTarget::File {
                    root: second.path().canonicalize().unwrap(),
                    path: "same.txt".to_string(),
                    kind: MentionFileKind::File,
                },
            }],
        );

        let expanded = expand_mentions(
            input,
            &bindings,
            first.path(),
            &[first.path().to_path_buf(), second.path().to_path_buf()],
            &orca_mcp::McpRegistry::default(),
        )
        .unwrap();

        assert!(expanded.contains("second root"));
        assert!(!expanded.contains("first root"));
    }

    #[test]
    fn all_kind_search_does_not_truncate_skill_results() {
        let candidates = (0..30)
            .map(|index| {
                let id = format!("skill-{index:02}");
                let target = MentionTarget::Skill {
                    id: id.clone(),
                    path: PathBuf::from(format!("/skills/{id}/SKILL.md")),
                };
                MentionCandidate {
                    id: target.stable_id(),
                    kind: MentionKind::Skill,
                    display: id,
                    description: "Skill".to_string(),
                    score: 0,
                    indices: Vec::new(),
                    target,
                }
            })
            .collect();
        let catalog = MentionCatalog {
            candidates,
            errors: Vec::new(),
        };

        assert_eq!(catalog.search_all_kind("", MentionKind::Skill).len(), 30);
    }

    #[test]
    fn unified_catalog_discovers_skills_and_codex_compatible_plugins() {
        let project = tempfile::tempdir().unwrap();
        let orca_home = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join("Cargo.toml"),
            "[package]\nname='mentions-test'\nversion='0.1.0'\n",
        )
        .unwrap();
        let skill_dir = project.path().join(".orca/skills/review");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: Review\ndescription: Review changes safely\n---\n\nRead the diff.\n",
        )
        .unwrap();
        let plugin_dir = project.path().join(".orca/plugins/github/.codex-plugin");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("plugin.json"),
            r#"{"name":"github","description":"GitHub workflows","interface":{"displayName":"GitHub","shortDescription":"Review pull requests"}}"#,
        )
        .unwrap();

        let untrusted_catalog = MentionCatalog::discover_with_skill_dirs(
            &[project.path().to_path_buf()],
            &orca_mcp::McpRegistry::default(),
            Some(orca_home.path()),
            None,
        );
        assert!(!untrusted_catalog.candidates().iter().any(|candidate| {
            candidate.kind == MentionKind::Skill && candidate.display == "review"
        }));

        orca_core::config::folder_trust::set_trust_with_config_dir(
            project.path(),
            orca_home.path(),
            orca_core::config::folder_trust::TrustLevel::Trusted,
        )
        .unwrap();
        let skills_catalog = MentionCatalog::discover_skills_with_dirs(
            &[project.path().to_path_buf()],
            Some(orca_home.path()),
            None,
        );
        assert!(skills_catalog.candidates().iter().any(|candidate| {
            candidate.kind == MentionKind::Skill && candidate.display == "review"
        }));
        assert!(
            !skills_catalog
                .candidates()
                .iter()
                .any(|candidate| candidate.kind == MentionKind::Plugin)
        );

        let catalog = MentionCatalog::discover_with_skill_dirs(
            &[project.path().to_path_buf()],
            &orca_mcp::McpRegistry::default(),
            Some(orca_home.path()),
            None,
        );

        assert!(catalog.candidates().iter().any(|candidate| {
            candidate.kind == MentionKind::Skill && candidate.display == "review"
        }));
        assert!(catalog.candidates().iter().any(|candidate| {
            candidate.kind == MentionKind::Plugin && candidate.display == "GitHub"
        }));
        assert_eq!(catalog.search("review", 8)[0].kind, MentionKind::Skill);
        assert!(
            catalog
                .search_kind("", MentionKind::Skill, 8)
                .iter()
                .all(|candidate| candidate.kind == MentionKind::Skill)
        );
    }

    #[test]
    fn unified_catalog_rejects_oversized_plugin_manifests_before_parsing() {
        let project = tempfile::tempdir().unwrap();
        let plugin_dir = project.path().join(".orca/plugins/huge/.codex-plugin");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::File::create(plugin_dir.join("plugin.json"))
            .unwrap()
            .set_len((MAX_PLUGIN_MANIFEST_BYTES + 1) as u64)
            .unwrap();

        let catalog = MentionCatalog::discover(
            &[project.path().to_path_buf()],
            &orca_mcp::McpRegistry::default(),
        );

        assert!(
            catalog
                .errors()
                .iter()
                .any(|error| error.contains("file is too large"))
        );
        assert!(
            !catalog
                .candidates()
                .iter()
                .any(|candidate| candidate.kind == MentionKind::Plugin)
        );
    }

    #[test]
    fn same_visible_name_expands_the_bound_skill_or_plugin_target() {
        let project = tempfile::tempdir().unwrap();
        let orca_home = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join("Cargo.toml"),
            "[package]\nname='mention-collision-test'\nversion='0.1.0'\n",
        )
        .unwrap();
        let skill_dir = project.path().join(".orca/skills/review");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: Review\ndescription: Skill review instructions\n---\n\nUse the skill target.\n",
        )
        .unwrap();
        let plugin_dir = project.path().join(".orca/plugins/review/.codex-plugin");
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(
            plugin_dir.join("plugin.json"),
            r#"{"name":"review-plugin","description":"Plugin review instructions","interface":{"displayName":"review"}}"#,
        )
        .unwrap();

        let registry = orca_mcp::McpRegistry::default();
        let roots = vec![project.path().to_path_buf()];
        orca_core::config::folder_trust::set_trust_with_config_dir(
            project.path(),
            orca_home.path(),
            orca_core::config::folder_trust::TrustLevel::Trusted,
        )
        .unwrap();
        let catalog = MentionCatalog::discover_with_skill_dirs(
            &roots,
            &registry,
            Some(orca_home.path()),
            None,
        );
        let skill = catalog
            .candidates()
            .iter()
            .find(|candidate| candidate.kind == MentionKind::Skill && candidate.display == "review")
            .expect("skill candidate");
        let plugin = catalog
            .candidates()
            .iter()
            .find(|candidate| {
                candidate.kind == MentionKind::Plugin && candidate.display == "review"
            })
            .expect("plugin candidate");
        assert_ne!(skill.id, plugin.id);

        let input = "use @review";
        let bind = |target: MentionTarget| {
            MentionBindings::from_bindings(
                input,
                vec![MentionBinding {
                    start: 4,
                    end: input.len(),
                    visible: "@review".to_string(),
                    target,
                }],
            )
        };
        let skill_expanded = expand_mentions_with_skill_dirs(
            input,
            &bind(skill.target.clone()),
            project.path(),
            &roots,
            &registry,
            Some(orca_home.path()),
            None,
        )
        .unwrap();
        let plugin_expanded = expand_mentions_with_skill_dirs(
            input,
            &bind(plugin.target.clone()),
            project.path(),
            &roots,
            &registry,
            Some(orca_home.path()),
            None,
        )
        .unwrap();

        assert!(skill_expanded.contains("<skills>"));
        assert!(skill_expanded.contains("Use the skill target."));
        assert!(!skill_expanded.contains("<plugin"));
        assert!(plugin_expanded.contains("<plugin name=\"review-plugin\""));
        assert!(plugin_expanded.contains("Plugin review instructions"));
        assert!(!plugin_expanded.contains("<skills>"));
    }

    #[test]
    fn unified_catalog_and_atomic_expansion_include_mcp_resources_and_templates() {
        struct ResourceTransport;

        impl orca_mcp::transport::McpTransport for ResourceTransport {
            fn initialize(&self) -> Result<serde_json::Value, String> {
                Ok(serde_json::json!({"capabilities": {"resources": {}}}))
            }

            fn list_tools(&self) -> Result<serde_json::Value, String> {
                Ok(serde_json::json!({"tools": []}))
            }

            fn call_tool(
                &self,
                _name: &str,
                _arguments: serde_json::Value,
            ) -> Result<serde_json::Value, String> {
                Err("not used".to_string())
            }

            fn list_resources(&self) -> Result<serde_json::Value, String> {
                Ok(serde_json::json!({
                    "resources": [{
                        "uri": "memo://one",
                        "name": "project memo",
                        "description": "Current project notes"
                    }]
                }))
            }

            fn list_resource_templates(&self) -> Result<serde_json::Value, String> {
                Ok(serde_json::json!({
                    "resourceTemplates": [{
                        "uriTemplate": "memo://{id}",
                        "name": "project memo template",
                        "description": "Parameterized project notes"
                    }]
                }))
            }

            fn read_resource(&self, uri: &str) -> Result<serde_json::Value, String> {
                Ok(serde_json::json!({
                    "contents": [{"uri": uri, "text": "resource body"}]
                }))
            }
        }

        let registry = orca_mcp::McpRegistry::from_resource_transports_for_test([(
            "notes".to_string(),
            Box::new(ResourceTransport) as Box<dyn orca_mcp::transport::McpTransport>,
        )]);
        let cwd = tempfile::tempdir().unwrap();
        let catalog = MentionCatalog::discover(&[cwd.path().to_path_buf()], &registry);
        let resource = catalog
            .candidates()
            .iter()
            .find(|candidate| candidate.kind == MentionKind::Resource)
            .expect("resource candidate");
        assert_eq!(resource.display, "project memo");

        let input = "use @\"project memo\"";
        let bindings = MentionBindings::from_bindings(
            input,
            vec![MentionBinding {
                start: 4,
                end: input.len(),
                visible: "@\"project memo\"".to_string(),
                target: resource.target.clone(),
            }],
        );
        let expanded = expand_mentions(
            input,
            &bindings,
            cwd.path(),
            &[cwd.path().to_path_buf()],
            &registry,
        )
        .unwrap();

        assert!(expanded.contains("<mcp_resource"));
        assert!(expanded.contains("resource body"));

        let template = catalog
            .candidates()
            .iter()
            .find(|candidate| matches!(candidate.target, MentionTarget::ResourceTemplate { .. }))
            .expect("resource template candidate");
        let template_input = "use @template";
        let template_bindings = MentionBindings::from_bindings(
            template_input,
            vec![MentionBinding {
                start: 4,
                end: template_input.len(),
                visible: "@template".to_string(),
                target: template.target.clone(),
            }],
        );
        let template_expanded = expand_mentions(
            template_input,
            &template_bindings,
            cwd.path(),
            &[cwd.path().to_path_buf()],
            &registry,
        )
        .unwrap();
        assert!(template_expanded.contains("<mcp_resource_template"));
        assert!(template_expanded.contains("memo://{id}"));

        let stale_bindings = MentionBindings::from_bindings(
            template_input,
            vec![MentionBinding {
                start: 4,
                end: template_input.len(),
                visible: "@template".to_string(),
                target: MentionTarget::ResourceTemplate {
                    server: "notes".to_string(),
                    uri_template: "memo://missing/{id}".to_string(),
                },
            }],
        );
        let error = expand_mentions(
            template_input,
            &stale_bindings,
            cwd.path(),
            &[cwd.path().to_path_buf()],
            &registry,
        )
        .unwrap_err();
        assert!(error.contains("bound MCP resource template is no longer available"));
    }
}
