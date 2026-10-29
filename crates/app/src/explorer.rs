//! Sidebar: the project's connections as a tree (connection → schema →
//! tables, views, functions, triggers and enums for Postgres; connection →
//! database → keys for Redis, see [`redis_keys`]; connection → database →
//! collections for document databases, see [`documents`]), its saved
//! queries, and the query history.
//!
//! The sidebar owns the open connections: it opens each one when it is first
//! expanded (the first one opens on start), reading its password from the
//! system keychain, and hands an open [`Db`] to the tabs it asks the
//! workspace to open. The connection list follows the project model.

#[cfg(test)]
mod bench;
mod documents;
mod redis_keys;
mod rows;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui_kit::component::input::{Escape, Input, InputEvent, InputState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, PromptLevel, Render,
    ScrollStrategy, SharedString, Stateful, StatefulInteractiveElement as _, Styled as _,
    UniformListScrollHandle, Window, div, px, uniform_list,
};
use kuery_core::catalog::{
    CatalogOverview, EnumInfo, FunctionInfo, TableInfo, TableRef, TriggerRef,
};
use kuery_core::document::catalog::ContainerKind;
use kuery_core::document::value::CollectionRef;
use kuery_core::driver::Family;
use kuery_core::sql::{Dialect, POSTGRES};

use crate::app;
use crate::assets::AppIcon;
use crate::connection::{ConnMeta, Db, DocDb, RedisDb};
use crate::context_menus::{copy, item, item_icon, reveal};
use crate::drivers::{self, Connection};
use crate::history::{self, HistoryEntry};
use crate::project::{ConnectionSpec, ProjectModel, QueryEntry};
use crate::query_tab::describe_error;
use crate::table_tab::TableView;
use crate::theme::{self, color};

pub enum ExplorerEvent {
    /// A connection finished opening.
    Connected(Db),
    OpenTable {
        db: Db,
        info: TableInfo,
        view: TableView,
    },
    OpenQuery {
        db: Db,
        title: SharedString,
        sql: String,
    },
    /// A file from the project's saved queries.
    OpenSavedQuery(PathBuf),
    /// An empty query tab on this connection.
    NewQuery(Db),
    NewConnection,
    EditConnection(String),
    /// The server's sessions and locks.
    OpenActivity(Db),
    /// A schema's diagram.
    OpenDiagram {
        db: Db,
        schema: String,
    },
    /// A Redis connection finished opening.
    RedisConnected(RedisDb),
    OpenRedisKey {
        db: RedisDb,
        database: i64,
        key: String,
    },
    OpenRedisConsole {
        db: RedisDb,
        database: i64,
    },
    /// A document connection finished opening.
    DocumentConnected(DocDb),
    /// A collection (or a view) of a document database.
    OpenCollection {
        db: DocDb,
        collection: CollectionRef,
        kind: ContainerKind,
    },
}

enum ConnectionState {
    Idle,
    Connecting,
    Ready {
        db: Db,
        catalog: CatalogState,
    },
    RedisReady {
        db: RedisDb,
        keyspace: redis_keys::Keyspace,
    },
    DocumentReady {
        db: DocDb,
    },
    Failed(String),
}

enum CatalogState {
    Loading,
    Loaded(CatalogOverview),
    Failed(String),
}

struct ConnectionNode {
    spec: ConnectionSpec,
    state: ConnectionState,
}

/// What the sidebar shows; picked from the workspace's activity rail.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Explorer,
    Queries,
    History,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Group {
    Tables,
    Views,
    Functions,
    Triggers,
    Enums,
}

impl Group {
    const ALL: [Group; 5] = [
        Group::Tables,
        Group::Views,
        Group::Functions,
        Group::Triggers,
        Group::Enums,
    ];

    fn label(self) -> &'static str {
        match self {
            Group::Tables => "Tables",
            Group::Views => "Views",
            Group::Functions => "Functions",
            Group::Triggers => "Triggers",
            Group::Enums => "Enums",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Group::Tables => "tables",
            Group::Views => "views",
            Group::Functions => "functions",
            Group::Triggers => "triggers",
            Group::Enums => "enums",
        }
    }
}

pub struct Explorer {
    model: Entity<ProjectModel>,
    connections: Vec<ConnectionNode>,
    queries: Vec<QueryEntry>,
    /// Collapsed query folders, by relative path.
    collapsed_folders: HashSet<PathBuf>,
    selected_query: Option<PathBuf>,
    /// Keys of expanded nodes: `"0"`, `"0/public"`, `"0/public/tables"`.
    expanded: HashSet<String>,
    selected: Option<String>,
    search: Entity<InputState>,
    mode: Mode,
    history: Vec<HistoryEntry>,
    /// Saves run in the background; each gets a number, and an older one
    /// never overwrites a newer one. Holds the last number written.
    history_saved: Arc<Mutex<u64>>,
    history_saves: u64,
    /// Keys read so far per (connection id, Redis database).
    redis_keys: redis_keys::KeyBrowsers,
    /// Document databases' nodes read so far, per connection id.
    documents: documents::DocumentTrees,
    /// Bumped on every edit of the filter, so only the last one rescans.
    filter_edits: u64,
    /// The tree's rows, built again when the explorer changes or the filter
    /// does; elements are built only for the rows on screen.
    rows: Vec<rows::Row>,
    /// The filter `rows` was built with.
    rows_query: String,
    rows_stale: bool,
    scroll: UniformListScrollHandle,
    /// The sidebar itself, focused by a click anywhere in it, so its own
    /// shortcuts (Find in Sidebar) apply there and nowhere else.
    focus_handle: FocusHandle,
}

/// What Esc does in the filter field.
#[derive(Debug, PartialEq, Eq)]
enum FilterEscape {
    /// Empty the field and stay in it.
    Clear,
    /// Hand the focus back to the sidebar.
    Leave,
}

fn filter_escape(text: &str) -> FilterEscape {
    if text.is_empty() {
        FilterEscape::Leave
    } else {
        FilterEscape::Clear
    }
}

impl EventEmitter<ExplorerEvent> for Explorer {}

impl Explorer {
    pub fn new(model: Entity<ProjectModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Filter objects"));
        cx.subscribe(&search, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.filter_changed(cx);
            }
        })
        .detach();

        cx.observe(&model, |this, _, cx| this.sync_project(cx))
            .detach();
        // Whatever changes the tree (an expand, a connection opening, a
        // catalog arriving) notifies; a hover only redraws.
        cx.observe_self(|this, _| this.rows_stale = true).detach();

        let project = model.read(cx).project.clone();
        let explorer = Self {
            model,
            connections: project
                .connections
                .iter()
                .cloned()
                .map(|spec| ConnectionNode {
                    spec,
                    state: ConnectionState::Idle,
                })
                .collect(),
            queries: project.queries(),
            collapsed_folders: HashSet::new(),
            selected_query: None,
            expanded: HashSet::new(),
            selected: None,
            search,
            mode: Mode::Explorer,
            history: history::load(&project.root),
            history_saved: Arc::new(Mutex::new(0)),
            history_saves: 0,
            redis_keys: HashMap::new(),
            documents: HashMap::new(),
            filter_edits: 0,
            rows: Vec::new(),
            rows_query: String::new(),
            rows_stale: true,
            scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
        };
        // No connection opens by itself: the project file says where it goes
        // (host, SSH bastion, which stored password), and a cloned project
        // is someone else's file.
        explorer
    }

    /// The connection of the selected node, or the first open one.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.mode = mode;
        if mode == Mode::Queries {
            self.refresh_queries(cx);
        }
        cx.notify();
    }

    /// Moves the focus to the filter field with its text selected, so typing
    /// replaces it. The tree is shown first if the sidebar was on another view.
    pub fn focus_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Explorer {
            self.set_mode(Mode::Explorer, cx);
        }
        self.search.update(cx, |search, cx| {
            search.focus(window, cx);
            search.select_all(window, cx);
        });
    }

    /// Esc in the filter field empties it, or leaves it once it is empty.
    fn filter_escaped(&mut self, _: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        match filter_escape(&self.search.read(cx).value()) {
            FilterEscape::Clear => self
                .search
                .update(cx, |search, cx| search.clean(window, cx)),
            FilterEscape::Leave => window.focus(&self.focus_handle, cx),
        }
    }

    /// Collapses every expanded node.
    fn collapse_all(&mut self, cx: &mut Context<Self>) {
        self.expanded.clear();
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    /// Reloads the catalog of every open connection.
    fn refresh_catalogs(&mut self, cx: &mut Context<Self>) {
        for index in 0..self.connections.len() {
            match self.connections[index].state {
                ConnectionState::Ready { .. } => self.load_catalog(index, cx),
                ConnectionState::RedisReady { .. } => self.load_keyspace(index, cx),
                ConnectionState::DocumentReady { .. } => self.refresh_documents(index, cx),
                _ => {}
            }
        }
    }

    /// Reloads one connection's catalog, its databases for Redis, or the
    /// nodes read so far for a document database.
    fn refresh(&mut self, index: usize, cx: &mut Context<Self>) {
        match self.connections.get(index).map(|node| &node.state) {
            Some(ConnectionState::RedisReady { .. }) => self.load_keyspace(index, cx),
            Some(ConnectionState::DocumentReady { .. }) => self.refresh_documents(index, cx),
            _ => self.load_catalog(index, cx),
        }
    }

    /// The filter filters Postgres objects as typed; Redis key lists are
    /// read again with it as a `SCAN` pattern once typing pauses.
    fn filter_changed(&mut self, cx: &mut Context<Self>) {
        self.filter_edits += 1;
        let edit = self.filter_edits;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(350))
                .await;
            this.update(cx, |this, cx| {
                if this.filter_edits == edit {
                    this.rescan_redis(cx);
                }
            })
            .ok();
        })
        .detach();
        // The matches start at the top.
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    pub fn current_db(&self) -> Option<Db> {
        let selected = self
            .selected
            .as_ref()
            .and_then(|key| key.split('/').next()?.parse::<usize>().ok())
            .and_then(|index| self.db(index));
        selected.or_else(|| (0..self.connections.len()).find_map(|index| self.db(index)))
    }

    /// What the status bar shows with no tab open: the selected connection,
    /// SQL or documents, else the first one open.
    pub fn current_meta(&self) -> Option<ConnMeta> {
        let selected = self
            .selected
            .as_ref()
            .and_then(|key| key.split('/').next()?.parse::<usize>().ok())
            .and_then(|index| self.meta(index));
        selected.or_else(|| (0..self.connections.len()).find_map(|index| self.meta(index)))
    }

    fn meta(&self, index: usize) -> Option<ConnMeta> {
        match &self.connections.get(index)?.state {
            ConnectionState::Ready { db, .. } => Some(db.meta()),
            ConnectionState::DocumentReady { db } => Some(db.meta()),
            _ => None,
        }
    }

    /// Tables and views of every connection whose catalog is loaded, for the
    /// command palette.
    pub fn open_tables(&self) -> Vec<(Db, TableInfo)> {
        self.connections
            .iter()
            .filter_map(|node| match &node.state {
                ConnectionState::Ready {
                    db,
                    catalog: CatalogState::Loaded(overview),
                } => Some((db, overview)),
                _ => None,
            })
            .flat_map(|(db, overview)| {
                overview
                    .tables
                    .iter()
                    .map(move |info| (db.clone(), info.clone()))
            })
            .collect()
    }

    pub fn record(&mut self, entry: HistoryEntry, cx: &mut Context<Self>) {
        self.history.insert(0, entry);
        self.history.truncate(history::LIMIT);
        self.save_history(cx);
    }

    fn remove_history_entry(&mut self, position: usize, cx: &mut Context<Self>) {
        if position < self.history.len() {
            self.history.remove(position);
            self.save_history(cx);
        }
    }

    fn clear_history(&mut self, cx: &mut Context<Self>) {
        self.history.clear();
        self.save_history(cx);
    }

    /// Writes the history to disk off the UI thread.
    fn save_history(&mut self, cx: &mut Context<Self>) {
        let root = self.model.read(cx).project.root.clone();
        let entries = self.history.clone();
        self.history_saves += 1;
        let (number, saved) = (self.history_saves, self.history_saved.clone());
        cx.background_spawn(async move {
            let mut last = saved
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if number <= *last {
                return;
            }
            match history::save(&root, &entries) {
                Ok(()) => *last = number,
                Err(error) => eprintln!("kuery: could not save the query history: {error}"),
            }
        })
        .detach();
        cx.notify();
    }

    /// Highlights the table of the active tab.
    pub fn reveal_table(
        &mut self,
        connection: &str,
        table: Option<&TableRef>,
        cx: &mut Context<Self>,
    ) {
        self.selected = table.and_then(|table| {
            let index = self
                .connections
                .iter()
                .position(|c| c.spec.name == connection)?;
            Some(format!("{index}/{}/table/{}", table.schema, table.name))
        });
        cx.notify();
    }

    fn db(&self, index: usize) -> Option<Db> {
        match &self.connections.get(index)?.state {
            ConnectionState::Ready { db, .. } => Some(db.clone()),
            _ => None,
        }
    }

    fn connect(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(node) = self.connections.get_mut(index) else {
            return;
        };
        if matches!(
            node.state,
            ConnectionState::Connecting
                | ConnectionState::Ready { .. }
                | ConnectionState::RedisReady { .. }
                | ConnectionState::DocumentReady { .. }
        ) {
            return;
        }
        let spec = node.spec.clone();
        let driver = match drivers::for_spec(&spec) {
            Ok(driver) => driver,
            Err(error) => {
                node.state = ConnectionState::Failed(error);
                cx.notify();
                return;
            }
        };
        node.state = ConnectionState::Connecting;
        // A database file's relative path starts at the project folder.
        let resolved = spec.resolved(&self.model.read(cx).project.root);
        let state = app::state(cx);
        let (engines, secrets) = (state.engines(), state.secrets.clone());
        // The keychain may ask the user for permission: read it off the UI thread.
        let id = spec.id.clone();
        let password = cx.background_spawn(async move { secrets.get(&id) });
        cx.spawn(async move |this, cx| {
            let result = match password.await {
                Ok(password) => driver.connect(&resolved, password, &engines).await,
                Err(error) => Err(error),
            };
            this.update(cx, |this, cx| {
                let Some(index) = this.index_of(&spec.id) else {
                    return;
                };
                let node = &mut this.connections[index];
                if node.spec != spec {
                    return;
                }
                match result {
                    Ok(Connection::Sql(db)) => {
                        node.state = ConnectionState::Ready {
                            db: db.clone(),
                            catalog: CatalogState::Loading,
                        };
                        this.load_catalog(index, cx);
                        cx.emit(ExplorerEvent::Connected(db));
                    }
                    Ok(Connection::KeyValue(db)) => {
                        node.state = ConnectionState::RedisReady {
                            db: db.clone(),
                            keyspace: redis_keys::Keyspace::Loading,
                        };
                        this.load_keyspace(index, cx);
                        cx.emit(ExplorerEvent::RedisConnected(db));
                    }
                    Ok(Connection::Document(db)) => {
                        node.state = ConnectionState::DocumentReady { db: db.clone() };
                        this.load_children(index, Vec::new(), cx);
                        cx.emit(ExplorerEvent::DocumentConnected(db));
                    }
                    Err(error) => node.state = ConnectionState::Failed(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.connections.iter().position(|node| node.spec.id == id)
    }

    /// Where the connection opened with `spec` is now: connections move
    /// while a load is out. `None` once it is gone or edited.
    fn place_of(&self, spec: &ConnectionSpec) -> Option<usize> {
        self.connections.iter().position(|node| node.spec == *spec)
    }

    /// Follows the project model: keeps open connections whose settings did
    /// not change, and refreshes the saved queries.
    fn sync_project(&mut self, cx: &mut Context<Self>) {
        let project = self.model.read(cx).project.clone();
        let old_ids: Vec<String> = self.connections.iter().map(|n| n.spec.id.clone()).collect();
        let mut old: Vec<ConnectionNode> = std::mem::take(&mut self.connections);
        self.connections = project
            .connections
            .iter()
            .map(|spec| match old.iter().position(|n| n.spec == *spec) {
                Some(position) => old.swap_remove(position),
                None => ConnectionNode {
                    spec: spec.clone(),
                    state: ConnectionState::Idle,
                },
            })
            .collect();
        let new_ids: Vec<String> = self.connections.iter().map(|n| n.spec.id.clone()).collect();
        // Tree keys use positions: if existing connections moved, start fresh.
        if !new_ids.starts_with(&old_ids[..old_ids.len().min(new_ids.len())])
            || new_ids.len() < old_ids.len()
        {
            self.expanded.clear();
            self.selected = None;
        }
        self.queries = project.queries();
        let connections = &self.connections;
        self.redis_keys.retain(|(id, _), _| {
            connections.iter().any(|node| {
                node.spec.id == *id && matches!(node.state, ConnectionState::RedisReady { .. })
            })
        });
        self.documents.retain(|(id, _), _| {
            connections.iter().any(|node| {
                node.spec.id == *id && matches!(node.state, ConnectionState::DocumentReady { .. })
            })
        });
        cx.notify();
    }

    pub fn refresh_queries(&mut self, cx: &mut Context<Self>) {
        self.queries = self.model.read(cx).project.queries();
        cx.notify();
    }

    pub fn reveal_query(&mut self, relative: Option<PathBuf>, cx: &mut Context<Self>) {
        self.selected_query = relative;
        cx.notify();
    }

    /// The open connection with this name, if any.
    pub fn db_by_name(&self, name: &str) -> Option<Db> {
        let index = self.connections.iter().position(|n| n.spec.name == name)?;
        self.db(index)
    }

    /// The connections open or opening, by name.
    pub fn open_connections(&self) -> Vec<String> {
        self.connections
            .iter()
            .filter(|node| {
                matches!(
                    node.state,
                    ConnectionState::Connecting
                        | ConnectionState::Ready { .. }
                        | ConnectionState::RedisReady { .. }
                        | ConnectionState::DocumentReady { .. }
                )
            })
            .map(|node| node.spec.name.clone())
            .collect()
    }

    /// The tree's expanded nodes, in a stable order.
    pub fn expanded(&self) -> Vec<String> {
        let mut expanded: Vec<String> = self.expanded.iter().cloned().collect();
        expanded.sort();
        expanded
    }

    pub fn set_expanded(&mut self, expanded: Vec<String>, cx: &mut Context<Self>) {
        self.expanded = expanded.into_iter().collect();
        cx.notify();
    }

    /// Starts opening the named connection if it is not open yet. Returns
    /// whether such a connection exists.
    pub fn ensure_connected(&mut self, name: &str, cx: &mut Context<Self>) -> bool {
        let Some(index) = self.connections.iter().position(|n| n.spec.name == name) else {
            return false;
        };
        self.connect(index, cx);
        true
    }

    fn load_catalog(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(db) = self.db(index) else { return };
        let spec = self.connections[index].spec.clone();
        if let Some(ConnectionState::Ready { catalog, .. }) =
            self.connections.get_mut(index).map(|n| &mut n.state)
        {
            *catalog = CatalogState::Loading;
        }
        let load = db.session.overview();
        cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                let Some(index) = this.place_of(&spec) else {
                    return;
                };
                if let Some(ConnectionState::Ready { catalog, .. }) =
                    this.connections.get_mut(index).map(|n| &mut n.state)
                {
                    *catalog = match result {
                        Ok(overview) => CatalogState::Loaded(overview),
                        Err(error) => CatalogState::Failed(describe_error(error)),
                    };
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn toggle(&mut self, key: String, cx: &mut Context<Self>) {
        if !self.expanded.remove(&key) {
            self.expanded.insert(key);
        }
        cx.notify();
    }

    fn toggle_connection(&mut self, index: usize, cx: &mut Context<Self>) {
        let key = index.to_string();
        let opening = !self.expanded.contains(&key);
        self.toggle(key, cx);
        let idle = matches!(
            self.connections.get(index).map(|n| &n.state),
            Some(ConnectionState::Idle | ConnectionState::Failed(_))
        );
        if opening && idle {
            self.expanded.insert(format!("{index}/public"));
            self.connect(index, cx);
        }
    }

    fn open_table(
        &mut self,
        index: usize,
        info: TableInfo,
        view: TableView,
        cx: &mut Context<Self>,
    ) {
        let Some(db) = self.db(index) else { return };
        self.selected = Some(format!(
            "{index}/{}/table/{}",
            info.table.schema, info.table.name
        ));
        cx.emit(ExplorerEvent::OpenTable { db, info, view });
        cx.notify();
    }

    fn open_function(&mut self, index: usize, function: FunctionInfo, cx: &mut Context<Self>) {
        let Some(db) = self.db(index) else { return };
        self.selected = Some(format!(
            "{index}/{}/function/{}/{}",
            function.schema, function.name, function.oid
        ));
        let title: SharedString = format!("{}()", function.name).into();
        let load = db.session.routine_source(function);
        cx.spawn(async move |this, cx| {
            let sql = match load.await {
                Ok(definition) => definition,
                Err(error) => format!(
                    "-- Could not read the definition: {}\n",
                    describe_error(error)
                ),
            };
            this.update(cx, |_, cx| {
                cx.emit(ExplorerEvent::OpenQuery { db, title, sql })
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn open_enum(&mut self, index: usize, info: EnumInfo, cx: &mut Context<Self>) {
        let Some(db) = self.db(index) else { return };
        self.selected = Some(format!("{index}/{}/enum/{}", info.schema, info.name));
        let sql = format!(
            "-- Values of the enum {}.{}, in order.\nselect unnest(enum_range(null::{})) as value;\n",
            info.schema,
            info.name,
            POSTGRES.quote_qualified(&info.schema, &info.name)
        );
        cx.emit(ExplorerEvent::OpenQuery {
            db,
            title: info.name.into(),
            sql,
        });
        cx.notify();
    }

    fn open_trigger(&mut self, index: usize, trigger: TriggerRef, cx: &mut Context<Self>) {
        let info = match self.connections.get(index).map(|n| &n.state) {
            Some(ConnectionState::Ready {
                catalog: CatalogState::Loaded(overview),
                ..
            }) => overview
                .tables
                .iter()
                .find(|t| t.table == trigger.table)
                .cloned(),
            _ => None,
        };
        if let Some(info) = info {
            self.open_table(index, info, TableView::Definition, cx);
            self.selected = Some(format!(
                "{index}/{}/trigger/{}",
                trigger.table.schema, trigger.name
            ));
        }
    }

    /// Closes the sidebar's connection. Tabs opened on it keep their own
    /// handle to it until they close.
    fn disconnect(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(node) = self.connections.get_mut(index) {
            node.state = ConnectionState::Idle;
            self.expanded.remove(&index.to_string());
            let id = node.spec.id.clone();
            self.redis_keys.retain(|(other, _), _| *other != id);
            self.documents.retain(|(other, _), _| *other != id);
            cx.notify();
        }
    }

    fn open_diagram(&mut self, index: usize, schema: String, cx: &mut Context<Self>) {
        if let Some(db) = self.db(index) {
            cx.emit(ExplorerEvent::OpenDiagram { db, schema });
        }
    }

    fn open_activity(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(db) = self.db(index) {
            cx.emit(ExplorerEvent::OpenActivity(db));
        }
    }

    fn new_query(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(db) = self.db(index) {
            cx.emit(ExplorerEvent::NewQuery(db));
        }
    }

    fn query_table(&mut self, index: usize, table: &TableRef, cx: &mut Context<Self>) {
        let Some(db) = self.db(index) else { return };
        let sql = select_sql(db.session.dialect(), table);
        cx.emit(ExplorerEvent::OpenQuery {
            db,
            title: table.name.clone().into(),
            sql,
        });
    }

    /// Asks, then removes the connection from the project and its password
    /// from the keychain.
    fn delete_connection(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(spec) = self.connections.get(index).map(|n| n.spec.clone()) else {
            return;
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete the connection \u{201c}{}\u{201d}?", spec.name),
            Some("It is removed from the project and its password from the keychain."),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if !matches!(answer.await, Ok(0)) {
                return;
            }
            this.update(cx, |this, cx| {
                let removed = this.model.update(cx, |model, cx| {
                    let mut project = model.project.clone();
                    project.remove_connection(&spec.id);
                    project.save()?;
                    model.project = project;
                    cx.notify();
                    Ok::<_, String>(())
                });
                match removed {
                    Ok(()) => {
                        let _ = app::state(cx).secrets.delete(&spec.id);
                    }
                    Err(error) => eprintln!("kuery: {error}"),
                }
            })
            .ok();
        })
        .detach();
    }

    /// Asks, then deletes a saved query file.
    fn delete_saved_query(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete \u{201c}{name}\u{201d}?"),
            Some("The file is deleted from the project. This cannot be undone."),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if !matches!(answer.await, Ok(0)) {
                return;
            }
            if let Err(error) = std::fs::remove_file(&path) {
                eprintln!("kuery: could not delete {}: {error}", path.display());
            }
            this.update(cx, |this, cx| this.refresh_queries(cx)).ok();
        })
        .detach();
    }

    // Render ---------------------------------------------------------------

    /// Builds the tree's rows again if the explorer changed since, or the
    /// filter did.
    fn update_rows(&mut self, cx: &App) {
        let query = self.search.read(cx).value().trim().to_lowercase();
        if !self.rows_stale && query == self.rows_query {
            return;
        }
        let tree = rows::Tree {
            expanded: &self.expanded,
            redis_keys: &self.redis_keys,
            documents: &self.documents,
            query: &query,
        };
        self.rows = rows::tree_rows(self.connections.iter().map(rows::NodeView::of), &tree);
        self.rows_query = query;
        self.rows_stale = false;
    }

    fn catalog(&self, index: usize) -> Option<&CatalogOverview> {
        match &self.connections.get(index)?.state {
            ConnectionState::Ready {
                catalog: CatalogState::Loaded(overview),
                ..
            } => Some(overview),
            _ => None,
        }
    }

    /// The element of one row, in a slot of the same height as every other
    /// row, which the list needs. A row that no longer matches the catalog
    /// (it changed since the rows were built) is left empty.
    fn render_row(&self, position: usize, cx: &mut Context<Self>) -> AnyElement {
        let row = self
            .rows
            .get(position)
            .and_then(|row| self.row_element(row, position, cx));
        div()
            .h(px(ROW_HEIGHT + ROW_GAP))
            .pb(px(ROW_GAP))
            .children(row)
            .into_any_element()
    }

    fn row_element(
        &self,
        row: &rows::Row,
        position: usize,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        use rows::Row;
        let element = match row {
            Row::Connection { index, expanded } => {
                let node = self.connections.get(*index)?;
                self.connection_row(*index, node, *expanded, cx)
            }
            Row::Note { depth, text, tone } => {
                tree_note(*depth, text.clone(), *tone).into_any_element()
            }
            Row::Error { depth, text } => {
                let (tooltip, full) = (text.clone(), text.to_string());
                tree_note(*depth, one_line(text).into(), theme::RED)
                    .id(("explorer-error", position))
                    .tooltip(move |window, cx| error_tooltip(tooltip.clone(), window, cx))
                    .context_menu(move |menu, _, _| menu.item(copy("Copy Error", full.clone())))
                    .into_any_element()
            }
            Row::EditConnection { index } => {
                let id = self.connections.get(*index)?.spec.id.clone();
                h_flex()
                    .id(SharedString::from(format!("fix-{index}")))
                    .h(px(ROW_HEIGHT))
                    .ml(px(40.))
                    .text_size(px(12.))
                    .text_color(color(0x4da3ff))
                    .hover(|s| s.text_color(color(0x80bdff)))
                    .child("Edit connection…")
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(ExplorerEvent::EditConnection(id.clone()))
                    }))
                    .into_any_element()
            }
            Row::Schema {
                index,
                schema,
                expanded,
            } => {
                let schema = self.catalog(*index)?.schemas.get(*schema)?;
                self.schema_row(*index, schema, *expanded, cx)
            }
            Row::Group {
                index,
                schema,
                group,
                count,
                open,
            } => {
                let schema = self.catalog(*index)?.schemas.get(*schema)?;
                let key = format!("{index}/{schema}/{}", group.key());
                tree_row(
                    SharedString::from(format!("group-{key}")),
                    2,
                    Some(*open),
                    false,
                )
                .child(div().flex_1().child(group.label()))
                .child(counter(count.to_string()))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle(key.clone(), cx)))
                .into_any_element()
            }
            Row::Table { index, table } => {
                let info = self.catalog(*index)?.tables.get(*table)?;
                self.table_row(*index, info, cx)
            }
            Row::Function { index, function } => {
                let function = self.catalog(*index)?.functions.get(*function)?;
                self.function_row(*index, function, cx)
            }
            Row::Trigger { index, trigger } => {
                let trigger = self.catalog(*index)?.triggers.get(*trigger)?;
                self.trigger_row(*index, trigger, cx)
            }
            Row::Enum { index, item } => {
                let info = self.catalog(*index)?.enums.get(*item)?;
                self.enum_row(*index, info, cx)
            }
            Row::RedisDatabase {
                index,
                database,
                keys,
                expanded,
            } => self.database_row(*index, *database, *keys, *expanded, cx),
            Row::RedisFolder {
                key,
                name,
                count,
                depth,
                open,
                ..
            } => self.folder_row(key, name, *count, *depth, *open, cx),
            Row::RedisKey {
                index,
                database,
                label,
                key,
                depth,
            } => self.key_row(*index, *database, label, key, *depth, cx),
            Row::RedisMore {
                index,
                database,
                count,
            } => self.more_row(*index, *database, *count, cx),
            Row::DocumentNode {
                index,
                path,
                item,
                depth,
                expanded,
            } => self.document_row(*index, path, *item, *depth, *expanded, cx)?,
            Row::DocumentMore {
                index,
                parent,
                depth,
            } => self.document_more_row(*index, parent, *depth, cx),
        };
        Some(element)
    }

    fn connection_row(
        &self,
        index: usize,
        node: &ConnectionNode,
        expanded: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let environment = node.spec.environment();
        let family = node.spec.family();
        let ready = matches!(
            node.state,
            ConnectionState::Ready { .. }
                | ConnectionState::RedisReady { .. }
                | ConnectionState::DocumentReady { .. }
        );
        let activity = matches!(
            &node.state,
            ConnectionState::Ready { db, .. } if db.session.has_activity()
        );
        // The environment's color: filled once connected, an outline before;
        // amber while connecting, red when it failed.
        let dot = match &node.state {
            ConnectionState::Ready { .. }
            | ConnectionState::RedisReady { .. }
            | ConnectionState::DocumentReady { .. } => div()
                .size(px(6.))
                .rounded_full()
                .bg(color(environment.color())),
            ConnectionState::Connecting => {
                div().size(px(6.)).rounded_full().bg(color(theme::AMBER))
            }
            ConnectionState::Failed(_) => div().size(px(6.)).rounded_full().bg(color(theme::RED)),
            ConnectionState::Idle => div()
                .size(px(6.))
                .rounded_full()
                .border_1()
                .border_color(color(environment.color())),
        };
        let group = SharedString::from(format!("connection-row-{index}"));
        tree_row(
            SharedString::from(format!("connection-{index}")),
            0,
            Some(expanded),
            false,
        )
        .group(group.clone())
        .child(
            Icon::new(
                node.spec
                    .family()
                    .map_or(Icon::from(AppIcon::Database), drivers::family_icon),
            )
            .size(px(15.))
            .text_color(color(if ready {
                theme::TEXT_SECONDARY
            } else {
                theme::TEXT_QUIET
            })),
        )
        .child(
            div()
                .flex_1()
                .truncate()
                .font_weight(FontWeight::MEDIUM)
                .text_color(color(if ready {
                    theme::TEXT
                } else {
                    theme::TEXT_SECONDARY
                }))
                .child(node.spec.name.clone()),
        )
        .child(
            div()
                .id(SharedString::from(format!("edit-{index}")))
                .px_1()
                .rounded(px(4.))
                .invisible()
                .group_hover(group.clone(), |s| s.visible())
                .hover(|s| s.bg(color(0x3a3a3e)))
                .child(
                    Icon::new(AppIcon::Pencil)
                        .size(px(11.))
                        .text_color(color(theme::TEXT_TERTIARY)),
                )
                .on_click({
                    let id = node.spec.id.clone();
                    cx.listener(move |_, _, _, cx| {
                        cx.stop_propagation();
                        cx.emit(ExplorerEvent::EditConnection(id.clone()));
                    })
                }),
        )
        .when(ready, |row| {
            row.child(
                div()
                    .id(SharedString::from(format!("refresh-{index}")))
                    .px_1()
                    .rounded(px(4.))
                    .invisible()
                    .group_hover(group.clone(), |s| s.visible())
                    .text_size(px(11.))
                    .text_color(color(theme::TEXT_TERTIARY))
                    .hover(|s| s.bg(color(0x3a3a3e)).text_color(color(theme::TEXT)))
                    .child(Icon::new(AppIcon::Reload).size(px(11.)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.refresh(index, cx);
                    })),
            )
        })
        .child(
            h_flex()
                .gap(px(5.))
                .text_size(px(10.5))
                .text_color(color(theme::TEXT_TERTIARY))
                .child(dot)
                .child(environment.short_label().to_lowercase()),
        )
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_connection(index, cx)))
        .context_menu({
            let this = cx.entity().downgrade();
            let id = node.spec.id.clone();
            let target = node.spec.target();
            move |menu, _, _| {
                let menu = if ready && family == Some(Family::KeyValue) {
                    menu.item(item_icon(
                        "Open Console",
                        AppIcon::Code,
                        &this,
                        move |this, _, cx| this.open_default_console(index, cx),
                    ))
                    .item(item_icon(
                        "Refresh Databases",
                        AppIcon::Reload,
                        &this,
                        move |this, _, cx| this.load_keyspace(index, cx),
                    ))
                    .item(item("Disconnect", &this, move |this, _, cx| {
                        this.disconnect(index, cx)
                    }))
                } else if ready && family == Some(Family::Document) {
                    menu.item(item_icon(
                        "Refresh",
                        AppIcon::Reload,
                        &this,
                        move |this, _, cx| this.refresh_documents(index, cx),
                    ))
                    .item(item("Disconnect", &this, move |this, _, cx| {
                        this.disconnect(index, cx)
                    }))
                } else if ready {
                    menu.item(item_icon(
                        "New Query",
                        IconName::Plus,
                        &this,
                        move |this, _, cx| this.new_query(index, cx),
                    ))
                    .when(activity, |menu| {
                        menu.item(item_icon(
                            "Server Activity",
                            AppIcon::Activity,
                            &this,
                            move |this, _, cx| this.open_activity(index, cx),
                        ))
                    })
                    .item(item_icon(
                        "Refresh Catalog",
                        AppIcon::Reload,
                        &this,
                        move |this, _, cx| this.load_catalog(index, cx),
                    ))
                    .item(item("Disconnect", &this, move |this, _, cx| {
                        this.disconnect(index, cx)
                    }))
                } else {
                    menu.item(item_icon(
                        "Connect",
                        AppIcon::Database,
                        &this,
                        move |this, _, cx| {
                            this.expanded.insert(index.to_string());
                            this.expanded.insert(format!("{index}/public"));
                            this.connect(index, cx);
                            // `connect` does nothing while it is connecting.
                            cx.notify();
                        },
                    ))
                };
                let id = id.clone();
                menu.separator()
                    .item(item_icon(
                        "Edit Connection…",
                        AppIcon::Pencil,
                        &this,
                        move |_, _, cx| cx.emit(ExplorerEvent::EditConnection(id.clone())),
                    ))
                    .item(copy("Copy Host and Database", target.clone()))
                    .separator()
                    .item(item(
                        "Delete Connection…",
                        &this,
                        move |this, window, cx| this.delete_connection(index, window, cx),
                    ))
            }
        })
        .into_any_element()
    }

    fn schema_row(
        &self,
        index: usize,
        schema: &str,
        expanded: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let schema_key = format!("{index}/{schema}");
        tree_row(
            SharedString::from(format!("schema-{schema_key}")),
            1,
            Some(expanded),
            false,
        )
        .child(
            Icon::new(IconName::FolderClosed)
                .size(px(14.))
                .text_color(color(theme::ACCENT)),
        )
        .child(div().flex_1().truncate().child(schema.to_owned()))
        .on_click({
            let key = schema_key.clone();
            cx.listener(move |this, _, _, cx| this.toggle(key.clone(), cx))
        })
        .context_menu({
            let this = cx.entity().downgrade();
            let name = schema.to_owned();
            move |menu, _, _| {
                let schema = name.clone();
                menu.item(item_icon(
                    "New Query",
                    IconName::Plus,
                    &this,
                    move |this, _, cx| this.new_query(index, cx),
                ))
                .item(item_icon(
                    "Open Diagram",
                    IconName::Network,
                    &this,
                    move |this, _, cx| this.open_diagram(index, schema.clone(), cx),
                ))
                .item(item_icon(
                    "Refresh Catalog",
                    AppIcon::Reload,
                    &this,
                    move |this, _, cx| this.load_catalog(index, cx),
                ))
                .separator()
                .item(copy("Copy Name", name.clone()))
            }
        })
        .into_any_element()
    }

    fn function_row(
        &self,
        index: usize,
        function: &FunctionInfo,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = format!(
            "{index}/{}/function/{}/{}",
            function.schema, function.name, function.oid
        );
        let selected = self.selected.as_deref() == Some(key.as_str());
        let function = function.clone();
        let dialect = self.dialect_at(index);
        tree_row(SharedString::from(key), 3, None, selected)
            .child(icon(AppIcon::Code, 0xc49bff, selected))
            .child(
                div()
                    .flex_1()
                    .truncate()
                    .child(format!("{}({})", function.name, function.arguments)),
            )
            .child(
                counter(function.kind.label())
                    .when(selected, |c| c.text_color(color(theme::TEXT_SECONDARY))),
            )
            .on_click({
                let function = function.clone();
                cx.listener(move |this, _, _, cx| this.open_function(index, function.clone(), cx))
            })
            .context_menu({
                let this = cx.entity().downgrade();
                move |menu, _, _| {
                    let open = function.clone();
                    let (name, signature) = function_copies(dialect, &function);
                    menu.item(item("Open Definition", &this, move |this, _, cx| {
                        this.open_function(index, open.clone(), cx)
                    }))
                    .separator()
                    .item(copy("Copy Name", name))
                    .item(copy("Copy Signature", signature))
                }
            })
            .into_any_element()
    }

    fn trigger_row(
        &self,
        index: usize,
        trigger: &TriggerRef,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = format!("{index}/{}/trigger/{}", trigger.table.schema, trigger.name);
        let selected = self.selected.as_deref() == Some(key.as_str());
        let trigger = trigger.clone();
        tree_row(SharedString::from(key), 3, None, selected)
            .child(icon(AppIcon::Sort, theme::AMBER, selected))
            .child(div().flex_1().truncate().child(trigger.name.clone()))
            .child(
                counter(format!("on {}", trigger.table.name))
                    .when(selected, |c| c.text_color(color(theme::TEXT_SECONDARY))),
            )
            .on_click({
                let trigger = trigger.clone();
                cx.listener(move |this, _, _, cx| this.open_trigger(index, trigger.clone(), cx))
            })
            .context_menu({
                let this = cx.entity().downgrade();
                move |menu, _, _| {
                    let open = trigger.clone();
                    menu.item(item("Open Table Definition", &this, move |this, _, cx| {
                        this.open_trigger(index, open.clone(), cx)
                    }))
                    .separator()
                    .item(copy("Copy Name", trigger.name.clone()))
                }
            })
            .into_any_element()
    }

    fn enum_row(&self, index: usize, info: &EnumInfo, cx: &mut Context<Self>) -> AnyElement {
        let key = format!("{index}/{}/enum/{}", info.schema, info.name);
        let selected = self.selected.as_deref() == Some(key.as_str());
        let info = info.clone();
        tree_row(SharedString::from(key), 3, None, selected)
            .child(icon(AppIcon::Filter, theme::GREEN, selected))
            .child(div().flex_1().truncate().child(info.name.clone()))
            .child(
                counter(info.values.len().to_string())
                    .when(selected, |c| c.text_color(color(theme::TEXT_SECONDARY))),
            )
            .on_click({
                let info = info.clone();
                cx.listener(move |this, _, _, cx| this.open_enum(index, info.clone(), cx))
            })
            .context_menu({
                let this = cx.entity().downgrade();
                move |menu, _, _| {
                    let open = info.clone();
                    menu.item(item("Show Values", &this, move |this, _, cx| {
                        this.open_enum(index, open.clone(), cx)
                    }))
                    .separator()
                    .item(copy(
                        "Copy Name",
                        POSTGRES.quote_qualified(&info.schema, &info.name),
                    ))
                    .item(copy("Copy Values", info.values.join(", ")))
                }
            })
            .into_any_element()
    }

    /// The SQL the connection at `index` speaks, Postgres if none.
    fn dialect_at(&self, index: usize) -> &'static Dialect {
        self.connections
            .get(index)
            .and_then(|node| node.spec.dialect())
            .unwrap_or(&kuery_core::sql::POSTGRES)
    }

    fn table_row(&self, index: usize, info: &TableInfo, cx: &mut Context<Self>) -> AnyElement {
        let dialect = self.dialect_at(index);
        let key = format!("{index}/{}/table/{}", info.table.schema, info.table.name);
        let selected = self.selected.as_deref() == Some(key.as_str());
        let editable = info.kind.is_editable();
        let rows = info.estimated_rows.map(compact).unwrap_or_default();
        let info = info.clone();
        tree_row(SharedString::from(key), 3, None, selected)
            .child(icon(
                if editable {
                    AppIcon::Table
                } else {
                    AppIcon::Code
                },
                if editable { 0x4da3ff } else { 0xc49bff },
                selected,
            ))
            .child(div().flex_1().truncate().child(info.table.name.clone()))
            .child(counter(rows).when(selected, |c| c.text_color(color(theme::TEXT_SECONDARY))))
            .on_click({
                let info = info.clone();
                cx.listener(move |this, _, _, cx| {
                    this.open_table(index, info.clone(), TableView::Data, cx)
                })
            })
            .context_menu({
                let this = cx.entity().downgrade();
                move |menu, _, _| {
                    let (data, definition, query) = (info.clone(), info.clone(), info.clone());
                    let name = dialect.quote_qualified(&info.table.schema, &info.table.name);
                    menu.item(item_icon(
                        "Open Data",
                        AppIcon::Table,
                        &this,
                        move |this, _, cx| {
                            this.open_table(index, data.clone(), TableView::Data, cx)
                        },
                    ))
                    .item(item_icon(
                        "Open Definition",
                        IconName::FileText,
                        &this,
                        move |this, _, cx| {
                            this.open_table(index, definition.clone(), TableView::Definition, cx)
                        },
                    ))
                    .item(item_icon(
                        "Query in New Tab",
                        AppIcon::Code,
                        &this,
                        move |this, _, cx| this.query_table(index, &query.table, cx),
                    ))
                    .separator()
                    .item(copy("Copy Name", name))
                    .item(copy(
                        "Copy SELECT Statement",
                        select_sql(dialect, &info.table),
                    ))
                    .separator()
                    .item(item(
                        "Refresh Catalog",
                        &this,
                        move |this, _, cx| this.load_catalog(index, cx),
                    ))
                }
            })
            .into_any_element()
    }

    fn render_queries(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if self.queries.is_empty() {
            return vec![note_row(
                0,
                "No saved queries yet. Save a query tab (⌘S) to keep it in .kuery/queries.",
                theme::TEXT_TERTIARY,
            )];
        }
        let root = self.model.read(cx).project.queries_dir();
        let mut rows = Vec::new();
        for (position, entry) in self.queries.iter().enumerate() {
            // Hidden if any parent folder is collapsed.
            let hidden = entry
                .relative
                .ancestors()
                .skip(1)
                .any(|parent| self.collapsed_folders.contains(parent));
            if hidden {
                continue;
            }
            let selected = self.selected_query.as_ref() == Some(&entry.relative);
            let relative = entry.relative.clone();
            if entry.is_dir {
                let open = !self.collapsed_folders.contains(&entry.relative);
                rows.push(
                    tree_row(
                        SharedString::from(format!("query-dir-{position}")),
                        entry.depth(),
                        Some(open),
                        false,
                    )
                    .child(
                        Icon::new(IconName::FolderClosed)
                            .size(px(14.))
                            .text_color(color(theme::ACCENT)),
                    )
                    .child(div().flex_1().truncate().child(entry.name()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.collapsed_folders.remove(&relative) {
                            this.collapsed_folders.insert(relative.clone());
                        }
                        cx.notify();
                    }))
                    .context_menu({
                        let path = root.join(&entry.relative);
                        move |menu, _, _| {
                            menu.item(reveal(path.clone()))
                                .item(copy("Copy Path", path.display().to_string()))
                        }
                    })
                    .into_any_element(),
                );
            } else {
                let path = root.join(&entry.relative);
                rows.push(
                    tree_row(
                        SharedString::from(format!("query-{position}")),
                        entry.depth(),
                        None,
                        selected,
                    )
                    .child(icon(AppIcon::Code, 0xc49bff, selected))
                    .child(div().flex_1().truncate().child(entry.name()))
                    .on_click({
                        let (path, relative) = (path.clone(), relative.clone());
                        cx.listener(move |this, _, _, cx| {
                            this.selected_query = Some(relative.clone());
                            cx.emit(ExplorerEvent::OpenSavedQuery(path.clone()));
                            cx.notify();
                        })
                    })
                    .context_menu({
                        let this = cx.entity().downgrade();
                        move |menu, _, _| {
                            let (open, delete) = (path.clone(), path.clone());
                            let relative = relative.clone();
                            menu.item(item("Open", &this, move |this, _, cx| {
                                this.selected_query = Some(relative.clone());
                                cx.emit(ExplorerEvent::OpenSavedQuery(open.clone()));
                                cx.notify();
                            }))
                            .item(reveal(path.clone()))
                            .item(copy("Copy Path", path.display().to_string()))
                            .separator()
                            .item(item(
                                "Delete…",
                                &this,
                                move |this, window, cx| {
                                    this.delete_saved_query(delete.clone(), window, cx)
                                },
                            ))
                        }
                    })
                    .into_any_element(),
                );
            }
        }
        rows
    }

    /// Saved queries and history: few enough rows to build them all.
    fn render_list(&self, rows: Vec<AnyElement>) -> AnyElement {
        div()
            .id("explorer-body")
            .flex_1()
            .overflow_y_scroll()
            .px(px(6.))
            .pb_3()
            .child(v_flex().gap(px(1.)).children(rows))
            .into_any_element()
    }

    /// Opens a History entry's SQL in a new tab on its connection.
    fn open_history(&mut self, connection: &str, sql: &str, cx: &mut Context<Self>) {
        let db = self
            .connections
            .iter()
            .position(|c| c.spec.name == connection)
            .and_then(|index| self.db(index));
        if let Some(db) = db {
            cx.emit(ExplorerEvent::OpenQuery {
                db,
                title: "History".into(),
                sql: sql.to_string(),
            });
        }
    }

    fn render_history(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if self.history.is_empty() {
            return vec![note_row(
                0,
                "Queries you run appear here.",
                theme::TEXT_TERTIARY,
            )];
        }
        let mono = cx.theme().mono_font_family.clone();
        self.history
            .iter()
            .enumerate()
            .map(|(position, entry)| {
                let first_line = entry
                    .sql
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty() && !line.starts_with("--"))
                    .unwrap_or("")
                    .to_string();
                let connection = entry.connection.clone();
                let sql = entry.sql.clone();
                v_flex()
                    .id(("history", position))
                    .px_2()
                    .py(px(6.))
                    .gap(px(2.))
                    .rounded(px(6.))
                    .hover(|s| s.bg(color(0x2e2e32)))
                    .child(
                        div()
                            .font_family(mono.clone())
                            .text_size(px(11.5))
                            .truncate()
                            .child(first_line),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .text_size(px(11.))
                            .child(div().text_color(color(theme::TEXT_TERTIARY)).child(format!(
                                "{} · {}",
                                entry.connection,
                                history::ago(entry.at)
                            )))
                            .child(
                                div()
                                    .flex_1()
                                    .truncate()
                                    .text_color(color(if entry.failed {
                                        theme::RED
                                    } else {
                                        theme::TEXT_SECONDARY
                                    }))
                                    .child(entry.outcome.clone()),
                            ),
                    )
                    .on_click({
                        let (connection, sql) = (connection.clone(), sql.clone());
                        cx.listener(move |this, _, _, cx| this.open_history(&connection, &sql, cx))
                    })
                    .context_menu({
                        let this = cx.entity().downgrade();
                        move |menu, _, _| {
                            let (connection, open) = (connection.clone(), sql.clone());
                            menu.item(item_icon(
                                "Open in New Tab",
                                AppIcon::Code,
                                &this,
                                move |this, _, cx| this.open_history(&connection, &open, cx),
                            ))
                            .item(copy("Copy SQL", sql.clone()))
                            .separator()
                            .item(item(
                                "Remove from History",
                                &this,
                                move |this, _, cx| this.remove_history_entry(position, cx),
                            ))
                        }
                    })
                    .into_any_element()
            })
            .collect()
    }
}

/// `select * from "schema"."table" limit 100;`, for a new query tab.
fn select_sql(dialect: &Dialect, table: &TableRef) -> String {
    dialect.first_rows(&dialect.quote_qualified(&table.schema, &table.name), 100)
}

/// The sidebar's title row: what it shows, then its actions.
fn section_header(title: &'static str) -> gpui_kit::Div {
    h_flex()
        .h(px(38.))
        .flex_shrink_0()
        .pl(px(14.))
        .pr_2()
        .gap(px(2.))
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color(theme::TEXT_TERTIARY))
        .child(div().flex_1().child(title))
}

/// A small icon button in the sidebar header, with a tooltip.
fn header_button(
    id: &'static str,
    icon: impl Into<Icon>,
    tooltip: &'static str,
    on_click: impl Fn(&gpui_kit::ClickEvent, &mut Window, &mut gpui_kit::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .size(px(26.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .text_color(color(theme::TEXT_SECONDARY))
        .hover(|s| s.bg(color(theme::ACTIVE)).text_color(color(theme::TEXT)))
        .child(icon.into().size(px(14.)))
        .tooltip(move |window, cx| {
            gpui_kit::component::tooltip::Tooltip::new(tooltip).build(window, cx)
        })
        .on_click(on_click)
}

/// Every row of the explorer tree is this tall, with this gap below.
const ROW_HEIGHT: f32 = 28.;
const ROW_GAP: f32 = 1.;

/// A clickable tree row with indentation and an optional disclosure chevron.
fn tree_row(
    id: SharedString,
    depth: usize,
    expanded: Option<bool>,
    selected: bool,
) -> Stateful<gpui_kit::Div> {
    let chevron = match expanded {
        Some(true) => Icon::new(IconName::ChevronDown)
            .size(px(11.))
            .text_color(color(theme::TEXT_TERTIARY))
            .into_any_element(),
        Some(false) => Icon::new(IconName::ChevronRight)
            .size(px(11.))
            .text_color(color(theme::TEXT_TERTIARY))
            .into_any_element(),
        None => div().w(px(11.)).into_any_element(),
    };
    h_flex()
        .id(id)
        .h(px(ROW_HEIGHT))
        .pl(px(8. + depth as f32 * 16.))
        .pr_2()
        .gap(px(7.))
        .rounded(px(6.))
        .text_size(px(13.))
        .text_color(color(theme::TEXT))
        .when(selected, |row| row.bg(color(theme::ACTIVE)))
        .when(!selected, |row| row.hover(|s| s.bg(color(0x242428))))
        .child(chevron)
}

fn icon(icon: AppIcon, tone: u32, selected: bool) -> Icon {
    Icon::new(icon)
        .size(px(14.))
        .text_color(color(if selected { theme::ACCENT } else { tone }))
}

fn counter(text: impl Into<SharedString>) -> gpui_kit::Div {
    div()
        .flex_shrink_0()
        .text_size(px(11.))
        .text_color(color(theme::TEXT_QUIET))
        .child(text.into())
}

/// A line of text in the explorer tree, as tall as its rows and cut to one
/// line.
fn tree_note(depth: usize, text: SharedString, tone: u32) -> gpui_kit::Div {
    h_flex()
        .h(px(ROW_HEIGHT))
        .pl(px(26. + depth as f32 * 14.))
        .pr_2()
        .text_size(px(12.))
        .text_color(color(tone))
        .child(div().flex_1().truncate().child(text))
}

fn note_row(depth: usize, text: impl Into<SharedString>, tone: u32) -> AnyElement {
    div()
        .pl(px(26. + depth as f32 * 14.))
        .pr_2()
        .py_1()
        .text_size(px(12.))
        .text_color(color(tone))
        .child(text.into())
        .into_any_element()
}

/// An error on one line, for its row: line breaks would lay it out over
/// the rows below.
fn one_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Errors this long wrap in a tooltip of a fixed width; a tooltip is laid
/// out at its content's width, so it would otherwise be one clipped line.
const WRAP_ERRORS_FROM: usize = 60;

/// The whole error, wrapped, with its line breaks.
fn error_tooltip(
    text: SharedString,
    window: &mut Window,
    cx: &mut gpui_kit::App,
) -> gpui_kit::AnyView {
    gpui_kit::component::tooltip::Tooltip::element(move |_, _| {
        div()
            .when(text.len() > WRAP_ERRORS_FROM, |d| d.w(px(420.)))
            .whitespace_normal()
            .child(text.clone())
    })
    .build(window, cx)
}

/// `18204` → `18.2k`, as in the design's sidebar.
fn compact(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

impl Focusable for Explorer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Explorer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.mode {
            Mode::Explorer => {
                self.update_rows(cx);
                uniform_list(
                    "explorer-tree",
                    self.rows.len(),
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|position| this.render_row(position, cx))
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .px(px(6.))
                .pb_3()
                .into_any_element()
            }
            Mode::Queries => self.render_list(self.render_queries(cx)),
            Mode::History => self.render_list(self.render_history(cx)),
        };
        let open = self
            .connections
            .iter()
            .filter(|c| matches!(c.state, ConnectionState::Ready { .. }))
            .count();

        let header = match self.mode {
            Mode::Explorer => section_header("EXPLORER")
                .child(header_button(
                    "new-connection",
                    IconName::Plus,
                    "New Connection",
                    cx.listener(|_, _, _, cx| cx.emit(ExplorerEvent::NewConnection)),
                ))
                .child(header_button(
                    "refresh-catalogs",
                    AppIcon::Reload,
                    "Refresh Catalogs",
                    cx.listener(|this, _, _, cx| this.refresh_catalogs(cx)),
                ))
                .child(header_button(
                    "collapse-all",
                    AppIcon::CollapseAll,
                    "Collapse All",
                    cx.listener(|this, _, _, cx| this.collapse_all(cx)),
                )),
            Mode::Queries => section_header("SAVED QUERIES")
                .child(header_button(
                    "reveal-queries",
                    IconName::FolderOpen,
                    crate::context_menus::REVEAL_LABEL,
                    cx.listener(|this, _, _, cx| {
                        let dir = this.model.read(cx).project.queries_dir();
                        let _ = std::fs::create_dir_all(&dir);
                        cx.reveal_path(&dir);
                    }),
                ))
                .child(header_button(
                    "refresh-queries",
                    AppIcon::Reload,
                    "Refresh",
                    cx.listener(|this, _, _, cx| this.refresh_queries(cx)),
                )),
            Mode::History => section_header("HISTORY").when(!self.history.is_empty(), |header| {
                header.child(header_button(
                    "clear-history",
                    AppIcon::Trash,
                    "Clear History",
                    cx.listener(|this, _, _, cx| this.clear_history(cx)),
                ))
            }),
        };

        // The workspace sizes the sidebar and draws its right edge, which
        // is also the handle that resizes it.
        v_flex()
            .key_context("Explorer")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(color(theme::SIDEBAR))
            .child(header)
            .when(self.mode == Mode::Explorer, |col| {
                col.child(
                    div()
                        .px(px(10.))
                        .pb_2()
                        .on_action(cx.listener(Self::filter_escaped))
                        .child(Input::new(&self.search).small().cleanable(true)),
                )
            })
            .child(body)
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(color(theme::HAIRLINE))
                    .text_size(px(11.5))
                    .text_color(color(theme::TEXT_TERTIARY))
                    .child(format!(
                        "{open} of {} connections open",
                        self.connections.len()
                    )),
            )
    }
}

/// What Copy Name and Copy Signature copy for a function.
fn function_copies(dialect: &Dialect, function: &FunctionInfo) -> (String, String) {
    let name = dialect.quote_qualified(&function.schema, &function.name);
    let signature = format!("{name}({})", function.arguments);
    (name, signature)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::executor::block_on;
    use gpui::TestAppContext;
    use gpui_kit::{Bounds, Point, WindowBounds, WindowOptions, px, size};
    use kuery_core::session::{Access, Opener, Session};

    use super::*;
    use crate::connection::{Environment, RedisDb};
    use crate::project::Project;

    fn spec(id: &str, driver: &str, port: u16) -> ConnectionSpec {
        ConnectionSpec {
            id: id.into(),
            name: id.into(),
            driver: driver.into(),
            host: if port == 0 {
                String::new()
            } else {
                "127.0.0.1".into()
            },
            port,
            database: if port == 0 {
                "test.sqlite".into()
            } else {
                "0".into()
            },
            user: String::new(),
            sslmode: None,
            environment: Some("development".into()),
            ssh: None,
            options: Default::default(),
        }
    }

    /// A sidebar for a project with `connections`, none open.
    fn open(
        cx: &mut TestAppContext,
        connections: Vec<ConnectionSpec>,
    ) -> (Entity<ProjectModel>, Entity<Explorer>) {
        // The sessions answer from their own threads.
        cx.executor().allow_parking();
        let root =
            std::env::temp_dir().join(format!("kuery-explorer-{}", crate::project::new_id()));
        std::fs::create_dir_all(&root).unwrap();
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(cx);
        });
        let model = cx.new(|_| ProjectModel {
            project: Project {
                root,
                name: "shop".into(),
                workspace: None,
                connections,
            },
        });
        let explorer = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(256.), px(600.)),
                })),
                ..Default::default()
            };
            let model = model.clone();
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| Explorer::new(model, window, cx))
            })
            .unwrap()
            .1
        });
        (model, explorer)
    }

    fn sqlite_db(root: &std::path::Path) -> Db {
        let path = root.join("test.sqlite");
        std::fs::write(&path, b"").unwrap();
        let opener: Opener = Arc::new(move |access| kuery_sqlite::open(&path, access));
        let session = block_on(Session::open(opener, Access::ReadWrite)).unwrap();
        Db::new(
            "b".into(),
            Environment::Development,
            session,
            "test.sqlite".into(),
            None,
        )
    }

    /// Runs the UI until `done` holds or a few seconds pass: sessions answer
    /// from their own threads, possibly after the UI goes idle.
    fn run_until(
        cx: &mut TestAppContext,
        explorer: &Entity<Explorer>,
        done: impl Fn(&Explorer) -> bool,
    ) {
        for _ in 0..500 {
            cx.run_until_parked();
            if explorer.read_with(cx, |explorer, _| done(explorer)) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn move_connections(
        cx: &mut TestAppContext,
        model: &Entity<ProjectModel>,
        change: impl FnOnce(&mut Vec<ConnectionSpec>),
    ) {
        model.update(cx, |model, cx| {
            change(&mut model.project.connections);
            cx.notify();
        });
    }

    #[gpui::test]
    fn a_catalog_reaches_its_connection_after_another_is_deleted(cx: &mut TestAppContext) {
        let (model, explorer) = open(cx, vec![spec("a", "sqlite", 0), spec("b", "sqlite", 0)]);
        let root = model.read_with(cx, |model, _| model.project.root.clone());
        let db = sqlite_db(&root);
        explorer.update(cx, |explorer, cx| {
            explorer.connections[1].state = ConnectionState::Ready {
                db,
                catalog: CatalogState::Loading,
            };
            explorer.load_catalog(1, cx);
        });
        // A goes while B's catalog loads: B is first now.
        move_connections(cx, &model, |connections| {
            connections.remove(0);
        });
        run_until(cx, &explorer, |explorer| {
            !matches!(
                explorer.connections[0].state,
                ConnectionState::Ready {
                    catalog: CatalogState::Loading,
                    ..
                }
            )
        });
        explorer.read_with(cx, |explorer, _| {
            assert_eq!(explorer.connections[0].spec.id, "b");
            assert!(matches!(
                explorer.connections[0].state,
                ConnectionState::Ready {
                    catalog: CatalogState::Loaded(_),
                    ..
                }
            ));
        });
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    #[ignore = "needs the dev Redis"]
    fn redis_keys_stay_with_their_connection_after_a_reorder(cx: &mut TestAppContext) {
        let (model, explorer) = open(
            cx,
            vec![spec("a", "redis", 63799), spec("c", "redis", 63799)],
        );
        // Kept for the test: its runtime runs the sessions.
        let engine = Box::leak(Box::new(kuery_redis::Engine::new().unwrap()));
        explorer.update(cx, |explorer, cx| {
            for (index, name) in ["a", "c"].into_iter().enumerate() {
                let target = kuery_redis::Target {
                    host: "127.0.0.1".into(),
                    port: 63799,
                    db: 0,
                    username: None,
                    password: None,
                    tls: false,
                    server_name: "127.0.0.1".into(),
                };
                let session = block_on(engine.connect(target)).unwrap();
                let db = RedisDb::new(
                    name.into(),
                    Environment::Development,
                    session,
                    "127.0.0.1:63799/0".into(),
                    None,
                );
                explorer.connections[index].state = ConnectionState::RedisReady {
                    db,
                    keyspace: redis_keys::Keyspace::Loading,
                };
            }
            explorer.toggle_database(0, 0, cx);
        });
        cx.run_until_parked();
        explorer.read_with(cx, |explorer, _| {
            assert!(explorer.key_browser(0, 0).is_some())
        });

        // C's databases are being read when the two swap places.
        explorer.update(cx, |explorer, cx| explorer.load_keyspace(1, cx));
        move_connections(cx, &model, |connections| connections.reverse());
        run_until(cx, &explorer, |explorer| {
            !matches!(
                explorer.connections[0].state,
                ConnectionState::RedisReady {
                    keyspace: redis_keys::Keyspace::Loading,
                    ..
                }
            )
        });
        explorer.read_with(cx, |explorer, _| {
            let ids: Vec<&str> = explorer
                .connections
                .iter()
                .map(|node| node.spec.id.as_str())
                .collect();
            assert_eq!(ids, ["c", "a"]);
            let loaded = |index: usize| {
                matches!(
                    explorer.connections[index].state,
                    ConnectionState::RedisReady {
                        keyspace: redis_keys::Keyspace::Loaded(_),
                        ..
                    }
                )
            };
            assert!(loaded(0), "C's databases went elsewhere");
            assert!(!loaded(1), "A got C's databases");
            assert!(explorer.key_browser(0, 0).is_none(), "C got A's keys");
            assert!(explorer.key_browser(1, 0).is_some(), "A lost its keys");
        });
        let root = model.read_with(cx, |model, _| model.project.root.clone());
        let _ = std::fs::remove_dir_all(root);
    }

    /// A document connection to the dev MongoDB, as `root`.
    fn mongo_db(name: &str) -> DocDb {
        use kuery_core::document::session::{DocumentOpener, DocumentSession};
        let options = kuery_core::driver::ConnectOptions {
            host: "127.0.0.1".into(),
            port: 27019,
            user: "kuery".into(),
            password: Some("kuery".into()),
            options: [("authSource".to_string(), "admin".to_string())].into(),
            ..Default::default()
        };
        let opener: DocumentOpener =
            Arc::new(move |access| (kuery_mongo::DRIVER.open)(&options, access));
        let catalog = block_on(DocumentSession::open(opener, Access::ReadWrite)).unwrap();
        DocDb::new(
            name.into(),
            Environment::Development,
            catalog,
            "127.0.0.1:27019/".into(),
            None,
        )
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn the_status_bar_names_a_selected_document_connection(cx: &mut TestAppContext) {
        let (model, explorer) = open(
            cx,
            vec![
                spec("a", "sqlite", 0),
                spec("b", "mongodb", 27019),
                spec("c", "mongodb", 27019),
            ],
        );
        explorer.update(cx, |explorer, _| {
            assert!(explorer.current_meta().is_none(), "nothing is open yet");
            for (index, name) in [(1, "b"), (2, "c")] {
                explorer.connections[index].state =
                    ConnectionState::DocumentReady { db: mongo_db(name) };
            }
            // No SQL connection is open, and none selected: the first open.
            assert_eq!(explorer.current_meta().unwrap().name.as_ref(), "b");
            explorer.selected = Some("2/shop".into());
            assert_eq!(explorer.current_meta().unwrap().name.as_ref(), "c");
            assert!(explorer.current_db().is_none());
        });
        let root = model.read_with(cx, |model, _| model.project.root.clone());
        let _ = std::fs::remove_dir_all(root);
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn document_nodes_stay_with_their_connection_after_a_reorder(cx: &mut TestAppContext) {
        let (model, explorer) = open(
            cx,
            vec![spec("a", "mongodb", 27019), spec("c", "mongodb", 27019)],
        );
        explorer.update(cx, |explorer, cx| {
            for (index, name) in ["a", "c"].into_iter().enumerate() {
                explorer.connections[index].state =
                    ConnectionState::DocumentReady { db: mongo_db(name) };
            }
            explorer.load_children(0, Vec::new(), cx);
        });
        let read = |explorer: &Explorer, id: &str, path: &[&str]| {
            let path = path.iter().map(|s| s.to_string()).collect();
            match explorer.documents.get(&(id.to_string(), path)) {
                Some(documents::Children::Loaded { items, .. }) => Some(
                    items
                        .iter()
                        .map(|item| item.name.clone())
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            }
        };
        run_until(cx, &explorer, |explorer| read(explorer, "a", &[]).is_some());

        // C's collections are being read when the two swap places.
        explorer.update(cx, |explorer, cx| {
            explorer.load_children(1, vec!["shop".into()], cx)
        });
        move_connections(cx, &model, |connections| connections.reverse());
        run_until(cx, &explorer, |explorer| {
            read(explorer, "c", &["shop"]).is_some()
        });
        explorer.read_with(cx, |explorer, _| {
            let ids: Vec<&str> = explorer
                .connections
                .iter()
                .map(|node| node.spec.id.as_str())
                .collect();
            assert_eq!(ids, ["c", "a"]);
            let databases = read(explorer, "a", &[]).expect("A kept its databases");
            assert!(databases.contains(&"shop".to_string()), "{databases:?}");
            let collections = read(explorer, "c", &["shop"]).expect("C got its collections");
            assert!(
                collections.contains(&"orders".to_string()),
                "{collections:?}"
            );
            assert!(
                read(explorer, "a", &["shop"]).is_none(),
                "A got C's collections"
            );
            assert!(read(explorer, "c", &[]).is_none(), "C got A's databases");
        });
        let root = model.read_with(cx, |model, _| model.project.root.clone());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn copies_function_names_quoted_for_their_database() {
        let function = FunctionInfo {
            schema: "shop".into(),
            name: "total".into(),
            arguments: "p_id int".into(),
            result: "int".into(),
            kind: kuery_core::catalog::RoutineKind::Function,
            language: "sql".into(),
            oid: 1,
        };
        assert_eq!(
            function_copies(&kuery_core::sql::MYSQL, &function),
            ("`shop`.`total`".into(), "`shop`.`total`(p_id int)".into())
        );
        assert_eq!(
            function_copies(&kuery_core::sql::POSTGRES, &function).0,
            "\"shop\".\"total\""
        );
    }

    #[test]
    fn compacts_row_counts() {
        assert_eq!(compact(86), "86");
        assert_eq!(compact(18_204), "18.2k");
        assert_eq!(compact(1_000_000), "1.0M");
    }

    #[test]
    fn esc_in_the_filter_clears_it_then_leaves_it() {
        assert_eq!(filter_escape("orders"), FilterEscape::Clear);
        assert_eq!(filter_escape(" "), FilterEscape::Clear);
        assert_eq!(filter_escape(""), FilterEscape::Leave);
    }

    #[test]
    fn puts_an_error_on_one_line() {
        assert_eq!(
            one_line("could not connect:\n  ssh: Permission denied\r\n\n(publickey)"),
            "could not connect: ssh: Permission denied (publickey)"
        );
        assert_eq!(one_line("refused"), "refused");
    }
}
