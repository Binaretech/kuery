//! The Profile tab of a query's results. It opens on a table of every
//! column (nulls, distinct values, what it holds, a small chart) under a
//! line naming what stands out: empty, constant and mostly NULL columns,
//! outliers, likely keys. Clicking a column opens it in depth beside the
//! list; "Compare with" (or a ⌘-click) sets a second one against it. There
//! is nothing to run: every choice updates what is on screen.
//!
//! A click on a bar, a value or a period of a column's chart asks for the
//! rows behind it ([`ProfileEvent::ShowRows`]); the tab shows them in its
//! grid.
//!
//! Very large results are not summarized until asked. Hidden columns, the
//! open column and its options are kept by column name across runs.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::DropdownMenu as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, ClickEvent, Context, Div, ElementId, EventEmitter, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, Task, Window, div, px, relative,
};
use kuery_core::compare::{
    self, Aggregate, CompareOptions, Comparison, CorrelationMatrix, MAX_STACKED, Outcome,
};
use kuery_core::result_set::{ColumnKind, ResultSet};
use kuery_core::stats::{
    self, Analysis, Digest, Flag, Granularity, Match, Mode, Options, Overview, Summary,
};

use crate::assets::AppIcon;
use crate::chrome;
use crate::context_menus::item;
use crate::profile_charts::{self as charts, CHART_HEIGHT, format_number, message, share};
use crate::results_pane::Rows;
use crate::theme::{self, color};
use crate::ui::thousands;

const LIST_WIDTH: f32 = 280.;
/// Cells (rows × columns) up to which every column is summarized when the
/// tab is first shown; larger results wait for "Summarize columns".
const AUTO_CELLS: usize = 5_000_000;
/// Columns named per finding before "+N".
const NAMED: usize = 3;
/// |r| from which two columns are told as related.
const RELATED: f64 = 0.7;
/// Pairs listed over the grid of correlations, and the |r| they need.
const STRONGEST: usize = 8;
const LISTED: f64 = 0.3;
const ROW_LABEL: f32 = 150.;
const CELL_WIDTH: f32 = 68.;
const CELL_GAP: f32 = 2.;
const BIN_CHOICES: [Option<usize>; 4] = [None, Some(10), Some(20), Some(50)];
const TOP_CHOICES: [usize; 4] = [5, 10, 25, 50];
const STACK_CHOICES: [usize; 3] = [3, 5, MAX_STACKED];

#[derive(Default)]
enum Overviews {
    /// Not asked for.
    #[default]
    None,
    Computing(#[allow(dead_code)] Task<()>),
    /// One per column, in order; how the numeric ones move together; and
    /// how many rows repeat an earlier one.
    Ready(Vec<Overview>, CorrelationMatrix, usize),
}

/// What is shown in depth: one column, or two compared.
#[derive(Debug, Clone, PartialEq)]
enum Request {
    Single {
        column: String,
        options: Options,
    },
    Pair {
        x: String,
        y: String,
        comparison: Comparison,
        options: CompareOptions,
    },
}

enum Data {
    Single(Analysis),
    Pair(Outcome),
}

struct Shown {
    request: Request,
    data: Data,
}

/// What the panel asks of the tab it is in.
pub enum ProfileEvent {
    /// Show these rows in the grid.
    ShowRows(Rows),
}

impl EventEmitter<ProfileEvent> for ProfilePanel {}

#[derive(Default)]
pub struct ProfilePanel {
    /// The results, with every column's kind filled in.
    set: Option<ResultSet>,
    /// Per column: whether its kind was inferred from the values.
    inferred: Vec<bool>,
    overviews: Overviews,
    /// Summarize every column the next time the tab is drawn.
    summarize: bool,
    /// The correlations are shown in place of the table of columns.
    relations: bool,
    hidden: HashSet<String>,
    /// The column open in depth; none shows the table of columns.
    x: Option<String>,
    /// The column compared with it.
    y: Option<String>,
    /// The comparison picked for x and y; the first that fits when unset or
    /// when it no longer fits.
    comparison: Option<Comparison>,
    options: HashMap<String, Options>,
    compare_options: CompareOptions,
    shown: Option<Shown>,
    task: Option<Task<()>>,
    scope: Option<String>,
    generation: u64,
    analysis_version: u64,
    /// Timelines drawn as bars instead of a line.
    bars_over_time: bool,
}

impl ProfilePanel {
    /// Forgets the results (a new run started).
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.scope = None;
        self.set = None;
        self.overviews = Overviews::None;
        self.shown = None;
        self.task = None;
        cx.notify();
    }

    pub fn is_stale(&self) -> bool {
        self.set.is_none()
    }

    /// Takes new results. Only the kinds are worked out here (from a sample
    /// of values when the server did not report them); the columns are
    /// summarized when the tab is drawn, and the open column is updated.
    pub fn load(&mut self, mut set: ResultSet, cx: &mut Context<Self>) {
        self.generation += 1;
        self.scope = None;
        let kinds = stats::kinds(&set);
        set.set_kinds(kinds.iter().map(|(kind, _)| *kind).collect());
        self.inferred = kinds.iter().map(|(_, inferred)| *inferred).collect();
        let present = |name: &Option<String>| {
            name.clone()
                .filter(|name| set.columns().contains(name) && !self.hidden.contains(name))
        };
        let (x, y) = (present(&self.x), present(&self.y));
        self.summarize = set.rows().saturating_mul(set.columns().len()) <= AUTO_CELLS;
        self.set = Some(set);
        self.y = y.filter(|y| x.is_some() && Some(y) != x.as_ref());
        self.x = x;
        self.overviews = Overviews::None;
        self.shown = None;
        self.task = None;
        self.refresh(cx);
    }

    /// The same analyses/charts, with the file's scope and schema provenance.
    pub fn load_scoped(
        &mut self,
        set: ResultSet,
        scope: String,
        inferred: Vec<bool>,
        cx: &mut Context<Self>,
    ) {
        self.load(set, cx);
        self.scope = Some(scope);
        if inferred.len() == self.columns().len() {
            self.inferred = inferred;
        }
        cx.notify();
    }

    fn columns(&self) -> &[String] {
        self.set.as_ref().map_or(&[], |set| set.columns())
    }

    fn index_of(&self, name: &str) -> Option<usize> {
        self.columns().iter().position(|c| c == name)
    }

    fn kind(&self, index: usize) -> ColumnKind {
        self.set
            .as_ref()
            .and_then(|set| set.kinds().get(index).copied())
            .unwrap_or_default()
    }

    fn kind_of(&self, name: &str) -> ColumnKind {
        self.index_of(name)
            .map_or(ColumnKind::Unknown, |index| self.kind(index))
    }

    fn visible(&self) -> impl Iterator<Item = (usize, &String)> {
        self.columns()
            .iter()
            .enumerate()
            .filter(|(_, name)| !self.hidden.contains(*name))
    }

    fn options_of(&self, name: &str) -> Options {
        self.options.get(name).copied().unwrap_or_default()
    }

    fn comparisons(&self) -> Vec<Comparison> {
        let (Some(x), Some(y)) = (&self.x, &self.y) else {
            return Vec::new();
        };
        match (self.index_of(x), self.index_of(y)) {
            (Some(x), Some(y)) => compare::comparisons(x, self.kind(x), y, self.kind(y)),
            _ => Vec::new(),
        }
    }

    fn current_comparison(&self) -> Option<Comparison> {
        let available = self.comparisons();
        self.comparison
            .filter(|c| available.contains(c))
            .or_else(|| available.first().copied())
    }

    /// What the choices ask for.
    fn request(&self) -> Option<Request> {
        let x = self.x.clone()?;
        Some(match &self.y {
            None => Request::Single {
                options: self.options_of(&x),
                column: x,
            },
            Some(y) => Request::Pair {
                comparison: self.current_comparison()?,
                x,
                y: y.clone(),
                options: self.compare_options,
            },
        })
    }

    /// What a click on a mark of `column`'s chart does: asks for its rows.
    fn pick(&self, column: &str, cx: &mut Context<Self>) -> Option<charts::Pick> {
        let this = cx.entity().downgrade();
        let column = column.to_string();
        Some(Rc::new(move |matching, _, cx| {
            this.update(cx, |_, cx| {
                cx.emit(ProfileEvent::ShowRows(Rows::Matching {
                    column: column.clone(),
                    matching,
                }))
            })
            .ok();
        }))
    }

    /// Brings what is shown in line with the choices.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        match self.request() {
            Some(request) => {
                if self.shown.as_ref().map(|shown| &shown.request) != Some(&request) {
                    self.run(request, cx);
                }
            }
            None => {
                self.analysis_version += 1;
                self.shown = None;
                self.task = None;
            }
        }
        cx.notify();
    }

    /// Runs `request` off the UI thread; the previous result stays on
    /// screen meanwhile.
    fn run(&mut self, request: Request, cx: &mut Context<Self>) {
        let Some(set) = self.set.clone() else {
            return;
        };
        enum Job {
            Single(usize, Options),
            Pair(Comparison, CompareOptions),
        }
        let job = match &request {
            Request::Single { column, options } => match self.index_of(column) {
                Some(index) => Job::Single(index, *options),
                None => return,
            },
            Request::Pair {
                comparison,
                options,
                ..
            } => Job::Pair(*comparison, *options),
        };
        self.analysis_version += 1;
        let version = self.analysis_version;
        let generation = self.generation;
        self.task = Some(cx.spawn(async move |this, cx| {
            let data = cx
                .background_spawn(async move {
                    match job {
                        Job::Single(index, options) => {
                            Data::Single(stats::analyze(&set, index, &options))
                        }
                        Job::Pair(comparison, options) => {
                            Data::Pair(compare::compare(&set, comparison, &options))
                        }
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation || this.analysis_version != version {
                    return;
                }
                this.shown = Some(Shown { request, data });
                this.task = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Summarizes every column for the table.
    fn profile_all(&mut self, cx: &mut Context<Self>) {
        self.summarize = false;
        if matches!(self.overviews, Overviews::Computing(_)) {
            return;
        }
        let Some(set) = self.set.clone() else {
            return;
        };
        let generation = self.generation;
        let task = cx.spawn(async move |this, cx| {
            let (overviews, matrix, duplicates) = cx
                .background_spawn(async move {
                    (
                        stats::overview(&set),
                        compare::correlations(&set),
                        stats::duplicates(&set),
                    )
                })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.overviews = Overviews::Ready(overviews, matrix, duplicates);
                cx.notify();
            })
            .ok();
        });
        self.overviews = Overviews::Computing(task);
        cx.notify();
    }

    /// Opens `name` in depth, alone.
    fn open(&mut self, name: String, cx: &mut Context<Self>) {
        // A few far values flatten a histogram into one bar: a column
        // flagged for them opens without them, until its options say
        // otherwise.
        let outliers = self
            .index_of(&name)
            .and_then(|index| self.ready()?.get(index))
            .is_some_and(|o| o.flags().iter().any(|f| matches!(f, Flag::Outliers(_))));
        if outliers && !self.options.contains_key(&name) {
            self.options.insert(
                name.clone(),
                Options {
                    hide_outliers: true,
                    ..Options::default()
                },
            );
        }
        if self.x.as_ref() != Some(&name) || self.y.is_some() {
            self.x = Some(name);
            self.y = None;
            self.comparison = None;
        }
        self.refresh(cx);
    }

    /// Back to the table of columns.
    fn close(&mut self, cx: &mut Context<Self>) {
        self.x = None;
        self.y = None;
        self.comparison = None;
        self.refresh(cx);
    }

    fn set_y(&mut self, name: Option<String>, cx: &mut Context<Self>) {
        if name.is_some() && name == self.x {
            return;
        }
        self.y = name;
        self.comparison = None;
        self.refresh(cx);
    }

    /// A click opens the column; a ⌘-click compares it with the open one.
    fn click_column(&mut self, name: String, event: &ClickEvent, cx: &mut Context<Self>) {
        if event.modifiers().platform && self.x.is_some() {
            let toggled = (self.y.as_ref() != Some(&name)).then_some(name);
            self.set_y(toggled, cx);
        } else {
            self.open(name, cx);
        }
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if let Some(x) = self.x.clone() {
            self.options.entry(x).or_default().mode = mode;
            self.refresh(cx);
        }
    }

    /// Changes the options of the single-column result on screen and
    /// updates it.
    fn change_single(&mut self, change: impl FnOnce(&mut Options), cx: &mut Context<Self>) {
        let Some(Shown {
            request: Request::Single { column, options },
            ..
        }) = &self.shown
        else {
            return;
        };
        let (column, mut options) = (column.clone(), *options);
        change(&mut options);
        self.options.insert(column.clone(), options);
        self.run(Request::Single { column, options }, cx);
    }

    /// Changes the options of the comparison on screen and updates it.
    fn change_pair(&mut self, change: impl FnOnce(&mut CompareOptions), cx: &mut Context<Self>) {
        let Some(Shown {
            request: Request::Pair {
                x, y, comparison, ..
            },
            ..
        }) = &self.shown
        else {
            return;
        };
        let request = Request::Pair {
            x: x.clone(),
            y: y.clone(),
            comparison: *comparison,
            options: {
                change(&mut self.compare_options);
                self.compare_options
            },
        };
        self.run(request, cx);
    }

    fn hide(&mut self, name: String, cx: &mut Context<Self>) {
        self.hidden.insert(name.clone());
        if self.y.as_ref() == Some(&name) {
            self.y = None;
            self.comparison = None;
        }
        if self.x.as_ref() == Some(&name) {
            self.x = self.y.take();
            self.comparison = None;
        }
        self.refresh(cx);
    }

    fn show_all(&mut self, cx: &mut Context<Self>) {
        self.hidden.clear();
        cx.notify();
    }

    /// How a comparison reads, with its columns' names.
    fn describe(&self, comparison: Comparison) -> String {
        let name = |index: usize| self.columns().get(index).cloned().unwrap_or_default();
        match comparison {
            Comparison::Groups { measure, group } => {
                format!("{} by {}", name(measure), name(group))
            }
            Comparison::CrossTab { rows, columns } => format!("{} × {}", name(rows), name(columns)),
            Comparison::Series { time, measure } => {
                format!("{} over {}", name(measure), name(time))
            }
            Comparison::Breakdown { time, category } => {
                format!("{} over {}", name(category), name(time))
            }
            Comparison::Correlation { x, y } => format!("{} vs {}", name(x), name(y)),
        }
    }

    fn hidden_count(&self) -> usize {
        self.columns()
            .iter()
            .filter(|c| self.hidden.contains(*c))
            .count()
    }

    fn ready(&self) -> Option<&Vec<Overview>> {
        match &self.overviews {
            Overviews::Ready(overviews, ..) => Some(overviews),
            _ => None,
        }
    }

    /// The measured pairs of visible numeric columns, the strongest first.
    fn related(&self) -> Vec<(usize, usize, f64)> {
        let Overviews::Ready(_, matrix, _) = &self.overviews else {
            return Vec::new();
        };
        let visible = |index: usize| {
            self.columns()
                .get(index)
                .is_some_and(|name| !self.hidden.contains(name))
        };
        matrix
            .strongest()
            .into_iter()
            .filter(|(a, b, _)| visible(*a) && visible(*b))
            .collect()
    }

    /// The numeric columns compared with each other, when two or more are
    /// visible, and how many more were left out.
    fn matrix(&self) -> Option<(&CorrelationMatrix, Vec<usize>)> {
        let Overviews::Ready(_, matrix, _) = &self.overviews else {
            return None;
        };
        // Positions in the matrix of the columns that are not hidden.
        let shown: Vec<usize> = (0..matrix.columns.len())
            .filter(|at| {
                self.columns()
                    .get(matrix.columns[*at])
                    .is_some_and(|name| !self.hidden.contains(name))
            })
            .collect();
        (shown.len() >= 2).then_some((matrix, shown))
    }

    /// Opens two numeric columns against each other.
    fn open_pair(&mut self, x: usize, y: usize, cx: &mut Context<Self>) {
        let (Some(x_name), Some(y_name)) = (
            self.columns().get(x).cloned(),
            self.columns().get(y).cloned(),
        ) else {
            return;
        };
        self.x = Some(x_name);
        self.y = Some(y_name);
        self.comparison = Some(Comparison::Correlation { x, y });
        self.refresh(cx);
    }

    /// "12 columns", or "9 of 12 columns" with a way to bring the rest back.
    fn column_count(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let total = self.columns().len();
        let hidden = self.hidden_count();
        h_flex()
            .gap_2()
            .child(div().child(match (hidden, total) {
                (0, 1) => "1 column".to_string(),
                (0, _) => format!("{} columns", thousands(total as u64)),
                _ => format!("{} of {} columns", total - hidden, total),
            }))
            .when(hidden > 0, |row| {
                row.child(
                    text_button("profile-show-all", "Show all")
                        .on_click(cx.listener(|this, _, _, cx| this.show_all(cx))),
                )
            })
    }

    /// The table of columns: what the tab opens on.
    fn render_overview(&self, mono: SharedString, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.set.as_ref().map_or(0, |set| set.rows());
        let overviews = self.ready();
        let relations_shown = self.relations && self.matrix().is_some();
        let state = match &self.overviews {
            Overviews::Ready(..) => div().into_any_element(),
            Overviews::Computing(_) => div().child("Summarizing…").into_any_element(),
            Overviews::None if self.summarize => div().into_any_element(),
            Overviews::None => text_button("profile-all", "Summarize columns")
                .tooltip(|window, cx| {
                    Tooltip::new("Count nulls and distinct values of every column. This result is large, so it may take a moment")
                        .build(window, cx)
                })
                .on_click(cx.listener(|this, _, _, cx| this.profile_all(cx)))
                .into_any_element(),
        };
        let head = |text: &'static str| {
            div()
                .text_size(px(11.5))
                .text_color(color(theme::TEXT_TERTIARY))
                .child(text)
        };
        let table: Vec<AnyElement> = self
            .visible()
            .map(|(index, name)| {
                self.overview_row(
                    index,
                    name,
                    overviews.and_then(|o| o.get(index)),
                    mono.clone(),
                    cx,
                )
                .into_any_element()
            })
            .collect();
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .h(px(40.))
                    .flex_shrink_0()
                    .px(px(16.))
                    .gap(px(14.))
                    .text_size(px(12.5))
                    .text_color(color(theme::TEXT_SECONDARY))
                    .child(
                        div()
                            .id("profile-rows")
                            .text_color(color(theme::TEXT))
                            .child(if rows == 1 {
                                "1 row".to_string()
                            } else {
                                format!("{} rows", thousands(rows as u64))
                            })
                            .when_some(self.scope.clone(), |rows, scope| {
                                // Which rows of the file these are.
                                rows.tooltip(move |window, cx| {
                                    Tooltip::new(scope.clone()).build(window, cx)
                                })
                            }),
                    )
                    .child(self.column_count(cx))
                    .child(div().flex_1())
                    .child(div().text_color(color(theme::TEXT_TERTIARY)).child(state))
                    .when(self.matrix().is_some(), |header| {
                        header.child(
                            chrome::segmented().children(
                                [("Columns", false), ("Correlations", true)]
                                    .into_iter()
                                    .map(|(text, relations)| {
                                        choice(
                                            SharedString::from(format!("profile-view-{text}")),
                                            text,
                                            relations_shown == relations,
                                        )
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| {
                                                this.relations = relations;
                                                cx.notify();
                                            }),
                                        )
                                    }),
                            ),
                        )
                    }),
            )
            .children(
                overviews.and_then(|overviews| self.render_findings(overviews, mono.clone(), cx)),
            )
            .when(relations_shown, |view| {
                view.child(self.render_relations(mono.clone(), cx))
            })
            .when(!relations_shown, |view| {
                view.child(
                    h_flex()
                        .h(px(28.))
                        .flex_shrink_0()
                        .px(px(16.))
                        .gap(px(COLUMN_GAP))
                        .child(head("Column").flex_1().min_w(px(NAME_MIN)))
                        .child(head("Nulls").w(px(NULLS_WIDTH)).text_right())
                        .child(head("Distinct").w(px(DISTINCT_WIDTH)).text_right())
                        .child(head("Values").flex_1().min_w(px(DIGEST_MIN)))
                        .child(head("Distribution").w(px(SPARK_WIDTH)))
                        .child(div().w(px(HIDE_WIDTH))),
                )
                .child(
                    div()
                        .id("profile-overview")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .px(px(8.))
                        .pb(px(8.))
                        .children(table),
                )
            })
    }

    /// How the numeric columns move together: the strongest pairs in words,
    /// then every pair in a grid. A pair opens its scatter plot.
    fn render_relations(&self, mono: SharedString, cx: &mut Context<Self>) -> impl IntoElement {
        let Some((matrix, shown)) = self.matrix() else {
            return div().into_any_element();
        };
        let name = |index: usize| self.columns().get(index).cloned().unwrap_or_default();
        let pairs = self.related();
        let positive = color(theme::ACCENT);
        let negative = color(theme::AMBER);
        let tone = |r: f64| if r < 0.0 { negative } else { positive };
        // Weaker pairs say nothing in a list; the grid has them all.
        let listed = pairs.iter().filter(|(_, _, r)| r.abs() >= LISTED).count();
        let strongest = pairs.iter().take(listed.min(STRONGEST)).map(|(a, b, r)| {
            let (a, b, r) = (*a, *b, *r);
            h_flex()
                .id(("profile-pair", a * self.columns().len() + b))
                .h(px(30.))
                .px(px(8.))
                .gap(px(10.))
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|s| s.bg(color(theme::ACTIVE)))
                .child(
                    div()
                        .w(px(52.))
                        .flex_shrink_0()
                        .text_right()
                        .font_family(mono.clone())
                        .child(signed(r)),
                )
                .child(
                    // The share of the bar is |r|, on a track of 1.
                    div()
                        .w(px(64.))
                        .h(px(4.))
                        .flex_shrink_0()
                        .rounded(px(2.))
                        .bg(color(theme::ACTIVE_STRONG))
                        .child(
                            div()
                                .h_full()
                                .rounded(px(2.))
                                .w(relative(r.abs() as f32))
                                .bg(tone(r)),
                        ),
                )
                .child(
                    h_flex()
                        .min_w_0()
                        .gap(px(6.))
                        .font_family(mono.clone())
                        .child(div().truncate().child(name(a)))
                        .child(div().text_color(color(theme::TEXT_TERTIARY)).child("and"))
                        .child(div().truncate().child(name(b))),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(color(theme::TEXT_TERTIARY))
                        .child(relation(r)),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.open_pair(a, b, cx)))
        });
        // The lower half of the grid: every pair once.
        let header = h_flex()
            .gap(px(CELL_GAP))
            .child(div().w(px(ROW_LABEL)).flex_shrink_0())
            .children(shown.iter().take(shown.len() - 1).map(|at| {
                div()
                    .w(px(CELL_WIDTH))
                    .flex_shrink_0()
                    .truncate()
                    .text_center()
                    .font_family(mono.clone())
                    .text_size(px(11.5))
                    .text_color(color(theme::TEXT_TERTIARY))
                    .child(name(matrix.columns[*at]))
            }));
        let grid = shown.iter().enumerate().skip(1).map(|(position, row)| {
            let row_column = matrix.columns[*row];
            h_flex()
                .gap(px(CELL_GAP))
                .child(
                    div()
                        .w(px(ROW_LABEL))
                        .flex_shrink_0()
                        .truncate()
                        .font_family(mono.clone())
                        .text_size(px(12.))
                        .text_color(color(theme::TEXT_SECONDARY))
                        .child(name(row_column)),
                )
                .children(shown.iter().take(position).map(|at| {
                    let column = matrix.columns[*at];
                    let cell = div()
                        .id(("profile-cell", row_column * self.columns().len() + column))
                        .w(px(CELL_WIDTH))
                        .h(px(26.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(3.))
                        .font_family(mono.clone())
                        .text_size(px(11.5));
                    match matrix.r[*row][*at] {
                        Some(r) => {
                            let tip = format!(
                                "{} and {} · r {} · {} · click to compare",
                                name(column),
                                name(row_column),
                                signed(r),
                                relation(r)
                            );
                            cell.bg(tone(r).opacity(r.abs() as f32 * 0.8))
                                .text_color(color(if r.abs() >= 0.3 {
                                    theme::TEXT
                                } else {
                                    theme::TEXT_TERTIARY
                                }))
                                .cursor_pointer()
                                .hover(|s| s.border_1().border_color(color(theme::TEXT_TERTIARY)))
                                .child(signed(r))
                                .tooltip(move |window, cx| {
                                    Tooltip::new(tip.clone()).build(window, cx)
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_pair(column, row_column, cx)
                                }))
                        }
                        None => cell
                            .text_color(color(theme::TEXT_QUIET))
                            .child("—")
                            .tooltip(|window, cx| {
                                Tooltip::new("Too few rows have a number in both columns")
                                    .build(window, cx)
                            }),
                    }
                }))
        });
        let swatch = |tone, text: &'static str| {
            h_flex()
                .gap(px(6.))
                .child(div().size(px(10.)).rounded(px(2.)).bg(tone))
                .child(text)
        };
        div()
            .id("profile-relations")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .px(px(16.))
                    .pb(px(16.))
                    .gap(px(14.))
                    .text_size(px(12.5))
                    .child(if pairs.is_empty() {
                        charts::footnote(
                            "No two numeric columns have enough rows in common to compare.",
                        )
                        .into_any_element()
                    } else if listed == 0 {
                        charts::footnote(format!(
                            "No two numeric columns move together: the closest are {} and {}, at {}.",
                            name(pairs[0].0),
                            name(pairs[0].1),
                            signed(pairs[0].2)
                        ))
                        .into_any_element()
                    } else {
                        v_flex()
                            .mx(px(-8.))
                            .children(strongest)
                            .into_any_element()
                    })
                    .child(
                        charts::panel().child(
                            div().id("profile-matrix").overflow_x_scroll().child(
                                v_flex()
                                    .gap(px(CELL_GAP))
                                    .child(header)
                                    .children(grid),
                            ),
                        ),
                    )
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_x(px(18.))
                            .gap_y(px(4.))
                            .text_size(px(11.5))
                            .text_color(color(theme::TEXT_TERTIARY))
                            .child(swatch(positive.opacity(0.8), "Rise together"))
                            .child(swatch(negative.opacity(0.8), "One rises as the other falls"))
                            .child(format!(
                                "Pearson r, from −1 to 1, over the rows with a number in both.{}",
                                match matrix.left_out {
                                    0 => String::new(),
                                    more => format!(
                                        " The first {} numeric columns; {more} more are not compared.",
                                        compare::MAX_MATRIX
                                    ),
                                }
                            )),
                    ),
            )
            .into_any_element()
    }

    /// What stands out, by kind, naming the columns: each opens on a click.
    fn render_findings(
        &self,
        overviews: &[Overview],
        mono: SharedString,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        // (label, [(column, note)]) in the order the flags are told.
        let mut groups: Vec<(&'static str, Vec<(usize, String)>)> = [
            "No values",
            "Constant",
            "Mostly NULL",
            "Blank",
            "Spaces at the ends",
            "Off format",
            "Outliers",
            "Likely key",
        ]
        .into_iter()
        .map(|label| (label, Vec::new()))
        .collect();
        for (index, _) in self.visible() {
            let Some(overview) = overviews.get(index) else {
                continue;
            };
            for flag in overview.flags() {
                let (group, note) = match flag {
                    Flag::Empty => (0, String::new()),
                    Flag::Constant => (1, String::new()),
                    Flag::MostlyNull => {
                        (2, format!("{:.0}%", share(overview.nulls, overview.rows)))
                    }
                    Flag::Blank(count) => (3, thousands(count as u64)),
                    Flag::Padded(count) => (4, thousands(count as u64)),
                    Flag::OffFormat(format, count) => (
                        5,
                        format!("{} not {}", thousands(count as u64), format.plural()),
                    ),
                    Flag::Outliers(count) => (6, thousands(count as u64)),
                    Flag::Key => (7, String::new()),
                };
                groups[group].1.push((index, note));
            }
        }
        groups.retain(|(_, columns)| !columns.is_empty());
        let related: Vec<(usize, usize, f64)> = self
            .related()
            .into_iter()
            .take_while(|(_, _, r)| r.abs() >= RELATED)
            .collect();
        let duplicates = match &self.overviews {
            Overviews::Ready(_, _, duplicates) => *duplicates,
            _ => 0,
        };
        if groups.is_empty() && related.is_empty() && duplicates == 0 {
            return None;
        }
        // Rows, not a column: it shows them in the grid.
        let duplicate_rows = (duplicates > 0).then(|| {
            h_flex()
                .id("profile-duplicates")
                .gap(px(5.))
                .px(px(5.))
                .mx(px(-5.))
                .rounded(px(4.))
                .cursor_pointer()
                .hover(|s| s.bg(color(theme::ACTIVE)))
                .child(
                    div()
                        .text_color(color(theme::TEXT_TERTIARY))
                        .child("Repeated rows"),
                )
                .child(div().child(thousands(duplicates as u64)))
                .tooltip(|window, cx| {
                    Tooltip::new("Rows equal to an earlier one in every column · click to see them")
                        .build(window, cx)
                })
                .on_click(
                    cx.listener(|_, _, _, cx| cx.emit(ProfileEvent::ShowRows(Rows::Duplicates))),
                )
        });
        let more_related = related.len().saturating_sub(NAMED);
        let related_group = (!related.is_empty()).then(|| {
            h_flex()
                .gap(px(4.))
                .child(
                    div()
                        .text_color(color(theme::TEXT_TERTIARY))
                        .child("Related"),
                )
                .children(related.into_iter().take(NAMED).map(|(a, b, r)| {
                    h_flex()
                        .id(("profile-related", a * self.columns().len() + b))
                        .px(px(5.))
                        .gap(px(5.))
                        .rounded(px(4.))
                        .max_w(px(320.))
                        .hover(|s| s.bg(color(theme::ACTIVE)))
                        .child(
                            div()
                                .truncate()
                                .font_family(mono.clone())
                                .text_size(px(12.))
                                .child(format!("{} · {}", self.columns()[a], self.columns()[b])),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_color(color(theme::TEXT_TERTIARY))
                                .child(signed(r)),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.open_pair(a, b, cx)))
                }))
                .when(more_related > 0, |group| {
                    group.child(
                        div()
                            .text_color(color(theme::TEXT_QUIET))
                            .child(format!("+{more_related}")),
                    )
                })
        });
        Some(
            h_flex()
                .flex_wrap()
                .flex_shrink_0()
                .px(px(16.))
                .pb(px(10.))
                .gap_x(px(20.))
                .gap_y(px(4.))
                .text_size(px(12.5))
                .children(duplicate_rows)
                .children(groups.into_iter().map(|(label, columns)| {
                    let more = columns.len().saturating_sub(NAMED);
                    h_flex()
                        .gap(px(4.))
                        .child(div().text_color(color(theme::TEXT_TERTIARY)).child(label))
                        .children(columns.into_iter().take(NAMED).map(|(index, note)| {
                            let name = self.columns()[index].clone();
                            h_flex()
                                .id((label, index))
                                .px(px(5.))
                                .gap(px(5.))
                                .rounded(px(4.))
                                .max_w(px(220.))
                                .hover(|s| s.bg(color(theme::ACTIVE)))
                                .child(
                                    div()
                                        .truncate()
                                        .font_family(mono.clone())
                                        .text_size(px(12.))
                                        .child(name.clone()),
                                )
                                .when(!note.is_empty(), |chip| {
                                    chip.child(
                                        div().text_color(color(theme::TEXT_TERTIARY)).child(note),
                                    )
                                })
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.open(name.clone(), cx)),
                                )
                        }))
                        .when(more > 0, |group| {
                            group.child(
                                div()
                                    .text_color(color(theme::TEXT_QUIET))
                                    .child(format!("+{more}")),
                            )
                        })
                }))
                .children(related_group)
                .into_any_element(),
        )
    }

    /// One column of the table.
    fn overview_row(
        &self,
        index: usize,
        name: &str,
        overview: Option<&Overview>,
        mono: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (icon, tone) = kind_icon(self.kind(index));
        let group = SharedString::from(format!("profile-overview-{index}"));
        let name = name.to_string();
        let nulls = overview.map(|o| (share(o.nulls, o.rows), o.nulls));
        h_flex()
            .id(("profile-overview-row", index))
            .group(group.clone())
            .h(px(34.))
            .px(px(8.))
            .gap(px(COLUMN_GAP))
            .rounded(px(6.))
            .text_size(px(12.5))
            .hover(|s| s.bg(color(theme::ACTIVE)))
            .child(
                h_flex()
                    .flex_1()
                    .min_w(px(NAME_MIN))
                    .gap(px(8.))
                    .child(Icon::new(icon).size(px(13.)).text_color(color(tone)))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(mono.clone())
                            .child(name.clone()),
                    )
                    .when(overview.is_some_and(|o| o.unique), |cell| {
                        cell.child(
                            Icon::new(AppIcon::Key)
                                .size(px(11.))
                                .text_color(color(theme::TEXT_QUIET)),
                        )
                    }),
            )
            .child(
                h_flex()
                    .w(px(NULLS_WIDTH))
                    .flex_shrink_0()
                    .justify_end()
                    .gap(px(8.))
                    .children(nulls.map(|(null_share, count)| {
                        let tone = if null_share >= 50.0 {
                            theme::AMBER
                        } else {
                            theme::TEXT_SECONDARY
                        };
                        h_flex()
                            .gap(px(8.))
                            .when(count > 0, |cell| {
                                cell.child(
                                    div()
                                        .w(px(40.))
                                        .h(px(4.))
                                        .rounded(px(2.))
                                        .bg(color(theme::ACTIVE_STRONG))
                                        .child(
                                            div()
                                                .h_full()
                                                .rounded(px(2.))
                                                .w(relative((null_share / 100.0) as f32))
                                                .bg(color(tone)),
                                        ),
                                )
                            })
                            .child(
                                div()
                                    .w(px(40.))
                                    .text_right()
                                    .font_family(mono.clone())
                                    .text_size(px(12.))
                                    .text_color(color(if count == 0 {
                                        theme::TEXT_QUIET
                                    } else {
                                        tone
                                    }))
                                    .child(percent(null_share, count)),
                            )
                    })),
            )
            .child(
                div()
                    .w(px(DISTINCT_WIDTH))
                    .flex_shrink_0()
                    .text_right()
                    .font_family(mono.clone())
                    .text_size(px(12.))
                    .text_color(color(theme::TEXT_SECONDARY))
                    .children(overview.map(|o| thousands(o.distinct as u64))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(DIGEST_MIN))
                    .children(overview.map(|o| digest(o, mono.clone()))),
            )
            .child(
                div()
                    .w(px(SPARK_WIDTH))
                    .flex_shrink_0()
                    .children(overview.map(|o| spark(&o.spark, SPARK_WIDTH, 18.))),
            )
            .child(self.hide_button(index, name.clone(), group, cx))
            .on_click(cx.listener(move |this, _, _, cx| this.open(name.clone(), cx)))
    }

    /// The eye of a row, shown while the row is hovered.
    fn hide_button(
        &self,
        index: usize,
        name: String,
        group: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(("profile-hide", index))
            .w(px(HIDE_WIDTH))
            .h(px(22.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.))
            .invisible()
            .group_hover(group, |s| s.visible())
            .text_color(color(theme::TEXT_TERTIARY))
            .hover(|s| {
                s.bg(color(theme::ACTIVE_STRONG))
                    .text_color(color(theme::TEXT))
            })
            .child(Icon::new(IconName::EyeOff).size(px(12.)))
            .tooltip(|window, cx| Tooltip::new("Hide column").build(window, cx))
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.hide(name.clone(), cx);
            }))
    }

    /// The columns beside the one open in depth.
    fn render_list(&self, mono: SharedString, cx: &mut Context<Self>) -> impl IntoElement {
        let overviews = self.ready();
        let rows: Vec<AnyElement> = self
            .visible()
            .map(|(index, name)| {
                self.render_row(
                    index,
                    name,
                    overviews.and_then(|o| o.get(index)),
                    mono.clone(),
                    cx,
                )
                .into_any_element()
            })
            .collect();
        v_flex()
            .w(px(LIST_WIDTH))
            .flex_shrink_0()
            .h_full()
            .border_r_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                h_flex()
                    .h(px(40.))
                    .flex_shrink_0()
                    .px(px(8.))
                    .gap_2()
                    .text_size(px(12.))
                    .text_color(color(theme::TEXT_TERTIARY))
                    .child(
                        h_flex()
                            .id("profile-back")
                            .h(px(26.))
                            .pl(px(4.))
                            .pr(px(8.))
                            .gap(px(4.))
                            .rounded(px(6.))
                            .text_color(color(theme::TEXT_SECONDARY))
                            .hover(|s| s.bg(color(theme::ACTIVE)).text_color(color(theme::TEXT)))
                            .child(Icon::new(IconName::ChevronLeft).size(px(13.)))
                            .child("All columns")
                            .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                    )
                    .child(div().flex_1())
                    .child(self.column_count(cx)),
            )
            .child(
                div()
                    .id("profile-columns")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(6.))
                    .children(rows),
            )
    }

    fn render_row(
        &self,
        index: usize,
        name: &str,
        overview: Option<&Overview>,
        mono: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (icon, tone) = kind_icon(self.kind(index));
        let group = SharedString::from(format!("profile-row-{index}"));
        let is_x = self.x.as_deref() == Some(name);
        let is_y = self.y.as_deref() == Some(name);
        let name = name.to_string();
        h_flex()
            .id(("profile-row", index))
            .group(group.clone())
            .h(px(32.))
            .px(px(8.))
            .gap(px(8.))
            .rounded(px(6.))
            .when(is_x || is_y, |row| row.bg(color(theme::ACTIVE)))
            .when(!is_x && !is_y, |row| {
                row.hover(|s| s.bg(color(theme::ROW_EVEN)))
            })
            .child(Icon::new(icon).size(px(13.)).text_color(color(tone)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(mono)
                    .text_size(px(12.5))
                    .text_color(color(if is_x || is_y {
                        theme::TEXT
                    } else {
                        theme::TEXT_SECONDARY
                    }))
                    .child(name.clone()),
            )
            .when(is_y || (is_x && self.y.is_some()), |row| {
                row.child(
                    div()
                        .px(px(5.))
                        .rounded(px(4.))
                        .bg(color(theme::ACCENT).opacity(0.25))
                        .text_size(px(10.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color(theme::TEXT))
                        .child(if is_x { "this" } else { "vs" }),
                )
            })
            .when_some(overview, |row, overview| {
                let null_share = share(overview.nulls, overview.rows);
                row.when(overview.unique, |row| {
                    row.child(
                        Icon::new(AppIcon::Key)
                            .size(px(11.))
                            .text_color(color(theme::TEXT_QUIET)),
                    )
                })
                .child(spark(&overview.spark, 44., 16.))
                .child(
                    div()
                        .w(px(36.))
                        .flex_shrink_0()
                        .text_right()
                        .whitespace_nowrap()
                        .text_size(px(11.))
                        .text_color(color(if null_share >= 50.0 {
                            theme::AMBER
                        } else {
                            theme::TEXT_QUIET
                        }))
                        .child(if overview.nulls == 0 {
                            String::new()
                        } else {
                            percent(null_share, overview.nulls)
                        })
                        .id(("profile-row-nulls", index))
                        .tooltip(move |window, cx| {
                            Tooltip::new(format!("{null_share:.1}% of the rows are NULL"))
                                .build(window, cx)
                        }),
                )
            })
            .child(self.hide_button(index, name.clone(), group, cx))
            .on_click(
                cx.listener(move |this, event, _, cx| this.click_column(name.clone(), event, cx)),
            )
    }

    /// The open column's name, what to compare it with, how to look at it,
    /// and the way back.
    fn render_title(&self, mono: SharedString, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self.x.clone().unwrap_or_default();
        let index = self.index_of(&name);
        let kind = self.kind_of(&name);
        let (icon, tone) = kind_icon(kind);
        let inferred = index
            .and_then(|index| self.inferred.get(index).copied())
            .unwrap_or(false);
        h_flex()
            .flex_wrap()
            .gap(px(8.))
            .child(
                h_flex()
                    .flex_1()
                    .min_w(px(160.))
                    .gap(px(8.))
                    .child(Icon::new(icon).size(px(15.)).text_color(color(tone)))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(mono)
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(name),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(12.))
                            .text_color(color(theme::TEXT_TERTIARY))
                            .child(if inferred {
                                format!("{} · inferred", kind.label())
                            } else {
                                kind.label().to_string()
                            }),
                    ),
            )
            .child(self.compare_picker(cx))
            .children(self.analysis_picker(cx))
            .child(
                chrome::icon_button(
                    "profile-close",
                    IconName::Close,
                    "Back to all columns",
                    true,
                )
                .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
            )
    }

    /// The column to compare the open one with.
    fn compare_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.y.clone();
        let entries: Vec<(String, ColumnKind)> = self
            .visible()
            .filter(|(_, name)| self.x.as_ref() != Some(*name))
            .map(|(index, name)| (name.clone(), self.kind(index)))
            .collect();
        let icon = current.as_ref().map(|name| kind_icon(self.kind_of(name)));
        let text = match &current {
            Some(name) => format!("vs {name}"),
            None => "Compare with".to_string(),
        };
        let weak = cx.entity().downgrade();
        picker("profile-y", icon, text.into(), current.is_some()).dropdown_menu(
            move |mut menu, _, _| {
                menu = menu
                    .item(
                        item("Nothing", &weak, |this, _, cx| this.set_y(None, cx))
                            .checked(current.is_none()),
                    )
                    .separator();
                for (name, kind) in &entries {
                    let picked = name.clone();
                    menu = menu.item(
                        item(name.clone(), &weak, move |this, _, cx| {
                            this.set_y(Some(picked.clone()), cx)
                        })
                        .icon(kind_icon(*kind).0)
                        .checked(current.as_ref() == Some(name)),
                    );
                }
                menu
            },
        )
    }

    /// How to look at the column, or which comparison of the two; nothing
    /// when there is a single way.
    fn analysis_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let weak = cx.entity().downgrade();
        let x = self.x.clone()?;
        if self.y.is_none() {
            let kind = self.kind_of(&x);
            let current = self.options_of(&x).mode;
            let modes: Vec<Mode> = Mode::ALL
                .into_iter()
                .filter(|m| m.applies_to(kind))
                .collect();
            if modes.len() < 2 {
                return None;
            }
            return Some(
                picker(
                    "profile-analysis",
                    Some((AppIcon::Chart, theme::TEXT_SECONDARY)),
                    current.label().into(),
                    true,
                )
                .dropdown_menu(move |mut menu, _, _| {
                    for mode in &modes {
                        let mode = *mode;
                        menu = menu.item(
                            item(mode.label(), &weak, move |this, _, cx| {
                                this.set_mode(mode, cx)
                            })
                            .checked(mode == current),
                        );
                    }
                    menu
                })
                .into_any_element(),
            );
        }
        let available: Vec<(Comparison, String)> = self
            .comparisons()
            .into_iter()
            .map(|c| (c, format!("{} · {}", c.label(), self.describe(c))))
            .collect();
        let current = self.current_comparison()?;
        if available.len() < 2 {
            return None;
        }
        Some(
            picker(
                "profile-analysis",
                Some((AppIcon::Chart, theme::TEXT_SECONDARY)),
                current.label().into(),
                true,
            )
            .dropdown_menu(move |mut menu, _, _| {
                for (comparison, text) in &available {
                    let comparison = *comparison;
                    menu = menu.item(
                        item(text.clone(), &weak, move |this, _, cx| {
                            this.comparison = Some(comparison);
                            this.refresh(cx);
                        })
                        .checked(comparison == current),
                    );
                }
                menu
            })
            .into_any_element(),
        )
    }

    fn render_detail(&self, mono: SharedString, cx: &mut Context<Self>) -> AnyElement {
        let body = match &self.shown {
            Some(shown) => self.render_shown(shown, mono.clone(), cx),
            None if self.task.is_some() => {
                message("Analyzing…").h(px(CHART_HEIGHT)).into_any_element()
            }
            None => message("These two columns have nothing to compare. Pick another one.")
                .h(px(CHART_HEIGHT))
                .into_any_element(),
        };
        div()
            .id("profile-detail")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .p(px(16.))
                    .gap(px(16.))
                    .max_w(px(960.))
                    .child(self.render_title(mono, cx))
                    .child(body),
            )
            .into_any_element()
    }

    fn render_shown(
        &self,
        shown: &Shown,
        mono: SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match (&shown.request, &shown.data) {
            (Request::Single { options, .. }, Data::Single(analysis)) => v_flex()
                .gap(px(14.))
                .child(self.single_facts(analysis, cx))
                .child(self.render_summary(analysis, options, mono, cx))
                .into_any_element(),
            (
                Request::Pair {
                    comparison,
                    options,
                    ..
                },
                Data::Pair(outcome),
            ) => self.render_outcome(*comparison, options, outcome, mono, cx),
            _ => message("").into_any_element(),
        }
    }

    /// The counts of the open column, and what stands out in it; what is
    /// about some of its rows shows them.
    fn single_facts(&self, analysis: &Analysis, cx: &mut Context<Self>) -> impl IntoElement {
        let overview = &analysis.overview;
        let null_share = share(overview.nulls, overview.rows);
        let some = |count: usize, are: &str, is: &str| {
            if count == 1 {
                format!("1 value {is}.")
            } else {
                format!("{} values {are}.", thousands(count as u64))
            }
        };
        let notes: Vec<(String, Option<(&'static str, Match)>)> = overview
            .flags()
            .into_iter()
            .filter_map(|flag| match flag {
                Flag::Empty => Some(("Every row is NULL.".to_string(), None)),
                Flag::Constant => Some((
                    "One value, repeated in every row that has one.".into(),
                    None,
                )),
                Flag::MostlyNull => Some((format!("{null_share:.0}% of the rows are NULL."), None)),
                Flag::Blank(count) => Some((
                    some(
                        count,
                        "are blank: empty or only spaces",
                        "is blank: empty or only spaces",
                    ),
                    Some(("profile-rows-blank", Match::Blank)),
                )),
                Flag::Padded(count) => Some((
                    some(count, "have spaces at the ends", "has spaces at the ends"),
                    Some(("profile-rows-padded", Match::Padded)),
                )),
                Flag::OffFormat(format, count) => Some((
                    format!(
                        "Nearly all values are {}. {}",
                        format.plural(),
                        some(count, "are not", "is not")
                    ),
                    Some(("profile-rows-format", Match::Unlike(format))),
                )),
                Flag::Key => Some(("Every value is different: a likely key.".into(), None)),
                Flag::Outliers(_) => None,
            })
            .collect();
        let pick = self.pick(&overview.name, cx);
        let notes = notes.into_iter().map(|(text, rows)| {
            h_flex()
                .gap(px(6.))
                .child(div().child(text))
                .children(rows.zip(pick.clone()).map(|((id, matching), pick)| {
                    text_button(id, "Show rows")
                        .on_click(move |_, window, cx| pick(matching.clone(), window, cx))
                }))
        });
        v_flex()
            .gap(px(10.))
            .child(
                h_flex()
                    .flex_wrap()
                    .gap(px(28.))
                    .child(stat("Rows", thousands(overview.rows as u64)))
                    .child(stat(
                        "Nulls",
                        if overview.nulls == 0 {
                            "0".into()
                        } else {
                            format!("{} · {null_share:.1}%", thousands(overview.nulls as u64))
                        },
                    ))
                    .child(stat("Distinct", thousands(overview.distinct as u64)))
                    .child(stat("Shown as", analysis.summary.label().to_string())),
            )
            .child(
                v_flex()
                    .gap(px(4.))
                    .text_size(px(12.5))
                    .text_color(color(theme::TEXT_SECONDARY))
                    .children(notes),
            )
    }

    fn render_summary(
        &self,
        analysis: &Analysis,
        options: &Options,
        mono: SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let with = |controls: AnyElement, chart: AnyElement| {
            v_flex()
                .gap(px(14.))
                .child(chart)
                .child(controls)
                .into_any_element()
        };
        let pick = self.pick(&analysis.overview.name, cx);
        match &analysis.summary {
            Summary::Numeric(numbers) => v_flex()
                .gap(px(14.))
                .child(charts::numeric(
                    numbers,
                    options.log_scale,
                    mono.clone(),
                    pick,
                ))
                .when(options.hide_outliers && numbers.outliers > 0, |column| {
                    column.child(charts::footnote(format!(
                        "{} not drawn: beyond 1.5 × the width of the middle half. The figures count {}.",
                        if numbers.outliers == 1 {
                            "1 outlier is".to_string()
                        } else {
                            format!("{} outliers are", thousands(numbers.outliers as u64))
                        },
                        if numbers.outliers == 1 { "it" } else { "them" },
                    )))
                })
                .child(
                    option_row()
                        .child(label("Bins"))
                        .child(
                            chrome::segmented().children(BIN_CHOICES.into_iter().map(|bins| {
                                let text = bins.map_or("Auto".to_string(), |b| b.to_string());
                                choice(
                                    SharedString::from(format!("profile-bins-{text}")),
                                    text,
                                    options.bins == bins,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| this.change_single(|o| o.bins = bins, cx),
                                ))
                            })),
                        )
                        .child(
                            toggle("profile-log", "Log scale", options.log_scale).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.change_single(|o| o.log_scale = !o.log_scale, cx)
                                }),
                            ),
                        )
                        .child(
                            toggle("profile-outliers", "Hide outliers", options.hide_outliers)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.change_single(|o| o.hide_outliers = !o.hide_outliers, cx)
                                })),
                        ),
                )
                .child(charts::numeric_figures(numbers, mono))
                .into_any_element(),
            Summary::Temporal(timeline) => with(
                self.granularity_row(
                    options.granularity,
                    timeline.granularity,
                    analysis.overview.kind == ColumnKind::Date,
                    false,
                    cx,
                )
                .child(self.shape_choice(cx))
                .into_any_element(),
                charts::temporal(timeline, !self.bars_over_time, mono, pick),
            ),
            Summary::Categorical(categories) => with(
                option_row()
                    .child(label("Show"))
                    .child(self.top_choices(options.top, false, cx))
                    .child(
                        toggle("profile-nulls", "Include NULL", options.include_nulls).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.change_single(|o| o.include_nulls = !o.include_nulls, cx)
                            }),
                        ),
                    )
                    .into_any_element(),
                charts::categorical(categories, mono, pick),
            ),
            Summary::Text(text) => with(
                option_row()
                    .child(label("Show"))
                    .child(self.top_choices(options.top, false, cx))
                    .into_any_element(),
                charts::text(text, mono, pick),
            ),
            Summary::Boolean { trues, falses } => {
                charts::boolean(*trues, *falses, analysis.overview.nulls, pick)
            }
            Summary::Key { min, max } => charts::key(min, max, mono),
            Summary::Empty => message("No values to summarize")
                .h(px(CHART_HEIGHT))
                .into_any_element(),
        }
    }

    /// "Top N" choices, for a single column's values or a comparison's
    /// groups.
    fn top_choices(&self, current: usize, pair: bool, cx: &mut Context<Self>) -> impl IntoElement {
        chrome::segmented().children(TOP_CHOICES.into_iter().map(move |top| {
            choice(
                SharedString::from(format!("profile-top-{top}")),
                format!("Top {top}"),
                current == top,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if pair {
                    this.change_pair(|o| o.groups = top, cx)
                } else {
                    this.change_single(|o| o.top = top, cx)
                }
            }))
        }))
    }

    /// Line or bars, for timelines. Only the drawing changes, so nothing is
    /// recomputed.
    fn shape_choice(&self, cx: &mut Context<Self>) -> impl IntoElement {
        chrome::segmented().children([("Line", false), ("Bars", true)].into_iter().map(
            |(text, bars)| {
                choice(
                    SharedString::from(format!("profile-shape-{text}")),
                    text,
                    self.bars_over_time == bars,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.bars_over_time = bars;
                    cx.notify();
                }))
            },
        ))
    }

    fn granularity_row(
        &self,
        asked: Granularity,
        used: Granularity,
        dates_only: bool,
        pair: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let fits = |g: &Granularity| {
            !(dates_only && matches!(g, Granularity::Hour | Granularity::HourOfDay))
        };
        let segments = |choices: &[Granularity]| {
            chrome::segmented().children(choices.iter().filter(|g| fits(g)).map(|granularity| {
                let granularity = *granularity;
                let active = asked == granularity;
                let text = match granularity {
                    Granularity::Auto if active => {
                        format!("Auto · {}", used.label().to_lowercase())
                    }
                    _ => granularity.label().to_string(),
                };
                choice(
                    SharedString::from(format!("profile-granularity-{}", granularity.label())),
                    text,
                    active,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if pair {
                        this.change_pair(|o| o.granularity = granularity, cx)
                    } else {
                        this.change_single(|o| o.granularity = granularity, cx)
                    }
                }))
            }))
        };
        let timeline = segments(&Granularity::TIMELINE);
        let cycles = segments(&Granularity::CYCLES);
        option_row()
            .child(label("Group by"))
            .child(timeline)
            .child(cycles)
            .when(asked != Granularity::Auto && asked != used, |row| {
                row.child(
                    div()
                        .text_size(px(12.))
                        .text_color(color(theme::TEXT_TERTIARY))
                        .child(format!(
                            "Too many buckets: grouped by {}",
                            used.label().to_lowercase()
                        )),
                )
            })
    }

    fn render_outcome(
        &self,
        comparison: Comparison,
        options: &CompareOptions,
        outcome: &Outcome,
        mono: SharedString,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = |index: usize| self.columns().get(index).cloned().unwrap_or_default();
        let dates_only = |time: usize| self.kind(time) == ColumnKind::Date;
        let with = |controls: Div, chart: AnyElement| {
            v_flex()
                .gap(px(14.))
                .child(chart)
                .child(controls)
                .into_any_element()
        };
        match (comparison, outcome) {
            (Comparison::Groups { measure, group }, Outcome::Groups(table)) => with(
                option_row().child(label("Groups")).child(self.top_choices(
                    options.groups,
                    true,
                    cx,
                )),
                charts::groups(table, &name(measure), &name(group), mono),
            ),
            (Comparison::CrossTab { rows, columns }, Outcome::CrossTab(table)) => with(
                option_row()
                    .child(label("Values"))
                    .child(self.top_choices(options.groups, true, cx))
                    .child(
                        toggle("profile-row-percent", "Row %", options.percent).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.change_pair(|o| o.percent = !o.percent, cx)
                            }),
                        ),
                    ),
                charts::cross_tab(table, &name(rows), &name(columns), options.percent, mono),
            ),
            (Comparison::Series { time, measure }, Outcome::Series(series)) => with(
                v_flex()
                    .gap(px(10.))
                    .child(option_row().child(label("Aggregate")).child(
                        chrome::segmented().children(Aggregate::ALL.into_iter().map(|aggregate| {
                            choice(
                                SharedString::from(format!(
                                    "profile-aggregate-{}",
                                    aggregate.label()
                                )),
                                aggregate.label(),
                                options.aggregate == aggregate,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.change_pair(|o| o.aggregate = aggregate, cx)
                                },
                            ))
                        })),
                    ))
                    .child(
                        self.granularity_row(
                            options.granularity,
                            series.granularity,
                            dates_only(time),
                            true,
                            cx,
                        )
                        .child(self.shape_choice(cx)),
                    ),
                charts::series(series, &name(measure), !self.bars_over_time, mono),
            ),
            (Comparison::Breakdown { time, category }, Outcome::Breakdown(breakdown)) => with(
                v_flex()
                    .gap(px(10.))
                    .child(self.granularity_row(
                        options.granularity,
                        breakdown.granularity,
                        dates_only(time),
                        true,
                        cx,
                    ))
                    .child(
                        option_row()
                            .child(label("Categories"))
                            .child(chrome::segmented().children(STACK_CHOICES.into_iter().map(
                                |top| {
                                    choice(
                                        SharedString::from(format!("profile-stack-{top}")),
                                        format!("Top {top}"),
                                        options.groups.min(MAX_STACKED) == top,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.change_pair(|o| o.groups = top, cx)
                                        },
                                    ))
                                },
                            )))
                            .child(
                                toggle("profile-stack-percent", "100%", options.percent).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.change_pair(|o| o.percent = !o.percent, cx)
                                    }),
                                ),
                            ),
                    ),
                charts::breakdown(breakdown, &name(category), options.percent, mono),
            ),
            (Comparison::Correlation { x, y }, Outcome::Correlation(found)) => {
                charts::correlation(found, &name(x), &name(y), mono)
            }
            _ => message("No row has values in both columns")
                .h(px(CHART_HEIGHT))
                .into_any_element(),
        }
    }
}

impl Render for ProfilePanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        if self.columns().is_empty() {
            return message("No columns to profile").into_any_element();
        }
        if self.summarize {
            // Summarized the first time the tab is drawn, not when the
            // results arrive: they may never be looked at here.
            let this = cx.entity().downgrade();
            cx.defer(move |cx| {
                this.update(cx, |this, cx| {
                    if this.summarize {
                        this.profile_all(cx);
                    }
                })
                .ok();
            });
        }
        v_flex()
            .size_full()
            .child(if self.x.is_none() {
                div()
                    .flex_1()
                    .w_full()
                    .min_h_0()
                    .child(self.render_overview(mono, cx))
                    .into_any_element()
            } else {
                h_flex()
                    .flex_1()
                    .w_full()
                    .min_h_0()
                    .items_start()
                    .child(self.render_list(mono.clone(), cx))
                    .child(self.render_detail(mono, cx))
                    .into_any_element()
            })
            .into_any_element()
    }
}

const COLUMN_GAP: f32 = 14.;
const NAME_MIN: f32 = 120.;
const NULLS_WIDTH: f32 = 96.;
const DISTINCT_WIDTH: f32 = 72.;
const DIGEST_MIN: f32 = 120.;
const SPARK_WIDTH: f32 = 96.;
const HIDE_WIDTH: f32 = 22.;

/// A share of the rows: "0%" only for none, "<1%" for a few.
fn percent(share: f64, count: usize) -> String {
    match share {
        _ if count == 0 => "0%".to_string(),
        share if share < 1.0 => "<1%".to_string(),
        share => format!("{share:.0}%"),
    }
}

/// What a column holds, in one line: the median and range of numbers, the
/// span of dates, the most frequent of other values.
fn digest(overview: &Overview, mono: SharedString) -> AnyElement {
    let quiet = |text: String| div().text_color(color(theme::TEXT_TERTIARY)).child(text);
    let line = h_flex().gap(px(6.)).text_size(px(12.)).overflow_hidden();
    // "Emails · all different" when the values have a format.
    let written = |rest: String| {
        let text = match overview.format {
            Some((format, _)) => format!("{} · {rest}", format.plural()),
            None => rest,
        };
        let mut letters = text.chars();
        match letters.next() {
            Some(first) => first.to_uppercase().chain(letters).collect(),
            None => text,
        }
    };
    match &overview.digest {
        Digest::Range {
            min, median, max, ..
        } if min == max => line
            .child(div().font_family(mono).child(format_number(*median)))
            .child(quiet("in every row".into()))
            .into_any_element(),
        Digest::Range {
            min, median, max, ..
        } => line
            .child(
                div()
                    .font_family(mono.clone())
                    .child(format_number(*median)),
            )
            .child(quiet("median ·".into()))
            .child(
                div()
                    .truncate()
                    .font_family(mono)
                    .text_color(color(theme::TEXT_SECONDARY))
                    .child(format!(
                        "{} to {}",
                        format_number(*min),
                        format_number(*max)
                    )),
            )
            .into_any_element(),
        Digest::Span { min, max } => line
            .child(div().truncate().font_family(mono).child(if min == max {
                min.clone()
            } else {
                format!("{min} to {max}")
            }))
            .into_any_element(),
        Digest::Top { .. } if overview.unique => line
            .child(quiet(written("all different".into())))
            .into_any_element(),
        // No value stands for the column when none reaches 1%.
        Digest::Top { count, .. } if count * 100 < overview.values() => line
            .child(quiet(written("almost all different".into())))
            .into_any_element(),
        Digest::Top { value, count } => line
            .child(
                div()
                    .truncate()
                    .font_family(mono)
                    .child(if value.trim().is_empty() {
                        "(blank)".to_string()
                    } else {
                        value.clone()
                    }),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(color(theme::TEXT_TERTIARY))
                    .child(format!("{:.0}%", share(*count, overview.values()))),
            )
            .into_any_element(),
        Digest::None => line.child(quiet("—".into())).into_any_element(),
    }
}

/// r with its sign and two decimals: "+0.82", "−0.31".
fn signed(r: f64) -> String {
    let rounded = (r * 100.0).round() / 100.0;
    if rounded == 0.0 {
        "0.00".to_string()
    } else if rounded < 0.0 {
        format!("−{:.2}", -rounded)
    } else {
        format!("+{rounded:.2}")
    }
}

/// How two columns move together, in words.
fn relation(r: f64) -> String {
    if r.abs() < 0.1 {
        "no relationship".to_string()
    } else {
        format!(
            "{} {}",
            charts::strength(r),
            if r > 0.0 { "positive" } else { "negative" }
        )
    }
}

/// A quiet text action.
fn text_button(id: &'static str, text: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .px(px(6.))
        .rounded(px(4.))
        .text_color(color(theme::ACCENT))
        .hover(|s| s.bg(color(theme::ACTIVE)))
        .child(text)
}

fn kind_icon(kind: ColumnKind) -> (AppIcon, u32) {
    match kind {
        ColumnKind::Integer | ColumnKind::Number => (AppIcon::Hash, 0x4da3ff),
        ColumnKind::Date | ColumnKind::Timestamp => (AppIcon::History, 0xb9a3e6),
        ColumnKind::Boolean => (AppIcon::Toggle, theme::GREEN),
        ColumnKind::Text | ColumnKind::Json | ColumnKind::Id | ColumnKind::Unknown => {
            (AppIcon::TextType, 0xc7c7cc)
        }
    }
}

/// A dropdown control: icon, text and a chevron.
fn picker(
    id: &'static str,
    icon: Option<(AppIcon, u32)>,
    text: SharedString,
    active: bool,
) -> Button {
    Button::new(id)
        .ghost()
        .compact()
        .h(px(30.))
        .px(px(10.))
        .rounded(px(7.))
        .bg(color(theme::CONTROL))
        .border_1()
        .border_color(color(theme::CONTROL_BORDER))
        .child(
            h_flex()
                .gap(px(7.))
                .text_size(px(12.5))
                .text_color(color(if active {
                    theme::TEXT
                } else {
                    theme::TEXT_TERTIARY
                }))
                .children(
                    icon.map(|(icon, tone)| Icon::new(icon).size(px(13.)).text_color(color(tone))),
                )
                .child(div().max_w(px(220.)).truncate().child(text))
                .child(
                    Icon::new(IconName::ChevronDown)
                        .size(px(12.))
                        .text_color(color(theme::TEXT_TERTIARY)),
                ),
        )
}

/// The small chart of a column row.
fn spark(heights: &[f32], width: f32, height: f32) -> impl IntoElement {
    h_flex()
        .w(px(width))
        .h(px(height))
        .flex_shrink_0()
        .items_end()
        .gap(px(1.))
        .children(heights.iter().map(|height| {
            div()
                .flex_1()
                .h(relative(height.max(0.08)))
                .rounded_t(px(1.))
                .bg(color(theme::ACCENT).opacity(0.55))
        }))
}

fn label(text: &'static str) -> impl IntoElement {
    div()
        .w(px(78.))
        .flex_shrink_0()
        .text_size(px(12.))
        .text_color(color(theme::TEXT_TERTIARY))
        .child(text)
}

fn option_row() -> Div {
    h_flex().flex_wrap().gap(px(10.))
}

fn stat(label: &'static str, value: String) -> impl IntoElement {
    v_flex()
        .gap(px(2.))
        .text_size(px(12.5))
        .child(
            div()
                .text_size(px(11.))
                .text_color(color(theme::TEXT_TERTIARY))
                .child(label),
        )
        .child(div().font_weight(FontWeight::MEDIUM).child(value))
}

/// A text option of a segmented control.
fn choice(id: impl Into<ElementId>, text: impl Into<SharedString>, active: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .px(px(10.))
        .rounded(px(5.))
        .when(active, |s| {
            s.bg(color(theme::ACTIVE_STRONG))
                .text_color(color(theme::TEXT))
                .font_weight(FontWeight::MEDIUM)
        })
        .when(!active, |s| {
            s.text_color(color(theme::TEXT_TERTIARY))
                .hover(|s| s.text_color(color(theme::TEXT)))
        })
        .child(text.into())
}

/// An on/off option, with a check when on.
fn toggle(id: impl Into<ElementId>, text: impl Into<SharedString>, on: bool) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(30.))
        .flex()
        .items_center()
        .gap(px(6.))
        .px(px(10.))
        .rounded(px(7.))
        .border_1()
        .text_size(px(12.))
        .when(on, |t| {
            t.bg(color(theme::ACTIVE_STRONG))
                .border_color(color(0x44444a))
                .text_color(color(theme::TEXT))
        })
        .when(!on, |t| {
            t.bg(color(theme::CONTROL))
                .border_color(color(theme::CONTROL_BORDER))
                .text_color(color(theme::TEXT_TERTIARY))
                .hover(|s| s.text_color(color(theme::TEXT)))
        })
        .when(on, |t| t.child(Icon::new(IconName::Check).size(px(12.))))
        .child(text.into())
}
