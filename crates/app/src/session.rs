//! What was open when Kuery restarted to update, so the new version opens
//! the same windows and tabs. It is written just before the restart and
//! read once, by the next start, which deletes it; quitting writes nothing.
//!
//! Tabs keep what opens them again (the connection, the table, the SQL in
//! the editor, saved or not), not their results: no query runs again, and
//! open transactions and staged row changes end with the old process, which
//! is why the restart still asks about those.

use std::path::{Path, PathBuf};

use gpui_kit::{App, Bounds, DisplayId, Window, WindowBounds, point, px, size};
use kuery_core::catalog::TableKind;
use kuery_core::document::catalog::ContainerKind;
use serde::{Deserialize, Serialize};

use crate::explorer::Mode;
use crate::table_tab::TableView;

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Back to front: the last one was the active window.
    pub windows: Vec<WindowState>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub placement: Option<Placement>,
    pub contents: Contents,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Contents {
    Project {
        folder: PathBuf,
        workspace: WorkspaceState,
    },
    /// The window for data files opened without a project.
    Files { files: Vec<PathBuf>, active: usize },
}

/// Where a window was, relative to its display, so it opens there again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    pub mode: WindowMode,
    /// The windowed bounds, also those a maximized window returns to.
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// The display's UUID; a display that is gone puts the window on the
    /// main one.
    pub display: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowMode {
    Windowed,
    Maximized,
    Fullscreen,
}

impl Placement {
    pub fn of(window: &Window, cx: &App) -> Self {
        let (mode, bounds) = match window.window_bounds() {
            WindowBounds::Windowed(bounds) => (WindowMode::Windowed, bounds),
            WindowBounds::Maximized(bounds) => (WindowMode::Maximized, bounds),
            WindowBounds::Fullscreen(bounds) => (WindowMode::Fullscreen, bounds),
        };
        // Some platforms report a maximized window as windowed.
        let mode = match mode {
            WindowMode::Windowed if window.is_maximized() => WindowMode::Maximized,
            mode => mode,
        };
        Self {
            mode,
            x: f32::from(bounds.origin.x),
            y: f32::from(bounds.origin.y),
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
            display: window
                .display(cx)
                .and_then(|display| display.uuid().ok())
                .map(|uuid| uuid.to_string()),
        }
    }

    /// The bounds and display to open the window with. Without its display
    /// the window keeps its size, centered on the main display.
    pub fn window_bounds(&self, cx: &App) -> (WindowBounds, Option<DisplayId>) {
        let display = self.display.as_deref().and_then(|uuid| {
            cx.displays()
                .into_iter()
                .find(|display| display.uuid().is_ok_and(|id| id.to_string() == uuid))
                .map(|display| display.id())
        });
        let size = size(px(self.width), px(self.height));
        let bounds = match display {
            Some(_) => Bounds::new(point(px(self.x), px(self.y)), size),
            None => Bounds::centered(None, size, cx),
        };
        let bounds = match self.mode {
            WindowMode::Windowed => WindowBounds::Windowed(bounds),
            WindowMode::Maximized => WindowBounds::Maximized(bounds),
            WindowMode::Fullscreen => WindowBounds::Fullscreen(bounds),
        };
        (bounds, display)
    }
}

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceState {
    /// Connections that were open, by name; they open again by themselves.
    pub connections: Vec<String>,
    pub tabs: Vec<TabState>,
    /// Index into `tabs`.
    pub active: usize,
    /// How many untitled queries were numbered, so new ones carry on.
    pub queries_opened: usize,
    pub sidebar_visible: bool,
    pub sidebar: SidebarMode,
    /// The sidebar tree's expanded nodes.
    pub expanded: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidebarMode {
    #[default]
    Explorer,
    Queries,
    History,
}

impl From<Mode> for SidebarMode {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::Explorer => Self::Explorer,
            Mode::Queries => Self::Queries,
            Mode::History => Self::History,
        }
    }
}

impl From<SidebarMode> for Mode {
    fn from(mode: SidebarMode) -> Self {
        match mode {
            SidebarMode::Explorer => Self::Explorer,
            SidebarMode::Queries => Self::Queries,
            SidebarMode::History => Self::History,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TabState {
    Query {
        connection: String,
        title: String,
        /// The saved query it edits, if any.
        file: Option<PathBuf>,
        /// The editor's text.
        text: String,
        /// The text as last saved (or as opened), to tell unsaved changes.
        saved: String,
    },
    Table {
        connection: String,
        schema: String,
        name: String,
        table_kind: TableKindState,
        view: TableViewState,
    },
    Diagram {
        connection: String,
        schema: String,
    },
    Activity {
        connection: String,
    },
    RedisKey {
        connection: String,
        database: i64,
        key: String,
    },
    RedisConsole {
        connection: String,
        database: i64,
        text: String,
    },
    /// A document collection, with its filter bar as typed.
    Collection {
        connection: String,
        /// `[database, collection]`.
        path: Vec<String>,
        #[serde(rename = "collection_kind")]
        kind: ContainerKindState,
        filter: String,
        projection: String,
        sort: String,
    },
    /// A document database's console, with its statements.
    DocumentConsole {
        connection: String,
        /// The container its runs start in (`["shop"]`); empty for the
        /// connection's own.
        path: Vec<String>,
        text: String,
        /// The language its statements are in, by name (`SQL`); empty for
        /// the engine's own.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        language: String,
        /// How its results show, once chosen; the language's own default
        /// (see [`ConsoleView::default_for`]) when not.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view: Option<ConsoleView>,
    },
    File {
        path: PathBuf,
    },
}

impl TabState {
    /// The connection the tab waits for, if it needs one.
    pub fn connection(&self) -> Option<&str> {
        match self {
            TabState::Query { connection, .. }
            | TabState::Table { connection, .. }
            | TabState::Diagram { connection, .. }
            | TabState::Activity { connection }
            | TabState::RedisKey { connection, .. }
            | TabState::RedisConsole { connection, .. }
            | TabState::Collection { connection, .. }
            | TabState::DocumentConsole { connection, .. } => Some(connection),
            TabState::File { .. } => None,
        }
    }
}

/// How a document console shows the documents a statement returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsoleView {
    /// Rows: the columns a SQL statement names, in its order and by its
    /// names; the documents' fields for the engine's own language.
    Table,
    /// The documents as the engine returned them, each shown whole beside
    /// the grid when picked.
    Documents,
}

impl ConsoleView {
    /// SQL shows rows, the engine's own language documents.
    pub fn default_for(frontend: bool) -> ConsoleView {
        match frontend {
            true => ConsoleView::Table,
            false => ConsoleView::Documents,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableKindState {
    Table,
    PartitionedTable,
    View,
    MaterializedView,
    ForeignTable,
}

impl From<TableKind> for TableKindState {
    fn from(kind: TableKind) -> Self {
        match kind {
            TableKind::Table => Self::Table,
            TableKind::PartitionedTable => Self::PartitionedTable,
            TableKind::View => Self::View,
            TableKind::MaterializedView => Self::MaterializedView,
            TableKind::ForeignTable => Self::ForeignTable,
        }
    }
}

impl From<TableKindState> for TableKind {
    fn from(kind: TableKindState) -> Self {
        match kind {
            TableKindState::Table => Self::Table,
            TableKindState::PartitionedTable => Self::PartitionedTable,
            TableKindState::View => Self::View,
            TableKindState::MaterializedView => Self::MaterializedView,
            TableKindState::ForeignTable => Self::ForeignTable,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerKindState {
    Database,
    Scope,
    Collection,
    View,
    TimeSeries,
    Capped,
}

impl From<ContainerKind> for ContainerKindState {
    fn from(kind: ContainerKind) -> Self {
        match kind {
            ContainerKind::Database => Self::Database,
            ContainerKind::Scope => Self::Scope,
            ContainerKind::Collection => Self::Collection,
            ContainerKind::View => Self::View,
            ContainerKind::TimeSeries => Self::TimeSeries,
            ContainerKind::Capped => Self::Capped,
        }
    }
}

impl From<ContainerKindState> for ContainerKind {
    fn from(kind: ContainerKindState) -> Self {
        match kind {
            ContainerKindState::Database => Self::Database,
            ContainerKindState::Scope => Self::Scope,
            ContainerKindState::Collection => Self::Collection,
            ContainerKindState::View => Self::View,
            ContainerKindState::TimeSeries => Self::TimeSeries,
            ContainerKindState::Capped => Self::Capped,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableViewState {
    Data,
    Definition,
}

impl From<TableView> for TableViewState {
    fn from(view: TableView) -> Self {
        match view {
            TableView::Data => Self::Data,
            TableView::Definition => Self::Definition,
        }
    }
}

impl From<TableViewState> for TableView {
    fn from(view: TableViewState) -> Self {
        match view {
            TableViewState::Data => Self::Data,
            TableViewState::Definition => Self::Definition,
        }
    }
}

/// Where a restored tab goes among the open ones, given each open tab's
/// place in the saved session (`None` for one opened since): after the tabs
/// that came before it, ahead of the others. Tabs come back out of order,
/// as their connections and files open.
pub fn restored_position(places: &[Option<usize>], place: usize) -> usize {
    places
        .iter()
        .position(|other| other.is_none_or(|other| other > place))
        .unwrap_or(places.len())
}

/// The tabs to save and the active one's index. Tabs still waiting for
/// their connection keep their place, so restarting again does not lose them.
pub fn tabs_to_save(
    open: Vec<(Option<usize>, Option<TabState>)>,
    active: usize,
    mut waiting: Vec<(usize, TabState)>,
    waiting_active: Option<usize>,
) -> (Vec<TabState>, usize) {
    let waiting_active =
        waiting_active.filter(|place| waiting.iter().any(|(other, _)| other == place));
    let mut tabs: Vec<(Option<usize>, Option<TabState>, bool)> = open
        .into_iter()
        .enumerate()
        .map(|(index, (place, tab))| (place, tab, waiting_active.is_none() && index == active))
        .collect();
    waiting.sort_by_key(|(place, _)| *place);
    for (place, tab) in waiting {
        let places: Vec<Option<usize>> = tabs.iter().map(|(place, ..)| *place).collect();
        tabs.insert(
            restored_position(&places, place),
            (Some(place), Some(tab), waiting_active == Some(place)),
        );
    }
    let tabs: Vec<(TabState, bool)> = tabs
        .into_iter()
        .filter_map(|(_, tab, active)| Some((tab?, active)))
        .collect();
    let active = tabs.iter().position(|(_, active)| *active).unwrap_or(0);
    (tabs.into_iter().map(|(tab, _)| tab).collect(), active)
}

fn path() -> PathBuf {
    crate::paths::config_dir().join("session.json")
}

pub fn save(session: &Session) -> std::io::Result<()> {
    save_in(&path(), session)
}

/// The session the last restart saved, if any. It is deleted, so it opens
/// once.
pub fn take() -> Option<Session> {
    take_in(&path())
}

fn save_in(path: &Path, session: &Session) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(session).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)
}

fn take_in(path: &Path) -> Option<Session> {
    let text = std::fs::read_to_string(path).ok()?;
    if let Err(error) = std::fs::remove_file(path) {
        eprintln!("kuery: could not delete {}: {error}", path.display());
    }
    serde_json::from_str(&text)
        .map_err(|error| eprintln!("kuery: could not read {}: {error}", path.display()))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("kuery-session-{name}-{}", crate::project::new_id()))
            .join("session.json")
    }

    fn session() -> Session {
        Session {
            windows: vec![
                WindowState {
                    placement: None,
                    contents: Contents::Files {
                        files: vec!["/data/orders.csv".into()],
                        active: 0,
                    },
                },
                WindowState {
                    placement: Some(Placement {
                        mode: WindowMode::Maximized,
                        x: 40.,
                        y: 30.,
                        width: 1440.,
                        height: 900.,
                        display: Some("8C1A2B3C-0000-0000-0000-000000000001".into()),
                    }),
                    contents: Contents::Project {
                        folder: "/work/shop".into(),
                        workspace: WorkspaceState {
                            connections: vec!["local".into(), "cache".into()],
                            tabs: vec![
                                TabState::Query {
                                    connection: "local".into(),
                                    title: "monthly".into(),
                                    file: Some("/work/shop/.kuery/queries/monthly.sql".into()),
                                    text: "select 2;\n".into(),
                                    saved: "select 1;\n".into(),
                                },
                                TabState::Table {
                                    connection: "local".into(),
                                    schema: "public".into(),
                                    name: "orders".into(),
                                    table_kind: TableKindState::View,
                                    view: TableViewState::Definition,
                                },
                                TabState::RedisConsole {
                                    connection: "cache".into(),
                                    database: 2,
                                    text: "GET a\n".into(),
                                },
                                TabState::Collection {
                                    connection: "docs".into(),
                                    path: vec!["shop".into(), "orders".into()],
                                    kind: ContainerKindState::TimeSeries,
                                    filter: "{status: \"paid\"}".into(),
                                    projection: String::new(),
                                    sort: "{total: -1}".into(),
                                },
                                TabState::File {
                                    path: "/data/orders.csv".into(),
                                },
                            ],
                            active: 1,
                            queries_opened: 3,
                            sidebar_visible: false,
                            sidebar: SidebarMode::History,
                            expanded: vec!["0".into(), "0/public".into()],
                        },
                    },
                },
            ],
        }
    }

    #[test]
    fn a_saved_session_opens_once() {
        let file = temp_file("once");
        save_in(&file, &session()).unwrap();
        assert_eq!(take_in(&file), Some(session()));
        // A normal start afterwards opens as usual.
        assert_eq!(take_in(&file), None);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn an_unreadable_session_is_dropped() {
        let file = temp_file("broken");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "{ not json").unwrap();
        assert_eq!(take_in(&file), None);
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn only_file_tabs_open_without_a_connection() {
        let WindowState {
            contents: Contents::Project { workspace, .. },
            ..
        } = &session().windows[1]
        else {
            unreachable!()
        };
        let connections: Vec<_> = workspace.tabs.iter().map(TabState::connection).collect();
        assert_eq!(
            connections,
            [
                Some("local"),
                Some("local"),
                Some("cache"),
                Some("docs"),
                None
            ]
        );
    }

    #[test]
    fn a_collection_tab_keeps_its_place_and_bar() {
        let tab = TabState::Collection {
            connection: "docs".into(),
            path: vec!["shop".into(), "orders".into()],
            kind: ContainerKindState::View,
            filter: "{status: \"paid\"}".into(),
            projection: "{total: 1}".into(),
            sort: String::new(),
        };
        let text = serde_json::to_string(&tab).unwrap();
        assert!(
            text.starts_with(r#"{"kind":"collection","connection":"docs","path":["shop","orders"],"collection_kind":"view""#),
            "{text}"
        );
        assert_eq!(serde_json::from_str::<TabState>(&text).unwrap(), tab);
        assert_eq!(tab.connection(), Some("docs"));
        assert_eq!(
            ContainerKind::from(ContainerKindState::from(ContainerKind::Capped)),
            ContainerKind::Capped
        );
    }

    #[test]
    fn a_document_console_keeps_its_database_and_statements() {
        let tab = TabState::DocumentConsole {
            connection: "docs".into(),
            path: vec!["shop".into()],
            text: "db.orders.find()\n".into(),
            language: String::new(),
            view: None,
        };
        let text = serde_json::to_string(&tab).unwrap();
        assert_eq!(
            text,
            r#"{"kind":"document_console","connection":"docs","path":["shop"],"text":"db.orders.find()\n"}"#
        );
        assert_eq!(serde_json::from_str::<TabState>(&text).unwrap(), tab);
        assert_eq!(tab.connection(), Some("docs"));
    }

    #[test]
    fn a_document_console_keeps_its_language_and_view() {
        let tab = TabState::DocumentConsole {
            connection: "docs".into(),
            path: Vec::new(),
            text: "SELECT * FROM orders".into(),
            language: "SQL".into(),
            view: Some(ConsoleView::Documents),
        };
        let text = serde_json::to_string(&tab).unwrap();
        assert_eq!(
            text,
            r#"{"kind":"document_console","connection":"docs","path":[],"text":"SELECT * FROM orders","language":"SQL","view":"documents"}"#
        );
        assert_eq!(serde_json::from_str::<TabState>(&text).unwrap(), tab);
        // A console saved before it had either is in the engine's language,
        // with that language's view.
        let old = r#"{"kind":"document_console","connection":"docs","path":["shop"],"text":"db.orders.find()"}"#;
        assert_eq!(
            serde_json::from_str::<TabState>(old).unwrap(),
            TabState::DocumentConsole {
                connection: "docs".into(),
                path: vec!["shop".into()],
                text: "db.orders.find()".into(),
                language: String::new(),
                view: None,
            }
        );
        assert_eq!(ConsoleView::default_for(true), ConsoleView::Table);
        assert_eq!(ConsoleView::default_for(false), ConsoleView::Documents);
    }

    #[test]
    fn restored_tabs_keep_their_order() {
        // Saved as 0 1 2 3; 2 came back first, then 0, then 3, then 1.
        assert_eq!(restored_position(&[], 2), 0);
        assert_eq!(restored_position(&[Some(2)], 0), 0);
        assert_eq!(restored_position(&[Some(0), Some(2)], 3), 2);
        assert_eq!(restored_position(&[Some(0), Some(2), Some(3)], 1), 1);
    }

    #[test]
    fn tabs_opened_since_stay_after_the_restored_ones() {
        assert_eq!(restored_position(&[Some(0), None], 1), 1);
        assert_eq!(restored_position(&[None], 0), 0);
    }

    fn activity(connection: &str) -> TabState {
        TabState::Activity {
            connection: connection.into(),
        }
    }

    #[test]
    fn tabs_waiting_for_their_connection_are_saved_in_their_place() {
        // Saved as a b c: b still waits, d was opened since.
        let open = vec![
            (Some(0), Some(activity("a"))),
            (Some(2), Some(activity("c"))),
            (None, Some(activity("d"))),
        ];
        let waiting = vec![(1, activity("b"))];
        assert_eq!(
            tabs_to_save(open, 1, waiting, None),
            (
                vec![activity("a"), activity("b"), activity("c"), activity("d")],
                2
            )
        );
    }

    #[test]
    fn the_active_tab_stays_active_while_it_waits() {
        let open = vec![(Some(1), Some(activity("b")))];
        let waiting = vec![(2, activity("c")), (0, activity("a"))];
        assert_eq!(
            tabs_to_save(open, 0, waiting, Some(2)),
            (vec![activity("a"), activity("b"), activity("c")], 2)
        );
    }

    #[test]
    fn tabs_that_do_not_come_back_are_left_out() {
        // A results tab before the active one.
        let open = vec![(None, None), (None, Some(activity("a")))];
        assert_eq!(
            tabs_to_save(open, 1, Vec::new(), None),
            (vec![activity("a")], 0)
        );
    }
}
