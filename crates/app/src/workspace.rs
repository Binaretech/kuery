//! Project window: custom title bar, the sidebar on the left, and query
//! and table tabs. Query and table tabs belong to the connection
//! they were opened on.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::base::{HandleEdge, resize_handle};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::resizable::resize_handle_appearance;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, TitleBar, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Axis, Context, DragMoveEvent, Empty, Entity, EntityId,
    FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement, MouseButton,
    MouseDownEvent, ParentElement as _, Pixels, PromptLevel, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, Window, div, px,
};
use kuery_core::catalog::{TableInfo, TableRef};
use kuery_core::document::catalog::ContainerKind;
use kuery_core::document::value::CollectionRef;

use crate::activity_tab::{ActivityEvent, ActivityTab};
use crate::assets::AppIcon;
use crate::audit::OpenAuditLog;
use crate::chrome;
use crate::collection_tab::CollectionTab;
use crate::commands::{
    ClearRecentProjects, CloseTab, FindInSidebar, NewConnection, NewQuery, OpenCommandPalette,
    OpenDataFile, OpenKeymapFile, OpenProject, OpenSchemaDiagram, ShowServerActivity,
    ToggleSidebar,
};
use crate::connection::{ConnMeta, Db, DocDb, Environment, RedisDb, WriteKind, status_tone};
use crate::connection_editor::ConnectionEditor;
use crate::context_menus::{copy, item, reveal};
use crate::data_file_tabs::DataFileTabs;
use crate::diagram_tab::{DiagramEvent, DiagramTab};
use crate::document_console::DocumentConsole;
use crate::explorer::{Explorer, ExplorerEvent, Mode as ExplorerMode};
use crate::file_query_tab::FileQueryTab;
use crate::file_tab::FileTab;
use crate::menus::CloseWindow;
use crate::palette::{CommandPalette, PaletteEvent};
use crate::project::{ProjectModel, query_connection};
use crate::query_tab::{self, QueryRan, QuerySaved, QueryTab};
use crate::recent::{self, RecentProject};
use crate::redis_console::ConsoleTab;
use crate::redis_key_tab::KeyTab;
use crate::session::{self, TabState, WorkspaceState};
use crate::settings;
use crate::table_tab::{TableTab, TableView};
use crate::theme::{self, color};

pub(crate) enum Tab {
    Query(Entity<QueryTab>),
    Table(Entity<TableTab>),
    RedisKey(Entity<KeyTab>),
    RedisConsole(Entity<ConsoleTab>),
    Collection(Entity<CollectionTab>),
    DocumentConsole(Entity<DocumentConsole>),
    Diagram(Entity<DiagramTab>),
    Activity(Entity<ActivityTab>),
    File(Entity<FileTab>),
    FileQuery(Entity<FileQueryTab>),
}

impl Tab {
    fn id(&self) -> EntityId {
        match self {
            Tab::Query(query) => query.entity_id(),
            Tab::Table(table) => table.entity_id(),
            Tab::RedisKey(key) => key.entity_id(),
            Tab::RedisConsole(console) => console.entity_id(),
            Tab::Collection(collection) => collection.entity_id(),
            Tab::DocumentConsole(console) => console.entity_id(),
            Tab::Diagram(diagram) => diagram.entity_id(),
            Tab::Activity(activity) => activity.entity_id(),
            Tab::File(file) => file.entity_id(),
            Tab::FileQuery(query) => query.entity_id(),
        }
    }

    /// What the chrome shows about the tab's connection, whichever its
    /// family. File tabs have none.
    fn connection(&self, cx: &App) -> Option<ConnMeta> {
        match self {
            Tab::Query(query) => Some(query.read(cx).db().meta()),
            Tab::Table(table) => Some(table.read(cx).db().meta()),
            Tab::Diagram(diagram) => Some(diagram.read(cx).db().meta()),
            Tab::Activity(activity) => Some(activity.read(cx).db().meta()),
            Tab::RedisKey(key) => Some(key.read(cx).db().meta()),
            Tab::RedisConsole(console) => Some(console.read(cx).db().meta()),
            Tab::Collection(collection) => Some(collection.read(cx).db().meta()),
            Tab::DocumentConsole(console) => Some(console.read(cx).db().meta()),
            Tab::File(_) | Tab::FileQuery(_) => None,
        }
    }

    /// Whether closing it needs a question first (unsaved SQL, an open
    /// transaction, pending row changes).
    fn needs_attention(&self, cx: &App) -> bool {
        match self {
            Tab::Query(query) => query.read(cx).is_dirty() || query.read(cx).in_transaction(),
            Tab::Table(table) => table.read(cx).has_pending(),
            Tab::RedisKey(key) => key.read(cx).has_pending(),
            Tab::Collection(collection) => collection.read(cx).has_pending(),
            Tab::RedisConsole(_)
            | Tab::DocumentConsole(_)
            | Tab::Diagram(_)
            | Tab::Activity(_)
            | Tab::File(_)
            | Tab::FileQuery(_) => false,
        }
    }

    /// Whether restarting to update loses work: the SQL comes back, but
    /// open transactions and staged row changes do not.
    fn loses_work_on_restart(&self, cx: &App) -> bool {
        match self {
            Tab::Query(query) => query.read(cx).in_transaction(),
            Tab::Table(table) => table.read(cx).has_pending(),
            Tab::RedisKey(key) => key.read(cx).has_pending(),
            Tab::Collection(collection) => collection.read(cx).has_pending(),
            Tab::RedisConsole(_)
            | Tab::DocumentConsole(_)
            | Tab::Diagram(_)
            | Tab::Activity(_)
            | Tab::File(_)
            | Tab::FileQuery(_) => false,
        }
    }

    /// What reopens the tab after a restart. Query files is not kept: it
    /// needs its files' previews first.
    fn session(&self, cx: &App) -> Option<TabState> {
        Some(match self {
            Tab::Query(query) => {
                let query = query.read(cx);
                TabState::Query {
                    connection: query.db().name.to_string(),
                    title: query.title().to_string(),
                    file: query.file().map(PathBuf::from),
                    text: query.text(cx),
                    saved: query.saved_text().to_string(),
                }
            }
            Tab::Table(table) => {
                let table = table.read(cx);
                let info = table.info();
                TabState::Table {
                    connection: table.db().name.to_string(),
                    schema: info.table.schema.clone(),
                    name: info.table.name.clone(),
                    table_kind: info.kind.into(),
                    view: table.view().into(),
                }
            }
            Tab::Diagram(diagram) => {
                let diagram = diagram.read(cx);
                TabState::Diagram {
                    connection: diagram.db().name.to_string(),
                    schema: diagram.schema().to_string(),
                }
            }
            Tab::Activity(activity) => TabState::Activity {
                connection: activity.read(cx).db().name.to_string(),
            },
            Tab::RedisKey(key) => {
                let key = key.read(cx);
                TabState::RedisKey {
                    connection: key.db().name.to_string(),
                    database: key.database(),
                    key: key.key().to_string(),
                }
            }
            Tab::RedisConsole(console) => {
                let console = console.read(cx);
                TabState::RedisConsole {
                    connection: console.db().name.to_string(),
                    database: console.database(),
                    text: console.text(cx),
                }
            }
            Tab::File(file) => TabState::File {
                path: file.read(cx).path().to_path_buf(),
            },
            Tab::Collection(collection) => {
                let collection = collection.read(cx);
                let (filter, projection, sort) = collection.bar(cx);
                TabState::Collection {
                    connection: collection.db().name.to_string(),
                    path: collection.collection().path.clone(),
                    kind: collection.kind().into(),
                    filter,
                    projection,
                    sort,
                }
            }
            Tab::DocumentConsole(console) => {
                let console = console.read(cx);
                TabState::DocumentConsole {
                    connection: console.db().name.to_string(),
                    path: console.path().to_vec(),
                    text: console.text(cx),
                }
            }
            Tab::FileQuery(_) => return None,
        })
    }

    /// The SQL connection of a SQL tab.
    fn db(&self, cx: &App) -> Option<Db> {
        match self {
            Tab::Query(query) => Some(query.read(cx).db().clone()),
            Tab::Table(table) => Some(table.read(cx).db().clone()),
            Tab::Diagram(diagram) => Some(diagram.read(cx).db().clone()),
            Tab::Activity(activity) => Some(activity.read(cx).db().clone()),
            Tab::RedisKey(_)
            | Tab::RedisConsole(_)
            | Tab::Collection(_)
            | Tab::DocumentConsole(_)
            | Tab::File(_)
            | Tab::FileQuery(_) => None,
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
            Tab::Collection(collection) => {
                let handle = collection.read(cx).focus_handle(cx);
                window.focus(&handle, cx);
            }
            Tab::DocumentConsole(console) => {
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
            Tab::File(file) => window.focus(&file.read(cx).focus_handle(cx), cx),
            Tab::FileQuery(query) => window.focus(&query.read(cx).focus_handle(cx), cx),
        }
    }
}

/// How long the sidebar's width has to stay put before it is saved.
const SIDEBAR_SAVE_DELAY: Duration = Duration::from_millis(400);

/// What the sidebar's right edge carries while it is dragged.
#[derive(Clone)]
struct SidebarResize;

impl Render for SidebarResize {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

pub struct WorkspaceView {
    model: Entity<ProjectModel>,
    explorer: Entity<Explorer>,
    /// Saved queries waiting for their connection to open: (connection, file).
    waiting_for_connection: Vec<(String, PathBuf)>,
    /// Tabs from before a restart waiting for their connection to open,
    /// with their place among the saved tabs.
    restoring: Vec<(usize, TabState)>,
    /// The saved place of each tab restored so far, so the ones still coming
    /// back go where they were.
    restored_places: HashMap<EntityId, usize>,
    /// The saved place of the tab that was active, until it is back.
    restore_active: Option<usize>,
    /// The window reopened tabs from before a restart, so connecting does
    /// not add a starter tab.
    restored_session: bool,
    tabs: Vec<Tab>,
    active: usize,
    queries_opened: usize,
    sidebar_visible: bool,
    /// Kept while the sidebar is hidden, so showing it again restores it.
    sidebar_width: Pixels,
    /// The pending save of the width; a new width replaces it, so a drag is
    /// written once it pauses rather than on every pixel.
    sidebar_width_save: Option<Task<()>>,
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
                    this.restore_sql_tabs(db, window, cx);
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
                    if this.tabs.is_empty() && !this.restored_session {
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
                    this.restore_redis_tabs(db, window, cx);
                    // The first connection to open gets a console to start with.
                    if this.tabs.is_empty() && !this.restored_session {
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
                ExplorerEvent::DocumentConnected(db) => {
                    this.restore_document_tabs(db, window, cx);
                    // The first connection to open gets a console to start with.
                    if this.tabs.is_empty() && !this.restored_session {
                        this.open_document_console(db.clone(), db.home.clone(), window, cx);
                    }
                    cx.notify();
                }
                ExplorerEvent::OpenDocumentConsole { db, path } => {
                    this.open_document_console(db.clone(), path.clone(), window, cx)
                }
                ExplorerEvent::OpenCollection {
                    db,
                    collection,
                    kind,
                } => this.open_collection(db.clone(), collection.clone(), *kind, window, cx),
            },
        )
        .detach();

        // The window's close button goes through the same check as Close Window.
        let this = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            let Some(view) = this.upgrade() else {
                return true;
            };
            if !view.read(cx).has_unsaved_changes(cx) {
                view.update(cx, |view, _| view.flush_sidebar_width());
                return true;
            }
            window.defer(cx, move |window, cx| {
                view.update(cx, |view, cx| view.request_close_window(window, cx));
            });
            false
        });

        // Quitting drops the windows with any save still pending.
        cx.on_app_quit(|this, _| {
            this.flush_sidebar_width();
            async {}
        })
        .detach();

        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        Self {
            model,
            explorer,
            waiting_for_connection: Vec::new(),
            restoring: Vec::new(),
            restored_places: HashMap::new(),
            restore_active: None,
            restored_session: false,
            tabs: Vec::new(),
            active: 0,
            queries_opened: 0,
            sidebar_visible: true,
            sidebar_width: px(settings::sidebar_width()),
            sidebar_width_save: None,
            palette: None,
            focus_before_palette: None,
            focus_handle,
        }
    }

    /// Whether closing the window now would lose work: a query with unsaved
    /// edits or an open transaction, or a table or collection with staged
    /// changes.
    pub fn has_unsaved_changes(&self, cx: &App) -> bool {
        self.tabs.iter().any(|tab| tab.needs_attention(cx))
    }

    /// Whether restarting to update now would lose work: an open
    /// transaction or staged row changes. Unsaved SQL comes back.
    pub fn loses_work_on_restart(&self, cx: &App) -> bool {
        self.tabs.iter().any(|tab| tab.loses_work_on_restart(cx))
    }

    /// What reopens this window's tabs after a restart.
    pub fn session(&self, cx: &App) -> WorkspaceState {
        let open = self
            .tabs
            .iter()
            .map(|tab| {
                (
                    self.restored_places.get(&tab.id()).copied(),
                    tab.session(cx),
                )
            })
            .collect();
        let (tabs, active) = session::tabs_to_save(
            open,
            self.active,
            self.restoring.clone(),
            self.restore_active,
        );
        let explorer = self.explorer.read(cx);
        WorkspaceState {
            connections: explorer.open_connections(),
            active,
            tabs,
            queries_opened: self.queries_opened,
            sidebar_visible: self.sidebar_visible,
            sidebar: explorer.mode().into(),
            expanded: explorer.expanded(),
        }
    }

    /// Reopens what [`Self::session`] saved before a restart. Data files
    /// open right away; the other tabs once their connection, opened again
    /// here, is ready. A tab whose connection fails or is gone stays closed.
    pub fn restore(&mut self, state: WorkspaceState, window: &mut Window, cx: &mut Context<Self>) {
        self.restored_session = true;
        self.sidebar_visible = state.sidebar_visible;
        self.queries_opened = state.queries_opened;
        self.restore_active = Some(state.active);
        self.explorer.update(cx, |explorer, cx| {
            explorer.set_mode(state.sidebar.into(), cx);
            explorer.set_expanded(state.expanded, cx);
        });
        let mut connections = state.connections;
        for (place, tab) in state.tabs.into_iter().enumerate() {
            match tab {
                TabState::File { path } => self.restore_data_file(place, path, window, cx),
                tab => {
                    if let Some(name) = tab.connection()
                        && !connections.iter().any(|open| open == name)
                    {
                        connections.push(name.to_string());
                    }
                    self.restoring.push((place, tab));
                }
            }
        }
        for name in connections {
            let exists = self
                .explorer
                .update(cx, |explorer, cx| explorer.ensure_connected(&name, cx));
            if !exists {
                self.restoring
                    .retain(|(_, tab)| tab.connection() != Some(name.as_str()));
            }
        }
        cx.notify();
    }

    fn restore_data_file(
        &mut self,
        place: usize,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let shown = path.clone();
            let source = cx
                .background_spawn(async move { crate::file_tab::load_source(path) })
                .await;
            this.update_in(cx, |this, window, cx| match source {
                Ok(source) => {
                    let tab = this.new_file_tab(source, window, cx);
                    this.insert_restored(place, tab, window, cx);
                }
                // Moved or deleted since: nothing to reopen.
                Err(error) => eprintln!("kuery: could not reopen {}: {error}", shown.display()),
            })
            .ok();
        })
        .detach();
    }

    /// Reopens the waiting tabs of a SQL connection that just opened.
    fn restore_sql_tabs(&mut self, db: &Db, window: &mut Window, cx: &mut Context<Self>) {
        for (place, state) in self.take_restoring(&db.name) {
            let tab = match state {
                TabState::Query {
                    title,
                    file,
                    text,
                    saved,
                    ..
                } => {
                    let tab = self.new_query_tab(
                        db.clone(),
                        Some(title.into()),
                        Some(text),
                        file,
                        window,
                        cx,
                    );
                    if let Tab::Query(query) = &tab {
                        query.update(cx, |query, cx| query.restore_saved_text(saved, cx));
                    }
                    tab
                }
                TabState::Table {
                    schema,
                    name,
                    table_kind,
                    view,
                    ..
                } => {
                    let info = TableInfo {
                        table: TableRef::new(schema, name),
                        kind: table_kind.into(),
                        estimated_rows: None,
                        comment: None,
                    };
                    self.new_table_tab(db.clone(), info, view.into(), window, cx)
                }
                TabState::Diagram { schema, .. } => {
                    self.new_diagram_tab(db.clone(), schema, window, cx)
                }
                TabState::Activity { .. } => self.new_activity_tab(db.clone(), window, cx),
                TabState::RedisKey { .. }
                | TabState::RedisConsole { .. }
                | TabState::Collection { .. }
                | TabState::DocumentConsole { .. }
                | TabState::File { .. } => {
                    continue;
                }
            };
            self.insert_restored(place, tab, window, cx);
        }
    }

    /// Reopens the waiting tabs of a Redis connection that just opened.
    fn restore_redis_tabs(&mut self, db: &RedisDb, window: &mut Window, cx: &mut Context<Self>) {
        for (place, state) in self.take_restoring(&db.name) {
            let tab = match state {
                TabState::RedisKey { database, key, .. } => {
                    self.new_redis_key_tab(db.clone(), database, key, window, cx)
                }
                TabState::RedisConsole { database, text, .. } => {
                    let console = self.new_redis_console_tab(db.clone(), database, window, cx);
                    console.update(cx, |console, cx| console.set_text(text, window, cx));
                    Tab::RedisConsole(console)
                }
                TabState::Query { .. }
                | TabState::Table { .. }
                | TabState::Diagram { .. }
                | TabState::Activity { .. }
                | TabState::Collection { .. }
                | TabState::DocumentConsole { .. }
                | TabState::File { .. } => continue,
            };
            self.insert_restored(place, tab, window, cx);
        }
    }

    /// Reopens the waiting tabs of a document connection that just opened,
    /// each with the filter bar it had.
    fn restore_document_tabs(&mut self, db: &DocDb, window: &mut Window, cx: &mut Context<Self>) {
        for (place, state) in self.take_restoring(&db.name) {
            let tab = match state {
                TabState::Collection {
                    path,
                    kind,
                    filter,
                    projection,
                    sort,
                    ..
                } => {
                    let tab = self.new_collection_tab(
                        db.clone(),
                        CollectionRef { path },
                        kind.into(),
                        window,
                        cx,
                    );
                    if let Tab::Collection(collection) = &tab {
                        collection.update(cx, |collection, cx| {
                            collection.set_bar(filter, projection, sort, window, cx)
                        });
                    }
                    tab
                }
                TabState::DocumentConsole { path, text, .. } => {
                    let console = self.new_document_console_tab(db.clone(), path, window, cx);
                    console.update(cx, |console, cx| console.set_text(text, window, cx));
                    Tab::DocumentConsole(console)
                }
                TabState::Query { .. }
                | TabState::Table { .. }
                | TabState::Diagram { .. }
                | TabState::Activity { .. }
                | TabState::RedisKey { .. }
                | TabState::RedisConsole { .. }
                | TabState::File { .. } => continue,
            };
            self.insert_restored(place, tab, window, cx);
        }
    }

    fn take_restoring(&mut self, connection: &str) -> Vec<(usize, TabState)> {
        let (taken, waiting) = std::mem::take(&mut self.restoring)
            .into_iter()
            .partition(|(_, tab)| tab.connection() == Some(connection));
        self.restoring = waiting;
        taken
    }

    /// Puts a restored tab back in its place, and shows it if it was the
    /// active one (or the first to come back).
    fn insert_restored(
        &mut self,
        place: usize,
        tab: Tab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let places: Vec<Option<usize>> = self
            .tabs
            .iter()
            .map(|tab| self.restored_places.get(&tab.id()).copied())
            .collect();
        let index = session::restored_position(&places, place);
        let first = self.tabs.is_empty();
        self.restored_places.insert(tab.id(), place);
        self.tabs.insert(index, tab);
        if self.restore_active == Some(place) {
            self.restore_active = None;
            self.activate(index, window, cx);
        } else if first {
            self.activate(index, window, cx);
        } else {
            if index <= self.active {
                self.active += 1;
            }
            cx.notify();
        }
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
        let tab = self.new_query_tab(db, title, sql, file, window, cx);
        self.tabs.push(tab);
        self.activate(self.tabs.len() - 1, window, cx);
    }

    fn new_query_tab(
        &mut self,
        db: Db,
        title: Option<SharedString>,
        sql: Option<String>,
        file: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Tab {
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
        Tab::Query(tab)
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
        let (name, table) = (db.name.clone(), info.table.clone());
        let index = self.focus_or_open(
            |tab, cx| match tab {
                Tab::Table(tab) => {
                    let tab = tab.read(cx);
                    tab.db().name == name && tab.info().table == table
                }
                _ => false,
            },
            |this, window, cx| this.new_table_tab(db, info, view, window, cx),
            window,
            cx,
        );
        // An open table switches to the view asked for.
        if let Some(Tab::Table(table)) = self.tabs.get(index) {
            table.update(cx, |table, cx| table.set_view(view, cx));
        }
    }

    fn new_table_tab(
        &self,
        db: Db,
        info: TableInfo,
        view: TableView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Tab {
        let project = self.model.read(cx).project.root.clone();
        Tab::Table(cx.new(|cx| TableTab::new(db, project, info, view, window, cx)))
    }

    /// Opens a schema's diagram, or shows the tab it is already open in.
    fn open_diagram(
        &mut self,
        db: Db,
        schema: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (name, shown) = (db.name.clone(), schema.clone());
        self.focus_or_open(
            |tab, cx| match tab {
                Tab::Diagram(tab) => {
                    let tab = tab.read(cx);
                    tab.db().name == name && tab.schema() == shown
                }
                _ => false,
            },
            |this, window, cx| this.new_diagram_tab(db, schema, window, cx),
            window,
            cx,
        );
    }

    fn new_diagram_tab(
        &self,
        db: Db,
        schema: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Tab {
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
        Tab::Diagram(tab)
    }

    /// Opens a connection's server activity, or shows the tab it is already
    /// open in.
    fn open_activity(&mut self, db: Db, window: &mut Window, cx: &mut Context<Self>) {
        let name = db.name.clone();
        self.focus_or_open(
            |tab, cx| matches!(tab, Tab::Activity(tab) if tab.read(cx).db().name == name),
            |this, window, cx| this.new_activity_tab(db, window, cx),
            window,
            cx,
        );
    }

    fn new_activity_tab(&self, db: Db, window: &mut Window, cx: &mut Context<Self>) -> Tab {
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
        Tab::Activity(tab)
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
        let (name, shown) = (db.name.clone(), key.clone());
        self.focus_or_open(
            |tab, cx| match tab {
                Tab::RedisKey(tab) => {
                    let tab = tab.read(cx);
                    tab.db().name == name && tab.database() == database && tab.key() == shown
                }
                _ => false,
            },
            |this, window, cx| this.new_redis_key_tab(db, database, key, window, cx),
            window,
            cx,
        );
    }

    fn new_redis_key_tab(
        &self,
        db: RedisDb,
        database: i64,
        key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Tab {
        let project = self.model.read(cx).project.root.clone();
        Tab::RedisKey(cx.new(|cx| KeyTab::new(db, database, key, project, window, cx)))
    }

    fn open_redis_console(
        &mut self,
        db: RedisDb,
        database: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let console = self.new_redis_console_tab(db, database, window, cx);
        self.tabs.push(Tab::RedisConsole(console));
        let index = self.tabs.len() - 1;
        self.activate(index, window, cx);
    }

    fn new_redis_console_tab(
        &self,
        db: RedisDb,
        database: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ConsoleTab> {
        let project = self.model.read(cx).project.root.clone();
        cx.new(|cx| ConsoleTab::new(db, database, project, window, cx))
    }

    fn open_document_console(
        &mut self,
        db: DocDb,
        path: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let console = self.new_document_console_tab(db, path, window, cx);
        self.tabs.push(Tab::DocumentConsole(console));
        let index = self.tabs.len() - 1;
        self.activate(index, window, cx);
    }

    fn new_document_console_tab(
        &self,
        db: DocDb,
        path: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<DocumentConsole> {
        let project = self.model.read(cx).project.root.clone();
        let console = cx.new(|cx| DocumentConsole::new(db, path, project, window, cx));
        // The status bar shows what runs.
        cx.observe(&console, |_, _, cx| cx.notify()).detach();
        console
    }

    /// Opens a collection in a tab, or shows the tab it is already open in.
    fn open_collection(
        &mut self,
        db: DocDb,
        collection: CollectionRef,
        kind: ContainerKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (name, shown) = (db.name.clone(), collection.clone());
        self.focus_or_open(
            |tab, cx| match tab {
                Tab::Collection(tab) => {
                    let tab = tab.read(cx);
                    tab.db().name == name && *tab.collection() == shown
                }
                _ => false,
            },
            |this, window, cx| this.new_collection_tab(db, collection, kind, window, cx),
            window,
            cx,
        );
    }

    fn new_collection_tab(
        &self,
        db: DocDb,
        collection: CollectionRef,
        kind: ContainerKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Tab {
        let project = self.model.read(cx).project.root.clone();
        let tab = cx.new(|cx| CollectionTab::new(db, collection, kind, project, window, cx));
        // The status bar shows the page as it loads.
        cx.observe(&tab, |_, _, cx| cx.notify()).detach();
        Tab::Collection(tab)
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
            Some(Tab::Collection(collection)) if collection.read(cx).has_pending() => {
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

    /// Closes the window, asking first if tabs have unsaved SQL, staged row
    /// changes or open transactions: closing loses the first two and drops
    /// the connections, which rolls the transactions back.
    fn request_close_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let unsaved = self
            .tabs
            .iter()
            .filter(|tab| tab.needs_attention(cx))
            .count();
        if unsaved == 0 {
            self.flush_sidebar_width();
            window.remove_window();
            return;
        }
        let open = self
            .tabs
            .iter()
            .filter(|tab| matches!(tab, Tab::Query(q) if q.read(cx).in_transaction()))
            .count();
        let tabs = if unsaved == 1 {
            "A tab has".to_string()
        } else {
            format!("{unsaved} tabs have")
        };
        let (title, detail, close) = if open == unsaved {
            (
                "Close the window and roll back?",
                format!(
                    "{} not committed. Closing the window rolls {} back.",
                    if open == 1 {
                        "A tab has a transaction that is".to_string()
                    } else {
                        format!("{open} tabs have transactions that are")
                    },
                    if open == 1 { "it" } else { "them" }
                ),
                "Roll Back and Close",
            )
        } else {
            (
                "Close the window without saving?",
                format!(
                    "{tabs} unsaved SQL, staged changes or an open transaction. \
                     They will be lost, and open transactions rolled back."
                ),
                "Close Without Saving",
            )
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            title,
            Some(&detail),
            &[close, "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(0) = answer.await {
                this.update(cx, |this, _| this.flush_sidebar_width()).ok();
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
                        // Save; a query without a file asks where, and one that
                        // failed to save shows why. Both stay open.
                        query.update(cx, |query, cx| query.request_save(window, cx));
                        if !query.read(cx).is_dirty() {
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

    // Commands -------------------------------------------------------------

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            // Nothing left to close in the window: close the window itself.
            window.dispatch_action(Box::new(CloseWindow), cx);
        } else {
            self.close(self.active, window, cx);
        }
    }

    fn new_query(&mut self, _: &NewQuery, window: &mut Window, cx: &mut Context<Self>) {
        self.open_new_query(window, cx);
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, window: &mut Window, cx: &mut Context<Self>) {
        self.set_sidebar_visible(!self.sidebar_visible, window, cx);
    }

    /// Shows or hides the sidebar. Hiding it takes the focus out of it, or
    /// the window's shortcuts would be left with nowhere to go.
    fn set_sidebar_visible(&mut self, visible: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !visible && self.explorer.focus_handle(cx).contains_focused(window, cx) {
            window.focus(&self.focus_handle, cx);
        }
        self.sidebar_visible = visible;
        cx.notify();
    }

    /// Shows the sidebar's tree and moves the focus to its filter field.
    fn find_in_sidebar(&mut self, _: &FindInSidebar, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_visible = true;
        self.explorer
            .update(cx, |explorer, cx| explorer.focus_filter(window, cx));
        cx.notify();
    }

    /// Writes the sidebar's width now if a save is still waiting, before the
    /// window closes or the app quits and the waiting save is dropped.
    fn flush_sidebar_width(&mut self) {
        if self.sidebar_width_save.take().is_some()
            && let Err(error) = settings::set_sidebar_width(f32::from(self.sidebar_width))
        {
            eprintln!("kuery: could not save the sidebar width: {error}");
        }
    }

    /// Follows the sidebar's right edge while it is dragged.
    fn on_sidebar_drag(&mut self, event: &DragMoveEvent<SidebarResize>, cx: &mut Context<Self>) {
        let width = event.event.position.x - event.bounds.left();
        self.resize_sidebar(f32::from(width), cx);
    }

    /// Sets the sidebar's width, within its limits, and saves it for every
    /// project once it stops changing.
    fn resize_sidebar(&mut self, width: f32, cx: &mut Context<Self>) {
        let width = settings::clamp_sidebar_width(width);
        if px(width) == self.sidebar_width {
            return;
        }
        self.sidebar_width = px(width);
        let executor = cx.background_executor().clone();
        self.sidebar_width_save = Some(cx.background_spawn(async move {
            executor.timer(SIDEBAR_SAVE_DELAY).await;
            if let Err(error) = settings::set_sidebar_width(width) {
                eprintln!("kuery: could not save the sidebar width: {error}");
            }
        }));
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
        if matches!(
            self.tabs.get(self.active),
            Some(Tab::File(_) | Tab::FileQuery(_))
        ) {
            return None;
        }
        self.tabs
            .get(self.active)
            .and_then(|tab| tab.db(cx))
            .or_else(|| self.explorer.read(cx).current_db())
    }

    /// What the chrome shows about the active tab's connection, or about
    /// the sidebar's when the tab has none of its own.
    fn active_connection(&self, cx: &App) -> Option<ConnMeta> {
        if matches!(
            self.tabs.get(self.active),
            Some(Tab::File(_) | Tab::FileQuery(_))
        ) {
            return None;
        }
        self.tabs
            .get(self.active)
            .and_then(|tab| tab.connection(cx))
            .or_else(|| self.explorer.read(cx).current_meta())
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
                .on_click(cx.listener(move |this, _, window, cx| {
                    let showing = this.sidebar_visible && this.explorer.read(cx).mode() == mode;
                    if showing {
                        this.set_sidebar_visible(false, window, cx);
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

    /// The sidebar at its width. Its right edge is the handle that resizes
    /// it; a double-click on the edge puts the default width back.
    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("sidebar")
            .relative()
            .flex()
            .h_full()
            .w(self.sidebar_width)
            .flex_shrink_0()
            .on_drag_move(cx.listener(|this, event, _, cx| this.on_sidebar_drag(event, cx)))
            .child(self.explorer.clone())
            .child(
                resize_handle::<SidebarResize, SidebarResize>("sidebar-edge", Axis::Horizontal)
                    .inside(HandleEdge::Trailing)
                    .with_appearance(resize_handle_appearance())
                    .on_drag(SidebarResize, |drag, _, _, cx| {
                        cx.stop_propagation();
                        cx.new(|_| drag.as_ref().clone())
                    }),
            )
            .child(
                // The handle takes the mouse for itself, so the double-click
                // is caught by a band laid over it that does not.
                div()
                    .id("sidebar-edge-reset")
                    .absolute()
                    .top_0()
                    .right_0()
                    .h_full()
                    .w(px(5.))
                    .cursor_col_resize()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            if event.click_count == 2 {
                                this.resize_sidebar(settings::SIDEBAR_DEFAULT_WIDTH, cx);
                            }
                        }),
                    ),
            )
    }

    /// The bottom bar: the active connection and mode on the left, the last
    /// run and the cursor on the right.
    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let connection = self.active_connection(cx);
        let mut left = h_flex().gap_4().min_w_0();
        let mut right = h_flex().gap_4().flex_shrink_0();
        match &connection {
            Some(connection) => {
                left = left
                    .child(
                        h_flex()
                            .gap(px(6.))
                            .text_color(color(status_tone(connection.environment)))
                            .child(chrome::dot(connection.environment.color()))
                            .child(format!(
                                "{} · {}",
                                connection.name,
                                connection.environment.label().to_lowercase()
                            )),
                    )
                    .child(div().child(connection.target.clone()));
                right = right
                    .children(notice_item(connection))
                    .child(connection.server.clone());
            }
            None => left = left.child("No open connection"),
        }
        let server = connection
            .as_ref()
            .map(|connection| connection.server.clone());
        match self.tabs.get(self.active) {
            Some(Tab::FileQuery(query)) => {
                left = h_flex().gap_4().min_w_0().child("Polars SQL · read only");
                right = h_flex()
                    .gap_4()
                    .flex_shrink_0()
                    .child(query.read(cx).status().to_owned());
            }
            Some(Tab::File(file)) => {
                let file = file.read(cx);
                left = h_flex().gap_4().min_w_0().child("Local file · read only");
                right = h_flex().gap_4().flex_shrink_0().child(file.status());
            }
            Some(Tab::Diagram(diagram)) => {
                let tab = diagram.read(cx);
                left = left.child(format!("Diagram of {}", tab.schema()));
                right = h_flex()
                    .gap_4()
                    .flex_shrink_0()
                    .children(tab.status())
                    .children(server.clone());
            }
            Some(Tab::Activity(activity)) => {
                let tab = activity.read(cx);
                left = left.child(format!("Server activity of {}", tab.db().name));
                right = h_flex()
                    .gap_4()
                    .flex_shrink_0()
                    .children(tab.status())
                    .children(server.clone());
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
                    .children(server.clone());
            }
            Some(Tab::RedisConsole(console)) => {
                let tab = console.read(cx);
                left = left.child(mode_item(tab.is_writing()));
                if tab.db().environment == Environment::Production && tab.db().allows_writes() {
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
                    .children(server.clone());
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
                if tab.db().environment == Environment::Production
                    && tab.db().allows(WriteKind::Transactional)
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
                    .children(server.clone());
            }
            Some(Tab::DocumentConsole(console)) => {
                let tab = console.read(cx);
                left = left.child(mode_item(tab.is_writing()));
                if tab.db().environment == Environment::Production && tab.db().allows_writes() {
                    let console = console.clone();
                    left = left.child(
                        div()
                            .id("stop-allowing-document-writes")
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
                    .children(connection.as_ref().and_then(notice_item))
                    .child(div().max_w(px(420.)).truncate().child(tab.status()))
                    .children(server.clone());
            }
            Some(Tab::Collection(collection)) => {
                let tab = collection.read(cx);
                left = left.child(div().truncate().child(tab.collection().display()));
                // The connection's notice stays: a document tab is where
                // its warnings matter.
                right = h_flex()
                    .gap_4()
                    .flex_shrink_0()
                    .children(connection.as_ref().and_then(notice_item))
                    .children(tab.page_status())
                    .children(server.clone());
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
                        .children(server.clone());
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
            .child(crate::updates::status_item(cx))
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
                Tab::DocumentConsole(console) => (AppIcon::Code, console.read(cx).title(), false),
                Tab::Collection(collection) => {
                    let collection = collection.read(cx);
                    let icon = match collection.kind() {
                        ContainerKind::View => AppIcon::Code,
                        _ => AppIcon::Table,
                    };
                    (icon, collection.title(), collection.has_pending())
                }
                Tab::Diagram(diagram) => (AppIcon::Link, diagram.read(cx).title(), false),
                Tab::Activity(activity) => (AppIcon::Activity, activity.read(cx).title(), false),
                Tab::File(file) => (AppIcon::Table, file.read(cx).title(), false),
                Tab::FileQuery(query) => (AppIcon::Code, query.read(cx).title(), false),
            };
            let tooltip: SharedString = match tab.connection(cx) {
                Some(connection) => format!("{title} · {}", connection.name).into(),
                None => title.clone(),
            };
            let tooltip = match tab {
                Tab::File(file) => file.read(cx).path().display().to_string().into(),
                _ => tooltip,
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
                            Tab::File(file) => Some(file.read(cx).path().to_path_buf()),
                            _ => None,
                        };
                        let table = match tab {
                            Tab::Table(table) => {
                                let tab = table.read(cx);
                                let table = &tab.info().table;
                                Some((
                                    "Copy Table Name",
                                    tab.db()
                                        .session
                                        .dialect()
                                        .quote_qualified(&table.schema, &table.name),
                                ))
                            }
                            Tab::Collection(collection) => Some((
                                "Copy Collection Name",
                                collection.read(cx).collection().display(),
                            )),
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
                            if let Some((label, name)) = &table {
                                menu = menu.separator().item(copy(*label, name.clone()));
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

    fn render_content(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.tabs.get(self.active) {
            Some(Tab::Query(query)) => query.clone().into_any_element(),
            Some(Tab::Table(table)) => table.clone().into_any_element(),
            Some(Tab::RedisKey(key)) => key.clone().into_any_element(),
            Some(Tab::RedisConsole(console)) => console.clone().into_any_element(),
            Some(Tab::Collection(collection)) => collection.clone().into_any_element(),
            Some(Tab::DocumentConsole(console)) => console.clone().into_any_element(),
            Some(Tab::Diagram(diagram)) => diagram.clone().into_any_element(),
            Some(Tab::Activity(activity)) => activity.clone().into_any_element(),
            Some(Tab::File(file)) => file.clone().into_any_element(),
            Some(Tab::FileQuery(query)) => query.clone().into_any_element(),
            None => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .child("Explore a data file, or open a table or saved query from the sidebar")
                .child(
                    Button::new("empty-open-file")
                        .label("Open Data File…")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_data_file(&OpenDataFile, window, cx)
                        })),
                )
                .into_any_element(),
        }
    }
}

impl DataFileTabs for WorkspaceView {
    type Tab = Tab;

    fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    fn tabs_mut(&mut self) -> &mut Vec<Tab> {
        &mut self.tabs
    }

    fn active(&self) -> usize {
        self.active
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

    fn as_file(tab: &Tab) -> Option<&Entity<FileTab>> {
        match tab {
            Tab::File(file) => Some(file),
            _ => None,
        }
    }

    fn file_tab(file: Entity<FileTab>) -> Tab {
        Tab::File(file)
    }

    fn query_files_tab(query: Entity<FileQueryTab>) -> Tab {
        Tab::FileQuery(query)
    }
}

/// Recent projects other than the one in `root`, newest first.
/// What the connection's driver said the user should know, in amber, for
/// the status bar.
fn notice_item(connection: &ConnMeta) -> Option<gpui_kit::Div> {
    connection
        .notice
        .clone()
        .map(|notice| div().text_color(color(theme::AMBER)).child(notice))
}

/// A console's mode, for the status bar: whether it writes.
fn mode_item(writing: bool) -> gpui_kit::Div {
    let (icon, label) = mode_label(writing);
    h_flex()
        .gap(px(5.))
        .child(Icon::new(icon).size(px(11.)))
        .child(label)
}

fn mode_label(writing: bool) -> (AppIcon, &'static str) {
    match writing {
        true => (AppIcon::Pencil, "Write"),
        false => (AppIcon::Lock, "Read-only"),
    }
}

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
            .on_action(cx.listener(Self::find_in_sidebar))
            .on_action(cx.listener(Self::toggle_palette))
            .on_action(cx.listener(Self::new_connection))
            .on_action(cx.listener(Self::open_schema_diagram))
            .on_action(cx.listener(Self::show_server_activity))
            .on_action(cx.listener(Self::open_data_file))
            .on_action(cx.listener(Self::query_data_files))
            .on_action(cx.listener(Self::close_window))
            .bg(color(theme::GROUND))
            .text_color(color(theme::TEXT))
            .text_size(px(13.))
            .child(self.render_title_bar(cx))
            // Production is marked by a thin line under the title bar.
            .when(
                self.active_connection(cx)
                    .is_some_and(|connection| connection.environment == Environment::Production),
                |root| root.child(div().h(px(2.)).flex_shrink_0().bg(color(theme::AMBER))),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(self.render_activity_rail(cx))
                    .when(self.sidebar_visible, |row| {
                        row.child(self.render_sidebar(cx))
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .child(self.render_tab_strip(cx))
                            .child(div().flex_1().min_h_0().child(self.render_content(cx))),
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

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use futures::executor::block_on;
    use gpui::TestAppContext;
    use gpui_kit::{AnyWindowHandle, Bounds, Point, WindowBounds, WindowOptions, size};
    use kuery_core::session::{Access, Opener, Session};

    use super::*;
    use crate::project::Project;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kuery-workspace-{name}-{}",
            crate::project::new_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn db(dir: &Path) -> Db {
        let path = dir.join("test.sqlite");
        std::fs::write(&path, b"").unwrap();
        let opener: Opener = Arc::new(move |access| kuery_sqlite::open(&path, access));
        let session = block_on(Session::open(opener, Access::ReadWrite)).unwrap();
        Db::new(
            "local".into(),
            Environment::Development,
            session,
            "test.sqlite".into(),
            None,
        )
    }

    /// A project window with one query tab saved to `file`, edited since.
    fn open_edited_query(
        cx: &mut TestAppContext,
        dir: &Path,
        file: PathBuf,
    ) -> (AnyWindowHandle, Entity<WorkspaceView>, Entity<QueryTab>) {
        // The SQLite session answers from its own thread.
        cx.executor().allow_parking();
        let db = db(dir);
        let root = dir.to_path_buf();
        let (window, workspace) = cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(cx);
            let model = cx.new(|_| ProjectModel {
                project: Project {
                    root,
                    name: "shop".into(),
                    workspace: None,
                    connections: Vec::new(),
                },
            });
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: size(px(1000.), px(600.)),
                    })),
                    ..Default::default()
                },
                cx,
                |window, cx| cx.new(|cx| WorkspaceView::new(model, window, cx)),
            )
            .unwrap()
        });
        let query = cx
            .update_window(window, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    let tab = workspace.new_query_tab(
                        db,
                        None,
                        Some("select 2;\n".into()),
                        Some(file),
                        window,
                        cx,
                    );
                    let Tab::Query(query) = &tab else {
                        unreachable!()
                    };
                    let query = query.clone();
                    query.update(cx, |query, cx| {
                        query.restore_saved_text("select 1;\n".into(), cx)
                    });
                    workspace.tabs.push(tab);
                    query
                })
            })
            .unwrap();
        cx.run_until_parked();
        (window, workspace, query)
    }

    fn close_and_save(
        cx: &mut TestAppContext,
        window: AnyWindowHandle,
        workspace: &Entity<WorkspaceView>,
    ) {
        cx.update_window(window, |_, window, cx| {
            workspace.update(cx, |workspace, cx| workspace.close(0, window, cx))
        })
        .unwrap();
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Save");
        cx.run_until_parked();
    }

    #[test]
    fn the_status_bar_says_whether_a_console_writes() {
        assert_eq!(mode_label(true).1, "Write");
        assert_eq!(mode_label(false).1, "Read-only");
    }

    #[gpui::test]
    fn a_query_that_cannot_be_saved_stays_open(cx: &mut TestAppContext) {
        let dir = temp_dir("unsaved");
        // A file where the query's folder should be: writing it fails.
        std::fs::write(dir.join("queries"), b"").unwrap();
        let file = dir.join("queries").join("monthly.sql");
        let (window, workspace, query) = open_edited_query(cx, &dir, file);

        close_and_save(cx, window, &workspace);

        cx.update(|cx| {
            assert_eq!(workspace.read(cx).tabs.len(), 1);
            let query = query.read(cx);
            assert!(query.is_dirty());
            assert_eq!(query.text(cx), "select 2;\n");
        });
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui::test]
    fn a_saved_query_closes(cx: &mut TestAppContext) {
        let dir = temp_dir("saved");
        let file = dir.join("queries").join("monthly.sql");
        let (window, workspace, _) = open_edited_query(cx, &dir, file.clone());

        close_and_save(cx, window, &workspace);

        cx.update(|cx| assert!(workspace.read(cx).tabs.is_empty()));
        assert!(std::fs::read_to_string(file).unwrap().contains("select 2;"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
