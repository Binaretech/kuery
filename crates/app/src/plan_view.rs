//! The Plan tab of a query tab: a query plan made readable. A summary (time,
//! rows, where the data came from), insights in plain words, and the plan
//! as a tree of steps, each with what it does, the share of the time (or
//! cost) it takes by itself, and estimated against actual rows. A step
//! opens to its conditions, keys, loops and buffers.

use std::collections::HashSet;

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, ClipboardItem, Context, EventEmitter, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, div, px, relative,
};
use kuery_core::plan::{self, BLOCK_BYTES, Category, Node, Plan, Severity, bytes, count};

use crate::assets::AppIcon;
use crate::chrome;
use crate::theme::{self, color};

/// What the tab asks the query tab for.
pub enum PlanEvent {
    /// Explain the statement under the cursor, measured or not.
    Explain { analyze: bool },
}

enum State {
    Empty,
    Loading {
        analyze: bool,
    },
    Ready {
        plan: Box<Plan>,
        json: String,
    },
    /// A plan in PostgreSQL's text format, shown as is.
    Text(String),
    Failed(String),
}

pub struct PlanPanel {
    state: State,
    /// Steps whose children are hidden.
    collapsed: HashSet<usize>,
    /// Steps showing their details.
    open: HashSet<usize>,
    selected: Option<usize>,
}

impl EventEmitter<PlanEvent> for PlanPanel {}

impl Default for PlanPanel {
    fn default() -> Self {
        Self {
            state: State::Empty,
            collapsed: HashSet::new(),
            open: HashSet::new(),
            selected: None,
        }
    }
}

impl PlanPanel {
    pub fn loading(&mut self, analyze: bool, cx: &mut Context<Self>) {
        self.state = State::Loading { analyze };
        cx.notify();
    }

    /// Shows a plan from PostgreSQL's JSON (or its text, when it is not
    /// JSON).
    pub fn show(&mut self, output: String, cx: &mut Context<Self>) {
        self.collapsed.clear();
        self.open.clear();
        self.selected = None;
        self.state = match plan::parse(&output) {
            Ok(plan) => {
                // The hottest step starts open, so its details are in view.
                let hottest = plan.hottest().id;
                self.open.insert(hottest);
                self.selected = Some(hottest);
                State::Ready {
                    plan: Box::new(plan),
                    json: output,
                }
            }
            Err(_) if !output.trim_start().starts_with(['[', '{']) => State::Text(output),
            Err(error) => State::Failed(error),
        };
        cx.notify();
    }

    pub fn fail(&mut self, error: String, cx: &mut Context<Self>) {
        self.state = State::Failed(error);
        cx.notify();
    }

    fn select(&mut self, id: usize, cx: &mut Context<Self>) {
        self.selected = Some(id);
        if !self.open.insert(id) {
            self.open.remove(&id);
        }
        cx.notify();
    }

    fn toggle_children(&mut self, id: usize, cx: &mut Context<Self>) {
        if !self.collapsed.insert(id) {
            self.collapsed.remove(&id);
        }
        cx.notify();
    }

    fn explain_buttons(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_2()
            .child(
                chrome::pill_button("plan-explain", "Explain", false)
                    .tooltip(|window, cx| {
                        Tooltip::new("The estimated plan, without running the query")
                            .build(window, cx)
                    })
                    .on_click(
                        cx.listener(|_, _, _, cx| cx.emit(PlanEvent::Explain { analyze: false })),
                    ),
            )
            .child(
                chrome::pill_button("plan-analyze", "Explain Analyze", false)
                    .tooltip(|window, cx| {
                        Tooltip::new("Runs the query and measures every step").build(window, cx)
                    })
                    .on_click(
                        cx.listener(|_, _, _, cx| cx.emit(PlanEvent::Explain { analyze: true })),
                    ),
            )
    }

    fn render_summary(&self, plan: &Plan, json: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let stat = |label: &'static str, value: String| {
            v_flex()
                .gap(px(1.))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(color(theme::TEXT_TERTIARY))
                        .child(label),
                )
                .child(div().font_weight(FontWeight::MEDIUM).child(value))
        };
        let buffers = plan.buffers();
        let root = &plan.root;
        let json = json.to_string();
        h_flex()
            .flex_wrap()
            .gap(px(22.))
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                chrome::pill()
                    .pr(px(10.))
                    .child(chrome::dot(if plan.analyzed {
                        theme::GREEN
                    } else {
                        theme::TEXT_TERTIARY
                    }))
                    .child(if plan.analyzed {
                        "Measured"
                    } else {
                        "Estimated"
                    }),
            )
            .children(plan.execution_ms.map(|ms| stat("execution", duration(ms))))
            .children(plan.planning_ms.map(|ms| stat("planning", duration(ms))))
            .child(match root.actual_rows() {
                Some(rows) => stat("rows", count(rows)),
                None => stat("estimated rows", count(root.estimated_rows)),
            })
            .child(stat("cost", count(root.total_cost)))
            .when(buffers.hit + buffers.read > 0, |row| {
                row.child(stat(
                    "data",
                    format!(
                        "{} cached · {} read",
                        bytes(buffers.hit * BLOCK_BYTES),
                        bytes(buffers.read * BLOCK_BYTES)
                    ),
                ))
            })
            .child(div().flex_1())
            .child(self.explain_buttons(cx))
            .child(
                chrome::icon_button("plan-copy", IconName::Copy, "Copy the plan as JSON", true)
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(json.clone()))
                    }),
            )
    }

    fn render_insights(&self, plan: &Plan, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if plan.insights.is_empty() {
            return None;
        }
        Some(
            v_flex()
                .gap(px(6.))
                .px_4()
                .py_3()
                .border_b_1()
                .border_color(color(theme::HAIRLINE))
                .children(plan.insights.iter().enumerate().map(|(index, insight)| {
                    let (icon, tone) = match insight.severity {
                        Severity::Warning => (IconName::TriangleAlert, theme::AMBER),
                        Severity::Info => (IconName::Info, theme::TEXT_TERTIARY),
                    };
                    let node = insight.node;
                    h_flex()
                        .id(("plan-insight", index))
                        .items_start()
                        .gap(px(10.))
                        .px(px(10.))
                        .py(px(7.))
                        .rounded(px(7.))
                        .bg(color(theme::SIDEBAR))
                        .when(node.is_some(), |row| {
                            row.hover(|s| s.bg(color(theme::ACTIVE)))
                        })
                        .child(
                            div()
                                .pt(px(2.))
                                .child(Icon::new(icon).size(px(13.)).text_color(color(tone))),
                        )
                        .child(
                            v_flex()
                                .gap(px(2.))
                                .min_w_0()
                                .child(
                                    div()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(insight.title.clone()),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(color(theme::TEXT_SECONDARY))
                                        .child(insight.detail.clone()),
                                ),
                        )
                        .when_some(node, |row, id| {
                            row.on_click(cx.listener(move |this, _, _, cx| {
                                this.selected = Some(id);
                                this.open.insert(id);
                                cx.notify();
                            }))
                        })
                })),
        )
    }

    fn render_tree(&self, plan: &Plan, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let mut rows = Vec::new();
        self.node_rows(plan, &plan.root, 0, mono, &mut rows, cx);
        v_flex().px_2().py_2().gap(px(1.)).children(rows)
    }

    fn node_rows(
        &self,
        plan: &Plan,
        node: &Node,
        depth: usize,
        mono: SharedString,
        rows: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        let id = node.id;
        let share = plan.share(node);
        let selected = self.selected == Some(id);
        let collapsed = self.collapsed.contains(&id);
        let has_children = !node.children.is_empty();
        let tone = if share >= 0.4 {
            theme::RED
        } else if share >= 0.15 {
            theme::AMBER
        } else {
            theme::ACCENT
        };
        let estimate = row_estimate(node);
        let subtitle = [
            Some(node.node_type.clone()),
            node.parent_relationship
                .clone()
                .filter(|r| r != "Outer" && r != "Inner" && r != "Member"),
            node.actual
                .filter(|a| a.loops > 1.0)
                .map(|a| format!("{} loops", count(a.loops))),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        rows.push(
            h_flex()
                .id(("plan-node", id))
                .min_h(px(44.))
                .pl(px(8. + depth as f32 * 22.))
                .pr_3()
                .gap(px(9.))
                .rounded(px(7.))
                .when(selected, |row| row.bg(color(theme::ACTIVE)))
                .when(!selected, |row| row.hover(|s| s.bg(color(0x1f1f22))))
                .child(div().id(("plan-fold", id)).w(px(14.)).flex_shrink_0().when(
                    has_children,
                    |fold| {
                        fold.child(
                            Icon::new(if collapsed {
                                IconName::ChevronRight
                            } else {
                                IconName::ChevronDown
                            })
                            .size(px(12.))
                            .text_color(color(theme::TEXT_TERTIARY)),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle_children(id, cx);
                        }))
                    },
                ))
                .child(category_icon(node.category))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(1.))
                        .child(
                            div()
                                .truncate()
                                .font_weight(FontWeight::MEDIUM)
                                .child(node.title.clone()),
                        )
                        .child(
                            div()
                                .truncate()
                                .font_family(mono.clone())
                                .text_size(px(11.))
                                .text_color(color(theme::TEXT_QUIET))
                                .child(subtitle),
                        ),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(12.))
                        .text_color(color(if estimate.1 {
                            theme::AMBER
                        } else {
                            theme::TEXT_TERTIARY
                        }))
                        .child(estimate.0),
                )
                .child(
                    div()
                        .w(px(72.))
                        .flex_shrink_0()
                        .text_right()
                        .text_size(px(12.))
                        .font_family(mono.clone())
                        .child(match node.inclusive_ms {
                            Some(ms) => duration(ms),
                            None => format!("cost {}", count(node.total_cost)),
                        }),
                )
                .child(
                    h_flex()
                        .w(px(132.))
                        .flex_shrink_0()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .h(px(6.))
                                .rounded_full()
                                .bg(color(theme::HAIRLINE))
                                .child(
                                    div()
                                        .h_full()
                                        .w(relative(share as f32))
                                        .rounded_full()
                                        .bg(color(tone)),
                                ),
                        )
                        .child(
                            div()
                                .w(px(34.))
                                .text_right()
                                .text_size(px(11.5))
                                .text_color(color(theme::TEXT_SECONDARY))
                                .child(format!("{:.0}%", share * 100.0)),
                        ),
                )
                .tooltip(move |window, cx| {
                    Tooltip::new(if share > 0.0 {
                        "The bar is the share this step takes by itself"
                    } else {
                        "Click for details"
                    })
                    .build(window, cx)
                })
                .on_click(cx.listener(move |this, _, _, cx| this.select(id, cx)))
                .into_any_element(),
        );
        if self.open.contains(&id) {
            rows.push(
                self.node_details(node, depth, mono.clone())
                    .into_any_element(),
            );
        }
        if !collapsed {
            for child in &node.children {
                self.node_rows(plan, child, depth + 1, mono.clone(), rows, cx);
            }
        }
    }

    fn node_details(&self, node: &Node, depth: usize, mono: SharedString) -> impl IntoElement {
        let mut facts: Vec<(String, String)> = Vec::new();
        if let Some(actual) = node.actual {
            facts.push((
                "Rows".into(),
                format!(
                    "{} estimated per loop, {} actual{}",
                    count(node.estimated_rows),
                    count(actual.rows_per_loop),
                    if actual.loops > 1.0 {
                        format!(" × {} loops", count(actual.loops))
                    } else {
                        String::new()
                    }
                ),
            ));
            if let Some(self_ms) = node.self_ms {
                facts.push((
                    "Time".into(),
                    format!(
                        "{} by itself, {} with what it reads from",
                        duration(self_ms),
                        duration(node.inclusive_ms.unwrap_or(0.0))
                    ),
                ));
            }
        } else {
            facts.push((
                "Rows".into(),
                format!("{} estimated", count(node.estimated_rows)),
            ));
        }
        facts.push((
            "Cost".into(),
            format!(
                "{} to start, {} in all ({} by itself)",
                count(node.startup_cost),
                count(node.total_cost),
                count(node.self_cost)
            ),
        ));
        if let Some(removed) = node.rows_removed.filter(|r| *r > 0.0) {
            facts.push(("Discarded".into(), format!("{} rows", count(removed))));
        }
        let buffers = node.buffers;
        if buffers.hit + buffers.read + buffers.temp_written > 0 {
            facts.push((
                "Data".into(),
                format!(
                    "{} cached, {} read{}",
                    bytes(buffers.hit * BLOCK_BYTES),
                    bytes(buffers.read * BLOCK_BYTES),
                    if buffers.temp_written > 0 {
                        format!(
                            ", {} written to temporary files",
                            bytes(buffers.temp_written * BLOCK_BYTES)
                        )
                    } else {
                        String::new()
                    }
                ),
            ));
        }
        facts.extend(node.details.iter().cloned());
        div()
            .ml(px(8. + depth as f32 * 22. + 23.))
            .mr_3()
            .mb_1()
            .p(px(10.))
            .rounded(px(7.))
            .bg(color(theme::SIDEBAR))
            .border_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                div()
                    .grid()
                    .grid_cols(1)
                    .gap(px(4.))
                    .text_size(px(12.))
                    .children(facts.into_iter().map(|(label, value)| {
                        h_flex()
                            .items_start()
                            .gap_3()
                            .child(
                                div()
                                    .w(px(120.))
                                    .flex_shrink_0()
                                    .text_color(color(theme::TEXT_TERTIARY))
                                    .child(label),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .font_family(mono.clone())
                                    .text_color(color(theme::TEXT))
                                    .child(value),
                            )
                    })),
            )
    }
}

/// The step's estimated and actual rows, and whether they disagree a lot.
fn row_estimate(node: &Node) -> (String, bool) {
    match (node.actual, node.misestimate()) {
        (Some(actual), Some(ratio)) => {
            let off = (ratio >= 10.0 || ratio <= 0.1)
                && actual.rows_per_loop.max(node.estimated_rows) >= 100.0;
            (
                format!(
                    "{} → {} rows",
                    count(node.estimated_rows),
                    count(actual.rows_per_loop)
                ),
                off,
            )
        }
        _ => (format!("~{} rows", count(node.estimated_rows)), false),
    }
}

fn category_icon(category: Category) -> impl IntoElement {
    let (icon, tone): (Icon, u32) = match category {
        Category::Scan => (Icon::from(AppIcon::Table), 0x4da3ff),
        Category::Join => (Icon::from(AppIcon::Link), 0xb9a3e6),
        Category::Aggregate => (Icon::from(AppIcon::Chart), theme::GREEN),
        Category::Sort => (Icon::from(AppIcon::Sort), theme::AMBER),
        Category::Modify => (Icon::from(AppIcon::Pencil), theme::RED),
        Category::Other => (Icon::from(IconName::Ellipsis), theme::TEXT_TERTIARY),
    };
    div()
        .size(px(26.))
        .flex_shrink_0()
        .rounded(px(7.))
        .bg(color(theme::CONTROL))
        .flex()
        .items_center()
        .justify_center()
        .child(icon.size(px(13.)).text_color(color(tone)))
}

/// `0.42` → `0.42 ms`, `1530` → `1.53 s`.
fn duration(ms: f64) -> String {
    if ms >= 1000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else if ms >= 10.0 {
        format!("{ms:.1} ms")
    } else {
        format!("{ms:.2} ms")
    }
}

impl Render for PlanPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let message = |text: String, tone: u32| {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px_6()
                .text_color(color(tone))
                .child(text)
        };
        match &self.state {
            State::Empty => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .text_color(color(theme::TEXT_TERTIARY))
                .child("Explain the statement under the cursor to see how PostgreSQL runs it.")
                .child(self.explain_buttons(cx))
                .into_any_element(),
            State::Loading { analyze } => message(
                if *analyze {
                    "Running the query to measure its plan…".into()
                } else {
                    "Asking for the plan…".into()
                },
                theme::TEXT_TERTIARY,
            )
            .size_full()
            .into_any_element(),
            State::Failed(error) => v_flex()
                .size_full()
                .child(message(error.clone(), theme::RED))
                .child(
                    h_flex()
                        .justify_center()
                        .pb_4()
                        .child(self.explain_buttons(cx)),
                )
                .into_any_element(),
            State::Text(text) => v_flex()
                .size_full()
                .child(
                    h_flex()
                        .px_4()
                        .py_2()
                        .gap_3()
                        .border_b_1()
                        .border_color(color(theme::HAIRLINE))
                        .text_size(px(12.))
                        .text_color(color(theme::TEXT_TERTIARY))
                        .child("A plan in text form. Explain it again to see it as a tree.")
                        .child(div().flex_1())
                        .child(self.explain_buttons(cx)),
                )
                .child(
                    div()
                        .id("plan-text")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .p_4()
                        .font_family(mono)
                        .text_size(px(12.))
                        .child(text.clone()),
                )
                .into_any_element(),
            State::Ready { plan, json } => {
                let plan = plan.clone();
                let json = json.clone();
                v_flex()
                    .size_full()
                    .child(self.render_summary(&plan, &json, cx))
                    .child(
                        div()
                            .id("plan-body")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .children(self.render_insights(&plan, cx))
                            .child(self.render_tree(&plan, cx)),
                    )
                    .into_any_element()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations() {
        assert_eq!(duration(0.4219), "0.42 ms");
        assert_eq!(duration(60.0), "60.0 ms");
        assert_eq!(duration(1530.0), "1.53 s");
    }
}
