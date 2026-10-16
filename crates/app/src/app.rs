//! App-wide state and windows: one window per project, plus a welcome window
//! when no project is open.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, Global, IntoElement,
    PathPromptOptions, PromptLevel, Render, Window, WindowBounds, WindowOptions, px, size,
};
use kuery_pg::Engine;

use crate::commands::{ClearRecentProjects, OpenProject, OpenRecentProject};
use crate::project::{Project, ProjectModel};
use crate::secrets::SecretStore;
use crate::welcome::Welcome;
use crate::workspace::WorkspaceView;

pub struct AppState {
    pub engine: Arc<Engine>,
    pub redis: Arc<kuery_redis::Engine>,
    pub mysql: Arc<kuery_mysql::Engine>,
    pub secrets: Arc<dyn SecretStore>,
    /// Open project windows, so opening a project twice focuses it.
    windows: Vec<ProjectWindowEntry>,
    welcome: Option<(AnyWindowHandle, Entity<Welcome>)>,
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

pub fn init(
    engine: Arc<Engine>,
    redis: Arc<kuery_redis::Engine>,
    mysql: Arc<kuery_mysql::Engine>,
    secrets: Arc<dyn SecretStore>,
    cx: &mut App,
) {
    cx.set_global(AppState {
        engine,
        redis,
        mysql,
        secrets,
        windows: Vec::new(),
        welcome: None,
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
            postgres: self.engine.clone(),
            redis: self.redis.clone(),
            mysql: self.mysql.clone(),
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
    let folder = canonical(folder);
    if focus_existing(&folder, cx) {
        return Ok(());
    }

    let project = Project::open_or_init(&folder)?;
    let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        ..crate::app_icon::window_options()
    };
    let (handle, view) = gpui_kit::open_window(options, cx, move |window, cx| {
        cx.new(|cx| ProjectWindow::new(project, window, cx))
    })
    .map_err(|error| format!("Could not open a window: {error}"))?;

    let state = cx.global_mut::<AppState>();
    state.windows.retain(|entry| entry.folder != folder);
    state.windows.push(ProjectWindowEntry {
        folder: folder.clone(),
        handle,
        view,
    });
    crate::recent::add(&folder);
    recents_changed(cx);
    close_welcome(cx);
    Ok(())
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
///
/// Deferred: the Quit action arrives while a window is handling the event,
/// and that window could not show the question then.
pub fn request_quit(cx: &mut App) {
    cx.defer(|cx| {
        let open = cx.windows();
        let unsaved: Vec<AnyWindowHandle> = state(cx)
            .windows
            .iter()
            .filter(|entry| open.contains(&entry.handle))
            .filter(|entry| {
                entry
                    .view
                    .read(cx)
                    .workspace
                    .read(cx)
                    .has_unsaved_changes(cx)
            })
            .map(|entry| entry.handle)
            .collect();
        let Some(&first) = unsaved.first() else {
            cx.quit();
            return;
        };
        let handle = cx
            .active_window()
            .filter(|active| unsaved.contains(active))
            .unwrap_or(first);
        let detail = format!(
            "{} unsaved SQL, an open transaction or staged row changes. They will \
             be lost, and open transactions rolled back.",
            if unsaved.len() == 1 {
                "A window has".to_string()
            } else {
                format!("{} windows have", unsaved.len())
            }
        );
        let answer = handle.update(cx, |_, window, cx| {
            window.activate_window();
            window.prompt(
                PromptLevel::Warning,
                "Quit Kuery?",
                Some(&detail),
                &["Quit", "Cancel"],
                cx,
            )
        });
        let Ok(answer) = answer else {
            cx.quit();
            return;
        };
        cx.spawn(async move |cx| {
            if let Ok(0) = answer.await {
                cx.update(|cx| cx.quit());
            }
        })
        .detach();
    });
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
