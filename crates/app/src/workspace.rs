//! Project window: custom title bar, the sidebar on the left, and query
//! and table tabs. Query and table tabs belong to the connection
//! they were opened on.

use std::path::PathBuf;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, TitleBar, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EntityId, FocusHandle, Focusable,
    FontWeight, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, PromptLevel,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use kuery_core::catalog::TableInfo;

use crate::activity_tab::{ActivityEvent, ActivityTab};
use crate::assets::AppIcon;
use crate::audit::OpenAuditLog;
use crate::chrome;
use crate::commands::{
    ClearRecentProjects, CloseTab, NewConnection, NewQuery, NextTab, OpenCommandPalette,
    OpenKeymapFile, OpenProject, OpenSchemaDiagram, PreviousTab, ShowServerActivity, ToggleSidebar,
};
use crate::connection::{Db, Environment, RedisDb, WriteKind};
use crate::connection_editor::ConnectionEditor;
use crate::context_menus::{copy, item, reveal};
use crate::diagram_tab::{DiagramEvent, DiagramTab};
use crate::explorer::{Explorer, ExplorerEvent, Mode as ExplorerMode};
use crate::menus::CloseWindow;
use crate::palette::{CommandPalette, PaletteEvent};
use crate::project::{ProjectModel, query_connection};
use crate::query_tab::{self, QueryRan, QuerySaved, QueryTab};
use crate::recent::{self, RecentProject};
use crate::redis_console::ConsoleTab;
use crate::redis_key_tab::KeyTab;
use crate::table_tab::{TableTab, TableView};
use crate::theme::{self, color};
use crate::ui::placeholder;

enum Tab {
    Query(Entity<QueryTab>),
    Table(Entity<TableTab>),
    RedisKey(Entity<KeyTab>),
    RedisConsole(Entity<ConsoleTab>),
    Diagram(Entity<DiagramTab>),
    Activity(Entity<ActivityTab>),
}

impl Tab {
    fn id(&self) -> EntityId {
        match self {
            Tab::Query(query) => query.entity_id(),
            Tab::Table(table) => table.entity_id(),
            Tab::RedisKey(key) => key.entity_id(),
            Tab::RedisConsole(console) => console.entity_id(),
            Tab::Diagram(diagram) => diagram.entity_id(),
            Tab::Activity(activity) => activity.entity_id(),
        }
    }

    /// The Redis connection of a Redis tab.
    fn redis(&self, cx: &App) -> Option<RedisDb> {
        match self {
            Tab::RedisKey(key) => Some(key.read(cx).db().clone()),
            Tab::RedisConsole(console) => Some(console.read(cx).db().clone()),
            Tab::Query(_) | Tab::Table(_) | Tab::Diagram(_) | Tab::Activity(_) => None,
        }
    }

    /// Whether closing it needs a question first (unsaved SQL, an open
    /// transaction, pending row changes).
    fn needs_attention(&self, cx: &App) -> bool {
        match self {
            Tab::Query(query) => query.read(cx).is_dirty() || query.read(cx).in_transaction(),
            Tab::Table(table) => table.read(cx).has_pending(),
            Tab::RedisKey(key) => key.read(cx).has_pending(),
            Tab::RedisConsole(_) | Tab::Diagram(_) | Tab::Activity(_) => false,
        }
    }

    fn db(&self, cx: &App) -> Option<Db> {
        match self {
            Tab::Query(query) => Some(query.read(cx).db().clone()),
            Tab::Table(table) => Some(table.read(cx).db().clone()),
            Tab::Diagram(diagram) => Some(diagram.read(cx).db().clone()),
            Tab::Activity(activity) => Some(activity.read(cx).db().clone()),
            Tab::RedisKey(_) | Tab::RedisConsole(_) => None,
        }
    }

    /// Gives the tab keyboard focus, so its own shortcuts (run query, reload
    /// table…) apply right away.
    fn focus(&self, window: &mut Window, cx: &mut App) {
        match self {
            Tab::Query(query) => query.update(cx, |query, cx| query.focus_editor(window, cx)),
            Tab::Table(table) => {
                let handle = table.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
            Tab::RedisKey(key) => {
                let handle = key.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
            Tab::RedisConsole(console) => {
                console.update(cx, |console, cx| console.focus_editor(window, cx))
            }
            Tab::Diagram(diagram) => {
                let handle = diagram.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
            Tab::Activity(activity) => {
                let handle = activity.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
        }
    }
}

pub struct WorkspaceView {
    model: Entity<ProjectModel>,
    explorer: Entity<Explorer>,
    /// Saved queries waiting for their connection to open: (connection, file).
    waiting_for_connection: Vec<(String, PathBuf)>,
    tabs: Vec<Tab>,
    active: usize,
    queries_opened: usize,
    sidebar_visible: bool,
    palette: Option<Entity<CommandPalette>>,
    /// What had focus before the palette opened, so commands run there.
    focus_before_palette: Option<FocusHandle>,
    focus_handle: FocusHandle,
}

impl WorkspaceView {
    pub fn new(model: Entity<ProjectModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe_in(&model, window, |_, model, window, cx| {
            window.set_window_title(&model.read(cx).project.name);
            cx.notify();
        })
        .detach();
        let explorer = cx.new(|cx| Explorer::new(model.clone(), window, cx));
        cx.subscribe_in(
            &explorer,
            window,
            |this, _, event: &ExplorerEvent, window, cx| match event {
                ExplorerEvent::Connected(db) => {
                    let waiting: Vec<PathBuf> = this
                        .waiting_for_connection
                        .iter()
                        .filter(|(name, _)| *name == db.name.as_ref())
                        .map(|(_, path)| path.clone())
                        .collect();
                    this.waiting_for_connection
                        .retain(|(name, _)| *name != db.name.as_ref());
                    for path in waiting {
                        this.open_saved_query(path, window, cx);
                    }
                    // The first connection to open gets a query tab to start with.
                    if this.tabs.is_empty() {
                        this.open_query(
                            db.clone(),
                            None,
                            Some(query_tab::EXAMPLE_SQL.to_string()),
                            None,
                            window,
                            cx,
                        );
                    }
                    cx.notify();
                }
                ExplorerEvent::OpenSavedQuery(path) => {
                    this.open_saved_query(path.clone(), window, cx)
                }
                ExplorerEvent::NewQuery(db) => {
                    this.open_query(db.clone(), None, None, None, window, cx)
                }
                ExplorerEvent::NewConnection => this.open_connection_editor(None, window, cx),
                ExplorerEvent::EditConnection(id) => {
                    this.open_connection_editor(Some(id.clone()), window, cx)
                }
                ExplorerEvent::OpenTable { db, info, view } => {
                    this.open_table(db.clone(), info.clone(), *view, window, cx)
                }
                ExplorerEvent::OpenQuery { db, title, sql } => this.open_query(
                    db.clone(),
                    Some(title.clone()),
                    Some(sql.clone()),
                    None,
                    window,
                    cx,
                ),
                ExplorerEvent::OpenActivity(db) => this.open_activity(db.clone(), window, cx),
                ExplorerEvent::OpenDiagram { db, schema } => {
                    this.open_diagram(db.clone(), schema.clone(), window, cx)
                }
                ExplorerEvent::RedisConnected(db) => {
                    // The first connection to open gets a console to start with.
                    if this.tabs.is_empty() {
                        this.open_redis_console(db.clone(), db.session.default_db(), window, cx);
                    }
                    cx.notify();
                }
                ExplorerEvent::OpenRedisKey { db, database, key } => {
                    this.open_redis_key(db.clone(), *database, key.clone(), window, cx)
                }
                ExplorerEvent::OpenRedisConsole { db, database } => {
                    this.open_redis_console(db.clone(), *database, window, cx)
                }
            },
        )
        .detach();

        // The window's close button goes through the same check as Close Window.
        let this = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            let Some(view) = this.upgrade() else {
                return true;
            };
            if !view
                .read(cx)
                .tabs
                .iter()
                .any(|tab| matches!(tab, Tab::Query(q) if q.read(cx).in_transaction()))
            {
                return true;
            }
            window.defer(cx, move |window, cx| {
                view.update(cx, |view, cx| view.request_close_window(window, cx));
            });
            false
        });

        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        Self {
            model,
            explorer,
            waiting_for_connection: Vec::new(),
            tabs: Vec::new(),
            active: 0,
            queries_opened: 0,
            sidebar_visible: true,
            palette: None,
            focus_before_palette: None,
            focus_handle,
        }
    }

    /// Whether closing the window now would lose work: a query with unsaved
    /// edits or an open transaction, or a table with pending row changes.
    pub fn has_unsaved_changes(&self, cx: &App) -> bool {
        self.tabs.iter().any(|tab| tab.needs_attention(cx))
    }

    fn open_query(
        &mut self,
        db: Db,
        title: Option<SharedString>,
        sql: Option<String>,
        file: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = title.unwrap_or_else(|| {
            self.queries_opened += 1;
            format!("Query {}", self.queries_opened).into()
        });
        let sql = sql.unwrap_or_else(|| "select 1;\n".into());
        let queries_dir = self.model.read(cx).project.queries_dir();
        let tab = cx.new(|cx| QueryTab::new(db, title, &sql, file, queries_dir, window, cx));
        let explorer = self.explorer.clone();
        cx.subscribe(&tab, move |_, _, QueryRan(entry), cx| {
            explorer.update(cx, |explorer, cx| explorer.record(entry.clone(), cx));
        })
        .detach();
        cx.subscribe(&tab, |this, _, QuerySaved(path), cx| {
            let relative = this.relative_query(path, cx);
            this.explorer.update(cx, |explorer, cx| {
                explorer.refresh_queries(cx);
                explorer.reveal_query(relative, cx);
            });
            cx.notify();
        })
        .detach();
        self.tabs.push(Tab::Query(tab));
        self.activate(self.tabs.len() - 1, window, cx);
    }

    fn open_new_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let db = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.db(cx))
            .or_else(|| self.explorer.read(cx).current_db());
        if let Some(db) = db {
            self.open_query(db, None, None, None, window, cx);
        }
    }

    /// `queries/reports/monthly.sql` → `reports/monthly.sql`.
    fn relative_query(&self, path: &std::path::Path, cx: &App) -> Option<PathBuf> {
        let root = self.model.read(cx).project.queries_dir();
        path.strip_prefix(root).ok().map(PathBuf::from)
    }

    /// Opens a saved query on the connection named in its first line (opening
    /// that connection first if needed), or on the current one.
    fn open_saved_query(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let existing = self.tabs.iter().position(|tab| match tab {
            Tab::Query(query) => query.read(cx).file() == Some(path.as_path()),
            _ => false,
        });
        if let Some(index) = existing {
            self.activate(index, window, cx);
            return;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("kuery: could not read {}: {error}", path.display());
                return;
            }
        };
        let connection = query_connection(&text).map(str::to_owned);
        let body = match &connection {
            Some(_) => text
                .split_once('\n')
                .map(|(_, rest)| rest.to_string())
                .unwrap_or_default(),
            None => text,
        };
        let db = match &connection {
            Some(name) => match self.explorer.read(cx).db_by_name(name) {
                Some(db) => Some(db),
                None => {
                    let exists = self
                        .explorer
                        .update(cx, |explorer, cx| explorer.ensure_connected(name, cx));
                    if exists {
                        self.waiting_for_connection.push((name.to_string(), path));
                        return;
                    }
                    self.explorer.read(cx).current_db()
                }
            },
            None => self.explorer.read(cx).current_db(),
        };
        if let Some(db) = db {
            self.open_query(db, None, Some(body), Some(path), window, cx);
        }
    }

    fn open_connection_editor(
        &mut self,
        id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        ConnectionEditor::open(self.model.clone(), id, window, cx);
    }

    fn new_connection(&mut self, _: &NewConnection, window: &mut Window, cx: &mut Context<Self>) {
        self.open_connection_editor(None, window, cx);
    }

    fn open_table(
        &mut self,
        db: Db,
        info: TableInfo,
        view: TableView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = self.tabs.iter().position(|tab| match tab {
            Tab::Table(table) => {
                let table = table.read(cx);
                table.db().name == db.name && table.info().table == info.table
            }
            _ => false,
        });
        let index = match existing {
            Some(index) => {
                if let Some(Tab::Table(table)) = self.tabs.get(index) {
                    table.update(cx, |table, cx| table.set_view(view, cx));
                }
                index
            }
            None => {
                let tab = cx.new(|cx| TableTab::new(db, info, view, window, cx));
                self.tabs.push(Tab::Table(tab));
                self.tabs.len() - 1
            }
        };
        self.activate(index, window, cx);
    }

    /// Opens a schema's diagram, or shows the tab it is already open in.
    fn open_diagram(
        &mut self,
        db: Db,
        schema: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = self.tabs.iter().position(|tab| match tab {
            Tab::Diagram(tab) => {
                let tab = tab.read(cx);
                tab.db().name == db.name && tab.schema() == schema
            }
            _ => false,
        });
        let index = existing.unwrap_or_else(|| {
            let project = self.model.read(cx).project.root.clone();
            let tab = cx.new(|cx| DiagramTab::new(db.clone(), schema, project, cx));
            cx.subscribe_in(
                &tab,
                window,
                move |this, _, event: &DiagramEvent, window, cx| match event {
                    DiagramEvent::OpenTable(info) => {
                        this.open_table(db.clone(), info.clone(), TableView::Data, window, cx)
                    }
                },
            )
            .detach();
            self.tabs.push(Tab::Diagram(tab));
            self.tabs.len() - 1
        });
        self.activate(index, window, cx);
    }

    /// Opens a connection's server activity, or shows the tab it is already
    /// open in.
    fn open_activity(&mut self, db: Db, window: &mut Window, cx: &mut Context<Self>) {
        let existing = self.tabs.iter().position(|tab| match tab {
            Tab::Activity(tab) => tab.read(cx).db().name == db.name,
            _ => false,
        });
        let index = existing.unwrap_or_else(|| {
            let project = self.model.read(cx).project.root.clone();
            let tab = cx.new(|cx| ActivityTab::new(db.clone(), project, cx));
            cx.subscribe_in(
                &tab,
                window,
                move |this, _, event: &ActivityEvent, window, cx| match event {
                    ActivityEvent::OpenQuery(sql) => this.open_query(
                        db.clone(),
                        Some("Session query".into()),
                        Some(sql.clone()),
                        None,
                        window,
                        cx,
                    ),
                },
            )
            .detach();
            self.tabs.push(Tab::Activity(tab));
            self.tabs.len() - 1
        });
        self.activate(index, window, cx);
    }

    /// The palette's Show Server Activity, for the active connection.
    fn show_server_activity(
        &mut self,
        _: &ShowServerActivity,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(db) = self.active_db(cx).filter(|db| db.session.has_activity()) {
            self.open_activity(db, window, cx);
        }
    }

    /// The palette's Open Schema Diagram: the schema of the table in view,
    /// else `public`, on the active connection.
    fn open_schema_diagram(
        &mut self,
        _: &OpenSchemaDiagram,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(db) = self.active_db(cx) else {
            return;
        };
        let schema = match self.tabs.get(self.active) {
            Some(Tab::Table(table)) => table.read(cx).info().table.schema.clone(),
            Some(Tab::Diagram(diagram)) => diagram.read(cx).schema().to_string(),
            _ => "public".to_string(),
        };
        self.open_diagram(db, schema, window, cx);
    }

    /// Opens a key in a tab, or shows the tab it is already open in.
    fn open_redis_key(
        &mut self,
        db: RedisDb,
        database: i64,
        key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = self.tabs.iter().position(|tab| match tab {
            Tab::RedisKey(tab) => {
                let tab = tab.read(cx);
                tab.db().name == db.name && tab.database() == database && tab.key() == key
            }
            _ => false,
        });
        let index = existing.unwrap_or_else(|| {
            let project = self.model.read(cx).project.root.clone();
            let tab = cx.new(|cx| KeyTab::new(db, database, key, project, window, cx));
            self.tabs.push(Tab::RedisKey(tab));
            self.tabs.len() - 1
        });
        self.activate(index, window, cx);
    }

    fn open_redis_console(
        &mut self,
        db: RedisDb,
        database: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let project = self.model.read(cx).project.root.clone();
        let tab = cx.new(|cx| ConsoleTab::new(db, database, project, window, cx));
        self.tabs.push(Tab::RedisConsole(tab));
        let index = self.tabs.len() - 1;
        self.activate(index, window, cx);
    }

    fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.active = index;
        match self.tabs.get(index) {
            Some(tab) => tab.focus(window, cx),
            None => window.focus(&self.focus_handle, cx),
        }
        let selection = match self.tabs.get(index) {
            Some(Tab::Table(table)) => {
                let table = table.read(cx);
                Some((table.db().name.clone(), table.info().table.clone()))
            }
            _ => None,
        };
        let query = match self.tabs.get(index) {
            Some(Tab::Query(query)) => query.read(cx).file().map(PathBuf::from),
            _ => None,
        };
        let query = query.and_then(|path| self.relative_query(&path, cx));
        self.explorer
            .update(cx, |explorer, cx| explorer.reveal_query(query, cx));
        self.explorer.update(cx, |explorer, cx| match &selection {
            Some((connection, table)) => explorer.reveal_table(connection, Some(table), cx),
            None => explorer.reveal_table("", None, cx),
        });
        cx.notify();
    }

    fn close(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        match self.tabs.get(index) {
            // A table with unsaved changes is not closed by accident.
            Some(Tab::Table(table)) if table.read(cx).has_pending() => {
                self.activate(index, window, cx);
            }
            Some(Tab::RedisKey(key)) if key.read(cx).has_pending() => {
                self.activate(index, window, cx);
            }
            Some(Tab::Query(query)) if query.read(cx).in_transaction() => {
                let query = query.clone();
                self.activate(index, window, cx);
                self.confirm_close_transaction(query, window, cx);
            }
            Some(Tab::Query(query)) if query.read(cx).is_dirty() => {
                let query = query.clone();
                self.activate(index, window, cx);
                self.confirm_close_query(query, window, cx);
            }
            Some(_) => self.remove_tab(index, window, cx),
            None => {}
        }
    }

    /// Asks whether to commit a query tab's open transaction before closing
    /// it; then closes it as usual (which may ask to save the SQL too).
    fn confirm_close_transaction(
        &mut self,
        query: Entity<QueryTab>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = query.read(cx).title();
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Commit the open transaction in \u{201c}{name}\u{201d}?"),
            Some("Closing the tab ends it. The changes are only kept if you commit them."),
            &["Commit", "Roll Back", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let commit = match answer.await {
                Ok(0) => true,
                Ok(1) => false,
                _ => return,
            };
            let ended = query.update(cx, |query, cx| query.end_transaction(commit, cx));
            // A failed commit keeps the tab open, with the error in its Output.
            if ended.await.is_err() {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                if let Some(index) = this
                    .tabs
                    .iter()
                    .position(|tab| matches!(tab, Tab::Query(q) if *q == query))
                {
                    this.close(index, window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Closes the window, asking first if tabs have open transactions: closing
    /// drops their connections, which rolls them back.
    fn request_close_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open = self
            .tabs
            .iter()
            .filter(|tab| matches!(tab, Tab::Query(q) if q.read(cx).in_transaction()))
            .count();
        if open == 0 {
            window.remove_window();
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            "Close the window and roll back?",
            Some(&format!(
                "{} not committed. Closing the window rolls {} back.",
                if open == 1 {
                    "A tab has a transaction that is".to_string()
                } else {
                    format!("{open} tabs have transactions that are")
                },
                if open == 1 { "it" } else { "them" }
            )),
            &["Roll Back and Close", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |_, cx| {
            if let Ok(0) = answer.await {
                cx.update(|window, _| window.remove_window()).ok();
            }
        })
        .detach();
    }

    fn close_window(&mut self, _: &CloseWindow, window: &mut Window, cx: &mut Context<Self>) {
        self.request_close_window(window, cx);
    }

    /// Asks whether to save a query with unsaved changes before closing it.
    fn confirm_close_query(
        &mut self,
        query: Entity<QueryTab>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = query.read(cx).title();
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Save changes to \"{name}\"?"),
            Some("Your changes will be lost if you don't save them."),
            &["Save", "Don't Save", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Ok(answer) = answer.await else { return };
            this.update_in(cx, |this, window, cx| {
                let Some(index) = this
                    .tabs
                    .iter()
                    .position(|tab| matches!(tab, Tab::Query(q) if *q == query))
                else {
                    return;
                };
                match answer {
                    0 => {
                        // Save; a query without a file asks where, and stays open.
                        let has_file = query.read(cx).file().is_some();
                        query.update(cx, |query, cx| query.request_save(window, cx));
                        if has_file {
                            this.remove_tab(index, window, cx);
                        }
                    }
                    1 => this.remove_tab(index, window, cx),
                    _ => {}
                }
            })
            .ok();
        })
        .detach();
    }

    /// Closes the tabs `close` picks. Tabs that need a question are left for
    /// last, and only the first of them asks (one prompt at a time).
    fn close_tabs(
        &mut self,
        close: impl Fn(usize) -> bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets: Vec<EntityId> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(index, _)| close(*index))
            .map(|(_, tab)| tab.id())
            .collect();
        let mut waiting = None;
        for id in targets {
            let Some(index) = self.tabs.iter().position(|tab| tab.id() == id) else {
                continue;
            };
            if self.tabs[index].needs_attention(cx) {
                waiting.get_or_insert(id);
            } else {
                self.remove_tab(index, window, cx);
            }
        }
        if let Some(index) = waiting.and_then(|id| self.tabs.iter().position(|t| t.id() == id)) {
            self.close(index, window, cx);
        }
    }

    fn remove_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index < self.tabs.len() {
            self.tabs.remove(index);
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        } else if self.active > index {
            self.active -= 1;
        }
        let active = self.active;
        self.activate(active, window, cx);
    }

    // Commands -------------------------------------------------------------

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            // Nothing left to close in the window: close the window itself.
            window.dispatch_action(Box::new(CloseWindow), cx);
        } else {
            self.close(self.active, window, cx);
        }
    }

    fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tabs.is_empty() {
            self.activate((self.active + 1) % self.tabs.len(), window, cx);
        }
    }

    fn previous_tab(&mut self, _: &PreviousTab, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tabs.is_empty() {
            self.activate(
                (self.active + self.tabs.len() - 1) % self.tabs.len(),
                window,
                cx,
            );
        }
    }

    fn new_query(&mut self, _: &NewQuery, window: &mut Window, cx: &mut Context<Self>) {
        self.open_new_query(window, cx);
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_visible = !self.sidebar_visible;
        cx.notify();
    }

    fn toggle_palette(
        &mut self,
        _: &OpenCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.palette.is_some() {
            self.close_palette(window, cx);
            return;
        }
        self.focus_before_palette = window.focused(cx);
        let tables = self.explorer.read(cx).open_tables();
        let projects = recent_projects_except(&self.model.read(cx).project.root);
        let palette = cx.new(|cx| CommandPalette::new(tables, projects, window, cx));
        cx.subscribe_in(
            &palette,
            window,
            |this, _, event: &PaletteEvent, window, cx| match event {
                PaletteEvent::Dismissed => this.close_palette(window, cx),
                PaletteEvent::Run(command) => {
                    this.close_palette(window, cx);
                    // Run it where the user was, as if its shortcut had been pressed.
                    let action = (command.action)();
                    window.defer(cx, move |window, cx| window.dispatch_action(action, cx));
                }
                PaletteEvent::OpenTable { db, info } => {
                    this.close_palette(window, cx);
                    this.open_table(db.clone(), info.clone(), TableView::Data, window, cx);
                }
                PaletteEvent::OpenProject(path) => {
                    this.close_palette(window, cx);
                    crate::app::request_open(Some(path.clone()), Some(window.window_handle()), cx);
                }
            },
        )
        .detach();
        self.palette = Some(palette);
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        let target = self
            .focus_before_palette
            .take()
            .unwrap_or_else(|| self.focus_handle.clone());
        window.focus(&target, cx);
        cx.notify();
    }

    /// The connection of the active tab, or the sidebar's.
    fn active_db(&self, cx: &App) -> Option<Db> {
        self.tabs
            .get(self.active)
            .and_then(|tab| tab.db(cx))
            .or_else(|| self.explorer.read(cx).current_db())
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let project = self.model.read(cx).project.clone();
        let project_name: SharedString = project.name.clone().into();
        let workspace_name: Option<SharedString> = project.workspace.clone().map(Into::into);
        let mac = cfg!(target_os = "macos");
        let mono = cx.theme().mono_font_family.clone();
        let root = project.root.clone();
        let initials: String = workspace_name
            .as_ref()
            .unwrap_or(&project_name)
            .split(|c: char| c.is_whitespace() || c == '-' || c == '_')
            .filter_map(|word| word.chars().next())
            .take(2)
            .collect::<String>()
            .to_uppercase();
        let panel_toggle = matches!(self.tabs.get(self.active), Some(Tab::Query(_)));

        TitleBar::new().child(
            h_flex()
                .w_full()
                .h_full()
                .pr_3()
                .text_size(px(12.5))
                .child(
                    h_flex().flex_1().min_w_0().child(
                        Button::new("project-switcher")
                            .ghost()
                            .compact()
                            .small()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .size(px(20.))
                                            .rounded(px(5.))
                                            .bg(color(theme::ACTIVE))
                                            .border_1()
                                            .border_color(color(0x36363b))
                                            .text_color(color(0xc7c7cc))
                                            .text_size(px(9.))
                                            .font_weight(FontWeight::BOLD)
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(initials),
                                    )
                                    .child(
                                        div()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(project_name.clone()),
                                    )
                                    .when_some(workspace_name.clone(), |row, workspace| {
                                        row.child(
                                            div()
                                                .text_color(color(theme::TEXT_TERTIARY))
                                                .child("/"),
                                        )
                                        .child(
                                            div()
                                                .text_color(color(theme::TEXT_SECONDARY))
                                                .child(workspace),
                                        )
                                    })
                                    .child(
                                        Icon::new(IconName::ChevronDown)
                                            .size(px(12.))
                                            .text_color(color(theme::TEXT_TERTIARY)),
                                    ),
                            )
                            .dropdown_menu(move |menu, _, _| {
                                // Read when the menu opens, not on every render.
                                let recent_projects = recent_projects_except(&root);
                                let mut menu = menu.label("Recent Projects");
                                if recent_projects.is_empty() {
                                    menu = menu.item(
                                        PopupMenuItem::new("No other projects").disabled(true),
                                    );
                                }
                                for project in recent_projects.iter() {
                                    let path = project.path.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(project.name.clone()).on_click(
                                            move |_, window, cx| {
                                                crate::app::request_open(
                                                    Some(path.clone()),
                                                    Some(window.window_handle()),
                                                    cx,
                                                )
                                            },
                                        ),
                                    );
                                }
                                menu.separator()
                                    .menu("Open Folder…", Box::new(OpenProject))
                                    .menu("Clear Recent Projects", Box::new(ClearRecentProjects))
                            }),
                    ),
                )
                .child(
                    h_flex()
                        .id("open-palette")
                        .w(px(420.))
                        .h(px(28.))
                        .flex_shrink_0()
                        .px(px(10.))
                        .gap_2()
                        .rounded(px(7.))
                        .bg(color(theme::CONTROL))
                        .border_1()
                        .border_color(color(theme::CONTROL_BORDER))
                        .text_color(color(theme::TEXT_TERTIARY))
                        .hover(|s| s.border_color(color(0x3a3a40)))
                        .child(Icon::new(IconName::Search).size(px(13.)))
                        .child(div().flex_1().child("Search tables, queries and commands"))
                        .child(chrome::kbd(if mac { "⌘K" } else { "Ctrl+K" }, mono))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_palette(&OpenCommandPalette, window, cx)
                        })),
                )
                .child(
                    h_flex()
                        .flex_1()
                        .justify_end()
                        .gap(px(2.))
                        .child(
                            chrome::icon_button(
                                "title-toggle-sidebar",
                                IconName::PanelLeft,
                                if mac {
                                    "Toggle Sidebar  ⌘B"
                                } else {
                                    "Toggle Sidebar  Ctrl+B"
                                },
                                true,
                            )
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.toggle_sidebar(&ToggleSidebar, window, cx)
                                },
                            )),
                        )
                        .child(
                            chrome::icon_button(
                                "title-toggle-panel",
                                IconName::PanelBottom,
                                if mac {
                                    "Toggle Results Panel  ⌘J"
                                } else {
                                    "Toggle Results Panel  Ctrl+J"
                                },
                                panel_toggle,
                            )
                            .when(panel_toggle, |b| {
                                b.on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(Tab::Query(query)) = this.tabs.get(this.active) {
                                        query.update(cx, |query, cx| query.toggle_panel(cx));
                                    }
                                }))
                            }),
                        ),
                ),
        )
    }

    /// The narrow column of view icons left of the sidebar. Clicking the
    /// active one hides the sidebar.
    fn render_activity_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.explorer.read(cx).mode();
        let visible = self.sidebar_visible;
        let view = |id: &'static str,
                    icon: AppIcon,
                    tooltip: &'static str,
                    mode: ExplorerMode,
                    cx: &mut Context<Self>| {
            let active = visible && current == mode;
            div()
                .id(id)
                .size(px(36.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(8.))
                .when(active, |b| {
                    b.bg(color(theme::ACTIVE)).text_color(color(theme::TEXT))
                })
                .when(!active, |b| {
                    b.text_color(color(theme::TEXT_TERTIARY))
                        .hover(|s| s.text_color(color(theme::TEXT)))
                })
                .child(Icon::new(icon).size(px(18.)))
                .tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(tooltip).build(window, cx)
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    let showing = this.sidebar_visible && this.explorer.read(cx).mode() == mode;
                    if showing {
                        this.sidebar_visible = false;
                    } else {
                        this.sidebar_visible = true;
                        this.explorer
                            .update(cx, |explorer, cx| explorer.set_mode(mode, cx));
                    }
                    cx.notify();
                }))
        };
        let action = |id: &'static str,
                      icon: AppIcon,
                      tooltip: &'static str,
                      action: fn() -> Box<dyn gpui_kit::Action>| {
            div()
                .id(id)
                .size(px(36.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(8.))
                .text_color(color(theme::TEXT_TERTIARY))
                .hover(|s| s.text_color(color(theme::TEXT)))
                .child(Icon::new(icon).size(px(18.)))
                .tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(tooltip).build(window, cx)
                })
                .on_click(move |_, window, cx| window.dispatch_action(action(), cx))
        };
        v_flex()
            .w(px(48.))
            .h_full()
            .flex_shrink_0()
            .items_center()
            .gap(px(4.))
            .py_2()
            .bg(color(theme::TOOLBAR))
            .border_r_1()
            .border_color(color(theme::HAIRLINE))
            .child(view(
                "rail-explorer",
                AppIcon::Database,
                "Explorer",
                ExplorerMode::Explorer,
                cx,
            ))
            .child(view(
                "rail-queries",
                AppIcon::Queries,
                "Saved Queries",
                ExplorerMode::Queries,
                cx,
            ))
            .child(view(
                "rail-history",
                AppIcon::History,
                "History",
                ExplorerMode::History,
                cx,
            ))
            .child(div().flex_1())
            .child(action(
                "rail-audit",
                AppIcon::Audit,
                "Open Audit Log",
                || Box::new(OpenAuditLog),
            ))
            .child(action(
                "rail-keymap",
                AppIcon::Keyboard,
                "Keyboard Shortcuts",
                || Box::new(OpenKeymapFile),
            ))
    }

    /// The bottom bar: the active connection and mode on the left, the last
    /// run and the cursor on the right.
    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let redis = self.tabs.get(self.active).and_then(|tab| tab.redis(cx));
        let db = if redis.is_some() {
            None
        } else {
            self.active_db(cx)
        };
        let mut left = h_flex().gap_4().min_w_0();
        let mut right = h_flex().gap_4().flex_shrink_0();
        match &db {
            Some(db) => {
                left = left
                    .child(
                        h_flex()
                            .gap(px(6.))
                            .text_color(color(if db.environment == Environment::Production {
                                0xffcf7f
                            } else {
                                0xc7c7cc
                            }))
                            .child(chrome::dot(db.environment.color()))
                            .child(format!(
                                "{} · {}",
                                db.name,
                                db.environment.label().to_lowercase()
                            )),
                    )
                    .child(div().child(db.target.clone()));
                right = right.child(db.session.server_label());
            }
            None => match &redis {
                Some(redis) => {
                    left = left
                        .child(
                            h_flex()
                                .gap(px(6.))
                                .text_color(color(
                                    if redis.environment == Environment::Production {
                                        0xffcf7f
                                    } else {
                                        0xc7c7cc
                                    },
                                ))
                                .child(chrome::dot(redis.environment.color()))
                                .child(format!(
                                    "{} · {}",
                                    redis.name,
                                    redis.environment.label().to_lowercase()
                                )),
                        )
                        .child(div().child(redis.target.clone()));
                }
                None => left = left.child("No open connection"),
            },
        }
        let redis_version = redis
            .as_ref()
            .map(|redis| format!("Redis {}", redis.session.server().version));
        match self.tabs.get(self.active) {
            Some(Tab::Diagram(diagram)) => {
                let tab = diagram.read(cx);
                left = left.child(format!("Diagram of {}", tab.schema()));
                right = h_flex()
                    .gap_4()
                    .flex_shrink_0()
                    .children(tab.status())
                    .children(db.as_ref().map(|db| db.session.server_label()));
            }
            Some(Tab::Activity(activity)) => {
                let tab = activity.read(cx);
                left = left.child(format!("Server activity of {}", tab.db().name));
                right = h_flex()
                    .gap_4()
                    .flex_shrink_0()
                    .children(tab.status())
                    .children(db.as_ref().map(|db| db.session.server_label()));
            }
            Some(Tab::RedisKey(key)) => {
                let tab = key.read(cx);
                left = left.child(div().truncate().child(format!(
                    "db{} · {}",
                    tab.database(),
                    tab.key()
                )));
                right = h_flex()
                    .gap_4()
                    .flex_shrink_0()
                    .children(tab.status())
                    .children(redis_version);
            }
            Some(Tab::RedisConsole(console)) => {
                let tab = console.read(cx);
                let writing = tab.is_writing();
                left = left.child(
                    h_flex()
                        .gap(px(5.))
                        .child(
                            Icon::new(if writing {
                                AppIcon::Pencil
                            } else {
                                AppIcon::Lock
                            })
                            .size(px(11.)),
                        )
                        .child(if writing { "Write" } else { "Read-only" }),
                );
                if let Some(redis) = &redis
                    && redis.environment == Environment::Production
                    && redis.allows_writes()
                {
                    let console = console.clone();
                    left = left.child(
                        div()
                            .id("stop-allowing-redis-writes")
                            .text_color(color(0xc7c7cc))
                            .hover(|s| s.text_color(color(theme::TEXT)))
                            .child("Stop allowing writes")
                            .on_click(move |_, _, cx| {
                                console.update(cx, |console, cx| console.stop_allowing_writes(cx))
                            }),
                    );
                }
                right = h_flex()
                    .gap_4()
                    .flex_shrink_0()
                    .child(div().max_w(px(420.)).truncate().child(tab.status()))
                    .children(redis_version);
            }
            Some(Tab::Query(query)) => {
                let tab = query.read(cx);
                let writing = tab.is_writing();
                left = left.child(
                    h_flex()
                        .gap(px(5.))
                        .min_w_0()
                        .child(
                            Icon::new(if writing {
                                AppIcon::Pencil
                            } else {
                                AppIcon::Lock
                            })
                            .size(px(11.)),
                        )
                        .child(div().truncate().child(tab.mode_status())),
                );
                if let Some(db) = &db
                    && db.environment == Environment::Production
                    && db.allows(WriteKind::Transactional)
                {
                    let query = query.clone();
                    left = left.child(
                        div()
                            .id("stop-allowing-writes")
                            .text_color(color(0xc7c7cc))
                            .hover(|s| s.text_color(color(theme::TEXT)))
                            .child("Stop allowing writes")
                            .on_click(move |_, window, cx| {
                                query.update(cx, |query, cx| query.stop_allowing_writes(window, cx))
                            }),
                    );
                }
                let (run, tone) = tab.run_status(cx);
                right = h_flex()
                    .gap_4()
                    .flex_shrink_0()
                    .when(run != "Ready", |row| {
                        row.child(
                            div()
                                .max_w(px(420.))
                                .truncate()
                                .text_color(color(if tone == theme::RED {
                                    theme::RED
                                } else {
                                    theme::TEXT_TERTIARY
                                }))
                                .child(run),
                        )
                    })
                    .child(tab.cursor_status(cx))
                    .children(db.as_ref().map(|db| db.session.server_label()));
            }
            Some(Tab::Table(table)) => {
                let tab = table.read(cx);
                let name = &tab.info().table;
                left = left.child(format!("{}.{}", name.schema, name.name));
                if let Some(status) = tab.page_status() {
                    right = h_flex()
                        .gap_4()
                        .flex_shrink_0()
                        .child(status)
                        .children(db.as_ref().map(|db| db.session.server_label()));
                }
            }
            None => {}
        }
        h_flex()
            .h(px(26.))
            .flex_shrink_0()
            .px_3()
            .gap_4()
            .bg(color(theme::TOOLBAR))
            .border_t_1()
            .border_color(color(theme::HAIRLINE))
            .text_size(px(11.5))
            .text_color(color(theme::TEXT_TERTIARY))
            .child(left.flex_1())
            .child(right)
    }

    fn render_tab_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut strip = h_flex()
            .h(px(36.))
            .flex_shrink_0()
            .items_end()
            .gap(px(2.))
            .px_2()
            .bg(color(theme::TOOLBAR))
            .border_b_1()
            .border_color(color(theme::HAIRLINE));
        for (index, tab) in self.tabs.iter().enumerate() {
            let active = index == self.active;
            let (icon, title, pending) = match tab {
                Tab::Query(query) => {
                    let query = query.read(cx);
                    (
                        AppIcon::Code,
                        query.title(),
                        query.is_dirty() || query.in_transaction(),
                    )
                }
                Tab::Table(table) => {
                    let table = table.read(cx);
                    (
                        AppIcon::Table,
                        SharedString::from(table.info().table.name.clone()),
                        table.has_pending(),
                    )
                }
                Tab::RedisKey(key) => {
                    let key = key.read(cx);
                    (
                        AppIcon::Key,
                        SharedString::from(key.key().to_string()),
                        key.has_pending(),
                    )
                }
                Tab::RedisConsole(console) => (AppIcon::Code, console.read(cx).title(), false),
                Tab::Diagram(diagram) => (AppIcon::Link, diagram.read(cx).title(), false),
                Tab::Activity(activity) => (AppIcon::Activity, activity.read(cx).title(), false),
            };
            let tooltip: SharedString = match (tab.db(cx), tab.redis(cx)) {
                (Some(db), _) => format!("{title} · {}", db.name).into(),
                (None, Some(redis)) => format!("{title} · {}", redis.name).into(),
                (None, None) => title.clone(),
            };
            let group = SharedString::from(format!("tab-{index}"));
            strip = strip.child(
                h_flex()
                    .id(("tab", index))
                    .group(group.clone())
                    .h(px(30.))
                    .max_w(px(240.))
                    .pl(px(12.))
                    .pr(px(8.))
                    .gap(px(8.))
                    .rounded_t(px(7.))
                    .text_size(px(12.5))
                    .when(active, |t| {
                        t.bg(color(theme::GROUND))
                            .border_1()
                            .border_b_0()
                            .border_color(color(theme::HAIRLINE))
                            .mb(px(-1.))
                            .text_color(color(theme::TEXT))
                    })
                    .when(!active, |t| {
                        t.text_color(color(theme::TEXT_TERTIARY))
                            .hover(|s| s.text_color(color(theme::TEXT)))
                    })
                    .child(Icon::new(icon).size(px(13.)).text_color(color(if active {
                        0x4da3ff
                    } else {
                        theme::TEXT_TERTIARY
                    })))
                    .child(div().truncate().child(title))
                    .when(pending, |t| t.child(chrome::dot(theme::AMBER)))
                    .child(
                        div()
                            .id(("close-tab", index))
                            .size(px(18.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .text_color(color(theme::TEXT_TERTIARY))
                            .when(!active, |b| {
                                b.invisible().group_hover(group.clone(), |s| s.visible())
                            })
                            .hover(|s| s.bg(color(theme::ACTIVE)).text_color(color(theme::TEXT)))
                            .child(Icon::new(IconName::Close).size(px(11.)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close(index, window, cx);
                            })),
                    )
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(tooltip.clone())
                            .build(window, cx)
                    })
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.activate(index, window, cx)),
                    )
                    .context_menu({
                        let this = cx.entity().downgrade();
                        let count = self.tabs.len();
                        let file = match tab {
                            Tab::Query(query) => query.read(cx).file().map(PathBuf::from),
                            _ => None,
                        };
                        let table = match tab {
                            Tab::Table(table) => {
                                let tab = table.read(cx);
                                let table = &tab.info().table;
                                Some(
                                    tab.db()
                                        .session
                                        .dialect()
                                        .quote_qualified(&table.schema, &table.name),
                                )
                            }
                            _ => None,
                        };
                        move |menu, _, _| {
                            let mut menu = menu
                                .item(
                                    item("Close", &this, move |this, window, cx| {
                                        this.close(index, window, cx)
                                    })
                                    .icon(IconName::Close),
                                )
                                .item(
                                    item("Close Others", &this, move |this, window, cx| {
                                        this.close_tabs(|i| i != index, window, cx)
                                    })
                                    .disabled(count < 2),
                                )
                                .item(
                                    item(
                                        "Close Tabs to the Right",
                                        &this,
                                        move |this, window, cx| {
                                            this.close_tabs(|i| i > index, window, cx)
                                        },
                                    )
                                    .disabled(index + 1 >= count),
                                )
                                .item(item("Close All", &this, |this, window, cx| {
                                    this.close_tabs(|_| true, window, cx)
                                }));
                            if let Some(file) = &file {
                                menu = menu
                                    .separator()
                                    .item(reveal(file.clone()))
                                    .item(copy("Copy Path", file.display().to_string()));
                            }
                            if let Some(table) = &table {
                                menu = menu
                                    .separator()
                                    .item(copy("Copy Table Name", table.clone()));
                            }
                            menu
                        }
                    }),
            );
        }
        strip.child(
            div()
                .id("new-tab")
                .h_full()
                .px_3()
                .flex()
                .items_center()
                .text_color(color(theme::TEXT_SECONDARY))
                .hover(|s| s.text_color(color(theme::TEXT)))
                .child("+")
                .on_click(cx.listener(|this, _, window, cx| this.open_new_query(window, cx))),
        )
    }

    fn render_content(&self) -> AnyElement {
        match self.tabs.get(self.active) {
            Some(Tab::Query(query)) => query.clone().into_any_element(),
            Some(Tab::Table(table)) => table.clone().into_any_element(),
            Some(Tab::RedisKey(key)) => key.clone().into_any_element(),
            Some(Tab::RedisConsole(console)) => console.clone().into_any_element(),
            Some(Tab::Diagram(diagram)) => diagram.clone().into_any_element(),
            Some(Tab::Activity(activity)) => activity.clone().into_any_element(),
            None => placeholder(
                "Open a table or saved query from the sidebar, or add a connection with +",
            ),
        }
    }
}

/// Recent projects other than the one in `root`, newest first.
fn recent_projects_except(root: &std::path::Path) -> Vec<RecentProject> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    recent::available()
        .into_iter()
        .filter(|project| project.path != root)
        .collect()
}

impl Focusable for WorkspaceView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Keep focus inside the workspace (closing a tab drops the focused
        // element), or window-wide shortcuts would have nowhere to go.
        if window.focused(cx).is_none() && self.palette.is_none() {
            let handle = self.focus_handle.clone();
            window.defer(cx, move |window, cx| window.focus(&handle, cx));
        }

        v_flex()
            .relative()
            .size_full()
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::previous_tab))
            .on_action(cx.listener(Self::new_query))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_palette))
            .on_action(cx.listener(Self::new_connection))
            .on_action(cx.listener(Self::open_schema_diagram))
            .on_action(cx.listener(Self::show_server_activity))
            .on_action(cx.listener(Self::close_window))
            .bg(color(theme::GROUND))
            .text_color(color(theme::TEXT))
            .text_size(px(13.))
            .child(self.render_title_bar(cx))
            // Production is marked by a thin line under the title bar.
            .when(
                match self.tabs.get(self.active).and_then(|tab| tab.redis(cx)) {
                    Some(redis) => redis.environment == Environment::Production,
                    None => self
                        .active_db(cx)
                        .is_some_and(|db| db.environment == Environment::Production),
                },
                |root| root.child(div().h(px(2.)).flex_shrink_0().bg(color(theme::AMBER))),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(self.render_activity_rail(cx))
                    .when(self.sidebar_visible, |row| row.child(self.explorer.clone()))
                    .child(
                        v_flex()
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .child(self.render_tab_strip(cx))
                            .child(div().flex_1().min_h_0().child(self.render_content())),
                    ),
            )
            .child(self.render_status_bar(cx))
            .when_some(self.palette.clone(), |root, palette| {
                root.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .justify_center()
                        .pt(px(72.))
                        .bg(gpui_kit::black().opacity(0.25))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| this.close_palette(window, cx)),
                        )
                        .child(palette),
                )
            })
    }
}
