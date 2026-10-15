//! Recently opened projects, newest first, kept in the user config directory.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::project::Project;

const LIMIT: usize = 20;

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    projects: Vec<PathBuf>,
}

/// One entry of the list, as the welcome window and menus show it.
#[derive(Debug, Clone, PartialEq)]
pub struct RecentProject {
    pub path: PathBuf,
    /// The project's name, or the folder name if it cannot be read.
    pub name: String,
    /// False when the folder (or its `.dbadmin/`) is gone.
    pub available: bool,
}

impl RecentProject {
    /// The folder with the home directory shortened to `~`.
    pub fn display_path(&self) -> String {
        let path = self.path.display().to_string();
        match std::env::var("HOME") {
            Ok(home) if !home.is_empty() && path.starts_with(&home) => {
                format!("~{}", &path[home.len()..])
            }
            _ => path,
        }
    }
}

fn path() -> PathBuf {
    crate::paths::config_dir().join("recent.toml")
}

/// Every recent project, including ones whose folder no longer exists, so
/// they can be shown and removed.
pub fn entries() -> Vec<RecentProject> {
    read(&path()).into_iter().map(entry).collect()
}

/// Recent projects that can still be opened.
pub fn available() -> Vec<RecentProject> {
    entries().into_iter().filter(|p| p.available).collect()
}

fn entry(path: PathBuf) -> RecentProject {
    let folder_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    match Project::open(&path) {
        Ok(project) => RecentProject {
            path,
            name: project.name,
            available: true,
        },
        Err(_) => RecentProject {
            available: Project::is_project(&path),
            path,
            name: folder_name,
        },
    }
}

/// Moves `project` to the top of the list.
pub fn add(project: &Path) {
    add_in(&path(), project);
}

/// Takes `project` off the list. The folder itself is left alone.
pub fn remove(project: &Path) {
    remove_in(&path(), project);
}

fn add_in(file: &Path, project: &Path) {
    update(file, |projects| {
        projects.retain(|p| p != project);
        projects.insert(0, project.to_path_buf());
        projects.truncate(LIMIT);
    });
}

fn remove_in(file: &Path, project: &Path) {
    update(file, |projects| projects.retain(|p| p != project));
}

/// Empties the list.
pub fn clear() {
    update(&path(), Vec::clear);
}

fn update(path: &Path, change: impl FnOnce(&mut Vec<PathBuf>)) {
    let mut projects = read(path);
    change(&mut projects);
    let text = toml::to_string(&File { projects }).unwrap_or_default();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, text);
}

fn read(path: &Path) -> Vec<PathBuf> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<File>(&text).ok())
        .map(|file| file.projects)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "dbadmin-recent-{name}-{}",
                crate::project::new_id()
            ))
            .join("recent.toml")
    }

    #[test]
    fn newest_first_without_duplicates() {
        let file = temp_file("order");
        for project in ["/a", "/b", "/a"] {
            add_in(&file, Path::new(project));
        }
        assert_eq!(read(&file), [PathBuf::from("/a"), PathBuf::from("/b")]);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn remove_and_clear() {
        let file = temp_file("remove");
        for project in ["/c", "/b", "/a"] {
            add_in(&file, Path::new(project));
        }
        remove_in(&file, Path::new("/b"));
        assert_eq!(read(&file), [PathBuf::from("/a"), PathBuf::from("/c")]);
        update(&file, Vec::clear);
        assert!(read(&file).is_empty());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn missing_folders_are_listed_as_unavailable() {
        let missing = entry(PathBuf::from("/definitely/not/a/dbadmin/project"));
        assert!(!missing.available);
        assert_eq!(missing.name, "project");
    }
}
