//! A table's data grid: one page from the server, headers with types and keys,
//! server-side sorting and pending changes marked on the cells.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui_kit::component::menu::PopupMenu;
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, Icon, h_flex};
use gpui_kit::{
    AnyElement, App, Context, FontWeight, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, SharedString, Styled as _, Window, div, px,
};
use kuery_core::browse::Direction;
use kuery_core::catalog::ColumnInfo;
use kuery_core::result_set::ResultSet;

use crate::assets::AppIcon;
use crate::context_menus::{Cell, copy_cell_items};
use crate::theme::{self, color};

/// Called by the grid when a header is clicked to sort.
pub type OnSort = Rc<dyn Fn(String, Option<Direction>, &mut App)>;

/// Adds the table tab's own items to a cell's context menu.
pub type OnMenu = Rc<dyn Fn(PopupMenu, &Cell, &App) -> PopupMenu>;

#[derive(Default)]
pub struct BrowseDelegate {
    columns: Vec<ColumnInfo>,
    table_columns: Vec<Column>,
    page: ResultSet,
    /// Position of each definition column within the page.
    page_index: Vec<Option<usize>>,
    loading: bool,
    sort: Option<(String, Direction)>,
    edits: HashMap<(usize, usize), Option<String>>,
    deleted: HashSet<usize>,
    on_sort: Option<OnSort>,
    on_menu: Option<OnMenu>,
    /// The cell last right-clicked, for the row's context menu.
    menu_cell: Option<(usize, usize)>,
}

impl BrowseDelegate {
    pub fn set_on_sort(&mut self, on_sort: OnSort) {
        self.on_sort = Some(on_sort);
    }

    pub fn set_on_menu(&mut self, on_menu: OnMenu) {
        self.on_menu = Some(on_menu);
    }

    /// What the cell shows: its staged edit if there is one.
    fn shown(&self, row: usize, column: usize) -> Option<Option<&str>> {
        match self.edits.get(&(row, column)) {
            Some(value) => Some(value.as_deref()),
            None => self.original(row, column),
        }
    }

    fn cell(&self, row: usize, column: usize) -> Cell {
        Cell {
            row,
            column,
            columns: self.columns.iter().map(|c| c.name.clone()).collect(),
            values: (0..self.columns.len())
                .map(|index| self.shown(row, index).flatten().map(str::to_owned))
                .collect(),
        }
    }

    pub fn set_columns(&mut self, columns: Vec<ColumnInfo>, sort: Option<(String, Direction)>) {
        self.sort = sort;
        self.table_columns = columns
            .iter()
            .enumerate()
            .map(|(index, info)| {
                let label_len = info.name.len() + info.data_type.len();
                let width = (label_len as f32 * 7.5 + 56.0).clamp(120.0, 340.0);
                let column = Column::new(
                    SharedString::from(format!("c{index}")),
                    SharedString::from(info.name.clone()),
                )
                .width(px(width))
                .sortable();
                match &self.sort {
                    Some((name, Direction::Ascending)) if *name == info.name => column.ascending(),
                    Some((name, Direction::Descending)) if *name == info.name => {
                        column.descending()
                    }
                    _ => column,
                }
            })
            .collect();
        self.columns = columns;
        self.remap();
    }

    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
    }

    pub fn set_page(&mut self, page: ResultSet) {
        self.page = page;
        self.loading = false;
        self.remap();
    }

    /// Cell value in the loaded page, without pending changes.
    pub fn original(&self, row: usize, column: usize) -> Option<Option<&str>> {
        let index = (*self.page_index.get(column)?)?;
        self.page.cell(row, index)
    }

    pub fn mark_edit(&mut self, row: usize, column: usize, value: Option<String>) {
        self.edits.insert((row, column), value);
    }

    pub fn mark_deleted(&mut self, row: usize) {
        self.deleted.insert(row);
    }

    pub fn clear_marks(&mut self) {
        self.edits.clear();
        self.deleted.clear();
    }

    fn remap(&mut self) {
        let names = self.page.columns();
        self.page_index = self
            .columns
            .iter()
            .map(|c| names.iter().position(|n| *n == c.name))
            .collect();
    }
}

impl TableDelegate for BrowseDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.table_columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.page.rows()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.table_columns[col_ix].clone()
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let info = &self.columns[col_ix];
        h_flex()
            .size_full()
            .gap_1()
            .overflow_hidden()
            .children(info.primary_key.then(|| {
                Icon::new(AppIcon::Key)
                    .size(px(12.))
                    .text_color(color(theme::AMBER))
            }))
            .children(info.foreign_key.is_some().then(|| {
                Icon::new(AppIcon::Link)
                    .size(px(12.))
                    .text_color(color(0xc49bff))
            }))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(color(theme::TEXT))
                    .child(info.name.clone()),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(color(theme::TEXT_TERTIARY))
                    .truncate()
                    .child(info.data_type.clone()),
            )
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let (Some(on_sort), Some(info)) = (self.on_sort.clone(), self.columns.get(col_ix)) else {
            return;
        };
        let direction = match sort {
            ColumnSort::Ascending => Some(Direction::Ascending),
            ColumnSort::Descending => Some(Direction::Descending),
            ColumnSort::Default => None,
        };
        let name = info.name.clone();
        // The page is reloaded outside this table update.
        cx.defer(move |cx| on_sort(name, direction, cx));
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let deleted = self.deleted.contains(&row_ix);
        let edited = self.edits.get(&(row_ix, col_ix));
        let value = self.shown(row_ix, col_ix);
        let mut cell = div()
            .size_full()
            .flex()
            .items_center()
            .overflow_hidden()
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |table, _, _, _| {
                    table.delegate_mut().menu_cell = Some((row_ix, col_ix));
                }),
            );
        if deleted {
            cell = cell
                .bg(color(theme::RED).opacity(0.14))
                .text_color(color(0xff9a94));
        } else if edited.is_some() {
            cell = cell
                .bg(color(theme::AMBER).opacity(0.16))
                .text_color(color(0xffd58a));
        }
        let content: AnyElement = match value {
            Some(Some(text)) => div()
                .truncate()
                .child(SharedString::from(text.to_owned()))
                .into_any_element(),
            Some(None) => div()
                .italic()
                .text_color(if edited.is_some() {
                    color(0xffd58a)
                } else {
                    cx.theme().muted_foreground
                })
                .child("NULL")
                .into_any_element(),
            None => div().into_any_element(),
        };
        cell.child(content)
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let column = match self.menu_cell {
            Some((row, column)) if row == row_ix => column,
            _ => 0,
        };
        let cell = self.cell(row_ix, column);
        let menu = copy_cell_items(menu, &cell);
        match &self.on_menu {
            Some(on_menu) => on_menu(menu.separator(), &cell, cx),
            None => menu,
        }
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(color(theme::TEXT_TERTIARY))
            .child(if self.loading {
                "Loading…"
            } else {
                "No rows"
            })
    }

    fn loading(&self, _: &App) -> bool {
        self.loading && self.page.rows() == 0
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        self.original(row_ix, col_ix)
            .flatten()
            .unwrap_or_default()
            .to_owned()
    }
}
