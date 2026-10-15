//! Main window: custom title bar, the connections sidebar on the left, and
//! query and table tabs. Every tab belongs to the connection it was opened on.

use std::sync::Arc;

use dbadmin_core::catalog::TableInfo;
use dbadmin_pg::Engine;
use gpui_kit::component::{ActiveTheme as _, Icon, TitleBar, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};

use crate::assets::AppIcon;
use crate::commands::{
    CloseTab, NewQuery, NextTab, OpenCommandPalette, PreviousTab, ToggleSidebar,
};
use crate::config::Config;
use crate::connection::Db;
use crate::explorer::{Explorer, ExplorerEvent};
use crate::menus::CloseWindow;
use crate::palette::{CommandPalette, PaletteEvent};
use crate::query_tab::{self, QueryRan, QueryTab};
use crate::table_tab::{TableTab, TableView};
use crate::theme::{self, color};
use crate::ui::placeholder;

enum Tab {
    Query(Entity<QueryTab>),
    Table(Entity<TableTab>),
}

impl Tab {
    fn db(&self, cx: &App) -> Db {
        match self {
            Tab::Query(query) => query.read(cx).db().clone(),
            Tab::Table(table) => table.read(cx).db().clone(),
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
    /// The connection runtime lives as long as the view: if it is dropped,
    /// tokio cancels the sessions' tasks.
    _engine: Arc<Engine>,
    project: SharedString,
    workspace: Option<SharedString>,
    explorer: Entity<Explorer>,
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
    pub fn new(
        engine: Arc<Engine>,
        config: Config,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let explorer = cx.new(|cx| Explorer::new(engine.clone(), &config, window, cx));
        cx.subscribe_in(
            &explorer,
            window,
            |this, _, event: &ExplorerEvent, window, cx| match event {
                ExplorerEvent::Connected(db) => {
                    // The first connection to open gets a query tab to start with.
                    if this.tabs.is_empty() {
                        this.open_query(
                            db.clone(),
                            None,
                            Some(query_tab::EXAMPLE_SQL.to_string()),
                            window,
                            cx,
                        );
                    }
                    cx.notify();
                }
                ExplorerEvent::OpenTable { db, info, view } => {
                    this.open_table(db.clone(), info.clone(), *view, window, cx)
                }
                ExplorerEvent::OpenQuery { db, title, sql } => this.open_query(
                    db.clone(),
                    Some(title.clone()),
                    Some(sql.clone()),
                    window,
                    cx,
                ),
            },
        )
        .detach();

        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle, cx);
        Self {
            _engine: engine,
            project: config.project.name.clone().into(),
            workspace: config.project.workspace.clone().map(Into::into),
            explorer,
            tabs: Vec::new(),
            active: 0,
            queries_opened: 0,
            sidebar_visible: true,
            palette: None,
            focus_before_palette: None,
            focus_handle,
        }
    }

    fn open_query(
        &mut self,
        db: Db,
        title: Option<SharedString>,
        sql: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = title.unwrap_or_else(|| {
            self.queries_opened += 1;
            format!("Query {}", self.queries_opened).into()
        });
        let sql = sql.unwrap_or_else(|| "select 1;\n".into());
        let tab = cx.new(|cx| QueryTab::new(db, title, &sql, window, cx));
        let explorer = self.explorer.clone();
        cx.subscribe(&tab, move |_, _, QueryRan(entry), cx| {
            explorer.update(cx, |explorer, cx| explorer.record(entry.clone(), cx));
        })
        .detach();
        self.tabs.push(Tab::Query(tab));
        self.activate(self.tabs.len() - 1, window, cx);
    }

    fn open_new_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let db = self
            .tabs
            .get(self.active)
            .map(|tab| tab.db(cx))
            .or_else(|| self.explorer.read(cx).current_db());
        if let Some(db) = db {
            self.open_query(db, None, None, window, cx);
        }
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
            Tab::Query(_) => false,
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
        self.explorer.update(cx, |explorer, cx| match &selection {
            Some((connection, table)) => explorer.reveal_table(connection, Some(table), cx),
            None => explorer.reveal_table("", None, cx),
        });
        cx.notify();
    }

    fn close(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        // A table with unsaved changes is not closed by accident.
        if let Some(Tab::Table(table)) = self.tabs.get(index)
            && table.read(cx).has_pending()
        {
            self.activate(index, window, cx);
            return;
        }
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
        let palette = cx.new(|cx| CommandPalette::new(tables, window, cx));
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
            .map(|tab| tab.db(cx))
            .or_else(|| self.explorer.read(cx).current_db());
        let search_shortcut = if cfg!(target_os = "macos") {
            "⌘K"
        } else {
            "Ctrl+K"
        };
        let initials: String = self
            .workspace
            .as_ref()
            .unwrap_or(&self.project)
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
                                .child(self.project.clone()),
                        )
                        .when_some(self.workspace.clone(), |row, workspace| {
                            row.child(div().text_color(color(theme::TEXT_TERTIARY)).child("/"))
                                .child(
                                    div()
                                        .text_color(color(theme::TEXT_SECONDARY))
                                        .child(workspace),
                                )
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
                Tab::Query(query) => (AppIcon::Code, query.read(cx).title(), false),
                Tab::Table(table) => {
                    let table = table.read(cx);
                    (
                        AppIcon::Table,
                        SharedString::from(table.info().table.name.clone()),
                        table.has_pending(),
                    )
                }
            };
            let connection = tab.db(cx).name;
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
            None => placeholder("Open a table from the sidebar, or a connection to start a query"),
        }
    }
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
