//! Project window: custom title bar, the sidebar on the left, and query
//! and table tabs. Query and table tabs belong to the connection
//! they were opened on.

use std::path::PathBuf;

use dbadmin_core::catalog::TableInfo;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, TitleBar, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, PromptLevel, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use crate::assets::AppIcon;
use crate::commands::{
    ClearRecentProjects, CloseTab, NewConnection, NewQuery, NextTab, OpenCommandPalette,
    OpenProject, PreviousTab, ToggleSidebar,
};
use crate::connection::Db;
use crate::connection_editor::ConnectionEditor;
use crate::explorer::{Explorer, ExplorerEvent};
use crate::menus::CloseWindow;
use crate::palette::{CommandPalette, PaletteEvent};
use crate::project::{ProjectModel, query_connection};
use crate::query_tab::{self, QueryRan, QuerySaved, QueryTab};
use crate::recent::{self, RecentProject};
use crate::table_tab::{TableTab, TableView};
use crate::theme::{self, color};
use crate::ui::placeholder;

enum Tab {
    Query(Entity<QueryTab>),
    Table(Entity<TableTab>),
}

impl Tab {
    fn db(&self, cx: &App) -> Option<Db> {
        match self {
            Tab::Query(query) => Some(query.read(cx).db().clone()),
            Tab::Table(table) => Some(table.read(cx).db().clone()),
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
        }
    }
}

pub struct DbAdminView {
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

impl DbAdminView {
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
        self.tabs.iter().any(|tab| match tab {
            Tab::Query(query) => query.read(cx).is_dirty() || query.read(cx).in_transaction(),
            Tab::Table(table) => table.read(cx).has_pending(),
        })
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
                eprintln!("dbadmin: could not read {}: {error}", path.display());
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

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let db = self
            .tabs
            .get(self.active)
            .and_then(|tab| tab.db(cx))
            .or_else(|| self.explorer.read(cx).current_db());
        let project = self.model.read(cx).project.clone();
        let project_name: SharedString = project.name.clone().into();
        let workspace_name: Option<SharedString> = project.workspace.clone().map(Into::into);
        let search_shortcut = if cfg!(target_os = "macos") {
            "⌘K"
        } else {
            "Ctrl+K"
        };
        let root = project.root.clone();
        let initials: String = workspace_name
            .as_ref()
            .unwrap_or(&project_name)
            .split_whitespace()
            .filter_map(|word| word.chars().next())
            .take(2)
            .collect::<String>()
            .to_uppercase();

        let connection = match db {
            Some(db) => h_flex()
                .gap_2()
                .px_2()
                .h(px(24.))
                .rounded(px(6.))
                .bg(color(0x2a2a2d))
                .child(div().size(px(7.)).rounded_full().bg(color(theme::GREEN)))
                .child(div().font_weight(FontWeight::MEDIUM).child(db.name.clone()))
                .child(
                    div()
                        .text_color(color(theme::TEXT_SECONDARY))
                        .child(format!(
                            "{} · PostgreSQL {}",
                            db.target,
                            db.session.server_version()
                        )),
                )
                .child(
                    div()
                        .text_size(px(10.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color(db.environment.color()))
                        .child(db.environment.label()),
                ),
            None => h_flex()
                .gap_2()
                .px_2()
                .h(px(24.))
                .rounded(px(6.))
                .bg(color(0x2a2a2d))
                .text_color(color(theme::TEXT_SECONDARY))
                .child(div().size(px(7.)).rounded_full().bg(color(theme::AMBER)))
                .child("No open connection"),
        };

        TitleBar::new().child(
            h_flex()
                .w_full()
                .h_full()
                .gap_3()
                .pr_3()
                .text_size(px(12.5))
                .child(
                    Button::new("project-switcher")
                        .ghost()
                        .compact()
                        .small()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .size(px(18.))
                                        .rounded(px(5.))
                                        .bg(color(0x1fa971))
                                        .text_color(color(0x04140c))
                                        .text_size(px(8.5))
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
                                        div().text_color(color(theme::TEXT_TERTIARY)).child("/"),
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
                                menu = menu
                                    .item(PopupMenuItem::new("No other projects").disabled(true));
                            }
                            for project in recent_projects.iter() {
                                let path = project.path.clone();
                                menu =
                                    menu.item(PopupMenuItem::new(project.name.clone()).on_click(
                                        move |_, window, cx| {
                                            crate::app::request_open(
                                                Some(path.clone()),
                                                Some(window.window_handle()),
                                                cx,
                                            )
                                        },
                                    ));
                            }
                            menu.separator()
                                .menu("Open Folder…", Box::new(OpenProject))
                                .menu("Clear Recent Projects", Box::new(ClearRecentProjects))
                        }),
                )
                .child(connection)
                .child(div().flex_1())
                .child(
                    h_flex()
                        .w(px(240.))
                        .h(px(26.))
                        .px_2()
                        .gap_2()
                        .rounded(px(7.))
                        .bg(color(0x2a2a2d))
                        .text_color(color(theme::TEXT_TERTIARY))
                        .child(div().flex_1().child("Search or run a command"))
                        .child(
                            div()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_size(px(10.5))
                                .child(search_shortcut),
                        ),
                ),
        )
    }

    fn render_tab_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut strip = h_flex()
            .h(px(34.))
            .flex_shrink_0()
            .bg(color(0x19191b))
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
            };
            let connection = tab.db(cx).map(|db| db.name).unwrap_or_default();
            strip = strip.child(
                h_flex()
                    .id(("tab", index))
                    .h_full()
                    .min_w(px(150.))
                    .max_w(px(260.))
                    .px_3()
                    .gap_2()
                    .border_r_1()
                    .border_color(color(theme::HAIRLINE))
                    .text_size(px(12.5))
                    .when(active, |t| {
                        t.bg(color(theme::GROUND)).text_color(color(theme::TEXT))
                    })
                    .when(!active, |t| {
                        t.text_color(color(theme::TEXT_SECONDARY))
                            .hover(|s| s.bg(color(0x222225)))
                    })
                    .child(Icon::new(icon).size(px(13.)).text_color(color(if active {
                        theme::ACCENT
                    } else {
                        theme::TEXT_TERTIARY
                    })))
                    .child(div().truncate().child(title))
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(color(theme::TEXT_TERTIARY))
                            .child(connection),
                    )
                    .when(pending, |t| {
                        t.child(div().size(px(6.)).rounded_full().bg(color(theme::AMBER)))
                    })
                    .child(
                        div()
                            .id(("close-tab", index))
                            .px_1()
                            .rounded(px(4.))
                            .text_color(color(theme::TEXT_TERTIARY))
                            .hover(|s| s.bg(color(0x2e2e32)).text_color(color(theme::TEXT)))
                            .child("×")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close(index, window, cx);
                            })),
                    )
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.activate(index, window, cx)),
                    ),
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

impl Focusable for DbAdminView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DbAdminView {
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
            .key_context("DbAdmin")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::previous_tab))
            .on_action(cx.listener(Self::new_query))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_palette))
            .on_action(cx.listener(Self::new_connection))
            .on_action(cx.listener(Self::close_window))
            .bg(color(theme::GROUND))
            .text_color(color(theme::TEXT))
            .text_size(px(13.))
            .child(self.render_title_bar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
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
