//! Recently opened projects, newest first, kept in the user config directory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const LIMIT: usize = 12;

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    projects: Vec<PathBuf>,
}

fn path() -> PathBuf {
    crate::paths::config_dir().join("recent.toml")
}

/// Recent project folders that still exist.
pub fn load() -> Vec<PathBuf> {
    read(&path())
        .into_iter()
        .filter(|p| crate::project::Project::is_project(p))
        .collect()
}

/// Moves `project` to the top of the list.
pub fn add(project: &Path) {
    let path = path();
    let mut projects = read(&path);
    projects.retain(|p| p != project);
    projects.insert(0, project.to_path_buf());
    projects.truncate(LIMIT);
    let text = toml::to_string(&File { projects }).unwrap_or_default();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&path, text);
}

fn read(path: &Path) -> Vec<PathBuf> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<File>(&text).ok())
        .map(|file| file.projects)
        .unwrap_or_default()
}
