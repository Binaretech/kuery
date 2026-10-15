//! Sidebar: the project's connections as a tree (connection → schema →
//! tables, views, functions, triggers and enums), plus the query history.
//!
//! The sidebar owns the connections: it opens each one when it is first
//! expanded (the first one opens on start) and hands an open [`Db`] to the
//! tabs it asks the workspace to open.

use std::collections::HashSet;
use std::sync::Arc;

use dbadmin_core::catalog::{
    CatalogOverview, EnumInfo, FunctionInfo, TableInfo, TableRef, TriggerRef,
};
use dbadmin_core::sql::quote_qualified;
use dbadmin_pg::{Engine, catalog};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use crate::assets::AppIcon;
use crate::config::{Config, ConnectionConfig};
use crate::connection::{Db, describe_target};
use crate::query_tab::describe_error;
use crate::table_tab::TableView;
use crate::theme::{self, color};
use crate::ui::{segment_body, segmented};

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
}

/// A query that ran in this session, newest first in the History list.
#[derive(Clone)]
pub struct HistoryEntry {
    pub connection: SharedString,
    pub sql: String,
    pub outcome: String,
    pub failed: bool,
}

enum ConnectionState {
    Idle,
    Connecting,
    Ready { db: Db, catalog: CatalogState },
    Failed(String),
}

enum CatalogState {
    Loading,
    Loaded(CatalogOverview),
    Failed(String),
}

struct ConnectionNode {
    config: ConnectionConfig,
    state: ConnectionState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Explorer,
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
    engine: Arc<Engine>,
    connections: Vec<ConnectionNode>,
    /// Keys of expanded nodes: `"0"`, `"0/public"`, `"0/public/tables"`.
    expanded: HashSet<String>,
    selected: Option<String>,
    search: Entity<InputState>,
    mode: Mode,
    history: Vec<HistoryEntry>,
}

impl EventEmitter<ExplorerEvent> for Explorer {}

impl Explorer {
    pub fn new(
        engine: Arc<Engine>,
        config: &Config,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search objects…"));
        cx.subscribe(&search, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        })
        .detach();

        let mut explorer = Self {
            engine,
            connections: config
                .connections
                .iter()
                .cloned()
                .map(|config| ConnectionNode {
                    config,
                    state: ConnectionState::Idle,
                })
                .collect(),
            expanded: HashSet::new(),
            selected: None,
            search,
            mode: Mode::Explorer,
            history: Vec::new(),
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
    pub fn current_db(&self) -> Option<Db> {
        let selected = self
            .selected
            .as_ref()
            .and_then(|key| key.split('/').next()?.parse::<usize>().ok())
            .and_then(|index| self.db(index));
        selected.or_else(|| (0..self.connections.len()).find_map(|index| self.db(index)))
    }

    pub fn record(&mut self, entry: HistoryEntry, cx: &mut Context<Self>) {
        self.history.insert(0, entry);
        self.history.truncate(200);
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
                .position(|c| c.config.name == connection)?;
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
            ConnectionState::Connecting | ConnectionState::Ready { .. }
        ) {
            return;
        }
        node.state = ConnectionState::Connecting;
        let config = node.config.clone();
        let connect = self
            .engine
            .connect_with(config.url.clone(), config.environment.access());
        cx.spawn(async move |this, cx| {
            let result = connect.await;
            this.update(cx, |this, cx| {
                let Some(node) = this.connections.get_mut(index) else {
                    return;
                };
                match result {
                    Ok(session) => {
                        let db = Db {
                            name: config.name.clone().into(),
                            environment: config.environment,
                            session,
                            target: describe_target(&config.url).into(),
                        };
                        node.state = ConnectionState::Ready {
                            db: db.clone(),
                            catalog: CatalogState::Loading,
                        };
                        this.load_catalog(index, cx);
                        cx.emit(ExplorerEvent::Connected(db));
                    }
                    Err(error) => node.state = ConnectionState::Failed(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn load_catalog(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(db) = self.db(index) else { return };
        if let Some(ConnectionState::Ready { catalog, .. }) =
            self.connections.get_mut(index).map(|n| &mut n.state)
        {
            *catalog = CatalogState::Loading;
        }
        let load = catalog::overview(db.session.clone());
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
        let load = catalog::function_definition(db.session.clone(), function);
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

    // Render ---------------------------------------------------------------

    fn render_tree(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let query = self.search.read(cx).value().trim().to_lowercase();
        let searching = !query.is_empty();
        let mut rows = Vec::new();

        for (index, node) in self.connections.iter().enumerate() {
            let key = index.to_string();
            let expanded = self.expanded.contains(&key)
                || (searching && matches!(node.state, ConnectionState::Ready { .. }));
            rows.push(self.connection_row(index, node, expanded, cx));
            if !expanded {
                continue;
            }
            match &node.state {
                ConnectionState::Idle => {}
                ConnectionState::Connecting => {
                    rows.push(note_row(1, "Connecting…", theme::TEXT_TERTIARY))
                }
                ConnectionState::Failed(error) => rows.push(note_row(1, error.clone(), theme::RED)),
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
        let environment = node.config.environment;
        let dot = match &node.state {
            ConnectionState::Idle => div()
                .size(px(8.))
                .rounded_full()
                .border_1()
                .border_color(color(theme::TEXT_TERTIARY)),
            ConnectionState::Connecting => {
                div().size(px(8.)).rounded_full().bg(color(theme::AMBER))
            }
            ConnectionState::Ready { .. } => {
                div().size(px(8.)).rounded_full().bg(color(theme::GREEN))
            }
            ConnectionState::Failed(_) => div().size(px(8.)).rounded_full().bg(color(theme::RED)),
        };
        let ready = matches!(node.state, ConnectionState::Ready { .. });
        tree_row(
            SharedString::from(format!("connection-{index}")),
            0,
            Some(expanded),
            false,
        )
        .child(
            Icon::new(AppIcon::Database)
                .size(px(14.))
                .text_color(color(theme::TEXT_SECONDARY)),
        )
        .child(dot)
        .child(
            div()
                .flex_1()
                .truncate()
                .font_weight(FontWeight::MEDIUM)
                .child(node.config.name.clone()),
        )
        .when(ready, |row| {
            row.child(
                div()
                    .id(SharedString::from(format!("refresh-{index}")))
                    .px_1()
                    .rounded(px(4.))
                    .text_size(px(11.))
                    .text_color(color(theme::TEXT_TERTIARY))
                    .hover(|s| s.bg(color(0x3a3a3e)).text_color(color(theme::TEXT)))
                    .child("↻")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.load_catalog(index, cx);
                    })),
            )
        })
        .child(
            div()
                .text_size(px(10.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color(environment.color()))
                .child(environment.short_label()),
        )
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_connection(index, cx)))
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
                                    counter(function.kind.label())
                                        .when(selected, |c| c.text_color(color(0xdde9ff))),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_function(index, function.clone(), cx)
                                }))
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
                                        .when(selected, |c| c.text_color(color(0xdde9ff))),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_trigger(index, trigger.clone(), cx)
                                }))
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
                                    counter(info.values.len().to_string())
                                        .when(selected, |c| c.text_color(color(0xdde9ff))),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_enum(index, info.clone(), cx)
                                }))
                                .into_any_element(),
                        );
                    }
                }
            }
        }
    }

    fn table_row(&self, index: usize, info: &TableInfo, cx: &mut Context<Self>) -> AnyElement {
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
            .children(info.rls_enabled.then(|| {
                Icon::new(AppIcon::Shield)
                    .size(px(11.))
                    .text_color(color(if selected { 0xffffff } else { theme::GREEN }))
            }))
            .child(counter(rows).when(selected, |c| c.text_color(color(0xdde9ff))))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_table(index, info.clone(), TableView::Data, cx)
            }))
            .into_any_element()
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
                            .child(
                                div()
                                    .text_color(color(theme::TEXT_TERTIARY))
                                    .child(entry.connection.clone()),
                            )
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
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let db = this
                            .connections
                            .iter()
                            .position(|c| c.config.name == connection.as_ref())
                            .and_then(|index| this.db(index));
                        if let Some(db) = db {
                            cx.emit(ExplorerEvent::OpenQuery {
                                db,
                                title: "History".into(),
                                sql: sql.clone(),
                            });
                        }
                    }))
                    .into_any_element()
            })
            .collect()
    }
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
        .h(px(26.))
        .pl(px(8. + depth as f32 * 14.))
        .pr_2()
        .gap(px(7.))
        .rounded(px(6.))
        .text_size(px(13.))
        .when(selected, |row| {
            row.bg(color(theme::SELECTION)).text_color(color(0xffffff))
        })
        .when(!selected, |row| {
            row.text_color(color(theme::TEXT))
                .hover(|s| s.bg(color(0x2e2e32)))
        })
        .child(chevron)
}

fn icon(icon: AppIcon, tone: u32, selected: bool) -> Icon {
    Icon::new(icon)
        .size(px(14.))
        .text_color(color(if selected { 0xffffff } else { tone }))
}

fn counter(text: impl Into<SharedString>) -> gpui_kit::Div {
    div()
        .flex_shrink_0()
        .text_size(px(11.5))
        .text_color(color(theme::TEXT_TERTIARY))
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
            Mode::History => self.render_history(cx),
        };
        let open = self
            .connections
            .iter()
            .filter(|c| matches!(c.state, ConnectionState::Ready { .. }))
            .count();

        v_flex()
            .w(px(272.))
            .h_full()
            .flex_shrink_0()
            .bg(color(theme::SIDEBAR))
            .border_r_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                v_flex()
                    .p_2()
                    .gap_2()
                    .child(
                        segmented()
                            .child(
                                segment_body("Explorer", self.mode == Mode::Explorer)
                                    .flex_1()
                                    .justify_center()
                                    .id("mode-explorer")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.mode = Mode::Explorer;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                segment_body("History", self.mode == Mode::History)
                                    .flex_1()
                                    .justify_center()
                                    .id("mode-history")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.mode = Mode::History;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .when(self.mode == Mode::Explorer, |col| {
                        col.child(Input::new(&self.search).small().cleanable(true))
                    }),
            )
            .when(self.mode == Mode::Explorer, |col| {
                col.child(
                    div()
                        .px_4()
                        .pt_1()
                        .pb_1()
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color(theme::TEXT_TERTIARY))
                        .child("Connections"),
                )
            })
            .child(
                div()
                    .id("explorer-body")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_2()
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
