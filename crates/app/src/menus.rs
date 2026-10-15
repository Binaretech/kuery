//! Native application menus (the menu bar on macOS) and the window shortcuts
//! that go with them: quit, close, minimize, zoom, full screen and editing.

use gpui_kit::component::input::{Copy, Cut, Paste, Redo, SelectAll, Undo};
use gpui_kit::{App, KeyBinding, Menu, MenuItem, OsAction, SystemMenuType, Window, actions};

actions!(
    dbadmin,
    [Quit, CloseWindow, Minimize, Zoom, ToggleFullScreen]
);

pub fn init(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &CloseWindow, cx| with_active_window(cx, |window| window.remove_window()));
    cx.on_action(|_: &Minimize, cx| with_active_window(cx, |window| window.minimize_window()));
    cx.on_action(|_: &Zoom, cx| with_active_window(cx, |window| window.zoom_window()));
    cx.on_action(|_: &ToggleFullScreen, cx| {
        with_active_window(cx, |window| window.toggle_fullscreen())
    });

    cx.bind_keys([
        KeyBinding::new("secondary-q", Quit, None),
        KeyBinding::new("secondary-w", CloseWindow, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-m", Minimize, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-cmd-f", ToggleFullScreen, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("f11", ToggleFullScreen, None),
    ]);

    cx.set_menus([
        Menu::new("dbadmin").items([
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Quit dbadmin", Quit),
        ]),
        Menu::new("File").items([MenuItem::action("Close Window", CloseWindow)]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", Undo, OsAction::Undo),
            MenuItem::os_action("Redo", Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", Cut, OsAction::Cut),
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Zoom),
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
