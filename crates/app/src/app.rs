//! App-wide state and windows: one window per project, plus a welcome window
//! when no project is open.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dbadmin_pg::Engine;
use gpui_kit::component::TitleBar;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Global, PathPromptOptions, WindowBounds,
    WindowOptions, px, size,
};

use crate::commands::OpenProject;
use crate::project::{Project, ProjectModel};
use crate::secrets::SecretStore;
use crate::welcome::Welcome;
use crate::workspace::DbAdminView;

pub struct AppState {
    pub engine: Arc<Engine>,
    pub secrets: Arc<dyn SecretStore>,
    /// Project windows by folder, so opening a project twice focuses it.
    windows: Vec<(PathBuf, AnyWindowHandle)>,
    welcome: Option<AnyWindowHandle>,
}

impl Global for AppState {}

pub fn init(engine: Arc<Engine>, secrets: Arc<dyn SecretStore>, cx: &mut App) {
    cx.set_global(AppState {
        engine,
        secrets,
        windows: Vec::new(),
        welcome: None,
    });
    cx.on_action(|_: &OpenProject, cx| prompt_open_project(cx));
}

pub fn state(cx: &App) -> &AppState {
    cx.global::<AppState>()
}

/// Asks for a folder and opens it as a project (creating `.dbadmin/` if it
/// is not one yet).
pub fn prompt_open_project(cx: &mut App) {
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
            cx.update(|cx| {
                if let Err(error) = open_project(&path, cx) {
                    eprintln!("dbadmin: {error}");
                }
            });
        }
    })
    .detach();
}

/// Opens the project in `folder` in its own window, or focuses the window if
/// it is already open.
pub fn open_project(folder: &Path, cx: &mut App) -> Result<(), String> {
    let folder = folder
        .canonicalize()
        .unwrap_or_else(|_| folder.to_path_buf());

    let existing = cx
        .global::<AppState>()
        .windows
        .iter()
        .find(|(path, _)| *path == folder)
        .map(|(_, window)| *window);
    if let Some(window) = existing
        && window
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    {
        return Ok(());
    }

    let project = Project::open_or_init(&folder)?;
    crate::recent::add(&folder);
    let title = project.name.clone();
    let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        ..TitleBar::window_options()
    };
    let (window, _) = gpui_kit::open_window(options, cx, move |window, cx| {
        window.set_window_title(&title);
        let model = cx.new(|_| ProjectModel { project });
        cx.new(|cx| DbAdminView::new(model, window, cx))
    })
    .map_err(|error| format!("Could not open a window: {error}"))?;

    let state = cx.global_mut::<AppState>();
    state.windows.retain(|(path, _)| *path != folder);
    state.windows.push((folder, window));
    close_welcome(cx);
    Ok(())
}

pub fn open_welcome(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(760.), px(520.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        ..TitleBar::window_options()
    };
    if let Ok((window, _)) = gpui_kit::open_window(options, cx, |window, cx| {
        window.set_window_title("dbadmin");
        cx.new(|cx| Welcome::new(window, cx))
    }) {
        cx.global_mut::<AppState>().welcome = Some(window);
    }
}

fn close_welcome(cx: &mut App) {
    if let Some(window) = cx.global_mut::<AppState>().welcome.take() {
        window
            .update(cx, |_, window, _| window.remove_window())
            .ok();
    }
}
