//! Install `kuery` Command (macOS): links `/usr/local/bin/kuery` to the
//! binary inside Kuery.app, so `kuery [PATH...]` works from a terminal.
//! It asks for an administrator password when the folder needs one.

use std::path::Path;

use gpui_kit::{App, actions};

actions!(kuery, [InstallShellCommand]);

/// Where the command goes; on the PATH of every macOS shell.
pub const LINK: &str = "/usr/local/bin/kuery";

pub fn init(cx: &mut App) {
    #[cfg(target_os = "macos")]
    cx.on_action(|_: &InstallShellCommand, cx| install(cx));
    #[cfg(not(target_os = "macos"))]
    let _ = cx;
}

#[derive(Debug, PartialEq, Eq)]
enum Plan {
    AlreadyInstalled,
    /// Create the link, replacing one that points elsewhere.
    Link,
}

/// Whether `link` can be made to point at `exe`. Anything there that is not
/// a link is left alone.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn plan(link: &Path, exe: &Path) -> Result<Plan, String> {
    match std::fs::symlink_metadata(link) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Plan::Link),
        Err(error) => Err(format!("{}: {error}", link.display())),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            if std::fs::read_link(link).is_ok_and(|target| target == exe) {
                Ok(Plan::AlreadyInstalled)
            } else {
                Ok(Plan::Link)
            }
        }
        Ok(_) => Err(format!(
            "{} already exists and is not a link to Kuery; remove it first.",
            link.display()
        )),
    }
}

/// Single-quotes `text` for `sh`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn shell_quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The AppleScript that makes the link as an administrator.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn admin_script(link: &Path, exe: &Path) -> String {
    let folder = link.parent().unwrap_or(Path::new("/"));
    let shell = format!(
        "mkdir -p {} && ln -sfn {} {}",
        shell_quoted(&folder.to_string_lossy()),
        shell_quoted(&exe.to_string_lossy()),
        shell_quoted(&link.to_string_lossy()),
    );
    let literal = shell.replace('\\', r"\\").replace('"', "\\\"");
    format!("do shell script \"{literal}\" with administrator privileges")
}

#[cfg(target_os = "macos")]
fn install(cx: &mut App) {
    use gpui_kit::component::notification::Notification;

    let link = Path::new(LINK);
    let exe = match std::env::current_exe().and_then(|exe| exe.canonicalize()) {
        Ok(exe) => exe,
        Err(error) => {
            notify(
                Notification::error(format!("Couldn't find Kuery's binary: {error}")),
                cx,
            );
            return;
        }
    };
    match plan(link, &exe) {
        Ok(Plan::AlreadyInstalled) => notify(
            Notification::info(format!("The kuery command is already installed in {LINK}.")),
            cx,
        ),
        Ok(Plan::Link) => {
            cx.spawn(async move |cx| {
                let made = cx
                    .background_executor()
                    .spawn(async move { make_link(Path::new(LINK), &exe) })
                    .await;
                cx.update(|cx| match made {
                    Ok(true) => notify(
                        Notification::success(format!(
                            "Installed the kuery command in {LINK}. Run kuery --help in a new terminal."
                        )),
                        cx,
                    ),
                    // The password prompt was cancelled.
                    Ok(false) => {}
                    Err(error) => notify(
                        Notification::error(format!("Couldn't install the kuery command: {error}")),
                        cx,
                    ),
                });
            })
            .detach();
        }
        Err(error) => notify(Notification::error(error), cx),
    }
}

/// Makes the link, as an administrator when its folder needs one. `Ok(false)`
/// when the password prompt was cancelled.
#[cfg(target_os = "macos")]
fn make_link(link: &Path, exe: &Path) -> Result<bool, String> {
    let direct = (|| {
        if link.is_symlink() {
            std::fs::remove_file(link)?;
        }
        std::os::unix::fs::symlink(exe, link)
    })();
    match direct {
        Ok(()) => Ok(true),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
            ) =>
        {
            let output = std::process::Command::new("/usr/bin/osascript")
                .arg("-e")
                .arg(admin_script(link, exe))
                .output()
                .map_err(|error| error.to_string())?;
            let stderr = String::from_utf8_lossy(&output.stderr);
            if output.status.success() {
                Ok(true)
            } else if stderr.contains("(-128)") {
                Ok(false)
            } else {
                Err(stderr.trim().to_string())
            }
        }
        Err(error) => Err(error.to_string()),
    }
}

/// Shows `note` in the active window (or any open one).
#[cfg(target_os = "macos")]
fn notify(note: gpui_kit::component::notification::Notification, cx: &mut App) {
    use gpui_kit::component::WindowExt as _;

    let Some(window) = cx.active_window().or_else(|| cx.windows().first().copied()) else {
        return;
    };
    let _ = window.update(cx, |_, window, cx| window.push_notification(note, cx));
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    struct Dir(PathBuf);

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn dir() -> Dir {
        let path = std::env::temp_dir().join(format!("kuery-shell-{}", crate::project::new_id()));
        std::fs::create_dir_all(&path).unwrap();
        Dir(path)
    }

    #[test]
    fn a_missing_command_is_linked() {
        let dir = dir();
        assert_eq!(
            plan(
                &dir.0.join("kuery"),
                Path::new("/Applications/Kuery.app/Contents/MacOS/kuery")
            ),
            Ok(Plan::Link)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_this_kuery_is_already_installed_and_one_elsewhere_is_replaced() {
        let dir = dir();
        let link = dir.0.join("kuery");
        let exe = Path::new("/Applications/Kuery.app/Contents/MacOS/kuery");
        std::os::unix::fs::symlink(exe, &link).unwrap();
        assert_eq!(plan(&link, exe), Ok(Plan::AlreadyInstalled));
        assert_eq!(
            plan(&link, Path::new("/Users/me/Kuery.app/Contents/MacOS/kuery")),
            Ok(Plan::Link)
        );
    }

    #[test]
    fn a_file_that_is_not_a_link_is_left_alone() {
        let dir = dir();
        let link = dir.0.join("kuery");
        std::fs::write(&link, "#!/bin/sh\n").unwrap();
        assert_eq!(
            plan(
                &link,
                Path::new("/Applications/Kuery.app/Contents/MacOS/kuery")
            ),
            Err(format!(
                "{} already exists and is not a link to Kuery; remove it first.",
                link.display()
            ))
        );
    }

    #[test]
    fn quoting_survives_quotes_and_spaces() {
        assert_eq!(shell_quoted("/a b/c"), "'/a b/c'");
        assert_eq!(shell_quoted("it's"), r"'it'\''s'");
        assert_eq!(
            admin_script(
                Path::new("/usr/local/bin/kuery"),
                Path::new(r#"/Apps/My "K".app/kuery"#)
            ),
            r#"do shell script "mkdir -p '/usr/local/bin' && ln -sfn '/Apps/My \"K\".app/kuery' '/usr/local/bin/kuery'" with administrator privileges"#
        );
    }
}
