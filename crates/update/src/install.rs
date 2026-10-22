//! Replaces the installed app with a verified archive's.
//!
//! The archive is unpacked next to the installed app (so moving it into
//! place is a rename on the same disk), checked, and swapped in: the old app
//! is renamed aside, the new one renamed into its place, and the old one put
//! back if that fails. The running process keeps its files open, so it can
//! carry on until it restarts.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Error;

/// Where the next update unpacks, next to the installed app.
const STAGING: &str = ".kuery-update";

/// The running app, as it can be replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installation {
    /// `Kuery.app` on macOS.
    MacApp { bundle: PathBuf },
    /// The `kuery` or `kuery.exe` binary on Linux or Windows.
    Binary { exe: PathBuf },
}

impl Installation {
    /// The running app's.
    pub fn current() -> Result<Self, Error> {
        let exe = std::env::current_exe()?.canonicalize()?;
        if cfg!(target_os = "macos") {
            let bundle = exe
                .ancestors()
                .find(|path| path.extension().is_some_and(|ext| ext == "app"))
                .ok_or_else(|| Error::Install("Kuery is not running from Kuery.app".into()))?;
            Ok(Self::MacApp {
                bundle: bundle.to_path_buf(),
            })
        } else {
            Ok(Self::Binary { exe })
        }
    }

    /// The installed app: the bundle or the binary.
    pub fn path(&self) -> &Path {
        match self {
            Self::MacApp { bundle } => bundle,
            Self::Binary { exe } => exe,
        }
    }

    /// What the release archive calls the app.
    fn payload_name(&self) -> &'static str {
        match self {
            Self::MacApp { .. } => "Kuery.app",
            Self::Binary { .. } if cfg!(windows) => "kuery.exe",
            Self::Binary { .. } => "kuery",
        }
    }

    fn parent(&self) -> Result<&Path, Error> {
        self.path()
            .parent()
            .ok_or_else(|| Error::Install(format!("{} has no folder", self.path().display())))
    }

    /// Where the replaced app is moved while swapping.
    fn aside(&self) -> Result<PathBuf, Error> {
        let name = self
            .path()
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        Ok(self.parent()?.join(format!(".{name}.old")))
    }

    /// Replaces the installed app with the one in `archive` (already
    /// verified by [`crate::Updater::download`]) and returns what to
    /// relaunch.
    pub fn install(&self, archive: &Path) -> Result<PathBuf, Error> {
        let staging = self.parent()?.join(STAGING);
        remove(&staging);
        std::fs::create_dir_all(&staging).map_err(|e| writable(e, &staging))?;
        let result = self.install_from(archive, &staging);
        remove(&staging);
        result
    }

    fn install_from(&self, archive: &Path, staging: &Path) -> Result<PathBuf, Error> {
        extract(archive, staging)?;
        let payload = find(staging, self.payload_name())?;
        match self {
            Self::MacApp { bundle } => check_code_signature(&payload, bundle)?,
            Self::Binary { .. } => make_executable(&payload)?,
        }
        replace(self.path(), &payload, &self.aside()?)?;
        Ok(self.path().to_path_buf())
    }

    /// Removes what an earlier update left: its staging folder and the
    /// replaced app, which Windows cannot delete while it runs.
    pub fn clean_up(&self) {
        if let Ok(parent) = self.parent() {
            remove(&parent.join(STAGING));
        }
        if let Ok(aside) = self.aside() {
            remove(&aside);
        }
    }
}

/// Unpacks `archive` (a `.zip` or `.tar.gz`) into `dir` with the system's
/// own tools.
fn extract(archive: &Path, dir: &Path) -> Result<(), Error> {
    let mut command = if cfg!(target_os = "macos") {
        // ditto keeps the bundle's symlinks, permissions and signature.
        let mut command = Command::new("/usr/bin/ditto");
        command.arg("-x").arg("-k").arg(archive).arg(dir);
        command
    } else {
        // GNU tar on Linux; Windows' own tar.exe (bsdtar) also reads zips.
        let mut command = Command::new("tar");
        command.arg("-xf").arg(archive).arg("-C").arg(dir);
        command
    };
    run(&mut command, "unpack the update")
}

/// `name` at the top of `dir` or one folder down (the archives wrap the app
/// in a `kuery-<version>-<target>` folder).
fn find(dir: &Path, name: &str) -> Result<PathBuf, Error> {
    let direct = dir.join(name);
    if direct.exists() {
        return Ok(direct);
    }
    for entry in std::fs::read_dir(dir)? {
        let candidate = entry?.path().join(name);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(Error::Install(format!("the update has no {name}")))
}

/// Moves `new` to `target`, moving what was there to `aside` first and back
/// if the move fails. The replaced app is then removed, when the platform
/// allows it.
fn replace(target: &Path, new: &Path, aside: &Path) -> Result<(), Error> {
    remove(aside);
    std::fs::rename(target, aside).map_err(|e| writable(e, target))?;
    if let Err(error) = std::fs::rename(new, target) {
        let _ = std::fs::rename(aside, target);
        return Err(writable(error, target));
    }
    remove(aside);
    Ok(())
}

/// The new app must be validly signed, and by the same team as the running
/// one when that one is signed (a development build is not).
fn check_code_signature(new: &Path, current: &Path) -> Result<(), Error> {
    let mut verify = Command::new("/usr/bin/codesign");
    verify.args(["--verify", "--deep", "--strict"]).arg(new);
    run(&mut verify, "verify the new app's code signature")?;
    let Some(team) = team_identifier(current)? else {
        return Ok(());
    };
    match team_identifier(new)? {
        Some(new_team) if new_team == team => Ok(()),
        other => Err(Error::Install(format!(
            "the new app is signed by {}, not by {team}",
            other.as_deref().unwrap_or("no team")
        ))),
    }
}

/// The Team ID in `app`'s signature, `None` when unsigned or ad hoc.
fn team_identifier(app: &Path) -> Result<Option<String>, Error> {
    let output = Command::new("/usr/bin/codesign")
        .args(["-d", "--verbose=2"])
        .arg(app)
        .output()?;
    // codesign prints the details on stderr, and fails for unsigned code.
    if !output.status.success() {
        return Ok(None);
    }
    Ok(parse_team_identifier(&String::from_utf8_lossy(
        &output.stderr,
    )))
}

fn parse_team_identifier(details: &str) -> Option<String> {
    details
        .lines()
        .find_map(|line| line.strip_prefix("TeamIdentifier="))
        .map(str::trim)
        .filter(|team| !team.is_empty() && *team != "not set")
        .map(str::to_string)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_: &Path) -> Result<(), Error> {
    Ok(())
}

fn run(command: &mut Command, what: &str) -> Result<(), Error> {
    let output = command.output()?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(Error::Install(format!(
        "could not {what}: {}",
        stderr.trim()
    )))
}

/// A permission error says which folder Kuery could not write.
fn writable(error: std::io::Error, path: &Path) -> Error {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        let dir = path.parent().unwrap_or(path);
        Error::Install(format!("Kuery cannot write to {}", dir.display()))
    } else {
        Error::Io(error)
    }
}

/// Removes a file or folder, if there.
fn remove(path: &Path) {
    let _ = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::tests::tempdir;

    #[test]
    fn replaces_a_file_and_removes_the_old_one() {
        let dir = tempdir::Dir::new();
        let (target, new, aside) = (
            dir.path().join("kuery"),
            dir.path().join("new"),
            dir.path().join(".kuery.old"),
        );
        std::fs::write(&target, "old").unwrap();
        std::fs::write(&new, "new").unwrap();
        replace(&target, &new, &aside).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert!(!new.exists() && !aside.exists());
    }

    #[test]
    fn replaces_a_bundle_folder() {
        let dir = tempdir::Dir::new();
        let (target, new, aside) = (
            dir.path().join("Kuery.app"),
            dir.path().join("stage/Kuery.app"),
            dir.path().join(".Kuery.app.old"),
        );
        std::fs::create_dir_all(target.join("Contents")).unwrap();
        std::fs::write(target.join("Contents/version"), "old").unwrap();
        std::fs::create_dir_all(new.join("Contents")).unwrap();
        std::fs::write(new.join("Contents/version"), "new").unwrap();
        replace(&target, &new, &aside).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("Contents/version")).unwrap(),
            "new"
        );
        assert!(!aside.exists());
    }

    #[test]
    fn puts_the_old_app_back_when_the_new_one_cannot_move() {
        let dir = tempdir::Dir::new();
        let (target, aside) = (dir.path().join("kuery"), dir.path().join(".kuery.old"));
        std::fs::write(&target, "old").unwrap();
        let err = replace(&target, &dir.path().join("missing"), &aside).unwrap_err();
        assert!(matches!(err, Error::Io(_)), "{err:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
        assert!(!aside.exists());
    }

    #[test]
    fn finds_the_app_inside_the_archive_folder() {
        let dir = tempdir::Dir::new();
        std::fs::create_dir_all(dir.path().join("kuery-v0.3.0-x86_64-unknown-linux-gnu")).unwrap();
        let app = dir
            .path()
            .join("kuery-v0.3.0-x86_64-unknown-linux-gnu/kuery");
        std::fs::write(&app, "").unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();
        assert_eq!(find(dir.path(), "kuery").unwrap(), app);
        assert!(matches!(
            find(dir.path(), "Kuery.app"),
            Err(Error::Install(_))
        ));
    }

    #[test]
    fn reads_the_team_identifier() {
        let signed = "Executable=/Applications/Kuery.app/Contents/MacOS/kuery\n\
                      Authority=Developer ID Application: Someone (C6GRS8VQVL)\n\
                      TeamIdentifier=C6GRS8VQVL\n";
        assert_eq!(parse_team_identifier(signed).as_deref(), Some("C6GRS8VQVL"));
        assert_eq!(
            parse_team_identifier("Signature=adhoc\nTeamIdentifier=not set\n"),
            None
        );
        assert_eq!(parse_team_identifier("Signature=adhoc\n"), None);
    }

    /// An archive like the release's, made with the same tools: the app in a
    /// `kuery-<version>-<target>` folder next to a README.
    fn release_archive(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let source = dir.join("source");
        let folder = source.join("kuery-v0.3.0-test");
        if name.ends_with(".app") {
            std::fs::create_dir_all(folder.join(name).join("Contents")).unwrap();
            std::fs::write(folder.join(name).join("Contents/version"), contents).unwrap();
        } else {
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join(name), contents).unwrap();
        }
        std::fs::write(folder.join("README.md"), "").unwrap();
        let mut command = if cfg!(target_os = "macos") {
            let mut command = Command::new("/usr/bin/ditto");
            command
                .args(["-c", "-k", "--keepParent"])
                .arg(&folder)
                .arg(dir.join("archive.zip"));
            command
        } else {
            let mut command = Command::new("tar");
            command
                .arg("-czf")
                .arg(dir.join("archive.zip"))
                .arg("-C")
                .arg(&source)
                .arg("kuery-v0.3.0-test");
            command
        };
        run(&mut command, "pack the test archive").unwrap();
        dir.join("archive.zip")
    }

    #[test]
    fn installs_a_binary_from_an_archive() {
        let dir = tempdir::Dir::new();
        let name = if cfg!(windows) { "kuery.exe" } else { "kuery" };
        let installed = dir.path().join("bin").join(name);
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        std::fs::write(&installed, "old").unwrap();
        let archive = release_archive(dir.path(), name, "new");
        let installation = Installation::Binary {
            exe: installed.clone(),
        };
        assert_eq!(installation.install(&archive).unwrap(), installed);
        assert_eq!(std::fs::read_to_string(&installed).unwrap(), "new");
        // Nothing else is left next to it.
        let left: Vec<_> = std::fs::read_dir(installed.parent().unwrap())
            .unwrap()
            .collect();
        assert_eq!(left.len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&installed).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111);
        }
    }

    #[test]
    fn keeps_the_installed_app_when_the_archive_lacks_it() {
        let dir = tempdir::Dir::new();
        let installed = dir.path().join("bin/kuery");
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        std::fs::write(&installed, "old").unwrap();
        let archive = release_archive(dir.path(), "something-else", "new");
        let err = Installation::Binary {
            exe: installed.clone(),
        }
        .install(&archive)
        .unwrap_err();
        assert!(matches!(err, Error::Install(_)), "{err:?}");
        assert_eq!(std::fs::read_to_string(&installed).unwrap(), "old");
        assert!(!installed.parent().unwrap().join(STAGING).exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn refuses_an_unsigned_mac_app() {
        let dir = tempdir::Dir::new();
        let installed = dir.path().join("Applications/Kuery.app");
        std::fs::create_dir_all(installed.join("Contents")).unwrap();
        std::fs::write(installed.join("Contents/version"), "old").unwrap();
        let archive = release_archive(dir.path(), "Kuery.app", "new");
        let err = Installation::MacApp {
            bundle: installed.clone(),
        }
        .install(&archive)
        .unwrap_err();
        assert!(
            matches!(&err, Error::Install(m) if m.contains("code signature")),
            "{err:?}"
        );
        assert_eq!(
            std::fs::read_to_string(installed.join("Contents/version")).unwrap(),
            "old"
        );
    }

    #[test]
    fn cleans_up_what_an_update_left() {
        let dir = tempdir::Dir::new();
        let installed = dir.path().join("kuery");
        std::fs::write(&installed, "").unwrap();
        std::fs::create_dir_all(dir.path().join(STAGING)).unwrap();
        std::fs::write(dir.path().join(".kuery.old"), "").unwrap();
        Installation::Binary {
            exe: installed.clone(),
        }
        .clean_up();
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(left.len(), 1);
        assert!(installed.exists());
    }
}
