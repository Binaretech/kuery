//! Every command the app exposes, in one registry.
//!
//! A command has a stable id (what the user keymap refers to), a title and
//! category for the command palette, the gpui action it dispatches, its
//! default shortcuts, and the key context where those shortcuts apply. The
//! palette and the keymap both read from here, so a command added once is
//! searchable and bindable.

use std::path::PathBuf;

use gpui_kit::{Action, actions};

use crate::audit::OpenAuditLog;
use crate::menus::{CloseWindow, Minimize, Quit, ToggleFullScreen, Zoom};
use crate::query_tab::{
    CancelQuery, CommitTransaction, ExportResults, FormatSql, PreviewImpact, RollbackTransaction,
    RunAll, RunQuery, SaveQuery, SaveQueryAs, ToggleResults, UseFullWrite, UseReadOnly, UseWrite,
};
use crate::table_tab::{ReloadTable, SaveTableChanges};

actions!(
    kuery,
    [
        OpenCommandPalette,
        CloseTab,
        NextTab,
        PreviousTab,
        NewQuery,
        ToggleSidebar,
        OpenKeymapFile,
        ReloadKeymap,
        OpenProject,
        ClearRecentProjects,
        NewConnection,
    ]
);

/// Opens one of the recent projects (the File › Open Recent menu).
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = kuery, no_json)]
pub struct OpenRecentProject {
    pub path: PathBuf,
}

pub struct Command {
    /// Stable id used in the keymap file, e.g. `tab.close`.
    pub id: &'static str,
    pub title: &'static str,
    pub category: &'static str,
    pub action: fn() -> Box<dyn Action>,
    /// Default shortcuts. `secondary` is ⌘ on macOS and Ctrl elsewhere.
    pub defaults: &'static [&'static str],
    /// Key context the shortcuts apply in (`None` = anywhere in the window).
    pub context: Option<&'static str>,
}

macro_rules! action {
    ($action:expr) => {
        || Box::new($action)
    };
}

pub const COMMANDS: &[Command] = &[
    Command {
        id: "palette.open",
        title: "Command Palette",
        category: "View",
        action: action!(OpenCommandPalette),
        defaults: &["secondary-k", "secondary-shift-p"],
        context: None,
    },
    Command {
        id: "project.open",
        title: "Open Project…",
        category: "Project",
        action: action!(OpenProject),
        defaults: &["secondary-o"],
        context: None,
    },
    Command {
        id: "project.clear_recent",
        title: "Clear Recent Projects",
        category: "Project",
        action: action!(ClearRecentProjects),
        defaults: &[],
        context: None,
    },
    Command {
        id: "connection.new",
        title: "New Connection…",
        category: "Project",
        action: action!(NewConnection),
        defaults: &[],
        context: None,
    },
    Command {
        id: "tab.new_query",
        title: "New Query",
        category: "Tab",
        action: action!(NewQuery),
        defaults: &["secondary-t"],
        context: None,
    },
    Command {
        id: "tab.close",
        title: "Close Tab",
        category: "Tab",
        action: action!(CloseTab),
        defaults: &["secondary-w"],
        context: None,
    },
    Command {
        id: "tab.next",
        title: "Next Tab",
        category: "Tab",
        action: action!(NextTab),
        defaults: &["ctrl-tab", "secondary-shift-]"],
        context: None,
    },
    Command {
        id: "tab.previous",
        title: "Previous Tab",
        category: "Tab",
        action: action!(PreviousTab),
        defaults: &["ctrl-shift-tab", "secondary-shift-["],
        context: None,
    },
    Command {
        id: "sidebar.toggle",
        title: "Toggle Sidebar",
        category: "View",
        action: action!(ToggleSidebar),
        defaults: &["secondary-b"],
        context: None,
    },
    Command {
        id: "query.run",
        title: "Run Statement or Selection",
        category: "Query",
        action: action!(RunQuery),
        defaults: &["secondary-enter"],
        context: Some("QueryTab"),
    },
    Command {
        id: "query.run_all",
        title: "Run All",
        category: "Query",
        action: action!(RunAll),
        defaults: &["secondary-alt-enter"],
        context: Some("QueryTab"),
    },
    Command {
        id: "query.format",
        title: "Format SQL",
        category: "Query",
        action: action!(FormatSql),
        defaults: &["secondary-shift-f"],
        context: Some("QueryTab"),
    },
    Command {
        id: "query.save",
        title: "Save Query",
        category: "Query",
        action: action!(SaveQuery),
        defaults: &["secondary-s"],
        context: Some("QueryTab"),
    },
    Command {
        id: "query.save_as",
        title: "Save Query As…",
        category: "Query",
        action: action!(SaveQueryAs),
        defaults: &["secondary-shift-s"],
        context: Some("QueryTab"),
    },
    Command {
        id: "query.cancel",
        title: "Cancel Query",
        category: "Query",
        action: action!(CancelQuery),
        defaults: &["secondary-."],
        context: Some("QueryTab"),
    },
    Command {
        id: "query.toggle_results",
        title: "Toggle Results Panel",
        category: "Query",
        action: action!(ToggleResults),
        defaults: &["secondary-j"],
        context: Some("QueryTab"),
    },
    Command {
        id: "query.export",
        title: "Export Results…",
        category: "Query",
        action: action!(ExportResults),
        defaults: &["secondary-shift-e"],
        context: Some("QueryTab"),
    },
    Command {
        id: "query.preview",
        title: "Preview Impact",
        category: "Query",
        action: action!(PreviewImpact),
        defaults: &[],
        context: Some("QueryTab"),
    },
    Command {
        id: "audit.open",
        title: "Open Audit Log",
        category: "Preferences",
        action: action!(OpenAuditLog),
        defaults: &[],
        context: None,
    },
    Command {
        id: "query.read_only",
        title: "Read-only Mode",
        category: "Query",
        action: action!(UseReadOnly),
        defaults: &[],
        context: Some("QueryTab"),
    },
    Command {
        id: "query.write",
        title: "Write Mode (Transactional)",
        category: "Query",
        action: action!(UseWrite),
        defaults: &[],
        context: Some("QueryTab"),
    },
    Command {
        id: "query.full_write",
        title: "Full Write Mode",
        category: "Query",
        action: action!(UseFullWrite),
        defaults: &[],
        context: Some("QueryTab"),
    },
    Command {
        id: "query.commit",
        title: "Commit Transaction",
        category: "Query",
        action: action!(CommitTransaction),
        defaults: &[],
        context: Some("QueryTab"),
    },
    Command {
        id: "query.rollback",
        title: "Roll Back Transaction",
        category: "Query",
        action: action!(RollbackTransaction),
        defaults: &[],
        context: Some("QueryTab"),
    },
    Command {
        id: "table.reload",
        title: "Reload Table",
        category: "Table",
        action: action!(ReloadTable),
        defaults: &["secondary-r"],
        context: Some("TableTab"),
    },
    Command {
        id: "table.save",
        title: "Save Table Changes",
        category: "Table",
        action: action!(SaveTableChanges),
        defaults: &["secondary-s"],
        context: Some("TableTab"),
    },
    Command {
        id: "keymap.open",
        title: "Open Keymap File",
        category: "Preferences",
        action: action!(OpenKeymapFile),
        defaults: &[],
        context: None,
    },
    Command {
        id: "keymap.reload",
        title: "Reload Keymap",
        category: "Preferences",
        action: action!(ReloadKeymap),
        defaults: &[],
        context: None,
    },
    Command {
        id: "window.close",
        title: "Close Window",
        category: "Window",
        action: action!(CloseWindow),
        defaults: &["secondary-shift-w"],
        context: None,
    },
    Command {
        id: "window.minimize",
        title: "Minimize",
        category: "Window",
        action: action!(Minimize),
        #[cfg(target_os = "macos")]
        defaults: &["cmd-m"],
        #[cfg(not(target_os = "macos"))]
        defaults: &[],
        context: None,
    },
    Command {
        id: "window.zoom",
        title: "Zoom",
        category: "Window",
        action: action!(Zoom),
        defaults: &[],
        context: None,
    },
    Command {
        id: "window.fullscreen",
        title: "Toggle Full Screen",
        category: "Window",
        action: action!(ToggleFullScreen),
        #[cfg(target_os = "macos")]
        defaults: &["ctrl-cmd-f"],
        #[cfg(not(target_os = "macos"))]
        defaults: &["f11"],
        context: None,
    },
    Command {
        id: "app.quit",
        title: "Quit",
        category: "App",
        action: action!(Quit),
        defaults: &["secondary-q"],
        context: None,
    },
];

pub fn find(id: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_unique_and_dotted() {
        let mut seen = HashSet::new();
        for command in COMMANDS {
            assert!(seen.insert(command.id), "duplicated id {}", command.id);
            assert!(command.id.contains('.'), "{}", command.id);
        }
    }

    #[test]
    fn close_tab_is_secondary_w() {
        assert_eq!(find("tab.close").unwrap().defaults, &["secondary-w"]);
        assert_eq!(
            find("window.close").unwrap().defaults,
            &["secondary-shift-w"]
        );
    }
}
