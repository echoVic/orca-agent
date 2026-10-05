use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSetBuilder};
use orca_core::tool_types::{ToolRequest, ToolResult};
use serde::Deserialize;
use walkdir::WalkDir;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub paths: Vec<String>,
    pub source: SkillSource,
    pub path: PathBuf,
    pub body: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillSource {
    User,
    Project,
}

impl SkillSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

#[derive(Deserialize)]
struct ReadSkillArgs {
    id: String,
}

pub fn execute_list(request: &ToolRequest, cwd: &Path) -> ToolResult {
    match discover_from_env(cwd) {
        Ok(skills) => {
            let output = format_skill_list(&skills);
            ToolResult::completed(request, output, false)
        }
        Err(error) => ToolResult::failed(request, error, None),
    }
}

pub fn execute_read(request: &ToolRequest, cwd: &Path) -> ToolResult {
    let args = match parse_read_args(request) {
        Ok(args) => args,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    match discover_from_env(cwd) {
        Ok(skills) => match skills.into_iter().find(|skill| skill.id == args.id) {
            Some(skill) => ToolResult::completed(request, format_skill(&skill), false),
            None => ToolResult::failed(request, format!("unknown skill: {}", args.id), None),
        },
        Err(error) => ToolResult::failed(request, error, None),
    }
}

pub fn discover_from_env(cwd: &Path) -> Result<Vec<Skill>, String> {
    let orca_home = orca_core::home::orca_home();
    let agents_home = dirs::home_dir().map(|home| home.join(".agents"));
    discover(cwd, orca_home.as_deref(), agents_home.as_deref())
}

pub fn discover(
    cwd: &Path,
    orca_home: Option<&Path>,
    agents_home: Option<&Path>,
) -> Result<Vec<Skill>, String> {
    discover_with_body(cwd, orca_home, agents_home, true)
}

pub fn discover_metadata(
    cwd: &Path,
    orca_home: Option<&Path>,
    agents_home: Option<&Path>,
) -> Result<Vec<Skill>, String> {
    discover_with_body(cwd, orca_home, agents_home, false)
}

fn discover_with_body(
    cwd: &Path,
    orca_home: Option<&Path>,
    agents_home: Option<&Path>,
    include_body: bool,
) -> Result<Vec<Skill>, String> {
    let project_root = project_root(cwd);
    let mut skills = Vec::new();
    if let Some(home) = orca_home {
        collect_skills(
            &home.join("skills"),
            SkillSource::User,
            None,
            include_body,
            &mut skills,
        )?;
    }
    if let Some(home) = agents_home {
        collect_skills(
            &home.join("skills"),
            SkillSource::User,
            None,
            include_body,
            &mut skills,
        )?;
    }
    if let Some(project_root) = project_root
        && orca_home.is_some_and(|home| {
            orca_core::config::folder_trust::is_trusted_with_config_dir(&project_root, home)
        })
    {
        collect_skills(
            &project_root.join(".orca/skills"),
            SkillSource::Project,
            Some(&project_root),
            include_body,
            &mut skills,
        )?;
        collect_skills(
            &project_root.join(".agents/skills"),
            SkillSource::Project,
            Some(&project_root),
            include_body,
            &mut skills,
        )?;
    }
    skills.sort_by(|left, right| left.id.cmp(&right.id));
    skills.dedup_by(|left, right| left.id == right.id);
    Ok(skills)
}

pub fn explicit_skill_prompt_block(cwd: &Path, prompt: &str) -> Result<Option<String>, String> {
    let mentioned = mentioned_skill_mentions(prompt);
    if mentioned.is_empty() {
        return Ok(None);
    }
    let skills = discover_from_env(cwd)?;
    let selected: Vec<(Skill, Option<String>)> = mentioned
        .into_iter()
        .filter_map(|(id, arg)| {
            skills
                .iter()
                .find(|skill| skill.id == id)
                .map(|skill| (skill.clone(), arg))
        })
        .collect();
    Ok(format_skills_prompt_block_with_args(&selected))
}

/// Returns `(id, arg)` pairs for every `$id` or `$id:arg` mention in `text`.
pub fn mentioned_skill_mentions(text: &str) -> Vec<(String, Option<String>)> {
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'$' {
            index += 1;
            continue;
        }
        index += 1;
        let start = index;
        while index < bytes.len() {
            let byte = bytes[index];
            if byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' {
                index += 1;
            } else {
                break;
            }
        }
        if index == start {
            continue;
        }
        let id = text[start..index].to_string();
        // optional `:arg` suffix
        let arg = if index < bytes.len() && bytes[index] == b':' {
            let arg_start = index + 1;
            let mut arg_end = arg_start;
            while arg_end < bytes.len() && bytes[arg_end] != b' ' && bytes[arg_end] != b'\n' {
                arg_end += 1;
            }
            index = arg_end;
            if arg_end > arg_start {
                Some(text[arg_start..arg_end].to_string())
            } else {
                None
            }
        } else {
            None
        };
        if seen.insert(id.clone()) {
            result.push((id, arg));
        }
    }
    result
}

pub fn mentioned_skill_ids(text: &str) -> Vec<String> {
    mentioned_skill_mentions(text)
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

pub fn format_skills_prompt_block(skills: &[Skill]) -> Option<String> {
    let pairs: Vec<(Skill, Option<String>)> = skills.iter().map(|s| (s.clone(), None)).collect();
    format_skills_prompt_block_with_args(&pairs)
}

pub fn format_skills_prompt_block_with_args(skills: &[(Skill, Option<String>)]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut block = String::from("<skills>\n");
    for (skill, arg) in skills {
        block.push_str(&format!(
            r#"<skill id="{}" name="{}" source="{}" path="{}">"#,
            escape_attr(&skill.id),
            escape_attr(&skill.name),
            skill.source.as_str(),
            escape_attr(&skill.path.display().to_string())
        ));
        block.push('\n');
        if let Some(when) = &skill.when_to_use {
            block.push_str(&format!("When to use: {when}\n"));
        }
        let body = if let Some(arg) = arg {
            skill.body.replace("{{arg}}", arg)
        } else {
            skill.body.clone()
        };
        block.push_str(&body);
        if !body.ends_with('\n') {
            block.push('\n');
        }
        block.push_str("</skill>\n");
    }
    block.push_str("</skills>");
    Some(block)
}

fn collect_skills(
    skills_dir: &Path,
    source: SkillSource,
    allowed_root: Option<&Path>,
    include_body: bool,
    skills: &mut Vec<Skill>,
) -> Result<(), String> {
    if !skills_dir.is_dir() {
        return Ok(());
    }
    let entries = fs::read_dir(skills_dir).map_err(|error| {
        format!(
            "failed to read skills dir {}: {error}",
            skills_dir.display()
        )
    })?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !valid_skill_id(id) {
            continue;
        }
        let skill_path = path.join("SKILL.md");
        if !skill_path.is_file() {
            continue;
        }
        if let Some(root) = allowed_root
            && !canonical_or_self(&skill_path).starts_with(&canonical_or_self(root))
        {
            continue;
        }
        let parsed = if include_body {
            parse_skill(id, source, &skill_path)
        } else {
            parse_skill_metadata(id, source, &skill_path)
        };
        if let Ok(skill) = parsed {
            if let Some(root) = allowed_root {
                if !skill.paths.is_empty() && !paths_match(&skill.paths, root) {
                    continue;
                }
            }
            skills.push(skill);
        }
    }
    Ok(())
}

fn paths_match(patterns: &[String], root: &Path) -> bool {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        if let Ok(glob) = Glob::new(pat) {
            builder.add(glob);
        }
    }
    let Ok(set) = builder.build() else {
        return true;
    };
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_str().unwrap_or("");
            !matches!(
                name,
                "target" | "node_modules" | ".git" | "dist" | ".next" | "build"
            )
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .any(|e| {
            let rel = e.path().strip_prefix(root).unwrap_or(e.path());
            set.is_match(rel)
        })
}

fn parse_skill(id: &str, source: SkillSource, path: &Path) -> Result<Skill, String> {
    let content = fs::read_to_string(path)
        .map_err(|error| format!("failed to read skill {}: {error}", path.display()))?;
    let (frontmatter, body) = split_frontmatter(&content);
    let (name, description, when_to_use, paths) = parse_frontmatter(frontmatter);
    Ok(Skill {
        id: id.to_string(),
        name: name.unwrap_or_else(|| id.to_string()),
        description: description.unwrap_or_default(),
        when_to_use,
        paths,
        source,
        path: path.to_path_buf(),
        body: body.trim().to_string(),
    })
}

fn parse_skill_metadata(id: &str, source: SkillSource, path: &Path) -> Result<Skill, String> {
    const MAX_FRONTMATTER_BYTES: usize = 64 * 1024;

    let file = fs::File::open(path)
        .map_err(|error| format!("failed to read skill {}: {error}", path.display()))?;
    let mut lines = std::io::BufReader::new(file).lines();
    let frontmatter = if lines
        .next()
        .transpose()
        .map_err(|error| format!("failed to read skill metadata {}: {error}", path.display()))?
        == Some("---".to_string())
    {
        let mut frontmatter = String::new();
        let mut closed = false;
        for line in lines {
            let line = line.map_err(|error| {
                format!("failed to read skill metadata {}: {error}", path.display())
            })?;
            if line == "---" {
                closed = true;
                break;
            }
            if frontmatter.len().saturating_add(line.len()) > MAX_FRONTMATTER_BYTES {
                break;
            }
            frontmatter.push_str(&line);
            frontmatter.push('\n');
        }
        closed.then_some(frontmatter)
    } else {
        None
    };
    let (name, description, when_to_use, paths) = parse_frontmatter(frontmatter.as_deref());
    Ok(Skill {
        id: id.to_string(),
        name: name.unwrap_or_else(|| id.to_string()),
        description: description.unwrap_or_default(),
        when_to_use,
        paths,
        source,
        path: path.to_path_buf(),
        body: String::new(),
    })
}

fn split_frontmatter(content: &str) -> (Option<&str>, &str) {
    let Some(rest) = content.strip_prefix("---\n") else {
        return (None, content);
    };
    let Some(end) = rest.find("\n---\n") else {
        return (None, content);
    };
    let frontmatter = &rest[..end];
    let body = &rest[end + "\n---\n".len()..];
    (Some(frontmatter), body)
}

fn parse_frontmatter(
    frontmatter: Option<&str>,
) -> (Option<String>, Option<String>, Option<String>, Vec<String>) {
    let mut name = None;
    let mut description = None;
    let mut when_to_use = None;
    let mut paths = Vec::new();
    let Some(frontmatter) = frontmatter else {
        return (name, description, when_to_use, paths);
    };
    let mut in_paths = false;
    for line in frontmatter.lines() {
        if in_paths {
            let trimmed = line.trim();
            if trimmed.starts_with('-') {
                let pat = trimmed
                    .trim_start_matches('-')
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string();
                if !pat.is_empty() {
                    paths.push(pat);
                }
                continue;
            } else {
                in_paths = false;
            }
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_string();
        match key.trim() {
            "name" => name = Some(value),
            "description" => description = Some(value),
            "when_to_use" => when_to_use = Some(value),
            "paths" => in_paths = true,
            _ => {}
        }
    }
    (name, description, when_to_use, paths)
}

fn format_skill_list(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return "(no skills found)".to_string();
    }
    skills
        .iter()
        .map(|skill| {
            format!(
                "{} [{}] - {}",
                skill.id,
                skill.source.as_str(),
                if skill.description.is_empty() {
                    &skill.name
                } else {
                    &skill.description
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_skill(skill: &Skill) -> String {
    format!(
        "# {}\nsource: {}\nid: {}\npath: {}\n\n{}",
        skill.name,
        skill.source.as_str(),
        skill.id,
        skill.path.display(),
        skill.body
    )
}

fn parse_read_args(request: &ToolRequest) -> Result<ReadSkillArgs, String> {
    let raw = request
        .raw_arguments
        .as_deref()
        .ok_or_else(|| "missing read_skill arguments JSON".to_string())?;
    let args: ReadSkillArgs = serde_json::from_str(raw)
        .map_err(|error| format!("invalid read_skill arguments JSON: {error}"))?;
    if args.id.trim().is_empty() {
        return Err("missing required read_skill argument: id".to_string());
    }
    Ok(args)
}

fn valid_skill_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

fn project_root(cwd: &Path) -> Option<PathBuf> {
    for start in [Some(cwd.to_path_buf()), cwd.canonicalize().ok()]
        .into_iter()
        .flatten()
    {
        for candidate in start.ancestors() {
            if candidate.join(".git").exists()
                || candidate.join("Cargo.toml").exists()
                || candidate.join("package.json").exists()
            {
                return Some(candidate.to_path_buf());
            }
        }
    }
    Some(cwd.to_path_buf())
}

fn canonical_or_self(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
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

    fn trust_project(home: &Path, project: &Path) {
        orca_core::config::folder_trust::set_trust_with_config_dir(
            project,
            home,
            orca_core::config::folder_trust::TrustLevel::Trusted,
        )
        .unwrap();
    }

    #[test]
    fn discovers_user_and_project_skills_with_frontmatter() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\n",
        )
        .unwrap();
        write_skill(
            &home.path().join("skills/debugging"),
            "Debugging",
            "Find root causes",
            "Use logs first.",
        );
        write_skill(
            &project.path().join(".orca/skills/review"),
            "Review",
            "Review code safely",
            "Read the diff.",
        );
        trust_project(home.path(), project.path());

        let skills = discover(project.path(), Some(home.path()), None).unwrap();

        assert!(skills.iter().any(|skill| skill.id == "debugging"));
        assert!(skills.iter().any(|skill| skill.id == "review"));
        assert!(
            skills
                .iter()
                .any(|skill| skill.description == "Review code safely")
        );
    }

    #[test]
    fn discovers_user_skills_from_agents_skills_dir() {
        let agents_home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_skill(
            &agents_home.path().join("skills/lint"),
            "Lint",
            "Run linter",
            "Run cargo clippy.",
        );

        let skills = discover(project.path(), None, Some(agents_home.path())).unwrap();

        assert!(skills.iter().any(|s| s.id == "lint"));
    }

    #[test]
    fn metadata_discovery_reads_frontmatter_without_loading_body() {
        let agents_home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_skill(
            &agents_home.path().join("skills/lint"),
            "Lint",
            "Run linter",
            &"large body ".repeat(10_000),
        );

        let skill = discover_metadata(project.path(), None, Some(agents_home.path()))
            .unwrap()
            .into_iter()
            .find(|skill| skill.id == "lint")
            .unwrap();

        assert_eq!(skill.name, "Lint");
        assert_eq!(skill.description, "Run linter");
        assert!(skill.body.is_empty());
    }

    #[test]
    fn discovers_project_skills_from_agents_skills_dir() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\n",
        )
        .unwrap();
        write_skill(
            &project.path().join(".agents/skills/lint"),
            "Lint",
            "Run linter",
            "Run cargo clippy.",
        );
        trust_project(home.path(), project.path());

        let skills = discover(project.path(), Some(home.path()), None).unwrap();

        assert!(skills.iter().any(|s| s.id == "lint"));
    }

    #[test]
    fn orca_skills_take_priority_over_agents_skills_on_same_id() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\n",
        )
        .unwrap();
        write_skill(
            &project.path().join(".orca/skills/review"),
            "Review",
            "orca version",
            "orca body",
        );
        write_skill(
            &project.path().join(".agents/skills/review"),
            "Review",
            "agents version",
            "agents body",
        );
        trust_project(home.path(), project.path());

        let skills = discover(project.path(), Some(home.path()), None).unwrap();
        let review: Vec<_> = skills.iter().filter(|s| s.id == "review").collect();

        assert_eq!(review.len(), 1);
        assert_eq!(review[0].description, "orca version");
    }

    #[test]
    fn untrusted_project_skills_are_not_discovered() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\n",
        )
        .unwrap();
        write_skill(
            &project.path().join(".orca/skills/review"),
            "Review",
            "project skill",
            "untrusted body",
        );

        let skills = discover(project.path(), Some(home.path()), None).unwrap();

        assert!(!skills.iter().any(|skill| skill.id == "review"));
    }

    #[test]
    fn read_skill_formats_body_without_frontmatter() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_skill(
            &home.path().join("skills/debugging"),
            "Debugging",
            "Find root causes",
            "Use logs first.",
        );

        let skill = discover(project.path(), Some(home.path()), None)
            .unwrap()
            .into_iter()
            .find(|skill| skill.id == "debugging")
            .unwrap();

        let formatted = format_skill(&skill);
        assert!(formatted.contains("# Debugging"));
        assert!(formatted.contains("Use logs first."));
        assert!(!formatted.contains("---"));
    }

    #[test]
    fn mentioned_skill_ids_deduplicates_in_mention_order() {
        let ids = mentioned_skill_ids("use $review then $debugging and $review again");

        assert_eq!(ids, vec!["review", "debugging"]);
    }

    #[test]
    fn format_skills_prompt_block_includes_skill_body() {
        let skill = Skill {
            id: "debugging".to_string(),
            name: "Debugging".to_string(),
            description: "Find root causes".to_string(),
            when_to_use: None,
            paths: vec![],
            source: SkillSource::User,
            path: std::env::temp_dir().join("SKILL.md"),
            body: "Use logs first.".to_string(),
        };

        let block = format_skills_prompt_block(&[skill]).expect("skill block");

        assert!(block.contains(r#"<skill id="debugging""#));
        assert!(block.contains("Use logs first."));
    }

    fn write_skill(dir: &Path, name: &str, description: &str, body: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
        )
        .unwrap();
    }
}
