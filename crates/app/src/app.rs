//! App-wide state and windows: one window per project, a welcome window
//! when no project is open, and one for data files opened without a project.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, Global, IntoElement,
    PathPromptOptions, PromptLevel, Render, Window, WindowBounds, WindowOptions, px, size,
};

use crate::cli::Target;
use crate::commands::{ClearRecentProjects, OpenProject, OpenRecentProject};
use crate::data_file_tabs::DataFileTabs as _;
use crate::file_window::FileWindow;
use crate::project::{Project, ProjectModel};
use crate::secrets::SecretStore;
use crate::session::{self, Contents, Placement, Session, WindowState};
use crate::welcome::Welcome;
use crate::workspace::WorkspaceView;

pub struct AppState {
    pub redis: Arc<kuery_redis::Engine>,
    pub secrets: Arc<dyn SecretStore>,
    /// Open project windows, so opening a project twice focuses it.
    windows: Vec<ProjectWindowEntry>,
    welcome: Option<(AnyWindowHandle, Entity<Welcome>)>,
    /// The window for data files opened with no project window open.
    files: Option<(AnyWindowHandle, Entity<FileWindow>)>,
}

struct ProjectWindowEntry {
    folder: PathBuf,
    handle: AnyWindowHandle,
    view: Entity<ProjectWindow>,
}

impl Global for AppState {}

/// Root view of a project window. It holds the workspace instead of being
/// it, so another project can take over the same window.
pub struct ProjectWindow {
    workspace: Entity<WorkspaceView>,
}

impl ProjectWindow {
    fn new(project: Project, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            workspace: Self::workspace(project, window, cx),
        }
    }

    fn workspace(
        project: Project,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<WorkspaceView> {
        window.set_window_title(&project.name);
        let model = cx.new(|_| ProjectModel { project });
        cx.new(|cx| WorkspaceView::new(model, window, cx))
    }

    /// Swaps in `project`. The previous workspace, its tabs and its database
    /// sessions are dropped.
    fn replace(&mut self, project: Project, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace = Self::workspace(project, window, cx);
        cx.notify();
    }
}

impl Render for ProjectWindow {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.workspace.clone()
    }
}

pub fn init(redis: Arc<kuery_redis::Engine>, secrets: Arc<dyn SecretStore>, cx: &mut App) {
    cx.set_global(AppState {
        redis,
        secrets,
        windows: Vec::new(),
        welcome: None,
        files: None,
    });
    cx.on_action(|_: &OpenProject, cx| request_open(None, cx.active_window(), cx));
    cx.on_action(|action: &OpenRecentProject, cx| {
        request_open(Some(action.path.clone()), cx.active_window(), cx)
    });
    cx.on_action(|_: &ClearRecentProjects, cx| {
        crate::recent::clear();
        recents_changed(cx);
    });
}

impl AppState {
    /// The drivers' runtimes, for [`crate::drivers::Driver`].
    pub fn engines(&self) -> crate::drivers::Engines {
        crate::drivers::Engines {
            redis: self.redis.clone(),
        }
    }
}

pub fn state(cx: &App) -> &AppState {
    cx.global::<AppState>()
}

/// Opens a project the way the user asked for it: `folder`, or one picked in
/// a folder prompt when `None`. From a project window (`origin`) it asks
/// whether the project replaces that window or gets a new one.
///
/// Deferred, because callers are often inside an update of `origin` itself
/// (a palette pick, a click in the welcome window), where the window cannot
/// be updated again to show a prompt.
pub fn request_open(folder: Option<PathBuf>, origin: Option<AnyWindowHandle>, cx: &mut App) {
    cx.defer(move |cx| open_requested(folder, origin, cx));
}

fn open_requested(folder: Option<PathBuf>, origin: Option<AnyWindowHandle>, cx: &mut App) {
    let Some(folder) = folder else {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open".into()),
        });
        cx.spawn(async move |cx| {
            if let Ok(Ok(Some(paths))) = paths.await
                && let Some(path) = paths.into_iter().next()
            {
                cx.update(|cx| open_requested(Some(path), origin, cx));
            }
        })
        .detach();
        return;
    };

    let folder = canonical(&folder);
    if focus_existing(&folder, cx) {
        return;
    }
    let replaceable = origin.and_then(|origin| {
        state(cx)
            .windows
            .iter()
            .find(|entry| entry.handle == origin)
            .map(|entry| (entry.handle, entry.view.clone()))
    });
    let Some((handle, view)) = replaceable else {
        if let Err(error) = open_project(&folder, cx) {
            report_error(origin, &error, cx);
        }
        return;
    };

    let name = folder
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| folder.display().to_string());
    let answer = handle.update(cx, |_, window, cx| {
        window.prompt(
            PromptLevel::Info,
            &format!("Open \u{201c}{name}\u{201d} in a new window or in this one?"),
            None,
            &["New Window", "This Window", "Cancel"],
            cx,
        )
    });
    let Ok(answer) = answer else { return };
    cx.spawn(async move |cx| {
        match answer.await {
            Ok(0) => cx.update(|cx| {
                if let Err(error) = open_project(&folder, cx) {
                    report_error(Some(handle), &error, cx);
                }
            }),
            Ok(1) => {
                // Replacing the window drops its tabs: confirm first when
                // some of them have changes that would be lost.
                let unsaved =
                    cx.update(|cx| view.read(cx).workspace.read(cx).has_unsaved_changes(cx));
                if unsaved {
                    let confirm = cx.update(|cx| {
                        handle.update(cx, |_, window, cx| {
                            window.prompt(
                                PromptLevel::Warning,
                                "Discard unsaved changes?",
                                Some(
                                    "Some tabs in this window have unsaved changes. \
                                     They will be lost if the project opens here.",
                                ),
                                &["Discard and Open", "Cancel"],
                                cx,
                            )
                        })
                    });
                    let Ok(confirm) = confirm else { return };
                    if !matches!(confirm.await, Ok(0)) {
                        return;
                    }
                }
                cx.update(|cx| {
                    if let Err(error) = replace_project(handle, &view, &folder, cx) {
                        report_error(Some(handle), &error, cx);
                    }
                });
            }
            _ => {}
        }
    })
    .detach();
}

/// Opens the project in `folder` in its own window, or focuses the window if
/// it is already open.
pub fn open_project(folder: &Path, cx: &mut App) -> Result<(), String> {
    open_project_at(folder, None, cx).map(|_| ())
}

/// [`open_project`], with the window where `placement` says. Returns the
/// new window, or `None` if the project's window was already open.
fn open_project_at(
    folder: &Path,
    placement: Option<&Placement>,
    cx: &mut App,
) -> Result<Option<(AnyWindowHandle, Entity<ProjectWindow>)>, String> {
    let folder = canonical(folder);
    if focus_existing(&folder, cx) {
        return Ok(None);
    }

    let project = Project::open_or_init(&folder)?;
    let options = window_options(size(px(1440.), px(900.)), placement, cx);
    let (handle, view) = gpui_kit::open_window(options, cx, move |window, cx| {
        cx.new(|cx| ProjectWindow::new(project, window, cx))
    })
    .map_err(|error| format!("Could not open a window: {error}"))?;

    let state = cx.global_mut::<AppState>();
    state.windows.retain(|entry| entry.folder != folder);
    state.windows.push(ProjectWindowEntry {
        folder: folder.clone(),
        handle,
        view: view.clone(),
    });
    crate::recent::add(&folder);
    recents_changed(cx);
    close_welcome(cx);
    Ok(Some((handle, view)))
}

/// A new window's options: centered at `default` size, or where `placement`
/// says.
fn window_options(
    default: gpui_kit::Size<gpui_kit::Pixels>,
    placement: Option<&Placement>,
    cx: &App,
) -> WindowOptions {
    let (bounds, display_id) = match placement {
        Some(placement) => placement.window_bounds(cx),
        None => (
            WindowBounds::Windowed(Bounds::centered(None, default, cx)),
            None,
        ),
    };
    WindowOptions {
        window_bounds: Some(bounds),
        display_id,
        ..crate::app_icon::window_options()
    }
}

fn replace_project(
    handle: AnyWindowHandle,
    view: &Entity<ProjectWindow>,
    folder: &Path,
    cx: &mut App,
) -> Result<(), String> {
    let project = Project::open_or_init(folder)?;
    handle
        .update(cx, |_, window, cx| {
            view.update(cx, |view, cx| view.replace(project, window, cx))
        })
        .map_err(|_| "The window was closed".to_string())?;
    if let Some(entry) = cx
        .global_mut::<AppState>()
        .windows
        .iter_mut()
        .find(|entry| entry.handle == handle)
    {
        entry.folder = folder.to_path_buf();
    }
    crate::recent::add(folder);
    recents_changed(cx);
    Ok(())
}

/// What a `kuery` command opened.
pub struct Opened {
    pub any: bool,
    /// Why the paths that did not open failed, for [`report_open_errors`].
    pub errors: Vec<String>,
}

/// Opens what `kuery [PATH...]` names: each folder as a project, then each
/// data file where [`destination`] says.
pub fn open_targets(targets: &[Target], cx: &mut App) -> Opened {
    let mut any = false;
    let mut errors = Vec::new();
    let mut opened = None;
    for target in targets {
        if let Target::Project(folder) = target {
            match open_project(folder, cx) {
                Ok(()) => {
                    any = true;
                    let folder = canonical(folder);
                    opened = state(cx)
                        .windows
                        .iter()
                        .find(|entry| entry.folder == folder)
                        .map(|entry| entry.handle);
                }
                Err(error) => errors.push(error),
            }
        }
    }
    for target in targets {
        if let Target::DataFile(path) = target {
            match open_data_file(path.clone(), opened, cx) {
                Ok(()) => any = true,
                Err(error) => errors.push(format!("{}: {error}", path.display())),
            }
        }
    }
    Opened { any, errors }
}

fn open_data_file(
    path: PathBuf,
    opened: Option<AnyWindowHandle>,
    cx: &mut App,
) -> Result<(), String> {
    let state = state(cx);
    let projects: Vec<_> = state.windows.iter().map(|entry| entry.handle).collect();
    let files = state.files.as_ref().map(|(handle, _)| *handle);
    match destination(opened, cx.active_window(), files, &projects) {
        Destination::Project(handle) => {
            let view = state
                .windows
                .iter()
                .find(|entry| entry.handle == handle)
                .map(|entry| entry.view.clone())
                .ok_or("The project window was closed")?;
            handle
                .update(cx, |_, window, cx| {
                    window.activate_window();
                    let workspace = view.read(cx).workspace.clone();
                    workspace.update(cx, |workspace, cx| {
                        workspace.open_data_path(path, window, cx)
                    });
                })
                .map_err(|_| "The project window was closed".to_string())
        }
        Destination::Files => {
            let (handle, view) = files_window(cx)?;
            handle
                .update(cx, |_, window, cx| {
                    window.activate_window();
                    view.update(cx, |files, cx| files.open_data_path(path, window, cx));
                })
                .map_err(|_| "The data files window was closed".to_string())
        }
    }
}

/// The window for data files without a project, opened if needed.
fn files_window(cx: &mut App) -> Result<(AnyWindowHandle, Entity<FileWindow>), String> {
    files_window_at(None, cx)
}

fn files_window_at(
    placement: Option<&Placement>,
    cx: &mut App,
) -> Result<(AnyWindowHandle, Entity<FileWindow>), String> {
    if let Some((handle, view)) = state(cx).files.clone()
        && handle.update(cx, |_, _, _| ()).is_ok()
    {
        return Ok((handle, view));
    }
    let options = window_options(size(px(1200.), px(800.)), placement, cx);
    let opened = gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| FileWindow::new(window, cx))
    })
    .map_err(|error| format!("Could not open a window: {error}"))?;
    cx.global_mut::<AppState>().files = Some(opened.clone());
    close_welcome(cx);
    Ok(opened)
}

/// Brings the window of the project in `folder` to the front, if there is one.
fn focus_existing(folder: &Path, cx: &mut App) -> bool {
    let existing = state(cx)
        .windows
        .iter()
        .find(|entry| entry.folder == folder)
        .map(|entry| entry.handle);
    existing.is_some_and(|handle| {
        handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    })
}

fn canonical(folder: &Path) -> PathBuf {
    folder
        .canonicalize()
        .unwrap_or_else(|_| folder.to_path_buf())
}

fn report_error(origin: Option<AnyWindowHandle>, error: &str, cx: &mut App) {
    let shown = origin.is_some_and(|handle| {
        handle
            .update(cx, |_, window, cx| {
                // Nothing to wait for: the prompt only has an OK button.
                drop(window.prompt(
                    PromptLevel::Critical,
                    "Could not open the project",
                    Some(error),
                    &["OK"],
                    cx,
                ));
            })
            .is_ok()
    });
    if !shown {
        eprintln!("kuery: {error}");
    }
}

/// Quits, asking first if a project window has work that would be lost:
/// unsaved SQL, an open transaction (rolled back on quit) or staged row
/// changes.
pub fn request_quit(cx: &mut App) {
    confirm_leave(Leave::Quit, cx, |_| {}, |cx| cx.quit());
}

/// How Kuery goes away, which decides what is lost.
#[derive(Clone, Copy)]
pub enum Leave {
    Quit,
    /// To update: the windows and their SQL come back (see
    /// [`save_session`]), open transactions and staged row changes do not.
    Restart,
}

impl Leave {
    fn verb(self) -> &'static str {
        match self {
            Leave::Quit => "Quit",
            Leave::Restart => "Restart",
        }
    }

    fn loses_work(self, workspace: &WorkspaceView, cx: &App) -> bool {
        match self {
            Leave::Quit => workspace.has_unsaved_changes(cx),
            Leave::Restart => workspace.loses_work_on_restart(cx),
        }
    }

    fn lost(self) -> &'static str {
        match self {
            Leave::Quit => "unsaved SQL, an open transaction or staged row changes",
            Leave::Restart => "an open transaction or staged row changes",
        }
    }
}

/// Runs `then` (which quits or restarts) once nothing would be lost, or the
/// user agrees to lose it. `cancelled` runs instead when the user keeps
/// working.
///
/// Deferred: the action asking for it arrives while a window is handling the
/// event, and that window could not show the question then.
pub fn confirm_leave(
    leave: Leave,
    cx: &mut App,
    cancelled: impl FnOnce(&mut App) + 'static,
    then: impl FnOnce(&mut App) + 'static,
) {
    cx.defer(move |cx| {
        let open = cx.windows();
        let unsaved: Vec<AnyWindowHandle> = state(cx)
            .windows
            .iter()
            .filter(|entry| open.contains(&entry.handle))
            .filter(|entry| leave.loses_work(entry.view.read(cx).workspace.read(cx), cx))
            .map(|entry| entry.handle)
            .collect();
        let Some(&first) = unsaved.first() else {
            then(cx);
            return;
        };
        let handle = cx
            .active_window()
            .filter(|active| unsaved.contains(active))
            .unwrap_or(first);
        let detail = format!(
            "{} {}. They will be lost, and open transactions rolled back.",
            if unsaved.len() == 1 {
                "A window has".to_string()
            } else {
                format!("{} windows have", unsaved.len())
            },
            leave.lost()
        );
        let verb = leave.verb();
        let answer = handle.update(cx, |_, window, cx| {
            window.activate_window();
            window.prompt(
                PromptLevel::Warning,
                &format!("{verb} Kuery?"),
                Some(&detail),
                &[verb, "Cancel"],
                cx,
            )
        });
        let Ok(answer) = answer else {
            then(cx);
            return;
        };
        cx.spawn(async move |cx| {
            if let Ok(0) = answer.await {
                cx.update(then);
            } else {
                cx.update(cancelled);
            }
        })
        .detach();
    });
}

/// Saves the open windows and their tabs, for the next start to reopen:
/// called just before restarting to update.
pub fn save_session(cx: &mut App) {
    // Back to front, so the window in front opens last and stays in front.
    let mut order = cx.windows();
    match cx.window_stack() {
        Some(stack) => order.sort_by_key(|handle| {
            std::cmp::Reverse(stack.iter().position(|h| h == handle).unwrap_or(usize::MAX))
        }),
        None => {
            if let Some(active) = cx.active_window() {
                order.retain(|handle| *handle != active);
                order.push(active);
            }
        }
    }
    let state = state(cx);
    let projects: Vec<_> = state
        .windows
        .iter()
        .map(|entry| (entry.handle, entry.folder.clone(), entry.view.clone()))
        .collect();
    let files = state.files.clone();
    let mut windows = Vec::new();
    for handle in order {
        let saved = if let Some((_, folder, view)) = projects.iter().find(|(h, ..)| *h == handle) {
            handle.update(cx, |_, window, cx| WindowState {
                placement: Some(Placement::of(window, cx)),
                contents: Contents::Project {
                    folder: folder.clone(),
                    workspace: view.read(cx).workspace.read(cx).session(cx),
                },
            })
        } else if let Some((_, view)) = files.as_ref().filter(|(h, _)| *h == handle) {
            handle.update(cx, |_, window, cx| {
                let (files, active) = view.read(cx).session(cx);
                WindowState {
                    placement: Some(Placement::of(window, cx)),
                    contents: Contents::Files { files, active },
                }
            })
        } else {
            // The welcome window opens by itself when nothing else does.
            continue;
        };
        windows.extend(saved.ok());
    }
    if let Err(error) = session::save(&Session { windows }) {
        eprintln!("kuery: could not save the open windows for the restart: {error}");
    }
}

/// Reopens the windows [`save_session`] saved, where they were. Returns
/// whether any opened.
pub fn restore_session(session: Session, cx: &mut App) -> bool {
    let mut any = false;
    for saved in session.windows {
        let placement = saved.placement.as_ref();
        match saved.contents {
            Contents::Project { folder, workspace } => {
                // Gone since, or no longer a project: not created again.
                if !Project::is_project(&folder) {
                    continue;
                }
                match open_project_at(&folder, placement, cx) {
                    Ok(Some((handle, view))) => {
                        any = true;
                        let _ = handle.update(cx, |_, window, cx| {
                            let workspace_view = view.read(cx).workspace.clone();
                            workspace_view.update(cx, |workspace_view, cx| {
                                workspace_view.restore(workspace, window, cx)
                            });
                        });
                    }
                    Ok(None) => any = true,
                    Err(error) => eprintln!("kuery: {error}"),
                }
            }
            Contents::Files { files, active } => match files_window_at(placement, cx) {
                Ok((handle, view)) => {
                    any = true;
                    let _ = handle.update(cx, |_, window, cx| {
                        view.update(cx, |files_view, cx| {
                            files_view.restore(files, active, window, cx)
                        });
                    });
                }
                Err(error) => eprintln!("kuery: {error}"),
            },
        }
    }
    any
}

/// Refreshes what shows the recent projects: the menu bar and the welcome
/// window (deferred, since the change may come from the welcome window).
pub fn recents_changed(cx: &mut App) {
    crate::menus::set_menus(cx);
    if let Some((_, welcome)) = state(cx).welcome.clone() {
        cx.defer(move |cx| welcome.update(cx, |welcome, cx| welcome.reload(cx)));
    }
}

pub fn open_welcome(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(760.), px(520.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        ..crate::app_icon::window_options()
    };
    if let Ok((window, view)) = gpui_kit::open_window(options, cx, |window, cx| {
        window.set_window_title("Kuery");
        cx.new(|cx| Welcome::new(window, cx))
    }) {
        cx.global_mut::<AppState>().welcome = Some((window, view));
    }
}

fn close_welcome(cx: &mut App) {
    if let Some((window, _)) = cx.global_mut::<AppState>().welcome.take() {
        window
            .update(cx, |_, window, _| window.remove_window())
            .ok();
    }
}

/// The prompt for paths a `kuery` command named that did not open.
fn open_errors_prompt(errors: &[String]) -> Option<(String, String)> {
    let title = match errors.len() {
        0 => return None,
        1 => "Could not open a path".to_string(),
        count => format!("Could not open {count} paths"),
    };
    Some((title, errors.join("\n")))
}

/// Shows why paths a `kuery` command named did not open, in the active
/// window (the command has returned, and a detached or running Kuery has no
/// terminal), and on stderr.
pub fn report_open_errors(errors: &[String], cx: &mut App) {
    for error in errors {
        eprintln!("kuery: {error}");
    }
    let Some((title, detail)) = open_errors_prompt(errors) else {
        return;
    };
    // After the windows this command opened are on screen.
    cx.defer(move |cx| {
        let window = cx.active_window().or_else(|| cx.windows().last().copied());
        if let Some(window) = window {
            let _ = window.update(cx, |_, window, cx| {
                // Nothing to wait for: the prompt only has an OK button.
                drop(window.prompt(PromptLevel::Critical, &title, Some(&detail), &["OK"], cx));
            });
        }
    });
}

/// Where a data file from the command line opens.
#[derive(Debug, PartialEq, Eq)]
enum Destination<W> {
    /// The project window with this handle.
    Project(W),
    /// The window without a project, opened if needed.
    Files,
}

/// The project the same command opened wins, then the active window (a
/// project, or the one without), then the newest project window.
fn destination<W: Copy + PartialEq>(
    opened: Option<W>,
    active: Option<W>,
    files: Option<W>,
    projects: &[W],
) -> Destination<W> {
    if let Some(opened) = opened {
        return Destination::Project(opened);
    }
    match active {
        Some(active) if projects.contains(&active) => Destination::Project(active),
        Some(active) if files == Some(active) => Destination::Files,
        _ => projects
            .last()
            .map_or(Destination::Files, |newest| Destination::Project(*newest)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_failures_are_shown_together() {
        assert_eq!(open_errors_prompt(&[]), None);
        assert_eq!(
            open_errors_prompt(&["/work/shop: Permission denied".into()]),
            Some((
                "Could not open a path".into(),
                "/work/shop: Permission denied".into()
            ))
        );
        assert_eq!(
            open_errors_prompt(&["a: no".into(), "b: no".into()]),
            Some(("Could not open 2 paths".into(), "a: no\nb: no".into()))
        );
    }

    #[test]
    fn a_data_file_opens_in_the_project_the_command_opened() {
        assert_eq!(
            destination(Some(2), Some(1), None, &[1, 2]),
            Destination::Project(2)
        );
    }

    #[test]
    fn otherwise_in_the_active_window() {
        assert_eq!(
            destination(None, Some(1), Some(3), &[1, 2]),
            Destination::Project(1)
        );
        assert_eq!(
            destination(None, Some(3), Some(3), &[1, 2]),
            Destination::Files
        );
    }

    #[test]
    fn otherwise_in_the_newest_project_window() {
        // The welcome window, or none, is active.
        assert_eq!(
            destination(None, Some(9), None, &[1, 2]),
            Destination::Project(2)
        );
        assert_eq!(
            destination(None, None, Some(3), &[1, 2]),
            Destination::Project(2)
        );
    }

    #[test]
    fn without_a_project_window_it_opens_in_the_window_without_a_project() {
        assert_eq!(destination::<u8>(None, None, None, &[]), Destination::Files);
        assert_eq!(destination(None, Some(9), Some(3), &[]), Destination::Files);
    }
}
