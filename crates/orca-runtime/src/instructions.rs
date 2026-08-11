use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const INSTRUCTIONS_FILE: &str = "AGENTS.md";
const RULES_DIR: &str = ".orca/rules";
const ORCA_HOME_ENV: &str = "ORCA_HOME";

#[derive(Clone, Debug, Default)]
pub struct ProjectInstructions {
    sections: Vec<InstructionSection>,
}

#[derive(Clone, Debug)]
struct InstructionSection {
    source: PathBuf,
    content: String,
}

impl ProjectInstructions {
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    pub fn to_system_prompt_block(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }

        let mut block = String::from("<project-instructions>\n");
        for (index, section) in self.sections.iter().enumerate() {
            if index > 0 {
                block.push('\n');
            }
            block.push_str("# ");
            block.push_str(&section.source.display().to_string());
            block.push('\n');
            block.push_str(section.content.trim());
            block.push('\n');
        }
        block.push_str("</project-instructions>");
        Some(block)
    }
}

pub fn load_for_cwd_or_default(cwd: &Path) -> ProjectInstructions {
    match load_for_cwd(cwd) {
        Ok(instructions) => instructions,
        Err(error) => {
            eprintln!("orca: warning: failed to load project instructions: {error}");
            ProjectInstructions::default()
        }
    }
}

pub fn load_for_cwd(cwd: &Path) -> io::Result<ProjectInstructions> {
    let orca_home_path = orca_home();
    load_for_cwd_with_home(cwd, orca_home_path.as_deref())
}

fn load_for_cwd_with_home(
    cwd: &Path,
    orca_home_path: Option<&Path>,
) -> io::Result<ProjectInstructions> {
    let mut sections = Vec::new();
    let mut visited = HashSet::new();

    if let Some(user_agents) = orca_home_path.map(|home| home.join(INSTRUCTIONS_FILE))
        && user_agents.is_file()
    {
        sections.push(InstructionSection {
            source: user_agents.clone(),
            content: read_with_includes(&user_agents, &mut visited, orca_home_path)?,
        });
    }

    if let Some(project_root) = find_project_root(cwd)
        && orca_home_path.is_some_and(|home| {
            orca_core::config::folder_trust::is_trusted_with_config_dir(&project_root, home)
        })
    {
        let root_agents = project_root.join(INSTRUCTIONS_FILE);
        if root_agents.is_file() {
            sections.push(InstructionSection {
                source: root_agents.clone(),
                content: read_with_includes(&root_agents, &mut visited, Some(&project_root))?,
            });
        }

        let rules_dir = project_root.join(RULES_DIR);
        for rule in sorted_markdown_files(&rules_dir)? {
            sections.push(InstructionSection {
                source: rule.clone(),
                content: read_with_includes(&rule, &mut visited, Some(&project_root))?,
            });
        }
    }

    Ok(ProjectInstructions { sections })
}

fn orca_home() -> Option<PathBuf> {
    #[cfg(test)]
    let test_home = crate::history::read_test_orca_home();
    #[cfg(not(test))]
    let test_home = std::env::var_os(ORCA_HOME_ENV);
    test_home
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".orca")))
}

fn find_project_root(cwd: &Path) -> Option<PathBuf> {
    // Search both the original path and canonical path to handle symlinks
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
    None
}

fn sorted_markdown_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn read_with_includes(
    path: &Path,
    visited: &mut HashSet<PathBuf>,
    allowed_root: Option<&Path>,
) -> io::Result<String> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("cyclic @include in {}", path.display()),
        ));
    }

    let content = fs::read_to_string(path)?;
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut output = String::new();
    let mut seen_includes = HashSet::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(include_path) = trimmed.strip_prefix("@include ") {
            let include_path = include_path.trim();
            if include_path.is_empty() {
                continue;
            }
            let resolved = base_dir.join(include_path);
            // Path traversal guard: included file must reside within allowed_root
            if let Some(root) = allowed_root {
                let resolved_canonical =
                    resolved.canonicalize().unwrap_or_else(|_| resolved.clone());
                let root_canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
                if !resolved_canonical.starts_with(&root_canonical) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("@include path escapes project root: {}", include_path),
                    ));
                }
            }
            if seen_includes.insert(resolved.clone()) {
                let included = read_with_includes(&resolved, visited, allowed_root)?;
                output.push_str(included.trim());
                output.push('\n');
            }
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn trust_project(home: &Path, project: &Path) {
        orca_core::config::folder_trust::set_trust_with_config_dir(
            project,
            home,
            orca_core::config::folder_trust::TrustLevel::Trusted,
        )
        .unwrap();
    }

    #[test]
    fn loads_agents_file_and_sorted_project_rules() {
        let dir = TempDir::new().expect("temp dir");
        let home = TempDir::new().expect("temp home");
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        fs::write(dir.path().join(INSTRUCTIONS_FILE), "Root instructions\n").unwrap();
        fs::create_dir_all(dir.path().join(RULES_DIR)).unwrap();
        fs::write(
            dir.path().join(".orca/rules/020-second.md"),
            "Second rule\n",
        )
        .unwrap();
        fs::write(dir.path().join(".orca/rules/010-first.md"), "First rule\n").unwrap();
        trust_project(home.path(), dir.path());

        let instructions = load_for_cwd_with_home(dir.path(), Some(home.path())).unwrap();
        let block = instructions.to_system_prompt_block().unwrap();

        assert!(block.contains("<project-instructions>"));
        let root_index = block.find("Root instructions").unwrap();
        let first_index = block.find("First rule").unwrap();
        let second_index = block.find("Second rule").unwrap();
        assert!(root_index < first_index);
        assert!(first_index < second_index);
    }

    #[test]
    fn expands_relative_includes() {
        let dir = TempDir::new().expect("temp dir");
        let home = TempDir::new().expect("temp home");
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        fs::write(dir.path().join("shared.md"), "Shared instruction\n").unwrap();
        fs::write(
            dir.path().join(INSTRUCTIONS_FILE),
            "Before\n@include ./shared.md\nAfter\n",
        )
        .unwrap();
        trust_project(home.path(), dir.path());

        let instructions = load_for_cwd_with_home(dir.path(), Some(home.path())).unwrap();
        let block = instructions.to_system_prompt_block().unwrap();

        assert!(block.contains("Before\nShared instruction\nAfter"));
    }

    #[test]
    fn include_rejects_path_traversal_outside_project_root() {
        let dir = TempDir::new().expect("temp dir");
        let home = TempDir::new().expect("temp home");
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        fs::write(
            dir.path().join(INSTRUCTIONS_FILE),
            "@include ../../etc/passwd\n",
        )
        .unwrap();
        trust_project(home.path(), dir.path());

        let result = load_for_cwd_with_home(dir.path(), Some(home.path()));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("escapes project root"));
    }

    #[test]
    fn untrusted_project_instructions_are_not_loaded() {
        let dir = TempDir::new().expect("temp dir");
        let home = TempDir::new().expect("temp home");
        fs::write(home.path().join(INSTRUCTIONS_FILE), "User instructions\n").unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        fs::write(
            dir.path().join(INSTRUCTIONS_FILE),
            "Untrusted project instructions\n",
        )
        .unwrap();

        let instructions = load_for_cwd_with_home(dir.path(), Some(home.path())).unwrap();
        let block = instructions.to_system_prompt_block().unwrap();

        assert!(block.contains("User instructions"));
        assert!(!block.contains("Untrusted project instructions"));
    }
}
