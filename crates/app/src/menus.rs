//! Native application menus (the menu bar on macOS) and the window actions
//! behind them. Shortcuts are not set here: they come from the keymap, and the
//! menu bar shows whatever the keymap binds to each action.

use gpui_kit::component::input::{Copy, Cut, Paste, Redo, SelectAll, Undo};
use gpui_kit::{App, Menu, MenuItem, OsAction, SystemMenuType, Window, actions};

use crate::commands::{
    ClearRecentProjects, CloseTab, NewConnection, NewQuery, NextTab, OpenCommandPalette,
    OpenKeymapFile, OpenProject, OpenRecentProject, PreviousTab, ReloadKeymap, ToggleSidebar,
};
use crate::query_tab::{SaveQuery, SaveQueryAs};

actions!(
    dbadmin,
    [Quit, CloseWindow, Minimize, Zoom, ToggleFullScreen]
);

/// Registers the app- and window-level actions.
pub fn init(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &CloseWindow, cx| with_active_window(cx, |window| window.remove_window()));
    cx.on_action(|_: &Minimize, cx| with_active_window(cx, |window| window.minimize_window()));
    cx.on_action(|_: &Zoom, cx| with_active_window(cx, |window| window.zoom_window()));
    cx.on_action(|_: &ToggleFullScreen, cx| {
        with_active_window(cx, |window| window.toggle_fullscreen())
    });
}

/// Builds the menu bar. Called again after the keymap or the recent projects
/// change, so shortcuts and the Open Recent list stay current.
pub fn set_menus(cx: &mut App) {
    let mut recent: Vec<MenuItem> = crate::recent::available()
        .into_iter()
        .map(|project| {
            MenuItem::action(
                format!("{} — {}", project.name, project.display_path()),
                OpenRecentProject { path: project.path },
            )
        })
        .collect();
    if !recent.is_empty() {
        recent.push(MenuItem::separator());
    }
    recent.push(MenuItem::action(
        "Clear Recent Projects",
        ClearRecentProjects,
    ));

    cx.set_menus([
        Menu::new("dbadmin").items([
            MenuItem::action("Command Palette…", OpenCommandPalette),
            MenuItem::separator(),
            MenuItem::action("Open Keymap File", OpenKeymapFile),
            MenuItem::action("Reload Keymap", ReloadKeymap),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Quit dbadmin", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("Open Project…", OpenProject),
            MenuItem::submenu(Menu::new("Open Recent").items(recent)),
            MenuItem::separator(),
            MenuItem::action("New Query", NewQuery),
            MenuItem::action("New Connection…", NewConnection),
            MenuItem::separator(),
            MenuItem::action("Save", SaveQuery),
            MenuItem::action("Save As…", SaveQueryAs),
            MenuItem::separator(),
            MenuItem::action("Close Tab", CloseTab),
            MenuItem::action("Close Window", CloseWindow),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", Undo, OsAction::Undo),
            MenuItem::os_action("Redo", Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", Cut, OsAction::Cut),
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
        ]),
        Menu::new("View").items([
            MenuItem::action("Command Palette…", OpenCommandPalette),
            MenuItem::action("Toggle Sidebar", ToggleSidebar),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Zoom),
            MenuItem::separator(),
            MenuItem::action("Next Tab", NextTab),
            MenuItem::action("Previous Tab", PreviousTab),
            MenuItem::separator(),
            MenuItem::action("Toggle Full Screen", ToggleFullScreen),
        ]),
    ]);
}

/// Window actions apply to the active window, whatever has focus inside it.
fn with_active_window(cx: &mut App, f: impl FnOnce(&mut Window)) {
    if let Some(window) = cx.active_window() {
        window.update(cx, |_, window, _| f(window)).ok();
    }
}
