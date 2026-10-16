//! A schema's diagram in a tab: every table as a card with its columns (or
//! only its keys), and every foreign key as a line in crow's foot notation,
//! from the referencing column (three prongs: "many") to the referenced key
//! (two bars: "one"); dashed when optional. The layout comes from
//! [`kuery_core::diagram`]: referenced tables left, the tables that depend
//! on them right.
//!
//! Selecting a table highlights it, its relations and its neighbors;
//! double-clicking opens it. The diagram zooms in steps and exports to SVG.

use std::path::PathBuf;

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Bounds, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, PathBuilder, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, Window, canvas, div, fill, point, px, size,
};
use kuery_core::catalog::{TableInfo, TableKind};
use kuery_core::diagram::{self, CARD_WIDTH, Diagram, HEADER, Layout, ROW, Wire, end_marks, wires};

use crate::assets::AppIcon;
use crate::chrome;
use crate::connection::Db;
use crate::theme::{self, color};
use crate::ui::thousands;

const ZOOMS: [f32; 7] = [0.4, 0.55, 0.7, 0.85, 1.0, 1.25, 1.5];

pub enum DiagramEvent {
    OpenTable(TableInfo),
}

enum State {
    Loading,
    Ready { diagram: Diagram, layout: Layout },
    Failed(String),
}

pub struct DiagramTab {
    db: Db,
    schema: String,
    /// The project folder, where the SVG export starts.
    project: PathBuf,
    state: State,
    keys_only: bool,
    zoom: f32,
    selected: Option<usize>,
    task: Option<Task<()>>,
    focus_handle: FocusHandle,
}

impl EventEmitter<DiagramEvent> for DiagramTab {}

impl DiagramTab {
    pub fn new(db: Db, schema: String, project: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut tab = Self {
            db,
            schema,
            project,
            state: State::Loading,
            keys_only: false,
            zoom: 1.0,
            selected: None,
            task: None,
            focus_handle: cx.focus_handle(),
        };
        tab.reload(cx);
        tab
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub fn title(&self) -> SharedString {
        format!("Diagram · {}", self.schema).into()
    }

    /// For the status bar.
    pub fn status(&self) -> Option<String> {
        match &self.state {
            State::Ready { diagram, .. } => Some(format!(
                "{} tables · {} relations",
                diagram.tables.len(),
                diagram.drawn_relations().count()
            )),
            _ => None,
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let load = kuery_pg::catalog::diagram(self.db.session.clone(), self.schema.clone());
        let keys_only = self.keys_only;
        self.state = State::Loading;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                this.task = None;
                this.state = match result {
                    Ok(diagram) => {
                        // Big schemas start further out.
                        if diagram.tables.len() > 12 {
                            this.zoom = 0.7;
                        }
                        let layout = diagram::layout(&diagram, keys_only);
                        State::Ready { diagram, layout }
                    }
                    Err(error) => State::Failed(crate::query_tab::describe_error(error)),
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn set_keys_only(&mut self, keys_only: bool, cx: &mut Context<Self>) {
        self.keys_only = keys_only;
        if let State::Ready { diagram, layout } = &mut self.state {
            *layout = diagram::layout(diagram, keys_only);
        }
        cx.notify();
    }

    fn step_zoom(&mut self, direction: i32, cx: &mut Context<Self>) {
        let current = ZOOMS
            .iter()
            .position(|z| (*z - self.zoom).abs() < 0.01)
            .unwrap_or(4) as i32;
        let next = (current + direction).clamp(0, ZOOMS.len() as i32 - 1) as usize;
        self.zoom = ZOOMS[next];
        cx.notify();
    }

    fn click_table(&mut self, table: usize, event: &ClickEvent, cx: &mut Context<Self>) {
        if event.click_count() == 2 {
            if let State::Ready { diagram, .. } = &self.state {
                let found = &diagram.tables[table];
                cx.emit(DiagramEvent::OpenTable(TableInfo {
                    table: found.table.clone(),
                    kind: TableKind::Table,
                    estimated_rows: found.rows,
                    comment: None,
                }));
            }
            return;
        }
        self.selected = if self.selected == Some(table) {
            None
        } else {
            Some(table)
        };
        cx.notify();
    }

    fn export_svg(&mut self, cx: &mut Context<Self>) {
        let State::Ready { diagram, layout } = &self.state else {
            return;
        };
        let svg = diagram::to_svg(diagram, layout, self.keys_only);
        let suggested = format!("{}-diagram.svg", self.schema);
        let chosen = cx.prompt_for_new_path(&self.project, Some(&suggested));
        cx.spawn(async move |_, _| {
            if let Ok(Ok(Some(mut path))) = chosen.await {
                if path.extension().is_none() {
                    path.set_extension("svg");
                }
                if let Err(error) = std::fs::write(&path, svg) {
                    eprintln!("kuery: could not export the diagram: {error}");
                }
            }
        })
        .detach();
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ready = matches!(self.state, State::Ready { .. });
        h_flex()
            .h(px(44.))
            .flex_shrink_0()
            .px_3()
            .gap_2()
            .bg(color(theme::TOOLBAR))
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                Icon::new(IconName::Network)
                    .size(px(14.))
                    .text_color(color(theme::TEXT_TERTIARY)),
            )
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.schema.clone()),
            )
            .children(self.status().map(|status| {
                div()
                    .text_size(px(12.))
                    .text_color(color(theme::TEXT_TERTIARY))
                    .child(status)
            }))
            .child(div().flex_1())
            .child(
                chrome::segmented().children(
                    [
                        ("diagram-all", AppIcon::Table, "All columns", false),
                        ("diagram-keys", AppIcon::Key, "Keys only", true),
                    ]
                    .map(|(id, icon, label, keys)| {
                        chrome::segment(id, icon, label, self.keys_only == keys).on_click(
                            cx.listener(move |this, _, _, cx| this.set_keys_only(keys, cx)),
                        )
                    }),
                ),
            )
            .child(chrome::toolbar_divider())
            .child(
                chrome::icon_button("diagram-zoom-out", IconName::Minus, "Zoom out", true)
                    .on_click(cx.listener(|this, _, _, cx| this.step_zoom(-1, cx))),
            )
            .child(
                div()
                    .id("diagram-zoom")
                    .w(px(44.))
                    .text_center()
                    .text_size(px(12.))
                    .text_color(color(theme::TEXT_SECONDARY))
                    .child(format!("{:.0}%", self.zoom * 100.))
                    .tooltip(|window, cx| Tooltip::new("Back to 100%").build(window, cx))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.zoom = 1.0;
                        cx.notify();
                    })),
            )
            .child(
                chrome::icon_button("diagram-zoom-in", IconName::Plus, "Zoom in", true)
                    .on_click(cx.listener(|this, _, _, cx| this.step_zoom(1, cx))),
            )
            .child(chrome::toolbar_divider())
            .child(
                chrome::icon_button("diagram-export", AppIcon::Download, "Export as SVG", ready)
                    .when(ready, |b| {
                        b.on_click(cx.listener(|this, _, _, cx| this.export_svg(cx)))
                    }),
            )
            .child(
                chrome::icon_button("diagram-reload", AppIcon::Reload, "Reload", true)
                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            )
    }

    fn render_canvas(
        &self,
        diagram: &Diagram,
        layout: &Layout,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let zoom = self.zoom;
        let keys_only = self.keys_only;
        let selected = self.selected;
        let neighbors = selected.map(|s| diagram.neighbors(s)).unwrap_or_default();
        let selected_name = selected.map(|s| diagram.tables[s].table.clone());
        let wires: Vec<(Wire, bool, Option<bool>)> = wires(diagram, layout, keys_only)
            .into_iter()
            .map(|wire| {
                let relation = &diagram.relations[wire.relation];
                let touches = selected_name
                    .as_ref()
                    .map(|name| relation.from == *name || relation.to == *name);
                (wire, relation.optional, touches)
            })
            .collect();
        let lines = canvas(
            |_, _, _| {},
            move |bounds: Bounds<gpui_kit::Pixels>, _, window, _| {
                let origin = bounds.origin;
                let at = |(x, y): (f32, f32)| origin + point(px(x * zoom), px(y * zoom));
                for (wire, optional, touches) in &wires {
                    let tone = match touches {
                        Some(true) => color(theme::ACCENT),
                        Some(false) => color(0x2e2e33),
                        None => color(0x5a5a61),
                    };
                    let width = if *touches == Some(true) { 2.0 } else { 1.4 };
                    let (c1, c2) = wire.controls();
                    let mut path = PathBuilder::stroke(px(width));
                    if *optional {
                        path = path.dash_array(&[px(5. * zoom), px(4. * zoom)]);
                    }
                    path.move_to(at(wire.from));
                    path.cubic_bezier_to(at(wire.to), at(c1), at(c2));
                    if let Ok(path) = path.build() {
                        window.paint_path(path, tone);
                    }
                    let (marks, ring) = end_marks(wire, *optional);
                    for (start, end) in marks {
                        let mut mark = PathBuilder::stroke(px(width));
                        mark.move_to(at(start));
                        mark.line_to(at(end));
                        if let Ok(mark) = mark.build() {
                            window.paint_path(mark, tone);
                        }
                    }
                    if let Some(center) = ring {
                        let radius = 3.5 * zoom;
                        let c = at(center);
                        window.paint_quad(
                            fill(
                                Bounds::new(
                                    point(c.x - px(radius), c.y - px(radius)),
                                    size(px(radius * 2.), px(radius * 2.)),
                                ),
                                color(theme::GROUND),
                            )
                            .corner_radii(px(radius))
                            .border_widths(px(width))
                            .border_color(tone),
                        );
                    }
                }
            },
        )
        .absolute()
        .size_full();

        let mono = cx.theme().mono_font_family.clone();
        let cards =
            diagram
                .tables
                .iter()
                .zip(&layout.cards)
                .enumerate()
                .map(|(index, (table, card))| {
                    let is_selected = selected == Some(index);
                    let dimmed = selected.is_some() && !is_selected && !neighbors.contains(&index);
                    let name = table.table.name.clone();
                    let rows: Vec<AnyElement> = table
                        .shown_columns(keys_only)
                        .into_iter()
                        .map(|column| {
                            let (icon, tone) = if column.primary_key {
                                (Some(Icon::from(AppIcon::Key)), theme::AMBER)
                            } else if column.foreign_key {
                                (Some(Icon::from(AppIcon::Link)), 0x4da3ff)
                            } else {
                                (None, theme::TEXT_QUIET)
                            };
                            h_flex()
                                .h(px(ROW * zoom))
                                .px(px(10. * zoom))
                                .gap(px(6. * zoom))
                                .child(div().w(px(12. * zoom)).flex_shrink_0().children(
                                    icon.map(|icon| {
                                        icon.size(px(11. * zoom)).text_color(color(tone))
                                    }),
                                ))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(12. * zoom))
                                        .text_color(color(if column.is_key() {
                                            theme::TEXT
                                        } else {
                                            theme::TEXT_SECONDARY
                                        }))
                                        .child(column.name.clone()),
                                )
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .max_w(px(110. * zoom))
                                        .truncate()
                                        .font_family(mono.clone())
                                        .text_size(px(10.5 * zoom))
                                        .text_color(color(theme::TEXT_QUIET))
                                        .child(format!(
                                            "{}{}",
                                            column.short_type(),
                                            if column.nullable { "?" } else { "" }
                                        )),
                                )
                                .into_any_element()
                        })
                        .collect();
                    v_flex()
                        .id(("diagram-table", index))
                        .absolute()
                        .left(px(card.x * zoom))
                        .top(px(card.y * zoom))
                        .w(px(CARD_WIDTH * zoom))
                        .h(px(card.height * zoom))
                        .rounded(px(8. * zoom))
                        .overflow_hidden()
                        .bg(color(theme::SIDEBAR))
                        .border_1()
                        .border_color(color(if is_selected {
                            theme::ACCENT
                        } else {
                            theme::CONTROL_BORDER
                        }))
                        .when(dimmed, |card| card.opacity(0.35))
                        .child(
                            h_flex()
                                .h(px(HEADER * zoom))
                                .flex_shrink_0()
                                .px(px(10. * zoom))
                                .gap(px(7. * zoom))
                                .border_b_1()
                                .border_color(color(theme::HAIRLINE))
                                .child(
                                    Icon::new(AppIcon::Table)
                                        .size(px(13. * zoom))
                                        .text_color(color(0x4da3ff)),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_size(px(13. * zoom))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(name),
                                )
                                .children(table.rows.map(|rows| {
                                    div()
                                        .flex_shrink_0()
                                        .text_size(px(11. * zoom))
                                        .text_color(color(theme::TEXT_QUIET))
                                        .child(compact(rows))
                                })),
                        )
                        .children(rows)
                        .on_click(
                            cx.listener(move |this, event, _, cx| {
                                this.click_table(index, event, cx)
                            }),
                        )
                });

        div()
            .id("diagram-scroll")
            .size_full()
            .overflow_scroll()
            .child(
                div()
                    .relative()
                    .flex_shrink_0()
                    .w(px(layout.width * zoom))
                    .h(px(layout.height * zoom))
                    .child(lines)
                    .children(cards),
            )
            .into_any_element()
    }

    /// What the selected table refers to and what refers to it, or how to
    /// read the diagram.
    fn render_footer(&self, diagram: &Diagram) -> impl IntoElement {
        let text = match self.selected {
            Some(index) => {
                let name = &diagram.tables[index].table;
                let mut parts = Vec::new();
                for (_, relation) in diagram.drawn_relations() {
                    if relation.from == *name && relation.to != *name {
                        parts.push(format!(
                            "→ {} ({}{})",
                            relation.to.name,
                            relation.from_columns.join(", "),
                            match relation.on_delete {
                                diagram::OnDelete::NoAction | diagram::OnDelete::Restrict => {
                                    String::new()
                                }
                                other => format!(", on delete {}", other.label().to_lowercase()),
                            }
                        ));
                    } else if relation.to == *name && relation.from != *name {
                        parts.push(format!("← {}", relation.from.name));
                    } else if relation.to == *name {
                        parts.push(format!("↻ itself ({})", relation.from_columns.join(", ")));
                    }
                }
                let outside = diagram
                    .relations
                    .iter()
                    .filter(|r| r.from == *name && diagram.index_of(&r.to).is_none())
                    .map(|r| format!("→ {} (other schema)", r.to))
                    .collect::<Vec<_>>();
                parts.extend(outside);
                if parts.is_empty() {
                    format!("{}: no relations · double-click to open it", name.name)
                } else {
                    format!(
                        "{}: {} · double-click to open it",
                        name.name,
                        parts.join("  ")
                    )
                }
            }
            None => "Three prongs: many · two bars: one · dashed: optional · click a table to \
                     follow its relations"
                .to_string(),
        };
        div()
            .h(px(30.))
            .flex_shrink_0()
            .px_3()
            .flex()
            .items_center()
            .border_t_1()
            .border_color(color(theme::HAIRLINE))
            .bg(color(theme::TOOLBAR))
            .text_size(px(12.))
            .text_color(color(theme::TEXT_TERTIARY))
            .child(div().truncate().child(text))
    }
}

/// `18204` → `18.2k`.
fn compact(n: u64) -> String {
    match n {
        0..1_000 => thousands(n),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

impl Focusable for DiagramTab {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DiagramTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let message = |text: String, tone: u32| {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px_6()
                .text_color(color(tone))
                .child(text)
                .into_any_element()
        };
        let body: Vec<AnyElement> = match &self.state {
            State::Loading => vec![message("Reading the schema…".into(), theme::TEXT_TERTIARY)],
            State::Failed(error) => vec![message(error.clone(), theme::RED)],
            State::Ready { diagram, .. } if diagram.tables.is_empty() => vec![message(
                format!("{} has no tables", self.schema),
                theme::TEXT_TERTIARY,
            )],
            State::Ready { diagram, layout } => {
                let (diagram, layout) = (diagram.clone(), layout.clone());
                vec![
                    div()
                        .flex_1()
                        .min_h_0()
                        .child(self.render_canvas(&diagram, &layout, cx))
                        .into_any_element(),
                    self.render_footer(&diagram).into_any_element(),
                ]
            }
        };
        v_flex()
            .size_full()
            .bg(color(theme::GROUND))
            .track_focus(&self.focus_handle)
            .child(self.render_toolbar(cx))
            .children(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_compact_counts() {
        assert_eq!(compact(950), "950");
        assert_eq!(compact(18_204), "18.2k");
        assert_eq!(compact(1_000_000), "1.0M");
    }
}
