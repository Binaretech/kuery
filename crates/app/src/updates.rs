//! Keeps Kuery up to date: looks for a newer release shortly after startup
//! and every few hours (unless `[updates] check_automatically = false`),
//! downloads it in the background, and swaps it in when the user restarts.
//! Checking, verifying and installing live in `kuery-update`; this is the
//! app's side: the schedule, the notices and the restart.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::{
    AnyElement, App, AsyncApp, Global, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, actions, div,
};
use kuery_update::{Installation, Update, Updater};

use crate::settings;
use crate::theme::{self, color};

actions!(
    kuery,
    [
        CheckForUpdates,
        RestartToUpdate,
        ToggleAutomaticUpdateChecks
    ]
);

/// The key the release workflow signs updates with.
const PUBLIC_KEY: &str = include_str!("../assets/update.pub");
/// Long enough after startup not to slow it down.
const FIRST_CHECK: Duration = Duration::from_secs(10);
const EVERY: Duration = Duration::from_secs(6 * 60 * 60);

enum Status {
    Idle,
    Checking,
    Downloading {
        version: String,
        percent: Option<u8>,
    },
    Ready {
        update: Update,
        archive: PathBuf,
    },
    Installing,
}

struct Updates {
    status: Status,
    /// A restart was asked for and is waiting on the "Restart Kuery?"
    /// prompt or the install, so another one must not start.
    restart_pending: bool,
    /// Why updates are unavailable, when they are (a development build, an
    /// app not installed from a release).
    updater: Result<(Arc<Updater>, Installation), String>,
}

impl Global for Updates {}

/// Whether a restart into the downloaded update can start.
enum Restart {
    /// The archive to install, and where.
    Begin(PathBuf, Installation),
    /// One is already under way.
    Busy,
    NotReady,
}

impl Updates {
    /// Starts a restart unless one is under way; [`Self::end_restart`]
    /// allows the next one when it is cancelled or fails.
    fn begin_restart(&mut self) -> Restart {
        if self.restart_pending || matches!(self.status, Status::Installing) {
            return Restart::Busy;
        }
        let (Status::Ready { archive, .. }, Ok((_, installation))) = (&self.status, &self.updater)
        else {
            return Restart::NotReady;
        };
        let restart = Restart::Begin(archive.clone(), installation.clone());
        self.restart_pending = true;
        restart
    }

    fn end_restart(&mut self) {
        self.restart_pending = false;
    }
}

/// Registers the update actions and, in a release build, starts checking.
pub fn init(cx: &mut App) {
    let updater = updater();
    if let Ok((_, installation)) = &updater {
        let installation = installation.clone();
        cx.background_executor()
            .spawn(async move { installation.clean_up() })
            .detach();
    }
    let available = updater.is_ok();
    cx.set_global(Updates {
        status: Status::Idle,
        restart_pending: false,
        updater,
    });
    cx.on_action(|_: &CheckForUpdates, cx| check(true, cx));
    cx.on_action(|_: &RestartToUpdate, cx| restart_to_update(cx));
    cx.on_action(|_: &ToggleAutomaticUpdateChecks, cx| toggle_automatic_checks(cx));
    if available {
        cx.spawn(async move |cx| {
            cx.background_executor().timer(FIRST_CHECK).await;
            loop {
                if settings::check_updates_automatically() {
                    cx.update(|cx| check(false, cx));
                }
                cx.background_executor().timer(EVERY).await;
            }
        })
        .detach();
    }
}

fn updater() -> Result<(Arc<Updater>, Installation), String> {
    if cfg!(debug_assertions) {
        return Err("Updates are off in development builds.".into());
    }
    let platform =
        kuery_update::platform().ok_or("Updates are not published for this platform.")?;
    let installation =
        Installation::current().map_err(|error| format!("Kuery cannot update itself: {error}"))?;
    let updater = Updater::new(
        kuery_update::MANIFEST_URL,
        PUBLIC_KEY,
        env!("CARGO_PKG_VERSION"),
        platform,
    )
    .map_err(|error| error.to_string())?;
    Ok((Arc::new(updater), installation))
}

/// Looks for a newer release and downloads it. `manual` checks (the
/// command) say what they found; automatic ones stay quiet unless an update
/// is ready.
fn check(manual: bool, cx: &mut App) {
    let updates = cx.global::<Updates>();
    let updater = match &updates.updater {
        Ok((updater, _)) => updater.clone(),
        Err(why) => {
            if manual {
                notify(Notification::info(why.clone()), cx);
            }
            return;
        }
    };
    match &updates.status {
        Status::Idle => {}
        Status::Ready { update, .. } => {
            if manual {
                notify_ready(&update.version.to_string(), cx);
            }
            return;
        }
        Status::Checking | Status::Downloading { .. } | Status::Installing => {
            if manual {
                notify(
                    Notification::info("Kuery is already updating; this takes a moment."),
                    cx,
                );
            }
            return;
        }
    }
    set_status(Status::Checking, cx);
    cx.spawn(async move |cx| {
        let found = {
            let updater = updater.clone();
            cx.background_executor()
                .spawn(async move { updater.check() })
                .await
        };
        match found {
            Ok(Some(update)) => download(updater, update, manual, cx).await,
            Ok(None) => cx.update(|cx| {
                set_status(Status::Idle, cx);
                if manual {
                    notify(
                        Notification::success(format!(
                            "Kuery {} is the latest version.",
                            env!("CARGO_PKG_VERSION")
                        )),
                        cx,
                    );
                }
            }),
            Err(error) => cx.update(|cx| {
                set_status(Status::Idle, cx);
                report(manual, format!("Couldn't check for updates: {error}"), cx);
            }),
        }
    })
    .detach();
}

/// Downloads and verifies `update` into the cache, showing progress in the
/// status bar, then offers the restart.
async fn download(updater: Arc<Updater>, update: Update, manual: bool, cx: &mut AsyncApp) {
    let version = update.version.to_string();
    cx.update(|cx| {
        set_status(
            Status::Downloading {
                version: version.clone(),
                percent: None,
            },
            cx,
        )
    });
    let Some(dir) = crate::paths::cache_dir().map(|cache| cache.join("updates")) else {
        cx.update(|cx| {
            set_status(Status::Idle, cx);
            report(
                manual,
                format!(
                    "Couldn't download Kuery {version}: no folder to save it in (HOME is not set)."
                ),
                cx,
            );
        });
        return;
    };
    let (progress, received) = async_channel::unbounded();
    let task = cx.background_executor().spawn(async move {
        // Only the latest download is kept.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let mut last = None;
        let archive = updater.download(&update, &dir, |download| {
            let now = percent(download.received, download.total);
            if now != last {
                last = now;
                let _ = progress.try_send(now);
            }
        })?;
        Ok::<_, kuery_update::Error>((update, archive))
    });
    while let Ok(percent) = received.recv().await {
        let version = version.clone();
        cx.update(|cx| set_status(Status::Downloading { version, percent }, cx));
    }
    match task.await {
        Ok((update, archive)) => cx.update(|cx| {
            set_status(Status::Ready { update, archive }, cx);
            notify_ready(&version, cx);
        }),
        Err(error) => cx.update(|cx| {
            set_status(Status::Idle, cx);
            report(
                manual,
                format!("Couldn't download Kuery {version}: {error}"),
                cx,
            );
        }),
    }
}

/// Installs the downloaded update and restarts, once no window has work that
/// would be lost (or the user agrees to lose it).
fn restart_to_update(cx: &mut App) {
    let (archive, installation) = match cx.global_mut::<Updates>().begin_restart() {
        Restart::Begin(archive, installation) => (archive, installation),
        // The prompt or the install is already showing.
        Restart::Busy => return,
        Restart::NotReady => {
            notify(
                Notification::info("No update is ready yet. Check for Updates… looks for one."),
                cx,
            );
            return;
        }
    };
    let cancelled = |cx: &mut App| cx.global_mut::<Updates>().end_restart();
    crate::app::confirm_leave("Restart", cx, cancelled, move |cx| {
        let ready = std::mem::replace(&mut cx.global_mut::<Updates>().status, Status::Installing);
        cx.refresh_windows();
        cx.spawn(async move |cx| {
            let installed = cx
                .background_executor()
                .spawn(async move { installation.install(&archive) })
                .await;
            cx.update(|cx| match installed {
                Ok(path) => {
                    cx.set_restart_path(path);
                    cx.restart();
                }
                Err(error) => {
                    cx.global_mut::<Updates>().end_restart();
                    set_status(ready, cx);
                    notify(
                        Notification::error(format!(
                            "Couldn't install the update: {error}. You can download it from \
                             the releases page instead."
                        ))
                        .autohide(false),
                        cx,
                    );
                }
            });
        })
        .detach();
    });
}

fn toggle_automatic_checks(cx: &mut App) {
    let on = !settings::check_updates_automatically();
    match settings::set_check_updates_automatically(on) {
        Ok(()) if on => {
            notify(
                Notification::info("Kuery will check for updates automatically."),
                cx,
            );
            check(false, cx);
        }
        Ok(()) => notify(
            Notification::info(
                "Automatic update checks are off. Check for Updates… still looks for one.",
            ),
            cx,
        ),
        Err(error) => notify(
            Notification::error(format!("Couldn't save the setting: {error}")),
            cx,
        ),
    }
}

/// What the status bar shows while updating, or when an update is ready.
pub fn status_item(cx: &App) -> Option<AnyElement> {
    let status = &cx.try_global::<Updates>()?.status;
    let text = status_text(status)?;
    Some(match status {
        Status::Ready { .. } => div()
            .id("restart-to-update")
            .cursor_pointer()
            .text_color(color(theme::ACCENT))
            .child(text)
            .on_click(|_, _, cx| restart_to_update(cx))
            .into_any_element(),
        _ => div().child(text).into_any_element(),
    })
}

fn status_text(status: &Status) -> Option<String> {
    match status {
        Status::Idle | Status::Checking => None,
        Status::Downloading {
            version,
            percent: Some(percent),
        } => Some(format!("Downloading Kuery {version}… {percent}%")),
        Status::Downloading { version, .. } => Some(format!("Downloading Kuery {version}…")),
        Status::Ready { update, .. } => Some(format!("Restart to update to {}", update.version)),
        Status::Installing => Some("Installing the update…".into()),
    }
}

/// How much of a download has arrived, when its size is known.
fn percent(received: u64, total: Option<u64>) -> Option<u8> {
    let total = total.filter(|total| *total > 0)?;
    Some((received.min(total) * 100 / total) as u8)
}

fn set_status(status: Status, cx: &mut App) {
    cx.global_mut::<Updates>().status = status;
    cx.refresh_windows();
}

fn notify_ready(version: &str, cx: &mut App) {
    notify(
        Notification::new()
            .id::<Updates>()
            .title(format!("Kuery {version} is ready"))
            .message("Restart Kuery to finish updating.")
            .action(|_, _, _| {
                Button::new("restart-to-update-now")
                    .primary()
                    .label("Restart")
                    .on_click(|_, _, cx| restart_to_update(cx))
            }),
        cx,
    );
}

/// A failure the user asked about is shown; one from a background check only
/// goes to the log, since nothing is wrong with what they are doing.
fn report(manual: bool, message: String, cx: &mut App) {
    if manual {
        notify(Notification::error(message), cx);
    } else {
        eprintln!("kuery: {message}");
    }
}

/// Shows `note` in the active window (or any open one).
fn notify(note: Notification, cx: &mut App) {
    let Some(window) = cx.active_window().or_else(|| cx.windows().first().copied()) else {
        return;
    };
    let _ = window.update(cx, |_, window, cx| window.push_notification(note, cx));
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn percent_of_a_download() {
        assert_eq!(percent(0, Some(200)), Some(0));
        assert_eq!(percent(50, Some(200)), Some(25));
        assert_eq!(percent(200, Some(200)), Some(100));
        assert_eq!(percent(300, Some(200)), Some(100));
        assert_eq!(percent(50, None), None);
        assert_eq!(percent(50, Some(0)), None);
    }

    #[test]
    fn the_status_bar_shows_downloads_and_ready_updates() {
        let downloading = |percent| Status::Downloading {
            version: "0.3.0".into(),
            percent,
        };
        assert_eq!(status_text(&Status::Idle), None);
        assert_eq!(status_text(&Status::Checking), None);
        assert_eq!(
            status_text(&downloading(Some(42))).as_deref(),
            Some("Downloading Kuery 0.3.0… 42%")
        );
        assert_eq!(
            status_text(&downloading(None)).as_deref(),
            Some("Downloading Kuery 0.3.0…")
        );
        assert_eq!(
            status_text(&ready()).as_deref(),
            Some("Restart to update to 0.3.0")
        );
        assert_eq!(
            status_text(&Status::Installing).as_deref(),
            Some("Installing the update…")
        );
    }

    fn ready() -> Status {
        Status::Ready {
            update: Update {
                version: "0.3.0".parse().unwrap(),
                notes_url: String::new(),
                asset: kuery_update::Asset {
                    url: String::new(),
                    sha256: String::new(),
                    signature: String::new(),
                },
                file_name: "kuery.zip".into(),
            },
            archive: PathBuf::from("/cache/updates/kuery.zip"),
        }
    }

    fn updates(status: Status) -> Updates {
        let updater = Updater::new(
            kuery_update::MANIFEST_URL,
            PUBLIC_KEY,
            "0.2.1",
            "linux-x86_64",
        )
        .unwrap();
        let installation = Installation::Binary {
            exe: "/opt/kuery/kuery".into(),
        };
        Updates {
            status,
            restart_pending: false,
            updater: Ok((Arc::new(updater), installation)),
        }
    }

    #[test]
    fn a_restart_begins_once_until_it_ends() {
        // Restart clicked twice before the "Restart Kuery?" prompt is
        // answered must not install twice.
        let mut updates = updates(ready());
        assert!(matches!(
            updates.begin_restart(),
            Restart::Begin(archive, _) if archive == Path::new("/cache/updates/kuery.zip")
        ));
        assert!(matches!(updates.begin_restart(), Restart::Busy));
        // Cancelled, or the install failed: Restart works again.
        updates.end_restart();
        assert!(matches!(updates.begin_restart(), Restart::Begin(..)));
    }

    #[test]
    fn no_restart_without_a_ready_update() {
        assert!(matches!(
            updates(Status::Idle).begin_restart(),
            Restart::NotReady
        ));
        assert!(matches!(
            updates(Status::Installing).begin_restart(),
            Restart::Busy
        ));
        let mut unavailable = updates(ready());
        unavailable.updater = Err("Updates are off in development builds.".into());
        assert!(matches!(unavailable.begin_restart(), Restart::NotReady));
    }

    #[test]
    fn the_public_key_is_a_minisign_key() {
        assert!(PUBLIC_KEY.starts_with("untrusted comment: minisign public key"));
        assert_eq!(PUBLIC_KEY.lines().count(), 2);
    }
}
