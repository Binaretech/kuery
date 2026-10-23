//! What `kuery [PATH...]` asks for: each folder opens as a project and each
//! data file in a tab.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Version,
    /// Nothing to open shows the welcome window.
    Open(Vec<Target>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Project(PathBuf),
    DataFile(PathBuf),
}

/// Reads the arguments after the program name; relative paths are taken
/// from `cwd`. `--help` and `--version` win over everything else.
pub fn parse(args: impl IntoIterator<Item = OsString>, cwd: &Path) -> Result<Command, String> {
    let mut paths = Vec::new();
    let mut options = true;
    for arg in args {
        match arg.to_str() {
            Some("--") if options => options = false,
            Some("-h" | "--help") if options => return Ok(Command::Help),
            Some("-V" | "--version") if options => return Ok(Command::Version),
            Some(option) if options && option.starts_with('-') && option != "-" => {
                return Err(format!("unknown option {option} (see kuery --help)"));
            }
            _ => paths.push(cwd.join(arg)),
        }
    }
    paths
        .into_iter()
        .map(|path| target(&path))
        .collect::<Result<_, _>>()
        .map(Command::Open)
}

fn target(path: &Path) -> Result<Target, String> {
    let path = path
        .canonicalize()
        .map_err(|_| format!("{}: no such file or folder", path.display()))?;
    if path.is_dir() {
        Ok(Target::Project(path))
    } else if kuery_files::FileFormat::from_path(&path).is_ok() {
        Ok(Target::DataFile(path))
    } else {
        Err(format!(
            "{}: Kuery opens folders and csv, tsv, json, jsonl, ndjson and parquet files",
            path.display()
        ))
    }
}

pub fn help() -> String {
    format!(
        "Kuery {}

Usage: kuery [PATH...]

Opens each folder as a project and each data file (csv, tsv, json, jsonl,
ndjson, parquet) in a tab. Without a path, shows the welcome window.

Options:
  -h, --help     Print this help
  -V, --version  Print the version
",
        env!("CARGO_PKG_VERSION")
    )
}

pub fn version() -> String {
    format!("kuery {}", env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A folder holding `project/`, `orders.csv` and `notes.txt`.
    fn dir() -> Dir {
        let path = std::env::temp_dir().join(format!("kuery-cli-{}", crate::project::new_id()));
        std::fs::create_dir_all(path.join("project")).unwrap();
        std::fs::write(path.join("orders.csv"), "id\n1\n").unwrap();
        std::fs::write(path.join("notes.txt"), "hi").unwrap();
        Dir(path.canonicalize().unwrap())
    }

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn no_arguments_open_nothing() {
        let dir = dir();
        assert_eq!(parse(args(&[]), &dir.0), Ok(Command::Open(vec![])));
    }

    #[test]
    fn folders_open_as_projects_and_data_files_in_tabs() {
        let dir = dir();
        let absolute = dir.0.join("orders.csv");
        assert_eq!(
            parse(args(&["project", absolute.to_str().unwrap()]), &dir.0),
            Ok(Command::Open(vec![
                Target::Project(dir.0.join("project")),
                Target::DataFile(dir.0.join("orders.csv")),
            ]))
        );
        // `.` and `..` resolve to the folder itself.
        assert_eq!(
            parse(args(&["."]), &dir.0.join("project")),
            Ok(Command::Open(vec![Target::Project(dir.0.join("project"))]))
        );
    }

    #[test]
    fn a_missing_path_is_an_error() {
        let dir = dir();
        let missing = dir.0.join("missing.csv");
        assert_eq!(
            parse(args(&["missing.csv"]), &dir.0),
            Err(format!("{}: no such file or folder", missing.display()))
        );
    }

    #[test]
    fn a_file_kuery_cannot_open_is_an_error() {
        let dir = dir();
        assert_eq!(
            parse(args(&["notes.txt"]), &dir.0),
            Err(format!(
                "{}: Kuery opens folders and csv, tsv, json, jsonl, ndjson and parquet files",
                dir.0.join("notes.txt").display()
            ))
        );
    }

    #[test]
    fn help_and_version_win_over_paths() {
        let dir = dir();
        for flag in ["-h", "--help"] {
            assert_eq!(parse(args(&["missing", flag]), &dir.0), Ok(Command::Help));
        }
        for flag in ["-V", "--version"] {
            assert_eq!(parse(args(&[flag]), &dir.0), Ok(Command::Version));
        }
    }

    #[test]
    fn unknown_options_are_errors_and_dashes_end_options() {
        let dir = dir();
        assert_eq!(
            parse(args(&["--wat"]), &dir.0),
            Err("unknown option --wat (see kuery --help)".into())
        );
        std::fs::create_dir(dir.0.join("--help")).unwrap();
        assert_eq!(
            parse(args(&["--", "--help"]), &dir.0),
            Ok(Command::Open(vec![Target::Project(dir.0.join("--help"))]))
        );
    }

    #[test]
    fn help_names_the_usage_and_the_options() {
        let help = help();
        assert!(help.starts_with(&format!("Kuery {}\n", env!("CARGO_PKG_VERSION"))));
        for line in ["Usage: kuery [PATH...]", "-h, --help", "-V, --version"] {
            assert!(help.contains(line), "{line} missing from:\n{help}");
        }
        assert_eq!(version(), format!("kuery {}", env!("CARGO_PKG_VERSION")));
    }
}
