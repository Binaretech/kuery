//! Results table: a `TableDelegate` over the columnar [`ResultSet`].
//!
//! The table is virtualized; only visible cells are requested and formatted.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::menu::PopupMenu;
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::{
    AnyElement, App, Context, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, SharedString, Styled as _, Window, div, px,
};
use kuery_core::result_set::{Batch, ResultSet};

use crate::context_menus::{Cell, copy_cell_items};
use crate::theme;

#[derive(Default)]
pub struct ResultsDelegate {
    set: ResultSet,
    columns: Vec<Column>,
    running: bool,
    /// The cell last right-clicked, for the row's context menu.
    menu_cell: Option<(usize, usize)>,
}

impl ResultsDelegate {
    /// Prepares a new query: clears the table and marks it as running.
    pub fn start(&mut self) {
        *self = Self {
            running: true,
            ..Self::default()
        };
    }

    pub fn set_columns(&mut self, names: Vec<String>) {
        self.columns = names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let width = (name.len() as f32 * 8.0 + 40.0).clamp(140.0, 320.0);
                Column::new(
                    SharedString::from(format!("c{index}")),
                    SharedString::from(name.clone()),
                )
                .width(px(width))
            })
            .collect();
        self.set = ResultSet::new(names);
    }

    pub fn push(&mut self, batch: Batch) {
        self.set.push(batch);
    }

    pub fn finish(&mut self) {
        self.running = false;
    }

    pub fn rows(&self) -> usize {
        self.set.rows()
    }

    pub fn memory_size(&self) -> usize {
        self.set.memory_size()
    }

    fn cell(&self, row: usize, column: usize) -> Cell {
        Cell {
            row,
            column,
            columns: self.set.columns().to_vec(),
            values: (0..self.set.columns().len())
                .map(|index| self.set.cell(row, index).flatten().map(str::to_owned))
                .collect(),
        }
    }
}

impl TableDelegate for ResultsDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.set.rows()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let cell: AnyElement = match self.set.cell(row_ix, col_ix) {
            Some(Some(text)) => div()
                .font_family(mono)
                .truncate()
                .child(SharedString::from(text.to_owned()))
                .into_any_element(),
            Some(None) => div()
                .font_family(mono)
                .italic()
                .text_color(cx.theme().muted_foreground)
                .child("NULL")
                .into_any_element(),
            None => div().into_any_element(),
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |table, _, _, _| {
                    table.delegate_mut().menu_cell = Some((row_ix, col_ix));
                }),
            )
            .child(cell)
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let column = match self.menu_cell {
            Some((row, column)) if row == row_ix => column,
            _ => 0,
        };
        copy_cell_items(menu, &self.cell(row_ix, column))
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
            .text_color(theme::color(theme::TEXT_TERTIARY))
            .child(if self.running {
                "Running…"
            } else {
                "No rows"
            })
    }

    fn loading(&self, _: &App) -> bool {
        self.running && self.set.rows() == 0 && !self.columns.is_empty()
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        match self.set.cell(row_ix, col_ix) {
            Some(Some(text)) => text.to_owned(),
            _ => String::new(),
        }
    }
}
