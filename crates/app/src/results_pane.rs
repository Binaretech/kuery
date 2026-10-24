//! A results grid with what it cannot show by itself: the totals of the
//! selected column, in a bar under the grid, and the selected cell's whole
//! value, in an inspector beside it (JSON indented). Double-click a cell, or
//! use the bar's button, to open the inspector; it then follows the
//! selection.

use gpui_kit::component::table::{DataTable, TableEvent, TableSelection, TableState};
use gpui_kit::component::{ActiveTheme as _, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, Window, div, px,
};
use kuery_core::inspect::{self, Inspected, Totals};

use crate::chrome;
use crate::profile_charts::format_number;
use crate::results::ResultsDelegate;
use crate::theme::{self, color};
use crate::ui::thousands;

/// Characters of a value the inspector draws; Copy still copies all of it.
const INSPECTOR_LIMIT: usize = 100_000;

pub struct ResultsPane {
    table: Entity<TableState<ResultsDelegate>>,
    inspector_open: bool,
    /// The selected cell, as `(generation, row, column)`, and its value.
    inspected: Option<((u64, usize, usize), Option<Inspected>)>,
    /// The column the totals are for, as `(generation, column)`.
    totals: Option<((u64, usize), TotalsState)>,
}

enum TotalsState {
    Computing(#[allow(dead_code)] Task<()>),
    Ready(Totals),
}

impl ResultsPane {
    /// Wraps `table`, which should allow selecting cells.
    pub fn new(table: Entity<TableState<ResultsDelegate>>, cx: &mut Context<Self>) -> Self {
        cx.observe(&table, |this, _, cx| this.follow_selection(cx))
            .detach();
        cx.subscribe(&table, |this, _, event: &TableEvent, cx| {
            if let TableEvent::DoubleClickedCell(..) = event {
                this.inspector_open = true;
                this.follow_selection(cx);
            }
        })
        .detach();
        Self {
            table,
            inspector_open: false,
            inspected: None,
            totals: None,
        }
    }

    /// The selected cell and column, when they are within the results.
    fn selection(&self, cx: &gpui_kit::App) -> (Option<(usize, usize)>, Option<usize>) {
        let table = self.table.read(cx);
        let delegate = table.delegate();
        let (rows, columns) = (delegate.rows(), delegate.results().columns().len());
        match table.selection() {
            TableSelection::Cell(row, column) if row < rows && column < columns => {
                (Some((row, column)), Some(column))
            }
            TableSelection::Column(column) if column < columns => (None, Some(column)),
            _ => (None, None),
        }
    }

    /// Brings the inspected value and the totals up to date with the
    /// selection and the results.
    fn follow_selection(&mut self, cx: &mut Context<Self>) {
        let (cell, column) = self.selection(cx);
        let delegate = self.table.read(cx).delegate();
        let (generation, running) = (delegate.generation(), delegate.is_running());

        let inspected_key = cell.map(|(row, column)| (generation, row, column));
        let inspected =
            (self.inspected.as_ref().map(|(key, _)| *key) != inspected_key).then(|| {
                cell.map(|(row, column)| {
                    let value = delegate.results().cell(row, column).flatten();
                    ((generation, row, column), value.map(inspect::inspect))
                })
            });

        // Totals wait for the last rows, then run off the UI thread.
        let totals_key = column
            .filter(|_| !running)
            .map(|column| (generation, column));
        let stale = self.totals.as_ref().map(|(key, _)| *key) != totals_key;
        let set = (stale && totals_key.is_some()).then(|| delegate.results().clone());

        if let Some(inspected) = inspected {
            self.inspected = inspected;
            cx.notify();
        }
        if stale {
            self.totals = totals_key.zip(set).map(|(key @ (_, column), set)| {
                let totals = cx.background_spawn(async move { inspect::totals(&set, column) });
                let task = cx.spawn(async move |this, cx| {
                    let totals = totals.await;
                    this.update(cx, |this, cx| {
                        if let Some((current, state)) = &mut this.totals
                            && *current == key
                        {
                            *state = TotalsState::Ready(totals);
                            cx.notify();
                        }
                    })
                    .ok();
                });
                (key, TotalsState::Computing(task))
            });
            cx.notify();
        }
    }

    fn column_name(&self, column: usize, cx: &gpui_kit::App) -> String {
        let results = self.table.read(cx).delegate().results();
        results.columns().get(column).cloned().unwrap_or_default()
    }

    fn render_bar(&self, column: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let results = self.table.read(cx).delegate().results();
        let kind = inspect::column_kind(results, column).label();
        let part = |text: String| div().flex_shrink_0().child(text);
        let mut figures: Vec<String> = Vec::new();
        match self.totals.as_ref().map(|(_, state)| state) {
            Some(TotalsState::Ready(totals)) => {
                figures.push(format!(
                    "{} {}",
                    thousands(totals.values as u64),
                    if totals.values == 1 {
                        "value"
                    } else {
                        "values"
                    }
                ));
                if totals.nulls > 0 {
                    figures.push(format!("{} NULL", thousands(totals.nulls as u64)));
                }
                if let Some(numbers) = totals.numbers {
                    figures.push(format!("Sum {}", format_number(numbers.sum)));
                    figures.push(format!("Avg {}", format_number(numbers.mean())));
                    figures.push(format!("Min {}", format_number(numbers.min)));
                    figures.push(format!("Max {}", format_number(numbers.max)));
                }
            }
            Some(TotalsState::Computing(_)) => figures.push("Adding up…".into()),
            None => figures.push("Totals when the query ends".into()),
        }
        h_flex()
            .h(px(30.))
            .flex_shrink_0()
            .gap(px(14.))
            .px_3()
            .border_t_1()
            .border_color(color(theme::HAIRLINE))
            .bg(color(theme::TOOLBAR))
            .text_size(px(12.))
            .text_color(color(theme::TEXT_SECONDARY))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(14.))
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(color(theme::TEXT))
                            .font_weight(FontWeight::MEDIUM)
                            .child(self.column_name(column, cx)),
                    )
                    .child(part(kind.to_string()).text_color(color(theme::TEXT_TERTIARY)))
                    .children(figures.into_iter().map(part)),
            )
            .child(
                chrome::pill_button("results-inspector", "Inspector", self.inspector_open)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.inspector_open = !this.inspector_open;
                        cx.notify();
                    })),
            )
    }

    fn render_inspector(&self, cx: &mut Context<Self>) -> AnyElement {
        let mono = cx.theme().mono_font_family.clone();
        let header = |title: String, detail: String| {
            h_flex()
                .h(px(36.))
                .flex_shrink_0()
                .gap_2()
                .pl_3()
                .pr_1()
                .border_b_1()
                .border_color(color(theme::HAIRLINE))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_weight(FontWeight::MEDIUM)
                        .child(title),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(11.5))
                        .text_color(color(theme::TEXT_TERTIARY))
                        .child(detail),
                )
                .child(
                    chrome::icon_button("inspector-close", IconName::Close, "Close", true)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.inspector_open = false;
                            cx.notify();
                        })),
                )
        };
        let frame = v_flex()
            .w(px(360.))
            .flex_shrink_0()
            .h_full()
            .border_l_1()
            .border_color(color(theme::HAIRLINE))
            .bg(color(theme::GROUND))
            .text_size(px(12.5));
        let Some(((_, _, column), value)) = &self.inspected else {
            return frame
                .child(header("Inspector".into(), String::new()))
                .child(
                    div()
                        .p_3()
                        .text_color(color(theme::TEXT_TERTIARY))
                        .child("Select a cell to see its whole value."),
                )
                .into_any_element();
        };
        let title = self.column_name(*column, cx);
        let Some(value) = value else {
            return frame
                .child(header(title, String::new()))
                .child(
                    div()
                        .p_3()
                        .font_family(mono)
                        .italic()
                        .text_color(color(theme::TEXT_TERTIARY))
                        .child("NULL"),
                )
                .into_any_element();
        };
        let detail = format!(
            "{}{} {} · {} {}",
            if value.json { "JSON · " } else { "" },
            thousands(value.chars as u64),
            if value.chars == 1 { "char" } else { "chars" },
            thousands(value.lines as u64),
            if value.lines == 1 { "line" } else { "lines" },
        );
        let cut = value.text.chars().nth(INSPECTOR_LIMIT).is_some();
        let shown: SharedString = if cut {
            value
                .text
                .chars()
                .take(INSPECTOR_LIMIT)
                .collect::<String>()
                .into()
        } else {
            value.text.clone().into()
        };
        let (table, cell) = (
            self.table.clone(),
            self.inspected.as_ref().map(|(key, _)| *key),
        );
        frame
            .child(header(title, detail))
            .child(
                div()
                    .id("inspector-value")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_3()
                    .font_family(mono)
                    .text_color(color(theme::TEXT))
                    .child(shown)
                    .when(cut, |body| {
                        body.child(div().pt_3().text_color(color(theme::TEXT_TERTIARY)).child(
                            format!(
                                "Showing the first {} characters; Copy Value copies all of it.",
                                thousands(INSPECTOR_LIMIT as u64)
                            ),
                        ))
                    }),
            )
            .child(
                h_flex()
                    .h(px(36.))
                    .flex_shrink_0()
                    .px_2()
                    .border_t_1()
                    .border_color(color(theme::HAIRLINE))
                    .child(
                        chrome::pill_button("inspector-copy", "Copy Value", false).on_click(
                            move |_, _, cx| {
                                let Some((_, row, column)) = cell else { return };
                                let value = table
                                    .read(cx)
                                    .delegate()
                                    .results()
                                    .cell(row, column)
                                    .flatten()
                                    .map(str::to_string);
                                if let Some(value) = value {
                                    cx.write_to_clipboard(ClipboardItem::new_string(value));
                                }
                            },
                        ),
                    ),
            )
            .into_any_element()
    }
}

impl Render for ResultsPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (_, column) = self.selection(cx);
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(DataTable::new(&self.table).stripe(true).bordered(false)),
                    )
                    .when(self.inspector_open, |row| {
                        row.child(self.render_inspector(cx))
                    }),
            )
            .children(column.map(|column| self.render_bar(column, cx)))
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use gpui_kit::{AnyWindowHandle, Bounds, Point, WindowBounds, WindowOptions, size};

    use super::*;

    fn open(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<ResultsPane>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(cx);
            let bounds = Bounds {
                origin: Point::default(),
                size: size(px(1000.), px(600.)),
            };
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    let table = cx.new(|cx| {
                        TableState::new(ResultsDelegate::default(), window, cx)
                            .cell_selectable(true)
                    });
                    cx.new(|cx| ResultsPane::new(table, cx))
                },
            )
            .unwrap()
        })
    }

    fn show(cx: &mut TestAppContext, pane: &Entity<ResultsPane>, rows: &[[Option<&str>; 2]]) {
        let rows: Vec<Vec<Option<String>>> = rows
            .iter()
            .map(|row| row.iter().map(|v| v.map(str::to_string)).collect())
            .collect();
        cx.update(|cx| {
            let table = pane.read(cx).table.clone();
            table.update(cx, |table, cx| {
                table
                    .delegate_mut()
                    .show(vec!["doc".into(), "total".into()], &rows);
                cx.notify();
            });
        });
    }

    fn select(cx: &mut TestAppContext, pane: &Entity<ResultsPane>, row: usize, column: usize) {
        cx.update(|cx| {
            let table = pane.read(cx).table.clone();
            table.update(cx, |table, cx| table.set_selected_cell(row, column, cx));
        });
        cx.run_until_parked();
    }

    fn sum(cx: &mut TestAppContext, pane: &Entity<ResultsPane>) -> Option<f64> {
        cx.update(|cx| match &pane.read(cx).totals {
            Some((_, TotalsState::Ready(totals))) => totals.numbers.map(|n| n.sum),
            _ => None,
        })
    }

    #[gpui::test]
    fn inspects_the_selected_cell_and_adds_up_its_column(cx: &mut TestAppContext) {
        let (_, pane) = open(cx);
        show(
            cx,
            &pane,
            &[
                [Some(r#"{"a":1}"#), Some("2.5")],
                [Some("plain"), None],
                [None, Some("4")],
            ],
        );

        select(cx, &pane, 0, 0);
        cx.update(|cx| {
            let (_, value) = pane.read(cx).inspected.clone().unwrap();
            let value = value.unwrap();
            assert!(value.json);
            assert_eq!(value.text, "{\n  \"a\": 1\n}");
        });
        cx.update(|cx| match &pane.read(cx).totals {
            Some((_, TotalsState::Ready(totals))) => {
                assert_eq!((totals.values, totals.nulls, totals.numbers), (2, 1, None))
            }
            _ => panic!("no totals for the text column"),
        });

        select(cx, &pane, 1, 1);
        assert_eq!(sum(cx, &pane), Some(6.5));
        cx.update(|cx| assert_eq!(pane.read(cx).inspected.clone().unwrap().1, None));

        // New results replace the old totals, even for the same cell.
        show(
            cx,
            &pane,
            &[[Some("x"), Some("10")], [Some("y"), Some("20")]],
        );
        cx.run_until_parked();
        assert_eq!(sum(cx, &pane), Some(30.0));
    }

    #[gpui::test]
    fn double_clicking_a_cell_opens_the_inspector(cx: &mut TestAppContext) {
        let (window, pane) = open(cx);
        show(cx, &pane, &[[Some(r#"[1, {"b": null}]"#), Some("1")]]);
        select(cx, &pane, 0, 0);
        cx.update(|cx| {
            let table = pane.read(cx).table.clone();
            table.update(cx, |_, cx| cx.emit(TableEvent::DoubleClickedCell(0, 0)));
        });
        cx.run_until_parked();
        cx.update(|cx| assert!(pane.read(cx).inspector_open));
        // The grid, the totals bar and the inspector all draw.
        cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
    }
}
