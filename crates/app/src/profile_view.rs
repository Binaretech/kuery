//! The Profile tab of a query's results. Nothing is computed until asked:
//! the list on the left shows each column's name and kind, "Profile all"
//! summarizes every column in it, and the builder on the right picks one
//! column, or two to compare, and Analyze runs that. Once a result is on
//! screen, its options (bins, groups, periods…) update it right away.
//!
//! Hidden columns and the choices are kept by column name across runs, so
//! re-running a query keeps the builder as it was left.

use std::collections::{HashMap, HashSet};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::DropdownMenu as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, ClickEvent, Context, Div, ElementId, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, Task, Window, div, px, relative,
};
use kuery_core::compare::{self, Aggregate, CompareOptions, Comparison, MAX_STACKED, Outcome};
use kuery_core::result_set::{ColumnKind, ResultSet};
use kuery_core::stats::{self, Analysis, Granularity, Mode, Options, Overview, Summary};

use crate::assets::AppIcon;
use crate::chrome;
use crate::context_menus::item;
use crate::profile_charts::{self as charts, CHART_HEIGHT, message, share};
use crate::theme::{self, color};
use crate::ui::thousands;

const LIST_WIDTH: f32 = 280.;
const BIN_CHOICES: [Option<usize>; 4] = [None, Some(10), Some(20), Some(50)];
const TOP_CHOICES: [usize; 4] = [5, 10, 25, 50];
const STACK_CHOICES: [usize; 3] = [3, 5, MAX_STACKED];

#[derive(Default)]
enum Overviews {
    /// Not asked for.
    #[default]
    None,
    Computing(#[allow(dead_code)] Task<()>),
    /// One per column, in order.
    Ready(Vec<Overview>),
}

/// What Analyze runs.
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

#[derive(Default)]
pub struct ProfilePanel {
    /// The results, with every column's kind filled in.
    set: Option<ResultSet>,
    /// Per column: whether its kind was inferred from the values.
    inferred: Vec<bool>,
    overviews: Overviews,
    hidden: HashSet<String>,
    x: Option<String>,
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

    /// Takes new results. Only the kinds are worked out (from a sample of
    /// values when the server did not report them); nothing is analyzed.
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
        self.set = Some(set);
        self.x = x.or_else(|| self.first_visible());
        self.y = y.filter(|y| Some(y) != self.x.as_ref());
        self.overviews = Overviews::None;
        self.shown = None;
        self.task = None;
        cx.notify();
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

    fn first_visible(&self) -> Option<String> {
        self.visible().next().map(|(_, name)| name.clone())
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

    /// What the builder would run now.
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

    fn analyze(&mut self, cx: &mut Context<Self>) {
        if let Some(request) = self.request() {
            self.run(request, cx);
        }
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

    /// Summarizes every column for the list.
    fn profile_all(&mut self, cx: &mut Context<Self>) {
        if matches!(self.overviews, Overviews::Computing(_)) {
            return;
        }
        let Some(set) = self.set.clone() else {
            return;
        };
        let generation = self.generation;
        let task = cx.spawn(async move |this, cx| {
            let overviews = cx
                .background_spawn(async move { stats::overview(&set) })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.overviews = Overviews::Ready(overviews);
                cx.notify();
            })
            .ok();
        });
        self.overviews = Overviews::Computing(task);
        cx.notify();
    }

    fn set_x(&mut self, name: String, cx: &mut Context<Self>) {
        if self.y.as_ref() == Some(&name) {
            self.y = None;
        }
        self.x = Some(name);
        self.comparison = None;
        cx.notify();
    }

    fn set_y(&mut self, name: Option<String>, cx: &mut Context<Self>) {
        if name.is_some() && name == self.x {
            return;
        }
        self.y = name;
        self.comparison = None;
        cx.notify();
    }

    fn click_column(&mut self, name: String, event: &ClickEvent, cx: &mut Context<Self>) {
        if event.modifiers().platform {
            let toggled = (self.y.as_ref() != Some(&name)).then_some(name);
            self.set_y(toggled, cx);
        } else {
            if self.x.as_ref() != Some(&name) {
                self.y = None;
            }
            self.set_x(name, cx);
            if event.click_count() == 2 {
                self.analyze(cx);
            }
        }
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if let Some(x) = self.x.clone() {
            self.options.entry(x).or_default().mode = mode;
            cx.notify();
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
        }
        if self.x.as_ref() == Some(&name) {
            self.x = self.y.take().or_else(|| self.first_visible());
        }
        cx.notify();
    }

    fn show_all(&mut self, cx: &mut Context<Self>) {
        self.hidden.clear();
        if self.x.is_none() {
            self.x = self.first_visible();
        }
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

    fn render_list(&self, mono: SharedString, cx: &mut Context<Self>) -> impl IntoElement {
        let total = self.columns().len();
        let hidden = self
            .columns()
            .iter()
            .filter(|c| self.hidden.contains(*c))
            .count();
        let overviews = match &self.overviews {
            Overviews::Ready(overviews) => Some(overviews),
            _ => None,
        };
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
        let profile_all = match &self.overviews {
            Overviews::None => div()
                .id("profile-all")
                .px(px(6.))
                .rounded(px(4.))
                .text_color(color(theme::ACCENT))
                .hover(|s| s.bg(color(theme::ACTIVE)))
                .child("Profile all")
                .tooltip(|window, cx| {
                    Tooltip::new("Count nulls and distinct values of every column")
                        .build(window, cx)
                })
                .on_click(cx.listener(|this, _, _, cx| this.profile_all(cx)))
                .into_any_element(),
            Overviews::Computing(_) => div().child("Profiling…").into_any_element(),
            Overviews::Ready(_) => div().into_any_element(),
        };
        v_flex()
            .w(px(LIST_WIDTH))
            .flex_shrink_0()
            .h_full()
            .border_r_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                h_flex()
                    .h(px(32.))
                    .flex_shrink_0()
                    .px(px(12.))
                    .gap_2()
                    .text_size(px(11.))
                    .text_color(color(theme::TEXT_TERTIARY))
                    .child(div().child("COLUMNS"))
                    .child(div().flex_1().text_color(color(theme::TEXT_QUIET)).child(
                        if hidden == 0 {
                            thousands(total as u64)
                        } else {
                            format!("{} of {}", total - hidden, total)
                        },
                    ))
                    .child(profile_all),
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
            .child(
                div()
                    .px(px(12.))
                    .py(px(8.))
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(color(theme::HAIRLINE))
                    .text_size(px(11.))
                    .text_color(color(theme::TEXT_QUIET))
                    .child("Double-click to analyze · ⌘-click to compare"),
            )
            .when(hidden > 0, |list| {
                list.child(
                    h_flex()
                        .h(px(32.))
                        .flex_shrink_0()
                        .px(px(12.))
                        .gap_2()
                        .border_t_1()
                        .border_color(color(theme::HAIRLINE))
                        .text_size(px(12.))
                        .text_color(color(theme::TEXT_TERTIARY))
                        .child(Icon::new(IconName::EyeOff).size(px(12.)))
                        .child(div().flex_1().child(if hidden == 1 {
                            "1 hidden column".to_string()
                        } else {
                            format!("{hidden} hidden columns")
                        }))
                        .child(
                            div()
                                .id("profile-show-all")
                                .px(px(6.))
                                .rounded(px(4.))
                                .text_color(color(theme::ACCENT))
                                .hover(|s| s.bg(color(theme::ACTIVE)))
                                .child("Show all")
                                .on_click(cx.listener(|this, _, _, cx| this.show_all(cx))),
                        ),
                )
            })
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
                        .child(if is_x { "X" } else { "Y" }),
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
                .child(spark(&overview.spark))
                .child(
                    div()
                        .w(px(34.))
                        .flex_shrink_0()
                        .text_right()
                        .text_size(px(11.))
                        .text_color(color(if null_share >= 50.0 {
                            theme::AMBER
                        } else {
                            theme::TEXT_QUIET
                        }))
                        .child(if overview.nulls == 0 {
                            String::new()
                        } else {
                            format!("{null_share:.0}% ∅")
                        }),
                )
            })
            .child(
                div()
                    .id(("profile-hide", index))
                    .p(px(3.))
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
                    .on_click({
                        let name = name.clone();
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.hide(name.clone(), cx);
                        })
                    }),
            )
            .on_click(
                cx.listener(move |this, event, _, cx| this.click_column(name.clone(), event, cx)),
            )
    }

    fn render_builder(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let can_run = self.request().is_some();
        let analyze = chrome::primary_button("profile-analyze", IconName::Play, "Analyze", None)
            .when(can_run, |button| {
                button.on_click(cx.listener(|this, _, _, cx| this.analyze(cx)))
            })
            .when(!can_run, |button| button.opacity(0.5));
        h_flex()
            .flex_wrap()
            .gap(px(8.))
            .child(self.column_picker(false, cx))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(color(theme::TEXT_TERTIARY))
                    .child("vs"),
            )
            .child(self.column_picker(true, cx))
            .child(chrome::toolbar_divider())
            .child(self.analysis_picker(cx))
            .child(div().flex_1())
            .child(analyze)
    }

    /// The X or Y column dropdown.
    fn column_picker(&self, y: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let current = if y { self.y.clone() } else { self.x.clone() };
        let entries: Vec<(String, ColumnKind)> = self
            .visible()
            .filter(|(_, name)| !y || self.x.as_ref() != Some(*name))
            .map(|(index, name)| (name.clone(), self.kind(index)))
            .collect();
        let icon = current.as_ref().map(|name| kind_icon(self.kind_of(name)));
        let text = current
            .clone()
            .unwrap_or_else(|| (if y { "Add a column" } else { "Pick a column" }).into());
        let weak = cx.entity().downgrade();
        picker(
            if y { "profile-y" } else { "profile-x" },
            icon,
            text.into(),
            current.is_some(),
        )
        .dropdown_menu(move |mut menu, _, _| {
            if y {
                menu = menu
                    .item(
                        item("None", &weak, |this, _, cx| this.set_y(None, cx))
                            .checked(current.is_none()),
                    )
                    .separator();
            }
            for (name, kind) in &entries {
                let picked = name.clone();
                menu = menu.item(
                    item(name.clone(), &weak, move |this, _, cx| {
                        if y {
                            this.set_y(Some(picked.clone()), cx)
                        } else {
                            this.set_x(picked.clone(), cx)
                        }
                    })
                    .icon(kind_icon(*kind).0)
                    .checked(current.as_ref() == Some(name)),
                );
            }
            menu
        })
    }

    fn analysis_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.entity().downgrade();
        let Some(x) = self.x.clone() else {
            return picker("profile-analysis", None, "Analysis".into(), false).into_any_element();
        };
        if self.y.is_none() {
            let kind = self.kind_of(&x);
            let current = self.options_of(&x).mode;
            let modes: Vec<Mode> = Mode::ALL
                .into_iter()
                .filter(|m| m.applies_to(kind))
                .collect();
            return picker(
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
            .into_any_element();
        }
        let available: Vec<(Comparison, String)> = self
            .comparisons()
            .into_iter()
            .map(|c| (c, format!("{} · {}", c.label(), self.describe(c))))
            .collect();
        let Some(current) = self.current_comparison() else {
            return picker(
                "profile-analysis",
                None,
                "No comparison fits these columns".into(),
                false,
            )
            .into_any_element();
        };
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
                        cx.notify();
                    })
                    .checked(comparison == current),
                );
            }
            menu
        })
        .into_any_element()
    }

    fn render_detail(&self, mono: SharedString, cx: &mut Context<Self>) -> AnyElement {
        let running = self.task.is_some();
        let body = match &self.shown {
            None if running => message("Analyzing…").h(px(CHART_HEIGHT)).into_any_element(),
            None => v_flex()
                .h(px(CHART_HEIGHT))
                .items_center()
                .justify_center()
                .gap(px(6.))
                .text_color(color(theme::TEXT_TERTIARY))
                .child("Pick a column, or two to compare, and press Analyze.")
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(color(theme::TEXT_QUIET))
                        .child("Nothing is computed until then."),
                )
                .into_any_element(),
            Some(shown) => {
                let outdated = self.request().as_ref() != Some(&shown.request);
                v_flex()
                    .gap(px(14.))
                    .when(outdated || running, |body| {
                        body.child(
                            h_flex()
                                .gap_2()
                                .text_size(px(12.))
                                .text_color(color(theme::TEXT_TERTIARY))
                                .child(Icon::new(IconName::Info).size(px(12.)))
                                .child(if running {
                                    "Analyzing…"
                                } else {
                                    "Showing the previous choice · press Analyze to update"
                                }),
                        )
                    })
                    .child(self.render_shown(shown, mono, cx))
                    .into_any_element()
            }
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
                    .child(self.render_builder(cx))
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
            (Request::Single { column, options }, Data::Single(analysis)) => v_flex()
                .gap(px(14.))
                .child(self.single_header(column, analysis, mono.clone()))
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

    fn single_header(
        &self,
        column: &str,
        analysis: &Analysis,
        mono: SharedString,
    ) -> impl IntoElement {
        let overview = &analysis.overview;
        let (icon, tone) = kind_icon(overview.kind);
        let inferred = self
            .index_of(column)
            .and_then(|index| self.inferred.get(index).copied())
            .unwrap_or(false);
        let null_share = share(overview.nulls, overview.rows);
        h_flex()
            .flex_wrap()
            .gap(px(22.))
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(Icon::new(icon).size(px(15.)).text_color(color(tone)))
                    .child(
                        div()
                            .font_family(mono)
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(column.to_string()),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(color(theme::TEXT_TERTIARY))
                            .child(if inferred {
                                format!("{} · inferred", overview.kind.label())
                            } else {
                                overview.kind.label().to_string()
                            }),
                    ),
            )
            .child(stat("rows", thousands(overview.rows as u64)))
            .child(stat(
                "nulls",
                if overview.nulls == 0 {
                    "0".into()
                } else {
                    format!("{} · {null_share:.1}%", thousands(overview.nulls as u64))
                },
            ))
            .child(stat("distinct", thousands(overview.distinct as u64)))
            .child(stat("shown as", analysis.summary.label().to_string()))
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
                .child(controls)
                .child(chart)
                .into_any_element()
        };
        match &analysis.summary {
            Summary::Numeric(numbers) => with(
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
                        toggle(
                            "profile-outliers",
                            if numbers.outliers > 0 {
                                format!("Hide outliers · {}", thousands(numbers.outliers as u64))
                            } else {
                                "Hide outliers".to_string()
                            },
                            options.hide_outliers,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.change_single(|o| o.hide_outliers = !o.hide_outliers, cx)
                        })),
                    )
                    .into_any_element(),
                charts::numeric(numbers, options.log_scale, mono),
            ),
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
                charts::temporal(timeline, !self.bars_over_time, mono),
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
                charts::categorical(categories, mono),
            ),
            Summary::Text(text) => with(
                option_row()
                    .child(label("Show"))
                    .child(self.top_choices(options.top, false, cx))
                    .into_any_element(),
                charts::text(text, mono),
            ),
            Summary::Boolean { trues, falses } => {
                charts::boolean(*trues, *falses, analysis.overview.nulls)
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
                .child(controls)
                .child(chart)
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
        v_flex()
            .size_full()
            .children(self.scope.clone().map(|scope| {
                div()
                    .px_3()
                    .py_1()
                    .text_size(px(12.))
                    .text_color(color(theme::TEXT_SECONDARY))
                    .child(format!("Profile · {scope}"))
            }))
            .child(
                h_flex()
                    .flex_1()
                    .w_full()
                    .min_h_0()
                    .items_start()
                    .child(self.render_list(mono.clone(), cx))
                    .child(self.render_detail(mono, cx)),
            )
            .into_any_element()
    }
}

fn kind_icon(kind: ColumnKind) -> (AppIcon, u32) {
    match kind {
        ColumnKind::Integer | ColumnKind::Number => (AppIcon::Hash, 0x4da3ff),
        ColumnKind::Date | ColumnKind::Timestamp => (AppIcon::History, 0xb9a3e6),
        ColumnKind::Boolean => (AppIcon::Toggle, theme::GREEN),
        ColumnKind::Text | ColumnKind::Unknown => (AppIcon::TextType, 0xc7c7cc),
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
fn spark(heights: &[f32]) -> impl IntoElement {
    h_flex()
        .w(px(44.))
        .h(px(16.))
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
