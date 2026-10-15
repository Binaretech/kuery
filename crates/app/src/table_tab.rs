//! A table's tab: server-side paginated data with filters and sorting, its
//! definition, and row editing with pending changes.
//!
//! Writing is only possible if the connection allows it (never in production),
//! if it is a table (not a view) and if it has a primary key. Changes are
//! staged, the exact SQL is shown, and they are applied together in one
//! transaction.

use std::rc::Rc;
use std::time::{Duration, Instant};

use dbadmin_core::browse::{Browse, Direction, Filter, FilterOp, Sort};
use dbadmin_core::catalog::{ColumnInfo, TableDefinition, TableInfo};
use dbadmin_core::edit::{self, CellChange, EditTarget, KeyValue, RowChange, WriteStatement};
use dbadmin_pg::{RunOptions, catalog};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IndexPath, Selectable as _, Sizable as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, Window, actions, div, px,
};

actions!(dbadmin, [ReloadTable, SaveTableChanges]);

use crate::assets::AppIcon;
use crate::connection::Db;
use crate::definition;
use crate::grid::BrowseDelegate;
use crate::query_tab::describe_error;
use crate::theme::{self, color};
use crate::ui::{format_duration, muted, placeholder, segment_body, segmented, thousands};

type TextSelect = SelectState<SearchableVec<SharedString>>;

const PAGE_SIZES: [u64; 3] = [100, 500, 1000];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TableView {
    Data,
    Definition,
}

enum Load<T> {
    Loading,
    Ready(T),
    Failed(String),
}

enum PageState {
    Idle,
    Loading,
    Loaded { rows: usize, elapsed: Duration },
    Failed(String),
}

enum ApplyState {
    Idle,
    Applying,
    Applied(u64),
    Failed(String),
}

struct FilterRow {
    column: Entity<TextSelect>,
    op: Entity<TextSelect>,
    value: Entity<InputState>,
}

enum InspectorMode {
    Edit { row: usize, key: Vec<KeyValue> },
    Insert,
}

struct Field {
    column: ColumnInfo,
    input: Entity<InputState>,
    original: Option<String>,
    null: bool,
}

impl Field {
    fn value(&self, cx: &gpui_kit::App) -> Option<String> {
        if self.null {
            None
        } else {
            Some(self.input.read(cx).value().to_string())
        }
    }
}

struct Inspector {
    mode: InspectorMode,
    fields: Vec<Field>,
    error: Option<String>,
}

struct Pending {
    summary: String,
    statement: WriteStatement,
}

pub struct TableTab {
    db: Db,
    info: TableInfo,
    definition: Load<TableDefinition>,
    view: TableView,
    browse: Option<Browse>,
    grid: Entity<TableState<BrowseDelegate>>,
    page: PageState,
    total: Option<Result<u64, String>>,
    filters: Vec<FilterRow>,
    show_filters: bool,
    page_size: Entity<TextSelect>,
    inspector: Option<Inspector>,
    pending: Vec<Pending>,
    show_sql: bool,
    apply: ApplyState,
    page_task: Option<Task<()>>,
    count_task: Option<Task<()>>,
    apply_task: Option<Task<()>>,
    focus_handle: FocusHandle,
}

impl TableTab {
    pub fn new(
        db: Db,
        info: TableInfo,
        view: TableView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let grid = cx.new(|cx| TableState::new(BrowseDelegate::default(), window, cx));
        let weak = cx.entity().downgrade();
        grid.update(cx, |grid, _| {
            grid.delegate_mut()
                .set_on_sort(Rc::new(move |column, direction, cx| {
                    weak.update(cx, |tab, cx| tab.set_sort(column, direction, cx))
                        .ok();
                }));
        });
        cx.subscribe_in(&grid, window, |this, _, event: &TableEvent, window, cx| {
            if let TableEvent::SelectRow(row) = event {
                this.open_edit(*row, window, cx);
            }
        })
        .detach();

        let sizes: Vec<SharedString> = PAGE_SIZES
            .iter()
            .map(|n| format!("{n} rows").into())
            .collect();
        let page_size = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(sizes),
                Some(IndexPath::new(0)),
                window,
                cx,
            )
        });
        cx.subscribe(
            &page_size,
            |this, select, _: &SelectEvent<SearchableVec<SharedString>>, cx| {
                let index = select
                    .read(cx)
                    .selected_index(cx)
                    .map(|i| i.row)
                    .unwrap_or(0);
                if let Some(browse) = this.browse.as_mut() {
                    browse.page_size = PAGE_SIZES[index.min(PAGE_SIZES.len() - 1)];
                    browse.page = 0;
                }
                this.load_page(cx);
            },
        )
        .detach();

        let load = catalog::definition(db.session.clone(), info.table.clone());
        cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                this.on_definition(result.map_err(describe_error), cx)
            })
            .ok();
        })
        .detach();

        Self {
            db,
            info,
            definition: Load::Loading,
            view,
            browse: None,
            grid,
            page: PageState::Idle,
            total: None,
            filters: Vec::new(),
            show_filters: false,
            page_size,
            inspector: None,
            pending: Vec::new(),
            show_sql: false,
            apply: ApplyState::Idle,
            page_task: None,
            count_task: None,
            apply_task: None,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn info(&self) -> &TableInfo {
        &self.info
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn set_view(&mut self, view: TableView, cx: &mut Context<Self>) {
        self.view = view;
        cx.notify();
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    fn definition(&self) -> Option<&TableDefinition> {
        match &self.definition {
            Load::Ready(definition) => Some(definition),
            _ => None,
        }
    }

    fn on_definition(&mut self, result: Result<TableDefinition, String>, cx: &mut Context<Self>) {
        match result {
            Ok(definition) => {
                let names = definition.columns.iter().map(|c| c.name.clone()).collect();
                self.browse = Some(Browse::new(self.info.table.clone(), names));
                let columns = definition.columns.clone();
                self.grid.update(cx, |grid, cx| {
                    grid.delegate_mut().set_columns(columns, None);
                    grid.refresh(cx);
                });
                self.definition = Load::Ready(definition);
                self.load_page(cx);
            }
            Err(error) => self.definition = Load::Failed(error),
        }
        cx.notify();
    }

    /// Why editing is not possible, or `None` if it is.
    fn read_only_reason(&self) -> Option<&'static str> {
        if !self.db.can_write() {
            Some("Read-only connection: row editing is disabled in production.")
        } else if !self.info.kind.is_editable() {
            Some("Views cannot be edited row by row.")
        } else if self
            .definition()
            .is_some_and(|d| d.primary_key().is_empty())
        {
            Some("The table has no primary key: rows cannot be identified for editing.")
        } else {
            None
        }
    }

    fn edit_target(&self) -> Option<EditTarget> {
        let definition = self.definition()?;
        Some(EditTarget {
            table: self.info.table.clone(),
            columns: definition.columns.iter().map(|c| c.name.clone()).collect(),
            primary_key: definition.primary_key(),
        })
    }

    // Data -----------------------------------------------------------------

    fn load_page(&mut self, cx: &mut Context<Self>) {
        let (Some(browse), Some(definition)) = (&self.browse, self.definition()) else {
            return;
        };
        let pk = definition.primary_key();
        let (page_sql, count_sql) = match (browse.page_sql(&pk), browse.count_sql()) {
            (Ok(page), Ok(count)) => (page, count),
            (Err(error), _) | (_, Err(error)) => {
                self.page = PageState::Failed(error.to_string());
                cx.notify();
                return;
            }
        };

        self.page = PageState::Loading;
        self.inspector = None;
        self.grid.update(cx, |grid, cx| {
            grid.delegate_mut().set_loading(true);
            grid.clear_selection(cx);
            cx.notify();
        });

        let started = Instant::now();
        let page = self.db.session.fetch_all(page_sql, RunOptions::default());
        self.page_task = Some(cx.spawn(async move |this, cx| {
            let result = page.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(set) => {
                        this.page = PageState::Loaded {
                            rows: set.rows(),
                            elapsed: started.elapsed(),
                        };
                        this.grid.update(cx, |grid, cx| {
                            grid.delegate_mut().clear_marks();
                            grid.delegate_mut().set_page(set);
                            grid.refresh(cx);
                        });
                    }
                    Err(error) => {
                        this.page = PageState::Failed(describe_error(error));
                        this.grid.update(cx, |grid, cx| {
                            grid.delegate_mut().set_loading(false);
                            cx.notify();
                        });
                    }
                }
                cx.notify();
            })
            .ok();
        }));

        // Counting can be expensive on huge tables: it gets its own timeout.
        let count = self.db.session.fetch_all(
            count_sql,
            RunOptions {
                statement_timeout: Duration::from_secs(5),
                ..RunOptions::default()
            },
        );
        self.count_task = Some(cx.spawn(async move |this, cx| {
            let result = count.await;
            this.update(cx, |this, cx| {
                this.total = Some(result.map_err(describe_error).and_then(|set| {
                    set.cell(0, 0)
                        .flatten()
                        .and_then(|n| n.parse().ok())
                        .ok_or_else(|| "?".into())
                }));
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn set_sort(&mut self, column: String, direction: Option<Direction>, cx: &mut Context<Self>) {
        if self.has_pending() {
            return;
        }
        let Some(browse) = self.browse.as_mut() else {
            return;
        };
        browse.sort = direction
            .map(|direction| {
                vec![Sort {
                    column: column.clone(),
                    direction,
                }]
            })
            .unwrap_or_default();
        browse.page = 0;
        self.load_page(cx);
    }

    fn clear_sort(&mut self, cx: &mut Context<Self>) {
        let Some(columns) = self.definition().map(|d| d.columns.clone()) else {
            return;
        };
        if let Some(browse) = self.browse.as_mut() {
            browse.sort.clear();
            browse.page = 0;
            self.grid.update(cx, |grid, cx| {
                grid.delegate_mut().set_columns(columns, None);
                grid.refresh(cx);
            });
            self.load_page(cx);
        }
    }

    fn go_to_page(&mut self, page: u64, cx: &mut Context<Self>) {
        if let Some(browse) = self.browse.as_mut() {
            browse.page = page;
            self.load_page(cx);
        }
    }

    fn page_count(&self) -> Option<u64> {
        let browse = self.browse.as_ref()?;
        match &self.total {
            Some(Ok(total)) => Some(total.div_ceil(browse.page_size.max(1)).max(1)),
            _ => None,
        }
    }

    // Filters --------------------------------------------------------------

    fn toggle_filters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_filters = !self.show_filters;
        if self.show_filters && self.filters.is_empty() {
            self.add_filter(window, cx);
        }
        cx.notify();
    }

    fn add_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(definition) = self.definition() else {
            return;
        };
        let columns: Vec<SharedString> = definition
            .columns
            .iter()
            .map(|c| c.name.clone().into())
            .collect();
        let ops: Vec<SharedString> = FilterOp::ALL.iter().map(|op| op.label().into()).collect();
        let column = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(columns),
                Some(IndexPath::new(0)),
                window,
                cx,
            )
        });
        let op = cx.new(|cx| {
            SelectState::new(SearchableVec::new(ops), Some(IndexPath::new(0)), window, cx)
        });
        let value = cx.new(|cx| InputState::new(window, cx).placeholder("value"));
        cx.subscribe(
            &op,
            |_, _, _: &SelectEvent<SearchableVec<SharedString>>, cx| cx.notify(),
        )
        .detach();
        cx.subscribe_in(&value, window, |this, _, event: &InputEvent, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.apply_filters(cx);
            }
        })
        .detach();
        self.filters.push(FilterRow { column, op, value });
        cx.notify();
    }

    fn remove_filter(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.filters.len() {
            self.filters.remove(index);
        }
        self.apply_filters(cx);
    }

    fn apply_filters(&mut self, cx: &mut Context<Self>) {
        if self.has_pending() {
            return;
        }
        let filters: Vec<Filter> = self
            .filters
            .iter()
            .filter_map(|row| {
                let column = row.column.read(cx).selected_value()?.to_string();
                let op_index = row
                    .op
                    .read(cx)
                    .selected_index(cx)
                    .map(|i| i.row)
                    .unwrap_or(0);
                Some(Filter {
                    column,
                    op: FilterOp::ALL[op_index.min(FilterOp::ALL.len() - 1)],
                    value: row.value.read(cx).value().to_string(),
                })
            })
            .collect();
        if let Some(browse) = self.browse.as_mut() {
            browse.filters = filters;
            browse.page = 0;
        }
        self.load_page(cx);
    }

    fn active_filters(&self) -> usize {
        self.browse.as_ref().map(|b| b.filters.len()).unwrap_or(0)
    }

    // Inspector and changes ------------------------------------------------

    fn open_edit(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(definition) = self.definition() else {
            return;
        };
        let columns = definition.columns.clone();
        let pk = definition.primary_key();
        let originals: Vec<Option<String>> = {
            let grid = self.grid.read(cx).delegate();
            (0..columns.len())
                .map(|index| grid.original(row, index).flatten().map(str::to_owned))
                .collect()
        };
        let key: Vec<KeyValue> = columns
            .iter()
            .zip(&originals)
            .filter(|(column, _)| pk.contains(&column.name))
            .filter_map(|(column, value)| {
                value.clone().map(|value| KeyValue {
                    column: column.name.clone(),
                    value,
                })
            })
            .collect();
        let fields = columns
            .into_iter()
            .zip(originals)
            .map(|(column, original)| self.field(column, original, window, cx))
            .collect();
        self.inspector = Some(Inspector {
            mode: InspectorMode::Edit { row, key },
            fields,
            error: None,
        });
        cx.notify();
    }

    fn open_insert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(definition) = self.definition() else {
            return;
        };
        let columns: Vec<ColumnInfo> = definition
            .columns
            .iter()
            .filter(|c| !c.generated)
            .cloned()
            .collect();
        let fields = columns
            .into_iter()
            .map(|column| {
                let mut field = self.field(column, None, window, cx);
                // Empty = the column's DEFAULT, not NULL.
                field.null = false;
                field
            })
            .collect();
        self.inspector = Some(Inspector {
            mode: InspectorMode::Insert,
            fields,
            error: None,
        });
        self.grid.update(cx, |grid, cx| grid.clear_selection(cx));
        cx.notify();
    }

    fn field(
        &self,
        column: ColumnInfo,
        original: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Field {
        let hint = match (&original, &column.default) {
            (None, Some(default)) => format!("DEFAULT {default}"),
            (None, None) => "NULL".to_string(),
            _ => String::new(),
        };
        let initial = original.clone().unwrap_or_default();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(hint)
                .default_value(initial)
        });
        let name = column.name.clone();
        cx.subscribe(&input, move |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change)
                && let Some(field) = this.inspector.as_mut().and_then(|inspector| {
                    inspector.fields.iter_mut().find(|f| f.column.name == name)
                })
            {
                field.null = false;
                cx.notify();
            }
        })
        .detach();
        Field {
            null: original.is_none(),
            column,
            input,
            original,
        }
    }

    fn toggle_null(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(field) = self
            .inspector
            .as_mut()
            .and_then(|i| i.fields.get_mut(index))
        {
            field.null = !field.null;
            if field.null {
                field
                    .input
                    .update(cx, |input, cx| input.set_value("", window, cx));
            }
            cx.notify();
        }
    }

    fn stage_inspector(&mut self, cx: &mut Context<Self>) {
        let (Some(inspector), Some(target)) = (self.inspector.as_ref(), self.edit_target()) else {
            return;
        };
        let (change, summary, row) = match &inspector.mode {
            InspectorMode::Edit { row, key } => {
                let cells: Vec<CellChange> = inspector
                    .fields
                    .iter()
                    .filter(|f| !f.column.generated)
                    .map(|f| CellChange {
                        column: f.column.name.clone(),
                        old: f.original.clone(),
                        new: f.value(cx),
                    })
                    .filter(|c| c.old != c.new)
                    .collect();
                let names: Vec<&str> = cells.iter().map(|c| c.column.as_str()).collect();
                let summary = format!("Edit {} · {}", describe_key(key), names.join(", "));
                (
                    RowChange::Update {
                        key: key.clone(),
                        cells,
                    },
                    summary,
                    Some(*row),
                )
            }
            InspectorMode::Insert => {
                let values = inspector
                    .fields
                    .iter()
                    // In a new row, an empty field gets the DEFAULT.
                    .map(|f| (f.column.name.clone(), f.value(cx).filter(|v| !v.is_empty())))
                    .collect::<Vec<_>>();
                let filled: Vec<&str> = values
                    .iter()
                    .filter(|(_, v)| v.is_some())
                    .map(|(n, _)| n.as_str())
                    .collect();
                let summary = if filled.is_empty() {
                    "New row with default values".to_string()
                } else {
                    format!("New row · {}", filled.join(", "))
                };
                (RowChange::Insert { values }, summary, None)
            }
        };

        match edit::build(&target, &change) {
            Ok(statement) => {
                if let (Some(row), RowChange::Update { cells, .. }) = (row, &change) {
                    let columns: Vec<String> = target.columns.clone();
                    self.grid.update(cx, |grid, cx| {
                        for cell in cells {
                            if let Some(index) = columns.iter().position(|c| *c == cell.column) {
                                grid.delegate_mut().mark_edit(row, index, cell.new.clone());
                            }
                        }
                        cx.notify();
                    });
                }
                self.pending.push(Pending { summary, statement });
                self.inspector = None;
                self.apply = ApplyState::Idle;
            }
            Err(error) => {
                if let Some(inspector) = self.inspector.as_mut() {
                    inspector.error = Some(error.to_string());
                }
            }
        }
        cx.notify();
    }

    fn stage_delete(&mut self, cx: &mut Context<Self>) {
        let (Some(inspector), Some(target)) = (self.inspector.as_ref(), self.edit_target()) else {
            return;
        };
        let InspectorMode::Edit { row, key } = &inspector.mode else {
            return;
        };
        let (row, key) = (*row, key.clone());
        match edit::build(&target, &RowChange::Delete { key: key.clone() }) {
            Ok(statement) => {
                self.pending.push(Pending {
                    summary: format!("Delete {}", describe_key(&key)),
                    statement,
                });
                self.grid.update(cx, |grid, cx| {
                    grid.delegate_mut().mark_deleted(row);
                    cx.notify();
                });
                self.inspector = None;
                self.apply = ApplyState::Idle;
            }
            Err(error) => {
                if let Some(inspector) = self.inspector.as_mut() {
                    inspector.error = Some(error.to_string());
                }
            }
        }
        cx.notify();
    }

    fn discard(&mut self, cx: &mut Context<Self>) {
        self.pending.clear();
        self.show_sql = false;
        self.apply = ApplyState::Idle;
        self.grid.update(cx, |grid, cx| {
            grid.delegate_mut().clear_marks();
            cx.notify();
        });
        cx.notify();
    }

    fn apply_pending(&mut self, cx: &mut Context<Self>) {
        if self.pending.is_empty() || matches!(self.apply, ApplyState::Applying) {
            return;
        }
        let statements: Vec<WriteStatement> =
            self.pending.iter().map(|p| p.statement.clone()).collect();
        self.apply = ApplyState::Applying;
        let apply = self.db.session.apply(statements);
        self.apply_task = Some(cx.spawn(async move |this, cx| {
            let result = apply.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(rows) => {
                        this.pending.clear();
                        this.show_sql = false;
                        this.apply = ApplyState::Applied(rows);
                        this.load_page(cx);
                    }
                    Err(error) => this.apply = ApplyState::Failed(describe_error(error)),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // Render ---------------------------------------------------------------

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let filters = self.active_filters();
        let sort = self.browse.as_ref().and_then(|b| b.sort.first().cloned());
        let writable = self.read_only_reason().is_none() && self.definition().is_some();
        let locked = self.has_pending();

        h_flex()
            .h(px(44.))
            .px_3()
            .gap_2()
            .flex_shrink_0()
            .bg(color(theme::TOOLBAR))
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                Button::new("filters")
                    .small()
                    .ghost()
                    .icon(Icon::new(AppIcon::Filter))
                    .label(if filters > 0 {
                        format!("Filter ({filters})")
                    } else {
                        "Filter".into()
                    })
                    .selected(self.show_filters)
                    .disabled(self.view != TableView::Data)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_filters(window, cx))),
            )
            .child(
                h_flex()
                    .gap_1()
                    .text_size(px(12.5))
                    .text_color(color(theme::TEXT_SECONDARY))
                    .child(Icon::new(AppIcon::Sort).size(px(13.)))
                    .child(match &sort {
                        Some(sort) => format!(
                            "{} {}",
                            sort.column,
                            if sort.direction == Direction::Ascending {
                                "↑"
                            } else {
                                "↓"
                            }
                        ),
                        None => "Sorted by primary key · click a header to sort".into(),
                    })
                    .when(sort.is_some() && !locked, |row| {
                        row.child(
                            Button::new("clear-sort")
                                .xsmall()
                                .ghost()
                                .label("Remove")
                                .on_click(cx.listener(|this, _, _, cx| this.clear_sort(cx))),
                        )
                    }),
            )
            .child(div().flex_1())
            .child(
                Button::new("refresh")
                    .small()
                    .ghost()
                    .icon(Icon::new(AppIcon::Reload))
                    .tooltip("Reload")
                    .disabled(locked)
                    .on_click(cx.listener(|this, _, _, cx| this.load_page(cx))),
            )
            .child(
                Button::new("insert")
                    .small()
                    .primary()
                    .label("Add row")
                    .disabled(!writable)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.view = TableView::Data;
                        this.open_insert(window, cx);
                    })),
            )
            .child(
                segmented()
                    .child(
                        segment_body("Data", self.view == TableView::Data)
                            .id("view-data")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.view = TableView::Data;
                                cx.notify();
                            })),
                    )
                    .child(
                        segment_body("Definition", self.view == TableView::Definition)
                            .id("view-definition")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.view = TableView::Definition;
                                cx.notify();
                            })),
                    ),
            )
    }

    fn render_filters(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let locked = self.has_pending();
        let mut panel = v_flex()
            .px_3()
            .py_2()
            .gap_2()
            .flex_shrink_0()
            .bg(color(0x19191b))
            .border_b_1()
            .border_color(color(theme::HAIRLINE));
        for (index, row) in self.filters.iter().enumerate() {
            let op_index = row
                .op
                .read(cx)
                .selected_index(cx)
                .map(|i| i.row)
                .unwrap_or(0);
            let needs_value = FilterOp::ALL[op_index.min(FilterOp::ALL.len() - 1)].needs_value();
            panel = panel.child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .w(px(52.))
                            .text_size(px(12.))
                            .text_color(color(theme::TEXT_SECONDARY))
                            .child(if index == 0 { "Where" } else { "and" }),
                    )
                    .child(div().w(px(180.)).child(Select::new(&row.column).small()))
                    .child(div().w(px(170.)).child(Select::new(&row.op).small()))
                    .when(needs_value, |r| {
                        r.child(div().w(px(260.)).child(Input::new(&row.value).small()))
                    })
                    .child(
                        Button::new(SharedString::from(format!("remove-filter-{index}")))
                            .xsmall()
                            .ghost()
                            .label("Remove")
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.remove_filter(index, cx)),
                            ),
                    ),
            );
        }
        panel.child(
            h_flex()
                .gap_2()
                .pl(px(60.))
                .child(
                    Button::new("add-filter")
                        .xsmall()
                        .ghost()
                        .label("+ Add condition")
                        .on_click(cx.listener(|this, _, window, cx| this.add_filter(window, cx))),
                )
                .child(
                    Button::new("apply-filters")
                        .xsmall()
                        .primary()
                        .label("Apply")
                        .disabled(locked)
                        .on_click(cx.listener(|this, _, _, cx| this.apply_filters(cx))),
                )
                .when(locked, |row| {
                    row.child(muted("Save or discard changes to filter."))
                }),
        )
    }

    fn render_inspector(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let inspector = self.inspector.as_ref()?;
        let reason = self.read_only_reason();
        let editable = reason.is_none();
        let mono = cx.theme().mono_font_family.clone();
        let title = match &inspector.mode {
            InspectorMode::Edit { key, .. } => format!("Row {}", describe_key(key)),
            InspectorMode::Insert => "New row".to_string(),
        };

        let mut fields = v_flex().gap_3();
        for (index, field) in inspector.fields.iter().enumerate() {
            let column = &field.column;
            let locked = !editable || column.generated;
            fields = fields.child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_1()
                            .text_size(px(12.))
                            .children(column.primary_key.then(|| {
                                Icon::new(AppIcon::Key)
                                    .size(px(11.))
                                    .text_color(color(theme::AMBER))
                            }))
                            .child(
                                div()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(column.name.clone()),
                            )
                            .child(
                                div()
                                    .font_family(mono.clone())
                                    .text_size(px(11.))
                                    .text_color(color(theme::TEXT_TERTIARY))
                                    .child(column.data_type.clone()),
                            )
                            .child(div().flex_1())
                            .when(
                                column.nullable
                                    && !locked
                                    && matches!(inspector.mode, InspectorMode::Edit { .. }),
                                |row| {
                                    row.child(
                                        Button::new(SharedString::from(format!("null-{index}")))
                                            .xsmall()
                                            .ghost()
                                            .label("NULL")
                                            .selected(field.null)
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.toggle_null(index, window, cx)
                                            })),
                                    )
                                },
                            ),
                    )
                    .child(Input::new(&field.input).small().disabled(locked))
                    .when_some(column.foreign_key.as_ref(), |col, fk| {
                        col.child(muted(format!("→ {}.{}", fk.table, fk.column)))
                    }),
            );
        }

        let actions = match (&inspector.mode, editable) {
            (_, false) => h_flex().child(muted(reason.unwrap_or_default())),
            (InspectorMode::Edit { .. }, true) => h_flex()
                .gap_2()
                .child(
                    Button::new("delete-row")
                        .small()
                        .ghost()
                        .icon(Icon::new(AppIcon::Trash))
                        .label("Delete row")
                        .on_click(cx.listener(|this, _, _, cx| this.stage_delete(cx))),
                )
                .child(div().flex_1())
                .child(
                    Button::new("stage-row")
                        .small()
                        .primary()
                        .label("Stage change")
                        .on_click(cx.listener(|this, _, _, cx| this.stage_inspector(cx))),
                ),
            (InspectorMode::Insert, true) => h_flex().gap_2().child(div().flex_1()).child(
                Button::new("stage-insert")
                    .small()
                    .primary()
                    .label("Stage change")
                    .on_click(cx.listener(|this, _, _, cx| this.stage_inspector(cx))),
            ),
        };

        Some(
            v_flex()
                .w(px(340.))
                .h_full()
                .flex_shrink_0()
                .bg(color(theme::TOOLBAR))
                .border_l_1()
                .border_color(color(theme::HAIRLINE))
                .child(
                    h_flex()
                        .h(px(40.))
                        .px_3()
                        .gap_2()
                        .border_b_1()
                        .border_color(color(theme::HAIRLINE))
                        .child(
                            div()
                                .flex_1()
                                .font_weight(FontWeight::SEMIBOLD)
                                .truncate()
                                .child(title),
                        )
                        .child(
                            Button::new("close-inspector")
                                .xsmall()
                                .ghost()
                                .label("Close")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.inspector = None;
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    div()
                        .id("inspector-fields")
                        .flex_1()
                        .overflow_y_scroll()
                        .p_3()
                        .child(fields),
                )
                .children(inspector.error.clone().map(|error| {
                    div()
                        .px_3()
                        .pb_2()
                        .text_size(px(12.))
                        .text_color(color(theme::RED))
                        .child(error)
                }))
                .child(
                    div()
                        .p_3()
                        .border_t_1()
                        .border_color(color(theme::HAIRLINE))
                        .child(actions),
                )
                .into_any_element(),
        )
    }

    fn render_sql(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let mut list = v_flex().gap_2();
        for pending in &self.pending {
            list = list.child(
                v_flex()
                    .gap(px(2.))
                    .child(muted(pending.summary.clone()))
                    .child(
                        div()
                            .font_family(mono.clone())
                            .text_size(px(11.5))
                            .text_color(color(theme::TEXT))
                            .child(format!("{};", pending.statement.sql())),
                    ),
            );
        }
        div()
            .id("pending-sql")
            .max_h(px(220.))
            .overflow_y_scroll()
            .flex_shrink_0()
            .px_4()
            .py_3()
            .bg(color(0x111113))
            .border_t_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                v_flex()
                    .gap_2()
                    .child(muted("Runs in a single transaction; if any statement does not affect exactly one row, everything is rolled back."))
                    .child(list),
            )
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(browse) = self.browse.as_ref() else {
            return h_flex().h(px(38.)).into_any_element();
        };
        let page = browse.page;
        let pages = self.page_count();
        let locked = self.has_pending() || matches!(self.page, PageState::Loading);
        let total = match &self.total {
            Some(Ok(total)) => format!("{} records", thousands(*total)),
            Some(Err(_)) => "Total unavailable (count took too long)".into(),
            None => "Counting…".into(),
        };
        let status = match &self.page {
            PageState::Loaded { rows, elapsed } => {
                format!("{rows} rows in {}", format_duration(*elapsed))
            }
            PageState::Loading => "Loading…".into(),
            PageState::Failed(error) => error.clone(),
            PageState::Idle => String::new(),
        };
        let status_tone = if matches!(self.page, PageState::Failed(_)) {
            theme::RED
        } else {
            theme::TEXT_TERTIARY
        };

        let pending = self.pending.len();
        let changes = match &self.apply {
            ApplyState::Applying => Some(muted("Saving…").into_any_element()),
            ApplyState::Failed(error) => Some(
                div()
                    .max_w(px(420.))
                    .truncate()
                    .text_size(px(12.))
                    .text_color(color(theme::RED))
                    .child(error.clone())
                    .into_any_element(),
            ),
            ApplyState::Applied(rows) if pending == 0 => Some(
                div()
                    .text_size(px(12.))
                    .text_color(color(theme::GREEN))
                    .child(format!("Changes saved · {rows} rows"))
                    .into_any_element(),
            ),
            _ => None,
        };

        h_flex()
            .h(px(38.))
            .px_3()
            .gap_2()
            .flex_shrink_0()
            .bg(color(0x1c1c1e))
            .border_t_1()
            .border_color(color(theme::HAIRLINE))
            .text_size(px(12.))
            .child(
                Button::new("prev-page")
                    .xsmall()
                    .ghost()
                    .label("←")
                    .disabled(locked || page == 0)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.go_to_page(page.saturating_sub(1), cx)
                    })),
            )
            .child(muted(format!(
                "Page {} of {}",
                page + 1,
                pages.map(|p| p.to_string()).unwrap_or_else(|| "?".into())
            )))
            .child(
                Button::new("next-page")
                    .xsmall()
                    .ghost()
                    .label("→")
                    .disabled(locked || pages.is_some_and(|p| page + 1 >= p))
                    .on_click(cx.listener(move |this, _, _, cx| this.go_to_page(page + 1, cx))),
            )
            .child(
                div()
                    .w(px(120.))
                    .child(Select::new(&self.page_size).xsmall().disabled(locked)),
            )
            .child(muted(total))
            .child(
                div()
                    .text_color(color(status_tone))
                    .truncate()
                    .child(status),
            )
            .child(div().flex_1())
            .children(changes)
            .when(pending > 0, |row| {
                row.child(
                    h_flex()
                        .gap_1()
                        .text_color(color(0xffd58a))
                        .child(div().size(px(6.)).rounded_full().bg(color(theme::AMBER)))
                        .child(format!("{pending} unsaved changes")),
                )
                .child(
                    Button::new("show-sql")
                        .xsmall()
                        .ghost()
                        .label(if self.show_sql {
                            "Hide SQL"
                        } else {
                            "Show SQL"
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_sql = !this.show_sql;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("discard")
                        .xsmall()
                        .ghost()
                        .label("Discard")
                        .on_click(cx.listener(|this, _, _, cx| this.discard(cx))),
                )
                .child(
                    Button::new("apply")
                        .xsmall()
                        .primary()
                        .label("Save changes")
                        .loading(matches!(self.apply, ApplyState::Applying))
                        .on_click(cx.listener(|this, _, _, cx| this.apply_pending(cx))),
                )
            })
            .into_any_element()
    }
}

fn describe_key(key: &[KeyValue]) -> String {
    key.iter()
        .map(|k| format!("{}={}", k.column, k.value))
        .collect::<Vec<_>>()
        .join(", ")
}

impl Focusable for TableTab {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TableTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let read_only_banner = (!self.db.can_write()).then(|| {
            h_flex()
                .h(px(28.))
                .px_3()
                .gap_2()
                .flex_shrink_0()
                .bg(color(0x2b2414))
                .border_b_1()
                .border_color(color(theme::HAIRLINE))
                .text_size(px(12.))
                .text_color(color(0xffd58a))
                .child(Icon::new(AppIcon::Lock).size(px(13.)))
                .child(
                    "Read-only production connection: you can view and filter, but not edit rows.",
                )
        });

        let content: AnyElement = match (&self.definition, self.view) {
            (Load::Loading, _) => placeholder("Loading definition…"),
            (Load::Failed(error), _) => placeholder(error.clone()),
            (Load::Ready(definition), TableView::Definition) => {
                definition::render(&self.info, definition, mono)
            }
            (Load::Ready(_), TableView::Data) => h_flex()
                .size_full()
                .child(
                    div()
                        .flex_1()
                        .h_full()
                        .min_w_0()
                        .child(DataTable::new(&self.grid).stripe(true).bordered(false)),
                )
                .children(self.render_inspector(cx))
                .into_any_element(),
        };

        v_flex()
            .size_full()
            .key_context("TableTab")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &SaveTableChanges, _, cx| this.apply_pending(cx)))
            .on_action(cx.listener(|this, _: &ReloadTable, _, cx| {
                if !this.has_pending() {
                    this.load_page(cx);
                }
            }))
            .child(self.render_toolbar(cx))
            .children(read_only_banner)
            .when(self.show_filters && self.view == TableView::Data, |col| {
                col.child(self.render_filters(cx))
            })
            .child(div().flex_1().min_h_0().child(content))
            .when(self.show_sql && !self.pending.is_empty(), |col| {
                col.child(self.render_sql(cx))
            })
            .when(self.view == TableView::Data, |col| {
                col.child(self.render_footer(cx))
            })
    }
}
