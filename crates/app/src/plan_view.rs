//! The Plan tab of a query tab: a query plan made readable. A summary (time,
//! rows, where the data came from), insights in plain words, and the plan
//! as a tree of steps, each with what it does, the share of the time (or
//! cost) it takes by itself, and estimated against actual rows. A step
//! opens to its conditions, keys, loops and buffers. The same plan can be
//! drawn as a graph of boxes, and compared with the plan before it (say,
//! before and after creating an index).

use std::collections::HashSet;
use std::rc::Rc;

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, ClipboardItem, Context, EventEmitter, FontWeight, InteractiveElement as _,
    IntoElement, ParentElement as _, PathBuilder, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, canvas, div, point, px, relative,
};
use kuery_core::plan::{
    self, BLOCK_BYTES, Category, Change, Comparison, Insight, Node, Plan, Severity, StepPair,
    bytes, count,
};

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
        plan: Rc<Plan>,
        json: Rc<str>,
    },
    /// A plan in PostgreSQL's text format, shown as is.
    Text(String),
    Failed(String),
}

/// How the plan is drawn.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Tree,
    Graph,
}

pub struct PlanPanel {
    state: State,
    shape: Shape,
    /// The last plan shown, which the next one is compared with.
    latest: Option<Rc<Plan>>,
    /// The plan before the latest one, and how they differ.
    previous: Option<(Rc<Plan>, Rc<Comparison>)>,
    /// Showing the comparison with the previous plan instead of the plan.
    comparing: bool,
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
            shape: Shape::Tree,
            latest: None,
            previous: None,
            comparing: false,
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
    /// JSON). The plan shown before it is kept to compare with.
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
                let plan = Rc::new(plan);
                self.previous = self.latest.replace(plan.clone()).map(|before| {
                    let diff = Rc::new(plan::compare(&before, &plan));
                    (before, diff)
                });
                State::Ready {
                    plan,
                    json: output.into(),
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

    /// Selects a step without folding its details, as the graph and the
    /// comparison do.
    fn reveal(&mut self, id: usize, cx: &mut Context<Self>) {
        self.selected = Some(id);
        self.open.insert(id);
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
            .child(self.view_controls(cx))
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
                            row.on_click(cx.listener(move |this, _, _, cx| this.reveal(id, cx)))
                        })
                })),
        )
    }

    /// Tree or graph, and the comparison with the previous plan when there
    /// is one.
    fn view_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let shape = self.shape;
        let comparing = self.comparing;
        h_flex()
            .gap_2()
            .when(!comparing, |row| {
                row.child(
                    chrome::segmented()
                        .child(
                            chrome::segment(
                                "plan-tree",
                                AppIcon::Plan,
                                "Tree",
                                shape == Shape::Tree,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.shape = Shape::Tree;
                                cx.notify();
                            })),
                        )
                        .child(
                            chrome::segment(
                                "plan-graph",
                                IconName::Network,
                                "Graph",
                                shape == Shape::Graph,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.shape = Shape::Graph;
                                cx.notify();
                            })),
                        ),
                )
            })
            .when(self.previous.is_some(), |row| {
                row.child(
                    chrome::pill_button(
                        "plan-compare",
                        if comparing {
                            "Back to the plan"
                        } else {
                            "Compare with previous"
                        },
                        comparing,
                    )
                    .tooltip(|window, cx| {
                        Tooltip::new("This plan against the one shown before it").build(window, cx)
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.comparing = !this.comparing;
                        cx.notify();
                    })),
                )
            })
    }

    /// The previous plan against this one: totals, the steps that changed,
    /// time by table, and the insights that went away or appeared.
    fn render_comparison(
        &self,
        before: &Plan,
        after: &Plan,
        diff: &Comparison,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let figure = |value: f64| measure_text(value, diff.measured);

        let mut totals = h_flex().flex_wrap().gap(px(10.));
        if let (Some(from), Some(to)) = diff.time_ms {
            totals = totals.child(total_card(
                "Time",
                duration(from),
                duration(to),
                factor_change(from, to),
            ));
        }
        totals = totals.child(total_card(
            "Cost",
            count(diff.cost.0),
            count(diff.cost.1),
            percent_change(diff.cost.0, diff.cost.1),
        ));
        let rows = |plan: &Plan| {
            plan.root
                .actual_rows()
                .map_or_else(|| format!("~{}", count(plan.root.estimated_rows)), count)
        };
        totals = totals.child(total_card("Rows", rows(before), rows(after), None));

        let mut changed: Vec<StepPair> = diff
            .steps
            .iter()
            .copied()
            .filter(|pair| pair.change != Change::Same)
            .collect();
        changed.sort_by_key(|pair| match pair.change {
            Change::Retyped => 0,
            Change::Removed => 1,
            Change::Added => 2,
            Change::Same => 3,
        });
        let unchanged = diff.steps.len() - changed.len();
        let steps = v_flex()
            .gap(px(2.))
            .when(changed.is_empty(), |list| {
                list.child(quiet_line(
                    "The same steps in both plans: only their numbers changed.",
                ))
            })
            .children(changed.into_iter().enumerate().map(|(index, pair)| {
                let old = pair.before.and_then(|id| before.node(id));
                let new = pair.after.and_then(|id| after.node(id));
                let (badge, tone) = match pair.change {
                    Change::Retyped => ("Changed", theme::ACCENT),
                    Change::Added => ("New", theme::GREEN),
                    _ => ("Gone", theme::TEXT_TERTIARY),
                };
                let types = [old, new]
                    .into_iter()
                    .flatten()
                    .map(|node| match &node.index {
                        Some(index) => format!("{} ({index})", node.node_type),
                        None => node.node_type.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join("  →  ");
                let figures = [old, new]
                    .into_iter()
                    .map(|node| node.map_or("—".to_string(), |n| figure(diff.measure(n))))
                    .collect::<Vec<_>>()
                    .join(" → ");
                let title = |node: &Node, gone: bool| {
                    div()
                        .truncate()
                        .font_weight(FontWeight::MEDIUM)
                        .when(gone, |t| {
                            t.line_through().text_color(color(theme::TEXT_TERTIARY))
                        })
                        .child(node.title.clone())
                };
                let target = pair.after;
                h_flex()
                    .id(("plan-change", index))
                    .min_h(px(44.))
                    .px(px(10.))
                    .gap(px(10.))
                    .rounded(px(7.))
                    .when(target.is_some(), |row| {
                        row.hover(|s| s.bg(color(theme::ACTIVE)))
                    })
                    .child(
                        div()
                            .w(px(64.))
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(color(tone))
                            .child(badge),
                    )
                    .children(new.or(old).map(|node| category_icon(node.category)))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(1.))
                            .child(
                                h_flex()
                                    .gap(px(6.))
                                    .min_w_0()
                                    .when_some(
                                        old.filter(|_| pair.change == Change::Retyped),
                                        |line, old| {
                                            line.child(
                                                title(old, false)
                                                    .text_color(color(theme::TEXT_SECONDARY)),
                                            )
                                            .child(
                                                Icon::new(IconName::ArrowRight)
                                                    .size(px(12.))
                                                    .text_color(color(theme::TEXT_TERTIARY)),
                                            )
                                        },
                                    )
                                    .children(new.map(|node| title(node, false)))
                                    .when(new.is_none(), |line| {
                                        line.children(old.map(|node| title(node, true)))
                                    }),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .font_family(mono.clone())
                                    .text_size(px(11.))
                                    .text_color(color(theme::TEXT_QUIET))
                                    .child(types),
                            ),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .font_family(mono.clone())
                            .text_size(px(12.))
                            .text_color(color(theme::TEXT_SECONDARY))
                            .child(figures),
                    )
                    .when_some(target, |row, id| {
                        row.tooltip(|window, cx| {
                            Tooltip::new("Show this step in the plan").build(window, cx)
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.comparing = false;
                            this.reveal(id, cx);
                        }))
                    })
            }))
            .when(unchanged > 0, |list| {
                list.child(div().pt_1().px(px(10.)).child(quiet_line(format!(
                    "{unchanged} {} the same",
                    if unchanged == 1 {
                        "step stays"
                    } else {
                        "steps stay"
                    }
                ))))
            });

        let largest = diff
            .relations
            .iter()
            .flat_map(|r| [r.before, r.after])
            .flatten()
            .fold(0.0_f64, f64::max);
        let tables = v_flex()
            .gap(px(8.))
            .when(diff.relations.is_empty(), |list| {
                list.child(quiet_line("Neither plan reads a table."))
            })
            .children(diff.relations.iter().map(|delta| {
                let bar = |value: Option<f64>, tone: u32| {
                    div().h(px(5.)).w_full().rounded_full().child(
                        div()
                            .h_full()
                            .rounded_full()
                            .w(relative(if largest > 0.0 {
                                (value.unwrap_or(0.0) / largest) as f32
                            } else {
                                0.0
                            }))
                            .bg(color(tone)),
                    )
                };
                let change = match (delta.before, delta.after) {
                    (Some(_), None) => ("no longer read".to_string(), theme::GREEN),
                    (None, Some(_)) => ("newly read".to_string(), theme::AMBER),
                    (Some(from), Some(to)) => percent_change(from, to)
                        .unwrap_or(("same".to_string(), theme::TEXT_TERTIARY)),
                    (None, None) => (String::new(), theme::TEXT_TERTIARY),
                };
                h_flex()
                    .gap(px(12.))
                    .px(px(10.))
                    .child(
                        div()
                            .w(px(170.))
                            .flex_shrink_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .child(delta.relation.clone()),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w(px(80.))
                            .gap(px(3.))
                            .child(bar(delta.before, theme::TEXT_QUIET))
                            .child(bar(delta.after, change.1)),
                    )
                    .child(
                        div()
                            .w(px(190.))
                            .flex_shrink_0()
                            .text_right()
                            .font_family(mono.clone())
                            .text_size(px(12.))
                            .text_color(color(theme::TEXT_SECONDARY))
                            .child(format!(
                                "{} → {}",
                                delta.before.map_or("—".into(), figure),
                                delta.after.map_or("—".into(), figure)
                            )),
                    )
                    .child(
                        div()
                            .w(px(100.))
                            .flex_shrink_0()
                            .text_right()
                            .text_size(px(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(color(change.1))
                            .child(change.0),
                    )
            }));

        let insight_row = |insight: &Insight, icon: IconName, tone: u32, label: &'static str| {
            h_flex()
                .items_start()
                .gap(px(10.))
                .px(px(10.))
                .py(px(5.))
                .child(
                    div()
                        .pt(px(2.))
                        .child(Icon::new(icon).size(px(13.)).text_color(color(tone))),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .when(label == "Gone", |t| {
                            t.line_through().text_color(color(theme::TEXT_TERTIARY))
                        })
                        .child(insight.title.clone()),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .text_color(color(tone))
                        .child(label),
                )
        };
        let insights = v_flex()
            .children(
                diff.gone
                    .iter()
                    .map(|i| insight_row(i, IconName::CircleCheck, theme::GREEN, "Gone")),
            )
            .children(diff.appeared.iter().map(|i| match i.severity {
                Severity::Warning => insight_row(i, IconName::TriangleAlert, theme::AMBER, "New"),
                Severity::Info => insight_row(i, IconName::Info, theme::TEXT_SECONDARY, "New"),
            }))
            .children(
                diff.kept
                    .iter()
                    .map(|i| insight_row(i, IconName::Minus, theme::TEXT_TERTIARY, "Still there")),
            );

        let measured_note = (!diff.measured).then(|| {
            quiet_line(if before.analyzed || after.analyzed {
                "One of the plans is an estimate, so steps and tables compare by estimated cost."
            } else {
                "Both plans are estimates: steps and tables compare by estimated cost."
            })
        });
        div()
            .id("plan-compare")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .px_4()
                    .py_3()
                    .gap(px(18.))
                    .child(
                        v_flex()
                            .gap(px(8.))
                            .child(section_title("Previous plan → this plan"))
                            .child(totals)
                            .children(measured_note),
                    )
                    .child(
                        v_flex()
                            .gap(px(6.))
                            .child(section_title("Steps that changed"))
                            .child(steps),
                    )
                    .child(
                        v_flex()
                            .gap(px(6.))
                            .child(section_title(if diff.measured {
                                "Time by table"
                            } else {
                                "Estimated cost by table"
                            }))
                            .child(tables),
                    )
                    .when(
                        !(diff.gone.is_empty() && diff.appeared.is_empty() && diff.kept.is_empty()),
                        |page| {
                            page.child(
                                v_flex()
                                    .gap(px(2.))
                                    .child(section_title("Insights"))
                                    .child(insights),
                            )
                        },
                    ),
            )
    }

    /// The plan as boxes joined by lines, root at the top, with the
    /// selected step's details beside it.
    fn render_graph(&self, plan: &Plan, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let layout = place(&plan.root);
        let nodes = plan.root.walk();
        let at = |id: usize| layout.boxes.iter().find(|b| b.id == id).map(|b| (b.x, b.y));
        // Lines thicken with the rows that flow along them.
        let rows = |node: &Node| node.actual_rows().unwrap_or(node.estimated_rows).max(0.0);
        let most = nodes.iter().map(|n| rows(n)).fold(1.0_f64, f64::max);
        let edges: Vec<(f32, f32, f32, f32, f32)> = layout
            .edges
            .iter()
            .filter_map(|&(parent, child)| {
                let (px0, py0) = at(parent)?;
                let (cx0, cy0) = at(child)?;
                let flow = nodes
                    .iter()
                    .find(|n| n.id == child)
                    .map_or(0.0, |n| rows(n));
                let width = 1.5 + 4.0 * ((flow + 1.0).ln() / (most + 1.0).ln()) as f32;
                Some((px0 + BOX_W / 2., py0 + BOX_H, cx0 + BOX_W / 2., cy0, width))
            })
            .collect();
        let line = color(0x3a3a40);
        let wires = canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                let origin = bounds.origin;
                for &(x0, y0, x1, y1, width) in &edges {
                    let mid = (y0 + y1) / 2.;
                    let mut path = PathBuilder::stroke(px(width));
                    path.move_to(origin + point(px(x0), px(y0)));
                    path.cubic_bezier_to(
                        origin + point(px(x1), px(y1)),
                        origin + point(px(x0), px(mid)),
                        origin + point(px(x1), px(mid)),
                    );
                    if let Ok(path) = path.build() {
                        window.paint_path(path, line);
                    }
                }
            },
        )
        .absolute()
        .size_full();

        let boxes = layout.boxes.iter().filter_map(|placed| {
            let node = nodes.iter().find(|n| n.id == placed.id)?;
            let id = node.id;
            let share = plan.share(node);
            let tone = share_tone(share);
            let selected = self.selected == Some(id);
            let rows = match node.actual_rows() {
                Some(rows) => format!("{} rows", count(rows)),
                None => format!("~{} rows", count(node.estimated_rows)),
            };
            let tip = format!("{} · {}", node.node_type, node.title);
            Some(
                v_flex()
                    .id(("plan-box", id))
                    .absolute()
                    .left(px(placed.x))
                    .top(px(placed.y))
                    .w(px(BOX_W))
                    .h(px(BOX_H))
                    .rounded(px(8.))
                    .overflow_hidden()
                    .bg(color(theme::SIDEBAR))
                    .border_1()
                    .border_color(color(if selected {
                        theme::ACCENT
                    } else {
                        theme::CONTROL_BORDER
                    }))
                    .when(!selected, |b| b.hover(|s| s.bg(color(0x1f1f22))))
                    .child(
                        h_flex()
                            .flex_1()
                            .min_h_0()
                            .px(px(9.))
                            .gap(px(8.))
                            .child(category_icon(node.category))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap(px(2.))
                                    .child(
                                        div()
                                            .line_clamp(2)
                                            .text_size(px(12.))
                                            .line_height(px(15.))
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(node.title.clone()),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .text_size(px(11.))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .truncate()
                                                    .text_color(color(theme::TEXT_TERTIARY))
                                                    .child(rows),
                                            )
                                            .child(
                                                div()
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .text_color(color(tone))
                                                    .child(format!("{:.0}%", share * 100.0)),
                                            ),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .h(px(3.))
                            .w_full()
                            .bg(color(theme::HAIRLINE))
                            .child(div().h_full().w(relative(share as f32)).bg(color(tone))),
                    )
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    .on_click(cx.listener(move |this, _, _, cx| this.reveal(id, cx))),
            )
        });

        let details = self
            .selected
            .and_then(|id| plan.node(id))
            .map(|node| self.graph_details(plan, node, mono));
        h_flex()
            .flex_1()
            .min_h_0()
            .items_start()
            .child(
                div()
                    .id("plan-graph")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_scroll()
                    .child(
                        div()
                            .relative()
                            .flex_shrink_0()
                            .w(px(layout.width))
                            .h(px(layout.height))
                            .child(wires)
                            .children(boxes),
                    ),
            )
            .children(details)
    }

    /// The selected step beside the graph: what it does, its share, its
    /// facts and the insights about it.
    fn graph_details(&self, plan: &Plan, node: &Node, mono: SharedString) -> impl IntoElement {
        let share = plan.share(node);
        let insights: Vec<&Insight> = plan
            .insights
            .iter()
            .filter(|i| i.node == Some(node.id))
            .collect();
        div()
            .id("plan-graph-details")
            .w(px(340.))
            .h_full()
            .flex_shrink_0()
            .overflow_y_scroll()
            .border_l_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                v_flex()
                    .p(px(12.))
                    .gap(px(10.))
                    .child(
                        h_flex()
                            .gap(px(9.))
                            .child(category_icon(node.category))
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(node.title.clone()),
                                    )
                                    .child(
                                        div()
                                            .font_family(mono.clone())
                                            .text_size(px(11.))
                                            .text_color(color(theme::TEXT_QUIET))
                                            .child(node.node_type.clone()),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(color(share_tone(share)))
                            .child(format!(
                                "{:.0}% of the {} by itself",
                                share * 100.0,
                                if node.self_ms.is_some() && plan.analyzed {
                                    "time"
                                } else {
                                    "estimated cost"
                                }
                            )),
                    )
                    .children(insights.into_iter().map(|insight| {
                        h_flex()
                            .items_start()
                            .gap(px(8.))
                            .p(px(8.))
                            .rounded(px(7.))
                            .bg(color(theme::SIDEBAR))
                            .text_size(px(12.))
                            .child(
                                div().pt(px(1.)).child(
                                    Icon::new(match insight.severity {
                                        Severity::Warning => IconName::TriangleAlert,
                                        Severity::Info => IconName::Info,
                                    })
                                    .size(px(12.))
                                    .text_color(color(
                                        match insight.severity {
                                            Severity::Warning => theme::AMBER,
                                            Severity::Info => theme::TEXT_TERTIARY,
                                        },
                                    )),
                                ),
                            )
                            .child(div().min_w_0().child(insight.title.clone()))
                    }))
                    .child(facts_grid(facts(node), mono)),
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
        let tone = share_tone(share);
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
        div()
            .ml(px(8. + depth as f32 * 22. + 23.))
            .mr_3()
            .mb_1()
            .p(px(10.))
            .rounded(px(7.))
            .bg(color(theme::SIDEBAR))
            .border_1()
            .border_color(color(theme::HAIRLINE))
            .child(facts_grid(facts(node), mono))
    }
}

/// What a step's details list: rows, time, cost, data and its own details.
fn facts(node: &Node) -> Vec<(String, String)> {
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
    facts
}

/// Labelled facts, one per line.
fn facts_grid(facts: Vec<(String, String)>, mono: SharedString) -> impl IntoElement {
    div()
        .grid()
        .grid_cols(1)
        .gap(px(4.))
        .text_size(px(12.))
        .children(facts.into_iter().map(move |(label, value)| {
            h_flex()
                .items_start()
                .gap_3()
                .child(
                    div()
                        .w(px(110.))
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
        }))
}

/// The colour of a step's share: red for the hot ones, amber for the warm.
fn share_tone(share: f64) -> u32 {
    if share >= 0.4 {
        theme::RED
    } else if share >= 0.15 {
        theme::AMBER
    } else {
        theme::ACCENT
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

/// A step's box in the graph.
const BOX_W: f32 = 212.;
const BOX_H: f32 = 64.;
const GAP_X: f32 = 18.;
const GAP_Y: f32 = 38.;
const GRAPH_PAD: f32 = 16.;

/// Where a step's box goes: its top left corner.
#[derive(Debug, PartialEq)]
struct Placed {
    id: usize,
    x: f32,
    y: f32,
}

struct GraphLayout {
    boxes: Vec<Placed>,
    /// Parent and child ids.
    edges: Vec<(usize, usize)>,
    width: f32,
    height: f32,
}

/// Lays the plan out top down: leaves side by side in tree order, each
/// parent centred over its children, a row per depth.
fn place(root: &Node) -> GraphLayout {
    fn visit(
        node: &Node,
        depth: usize,
        next: &mut f32,
        slots: &mut Vec<(usize, f32, usize)>,
        edges: &mut Vec<(usize, usize)>,
    ) -> f32 {
        let slot = if node.children.is_empty() {
            *next += 1.0;
            *next - 1.0
        } else {
            let under: Vec<f32> = node
                .children
                .iter()
                .map(|child| {
                    edges.push((node.id, child.id));
                    visit(child, depth + 1, next, slots, edges)
                })
                .collect();
            (under[0] + under[under.len() - 1]) / 2.0
        };
        slots.push((node.id, slot, depth));
        slot
    }
    let (mut next, mut slots, mut edges) = (0.0, Vec::new(), Vec::new());
    visit(root, 0, &mut next, &mut slots, &mut edges);
    let depth = slots.iter().map(|(_, _, d)| *d).max().unwrap_or(0) as f32 + 1.0;
    GraphLayout {
        boxes: slots
            .into_iter()
            .map(|(id, slot, depth)| Placed {
                id,
                x: GRAPH_PAD + slot * (BOX_W + GAP_X),
                y: GRAPH_PAD + depth as f32 * (BOX_H + GAP_Y),
            })
            .collect(),
        edges,
        width: GRAPH_PAD * 2. + next * (BOX_W + GAP_X) - GAP_X,
        height: GRAPH_PAD * 2. + depth * (BOX_H + GAP_Y) - GAP_Y,
    }
}

/// A step's or table's figure: a duration when measured, a cost otherwise.
fn measure_text(value: f64, measured: bool) -> String {
    if measured {
        duration(value)
    } else {
        format!("cost {}", count(value))
    }
}

/// `40 → 10` reads "4.0× faster", in green; slower ones in red.
fn factor_change(from: f64, to: f64) -> Option<(String, u32)> {
    if from <= 0.0 || to <= 0.0 || from == to {
        return None;
    }
    let (factor, word, tone) = if to < from {
        (from / to, "faster", theme::GREEN)
    } else {
        (to / from, "slower", theme::RED)
    };
    Some((
        if factor >= 10.0 {
            format!("{factor:.0}× {word}")
        } else {
            format!("{factor:.1}× {word}")
        },
        tone,
    ))
}

/// `200 → 50` reads "−75%", in green; increases in red.
fn percent_change(from: f64, to: f64) -> Option<(String, u32)> {
    if from <= 0.0 || from == to {
        return None;
    }
    let change = (to - from) / from * 100.0;
    if change.abs() < 0.5 {
        return None;
    }
    Some(if change < 0.0 {
        (format!("\u{2212}{:.0}%", -change), theme::GREEN)
    } else {
        (format!("+{change:.0}%"), theme::RED)
    })
}

/// A before → after figure with how much it changed.
fn total_card(
    label: &'static str,
    from: String,
    to: String,
    change: Option<(String, u32)>,
) -> impl IntoElement {
    v_flex()
        .min_w(px(170.))
        .gap(px(3.))
        .px(px(12.))
        .py(px(9.))
        .rounded(px(8.))
        .bg(color(theme::SIDEBAR))
        .border_1()
        .border_color(color(theme::HAIRLINE))
        .child(
            div()
                .text_size(px(11.))
                .text_color(color(theme::TEXT_TERTIARY))
                .child(label),
        )
        .child(
            h_flex()
                .gap(px(6.))
                .child(div().text_color(color(theme::TEXT_SECONDARY)).child(from))
                .child(
                    Icon::new(IconName::ArrowRight)
                        .size(px(12.))
                        .text_color(color(theme::TEXT_TERTIARY)),
                )
                .child(div().font_weight(FontWeight::MEDIUM).child(to)),
        )
        .children(change.map(|(text, tone)| {
            div()
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(color(tone))
                .child(text)
        }))
}

fn section_title(text: &'static str) -> impl IntoElement {
    div()
        .text_size(px(11.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(color(theme::TEXT_TERTIARY))
        .child(text)
}

fn quiet_line(text: impl Into<SharedString>) -> impl IntoElement {
    div()
        .text_size(px(12.))
        .text_color(color(theme::TEXT_TERTIARY))
        .child(text.into())
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
                let body = match (&self.previous, self.comparing, self.shape) {
                    (Some((before, diff)), true, _) => {
                        let (before, diff) = (before.clone(), diff.clone());
                        self.render_comparison(&before, &plan, &diff, cx)
                            .into_any_element()
                    }
                    (_, _, Shape::Graph) => self.render_graph(&plan, cx).into_any_element(),
                    (_, _, Shape::Tree) => div()
                        .id("plan-body")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .children(self.render_insights(&plan, cx))
                        .child(self.render_tree(&plan, cx))
                        .into_any_element(),
                };
                v_flex()
                    .size_full()
                    .child(self.render_summary(&plan, &json, cx))
                    .child(body)
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

    #[test]
    fn describes_changes() {
        assert_eq!(
            factor_change(40.0, 10.0),
            Some(("4.0× faster".into(), theme::GREEN))
        );
        assert_eq!(
            factor_change(1.0, 30.0),
            Some(("30× slower".into(), theme::RED))
        );
        assert_eq!(factor_change(5.0, 5.0), None);
        assert_eq!(
            percent_change(200.0, 50.0),
            Some(("\u{2212}75%".into(), theme::GREEN))
        );
        assert_eq!(percent_change(100.0, 100.2), None);
    }

    #[test]
    fn centres_parents_over_their_children() {
        let plan = plan::parse(
            r#"[{"Plan": {"Node Type": "Sort", "Plans": [
              {"Node Type": "Hash Join", "Plans": [
                {"Node Type": "Seq Scan", "Relation Name": "orders"},
                {"Node Type": "Hash", "Plans": [
                  {"Node Type": "Seq Scan", "Relation Name": "customers"}]}]}]}}]"#,
        )
        .unwrap();
        let layout = place(&plan.root);
        let x = |id: usize| layout.boxes.iter().find(|b| b.id == id).unwrap().x;
        let y = |id: usize| layout.boxes.iter().find(|b| b.id == id).unwrap().y;
        // Two leaves: orders (2) and customers (4), one slot each.
        assert_eq!(x(2), GRAPH_PAD);
        assert_eq!(x(4), GRAPH_PAD + BOX_W + GAP_X);
        assert_eq!(x(3), x(4));
        assert_eq!(x(1), (x(2) + x(4)) / 2.0);
        assert_eq!(x(0), x(1));
        assert_eq!(y(4), GRAPH_PAD + 3.0 * (BOX_H + GAP_Y));
        assert_eq!(layout.edges, [(0, 1), (1, 2), (1, 3), (3, 4)]);
        assert_eq!(layout.width, GRAPH_PAD * 2. + 2. * BOX_W + GAP_X);
        assert_eq!(layout.height, GRAPH_PAD * 2. + 4. * BOX_H + 3. * GAP_Y);
    }
}
