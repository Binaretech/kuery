//! Sidebar: the project's connections as a tree (connection → schema →
//! tables, views, functions, triggers and enums for Postgres; connection →
//! database → keys for Redis, see [`redis_keys`]), its saved queries, and the
//! query history.
//!
//! The sidebar owns the open connections: it opens each one when it is first
//! expanded (the first one opens on start), reading its password from the
//! system keychain, and hands an open [`Db`] to the tabs it asks the
//! workspace to open. The connection list follows the project model.

mod redis_keys;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, PromptLevel, Render, SharedString,
    Stateful, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use kuery_core::catalog::{
    CatalogOverview, EnumInfo, FunctionInfo, TableInfo, TableRef, TriggerRef,
};
use kuery_core::driver::Family;
use kuery_core::sql::{Dialect, quote_qualified};

use crate::app;
use crate::assets::AppIcon;
use crate::connection::{Db, RedisDb};
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

#[derive(Clone, Copy, PartialEq, Eq)]
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

/// One schema's objects, already filtered by the search text.
#[derive(Default)]
struct SchemaObjects {
    tables: Vec<TableInfo>,
    views: Vec<TableInfo>,
    functions: Vec<FunctionInfo>,
    triggers: Vec<TriggerRef>,
    enums: Vec<EnumInfo>,
}

impl SchemaObjects {
    fn collect(overview: &CatalogOverview, schema: &str, query: &str) -> Self {
        let matches = |name: &str| query.is_empty() || name.to_lowercase().contains(query);
        let (tables, views) = overview
            .tables
            .iter()
            .filter(|t| t.table.schema == schema && matches(&t.table.name))
            .cloned()
            .partition(|t| t.kind.is_editable());
        Self {
            tables,
            views,
            functions: overview
                .functions
                .iter()
                .filter(|f| f.schema == schema && matches(&f.name))
                .cloned()
                .collect(),
            triggers: overview
                .triggers
                .iter()
                .filter(|t| {
                    t.table.schema == schema && (matches(&t.name) || matches(&t.table.name))
                })
                .cloned()
                .collect(),
            enums: overview
                .enums
                .iter()
                .filter(|e| e.schema == schema && matches(&e.name))
                .cloned()
                .collect(),
        }
    }

    fn count(&self, group: Group) -> usize {
        match group {
            Group::Tables => self.tables.len(),
            Group::Views => self.views.len(),
            Group::Functions => self.functions.len(),
            Group::Triggers => self.triggers.len(),
            Group::Enums => self.enums.len(),
        }
    }

    fn is_empty(&self) -> bool {
        Group::ALL.iter().all(|g| self.count(*g) == 0)
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
    /// Keys read so far per (connection, Redis database).
    redis_keys: HashMap<(usize, i64), redis_keys::KeyBrowser>,
    /// Bumped on every edit of the filter, so only the last one rescans.
    filter_edits: u64,
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

        let project = model.read(cx).project.clone();
        let mut explorer = Self {
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
            filter_edits: 0,
        };
        if !explorer.connections.is_empty() {
            explorer.expanded.insert("0".into());
            explorer.expanded.insert("0/public".into());
            explorer.expanded.insert("0/public/tables".into());
            explorer.connect(0, cx);
        }
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

    /// Collapses every expanded node.
    fn collapse_all(&mut self, cx: &mut Context<Self>) {
        self.expanded.clear();
        cx.notify();
    }

    /// Reloads the catalog of every open connection.
    fn refresh_catalogs(&mut self, cx: &mut Context<Self>) {
        for index in 0..self.connections.len() {
            match self.connections[index].state {
                ConnectionState::Ready { .. } => self.load_catalog(index, cx),
                ConnectionState::RedisReady { .. } => self.load_keyspace(index, cx),
                _ => {}
            }
        }
    }

    /// Reloads one connection's catalog, or its databases for Redis.
    fn refresh(&mut self, index: usize, cx: &mut Context<Self>) {
        match self.connections.get(index).map(|node| &node.state) {
            Some(ConnectionState::RedisReady { .. }) => self.load_keyspace(index, cx),
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
        self.redis_keys.retain(|(index, _), _| {
            matches!(
                connections.get(*index).map(|node| &node.state),
                Some(ConnectionState::RedisReady { .. })
            )
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
        if let Some(ConnectionState::Ready { catalog, .. }) =
            self.connections.get_mut(index).map(|n| &mut n.state)
        {
            *catalog = CatalogState::Loading;
        }
        let load = db.session.overview();
        cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
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
        let load = db.session.function_definition(function);
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
            quote_qualified(&info.schema, &info.name)
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
            self.redis_keys.retain(|(i, _), _| *i != index);
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

    fn render_tree(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let query = self.search.read(cx).value().trim().to_lowercase();
        let searching = !query.is_empty();
        let mut rows = Vec::new();

        for (index, node) in self.connections.iter().enumerate() {
            let key = index.to_string();
            let expanded = self.expanded.contains(&key)
                || (searching
                    && matches!(
                        node.state,
                        ConnectionState::Ready { .. } | ConnectionState::RedisReady { .. }
                    ));
            rows.push(self.connection_row(index, node, expanded, cx));
            if !expanded {
                continue;
            }
            match &node.state {
                ConnectionState::Idle => {}
                ConnectionState::Connecting => {
                    rows.push(note_row(1, "Connecting…", theme::TEXT_TERTIARY))
                }
                ConnectionState::Failed(error) => {
                    rows.push(note_row(1, error.clone(), theme::RED));
                    let id = node.spec.id.clone();
                    rows.push(
                        div()
                            .id(SharedString::from(format!("fix-{index}")))
                            .ml(px(40.))
                            .mb_1()
                            .text_size(px(12.))
                            .text_color(color(0x4da3ff))
                            .hover(|s| s.text_color(color(0x80bdff)))
                            .child("Edit connection…")
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(ExplorerEvent::EditConnection(id.clone()))
                            }))
                            .into_any_element(),
                    );
                }
                ConnectionState::RedisReady { db, keyspace } => {
                    self.redis_rows(index, db, keyspace, &mut rows, cx)
                }
                ConnectionState::Ready { catalog, .. } => match catalog {
                    CatalogState::Loading => {
                        rows.push(note_row(1, "Loading catalog…", theme::TEXT_TERTIARY))
                    }
                    CatalogState::Failed(error) => {
                        rows.push(note_row(1, error.clone(), theme::RED))
                    }
                    CatalogState::Loaded(overview) => {
                        for schema in &overview.schemas {
                            let objects = SchemaObjects::collect(overview, schema, &query);
                            if searching && objects.is_empty() {
                                continue;
                            }
                            self.schema_rows(index, schema, &objects, searching, &mut rows, cx);
                        }
                    }
                },
            }
        }
        rows
    }

    fn connection_row(
        &self,
        index: usize,
        node: &ConnectionNode,
        expanded: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let environment = node.spec.environment();
        let redis = node.spec.family() == Some(Family::KeyValue);
        let ready = matches!(
            node.state,
            ConnectionState::Ready { .. } | ConnectionState::RedisReady { .. }
        );
        let activity = matches!(
            &node.state,
            ConnectionState::Ready { db, .. } if db.session.has_activity()
        );
        // The environment's color: filled once connected, an outline before;
        // amber while connecting, red when it failed.
        let dot = match &node.state {
            ConnectionState::Ready { .. } | ConnectionState::RedisReady { .. } => div()
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
                let menu = if ready && redis {
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
                            this.connect(index, cx)
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

    fn schema_rows(
        &self,
        index: usize,
        schema: &str,
        objects: &SchemaObjects,
        searching: bool,
        rows: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        let schema_key = format!("{index}/{schema}");
        let expanded = searching || self.expanded.contains(&schema_key);
        rows.push(
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
            .into_any_element(),
        );
        if !expanded {
            return;
        }

        for group in Group::ALL {
            let count = objects.count(group);
            if count == 0
                && (searching || matches!(group, Group::Triggers | Group::Enums | Group::Views))
            {
                continue;
            }
            let group_key = format!("{schema_key}/{}", group.key());
            let open = searching || self.expanded.contains(&group_key);
            rows.push(
                tree_row(
                    SharedString::from(format!("group-{group_key}")),
                    2,
                    Some(open),
                    false,
                )
                .child(div().flex_1().child(group.label()))
                .child(counter(count.to_string()))
                .on_click({
                    let key = group_key.clone();
                    cx.listener(move |this, _, _, cx| this.toggle(key.clone(), cx))
                })
                .into_any_element(),
            );
            if !open {
                continue;
            }
            match group {
                Group::Tables | Group::Views => {
                    let list = if group == Group::Tables {
                        &objects.tables
                    } else {
                        &objects.views
                    };
                    for info in list {
                        rows.push(self.table_row(index, info, cx));
                    }
                }
                Group::Functions => {
                    for function in &objects.functions {
                        let key = format!(
                            "{index}/{}/function/{}/{}",
                            function.schema, function.name, function.oid
                        );
                        let selected = self.selected.as_deref() == Some(key.as_str());
                        let function = function.clone();
                        rows.push(
                            tree_row(SharedString::from(key), 3, None, selected)
                                .child(icon(AppIcon::Code, 0xc49bff, selected))
                                .child(
                                    div().flex_1().truncate().child(format!(
                                        "{}({})",
                                        function.name, function.arguments
                                    )),
                                )
                                .child(
                                    counter(function.kind.label()).when(selected, |c| {
                                        c.text_color(color(theme::TEXT_SECONDARY))
                                    }),
                                )
                                .on_click({
                                    let function = function.clone();
                                    cx.listener(move |this, _, _, cx| {
                                        this.open_function(index, function.clone(), cx)
                                    })
                                })
                                .context_menu({
                                    let this = cx.entity().downgrade();
                                    move |menu, _, _| {
                                        let open = function.clone();
                                        menu.item(item(
                                            "Open Definition",
                                            &this,
                                            move |this, _, cx| {
                                                this.open_function(index, open.clone(), cx)
                                            },
                                        ))
                                        .separator()
                                        .item(copy(
                                            "Copy Name",
                                            quote_qualified(&function.schema, &function.name),
                                        ))
                                        .item(copy(
                                            "Copy Signature",
                                            format!(
                                                "{}({})",
                                                quote_qualified(&function.schema, &function.name),
                                                function.arguments
                                            ),
                                        ))
                                    }
                                })
                                .into_any_element(),
                        );
                    }
                }
                Group::Triggers => {
                    for trigger in &objects.triggers {
                        let key =
                            format!("{index}/{}/trigger/{}", trigger.table.schema, trigger.name);
                        let selected = self.selected.as_deref() == Some(key.as_str());
                        let trigger = trigger.clone();
                        rows.push(
                            tree_row(SharedString::from(key), 3, None, selected)
                                .child(icon(AppIcon::Sort, theme::AMBER, selected))
                                .child(div().flex_1().truncate().child(trigger.name.clone()))
                                .child(
                                    counter(format!("on {}", trigger.table.name))
                                        .when(selected, |c| {
                                            c.text_color(color(theme::TEXT_SECONDARY))
                                        }),
                                )
                                .on_click({
                                    let trigger = trigger.clone();
                                    cx.listener(move |this, _, _, cx| {
                                        this.open_trigger(index, trigger.clone(), cx)
                                    })
                                })
                                .context_menu({
                                    let this = cx.entity().downgrade();
                                    move |menu, _, _| {
                                        let open = trigger.clone();
                                        menu.item(item(
                                            "Open Table Definition",
                                            &this,
                                            move |this, _, cx| {
                                                this.open_trigger(index, open.clone(), cx)
                                            },
                                        ))
                                        .separator()
                                        .item(copy("Copy Name", trigger.name.clone()))
                                    }
                                })
                                .into_any_element(),
                        );
                    }
                }
                Group::Enums => {
                    for info in &objects.enums {
                        let key = format!("{index}/{}/enum/{}", info.schema, info.name);
                        let selected = self.selected.as_deref() == Some(key.as_str());
                        let info = info.clone();
                        rows.push(
                            tree_row(SharedString::from(key), 3, None, selected)
                                .child(icon(AppIcon::Filter, theme::GREEN, selected))
                                .child(div().flex_1().truncate().child(info.name.clone()))
                                .child(
                                    counter(info.values.len().to_string()).when(selected, |c| {
                                        c.text_color(color(theme::TEXT_SECONDARY))
                                    }),
                                )
                                .on_click({
                                    let info = info.clone();
                                    cx.listener(move |this, _, _, cx| {
                                        this.open_enum(index, info.clone(), cx)
                                    })
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
                                            quote_qualified(&info.schema, &info.name),
                                        ))
                                        .item(copy("Copy Values", info.values.join(", ")))
                                    }
                                })
                                .into_any_element(),
                        );
                    }
                }
            }
        }
    }

    fn table_row(&self, index: usize, info: &TableInfo, cx: &mut Context<Self>) -> AnyElement {
        let dialect = self
            .connections
            .get(index)
            .map(|node| node.spec.dialect())
            .unwrap_or_default();
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
fn select_sql(dialect: Dialect, table: &TableRef) -> String {
    format!(
        "select *\nfrom {}\nlimit 100;\n",
        dialect.quote_qualified(&table.schema, &table.name)
    )
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
        .h(px(28.))
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

/// `18204` → `18.2k`, as in the design's sidebar.
fn compact(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

impl Render for Explorer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.mode {
            Mode::Explorer => self.render_tree(cx),
            Mode::Queries => self.render_queries(cx),
            Mode::History => self.render_history(cx),
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

        v_flex()
            .w(px(256.))
            .h_full()
            .flex_shrink_0()
            .bg(color(theme::SIDEBAR))
            .border_r_1()
            .border_color(color(theme::HAIRLINE))
            .child(header)
            .when(self.mode == Mode::Explorer, |col| {
                col.child(
                    div()
                        .px(px(10.))
                        .pb_2()
                        .child(Input::new(&self.search).small().cleanable(true)),
                )
            })
            .child(
                div()
                    .id("explorer-body")
                    .flex_1()
                    .overflow_y_scroll()
                    .px(px(6.))
                    .pb_3()
                    .child(v_flex().gap(px(1.)).children(body)),
            )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compacts_row_counts() {
        assert_eq!(compact(86), "86");
        assert_eq!(compact(18_204), "18.2k");
        assert_eq!(compact(1_000_000), "1.0M");
    }
}
