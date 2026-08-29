use std::collections::HashMap;
use std::path::PathBuf;

use orca_core::config::PermissionProfileNetworkAccess;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionResponseDecision {
    Allow,
    Deny,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionGrantScope {
    #[default]
    Turn,
    Session,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestPermissionProfile {
    #[serde(default)]
    pub file_system: Option<RequestFileSystemPermissions>,
    #[serde(default)]
    pub network: Option<RequestNetworkPermissions>,
}

impl RequestPermissionProfile {
    pub fn normalize_file_system_entries(mut self) -> Self {
        if let Some(file_system) = self.file_system.take() {
            self.file_system = Some(file_system.normalize_entries());
        }
        self
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestFileSystemPermissions {
    #[serde(default)]
    pub read: Option<Vec<PathBuf>>,
    #[serde(default)]
    pub write: Option<Vec<PathBuf>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entries: Option<Vec<FileSystemSandboxEntry>>,
}

impl RequestFileSystemPermissions {
    fn normalize_entries(mut self) -> Self {
        for entry in self.entries.iter().flatten() {
            match entry.access {
                FileSystemAccessMode::Read => {
                    push_unique_path(self.read.get_or_insert_with(Vec::new), entry.path.clone());
                }
                FileSystemAccessMode::Write => {
                    push_unique_path(self.write.get_or_insert_with(Vec::new), entry.path.clone());
                }
                FileSystemAccessMode::ReadWrite => {
                    push_unique_path(self.read.get_or_insert_with(Vec::new), entry.path.clone());
                    push_unique_path(self.write.get_or_insert_with(Vec::new), entry.path.clone());
                }
            }
        }
        self.entries = None;
        self
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemSandboxEntry {
    #[serde(deserialize_with = "deserialize_file_system_entry_path")]
    pub path: PathBuf,
    pub access: FileSystemAccessMode,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FileSystemAccessMode {
    Read,
    Write,
    ReadWrite,
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

fn deserialize_file_system_entry_path<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum WirePath {
        Legacy(PathBuf),
        Structured(StructuredFileSystemPath),
    }

    match WirePath::deserialize(deserializer)? {
        WirePath::Legacy(path) => Ok(path),
        WirePath::Structured(path) => Ok(path.into_path_label()),
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StructuredFileSystemPath {
    Path {
        path: PathBuf,
    },
    GlobPattern {
        pattern: String,
    },
    Special {
        value: StructuredFileSystemSpecialPath,
    },
}

impl StructuredFileSystemPath {
    fn into_path_label(self) -> PathBuf {
        match self {
            Self::Path { path } => path,
            Self::GlobPattern { pattern } => PathBuf::from(format!("glob:{pattern}")),
            Self::Special { value } => value.into_path_label(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StructuredFileSystemSpecialPath {
    Root,
    Minimal,
    #[serde(alias = "current_working_directory")]
    ProjectRoots {
        subpath: Option<PathBuf>,
    },
    Tmpdir,
    SlashTmp,
    Unknown {
        path: String,
        subpath: Option<PathBuf>,
    },
}

impl StructuredFileSystemSpecialPath {
    fn into_path_label(self) -> PathBuf {
        match self {
            Self::Root => PathBuf::from(":root"),
            Self::Minimal => PathBuf::from(":minimal"),
            Self::ProjectRoots { subpath } => special_path_label(":workspace_roots", subpath),
            Self::Tmpdir => PathBuf::from(":tmpdir"),
            Self::SlashTmp => PathBuf::from("/tmp"),
            Self::Unknown { path, subpath } => special_path_label(&path, subpath),
        }
    }
}

fn special_path_label(base: &str, subpath: Option<PathBuf>) -> PathBuf {
    match subpath {
        Some(subpath) => {
            let mut label = PathBuf::from(base);
            for component in subpath.components() {
                if let std::path::Component::Normal(part) = component {
                    label.push(part);
                }
            }
            label
        }
        None => PathBuf::from(base),
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestNetworkPermissions {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub domains: HashMap<String, PermissionProfileNetworkAccess>,
}
