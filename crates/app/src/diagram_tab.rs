//! A schema's diagram in a tab: every table as a card with its columns (or
//! only its keys), and every foreign key as a line in crow's foot notation,
//! from the referencing column (three prongs: "many") to the referenced key
//! (two bars: "one"); dashed when optional. The layout comes from
//! [`kuery_core::diagram`]: referenced tables left, the tables that depend
//! on them right.
//!
//! Cards can be dragged; where they were left is saved per project and
//! schema in `.kuery/diagrams/` (layout only, fine to commit) and "Reset
//! layout" forgets it. Selecting a table highlights it, its relations and
//! its neighbors, and Focus narrows the diagram to the tables within one or
//! two relations of it; double-clicking opens it. The diagram zooms in steps
//! or to fit the view, and exports what is shown to SVG, Mermaid or DBML.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::DropdownMenu as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Bounds, ClickEvent, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable,
    FontWeight, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    ParentElement as _, PathBuilder, Pixels, Point, Render, SharedString, Size,
    StatefulInteractiveElement as _, Styled as _, Task, Window, canvas, div, fill, point, px, size,
};
use kuery_core::catalog::{TableInfo, TableKind, TableRef};
use kuery_core::diagram::{self, CARD_WIDTH, Diagram, HEADER, Layout, ROW, Wire, end_marks, wires};
use serde::{Deserialize, Serialize};

use crate::assets::AppIcon;
use crate::chrome;
use crate::connection::Db;
use crate::context_menus::item;
use crate::project;
use crate::theme::{self, color};
use crate::ui::thousands;

const ZOOMS: [f32; 8] = [0.25, 0.4, 0.55, 0.7, 0.85, 1.0, 1.25, 1.5];
/// Points the pointer must travel before a press on a card becomes a drag.
const DRAG_START: f32 = 3.;

pub enum DiagramEvent {
    OpenTable(TableInfo),
}

/// What can be exported, and how.
#[derive(Clone, Copy)]
enum Export {
    Svg,
    Mermaid,
    Dbml,
}

impl Export {
    fn extension(self) -> &'static str {
        match self {
            Export::Svg => "svg",
            Export::Mermaid => "mmd",
            Export::Dbml => "dbml",
        }
    }
}

enum State {
    Loading,
    /// The whole schema, its layout with the saved card positions applied.
    Ready {
        diagram: Diagram,
        layout: Layout,
    },
    Failed(String),
}

/// A card being dragged.
struct Drag {
    table: usize,
    start: Point<Pixels>,
    origin: (f32, f32),
    moved: bool,
}

pub struct DiagramTab {
    db: Db,
    schema: String,
    /// The project folder: where layouts are saved and exports start.
    project: PathBuf,
    state: State,
    /// Saved card positions, by table name.
    saved: HashMap<String, (f32, f32)>,
    /// The focused table and how many relations around it are shown, with
    /// that part of the diagram laid out on its own.
    focus: Option<(TableRef, usize)>,
    view: Option<(Diagram, Layout)>,
    keys_only: bool,
    zoom: f32,
    selected: Option<TableRef>,
    drag: Option<Drag>,
    /// The press that just ended was a drag: its click selects nothing.
    dragged: bool,
    /// The visible area, for Fit.
    viewport: Rc<Cell<Option<Size<Pixels>>>>,
    task: Option<Task<()>>,
    focus_handle: FocusHandle,
}

impl EventEmitter<DiagramEvent> for DiagramTab {}

#[derive(Default, Serialize, Deserialize)]
struct SavedLayout {
    #[serde(default)]
    tables: BTreeMap<String, [f32; 2]>,
}

impl DiagramTab {
    pub fn new(db: Db, schema: String, project: PathBuf, cx: &mut Context<Self>) -> Self {
        let saved = load_layout(&layout_path(&project, &db.name, &schema));
        let mut tab = Self {
            db,
            schema,
            project,
            state: State::Loading,
            saved,
            focus: None,
            view: None,
            keys_only: false,
            zoom: 1.0,
            selected: None,
            drag: None,
            dragged: false,
            viewport: Rc::new(Cell::new(None)),
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
        let (diagram, _) = self.shown()?;
        Some(format!(
            "{} tables · {} relations",
            diagram.tables.len(),
            diagram.drawn_relations().count()
        ))
    }

    fn file(&self) -> PathBuf {
        layout_path(&self.project, &self.db.name, &self.schema)
    }

    /// What is on screen: the focused part, or the whole schema.
    fn shown(&self) -> Option<(&Diagram, &Layout)> {
        if let Some((diagram, layout)) = &self.view {
            return Some((diagram, layout));
        }
        match &self.state {
            State::Ready { diagram, layout } => Some((diagram, layout)),
            _ => None,
        }
    }

    fn shown_layout_mut(&mut self) -> Option<&mut Layout> {
        if let Some((_, layout)) = &mut self.view {
            return Some(layout);
        }
        match &mut self.state {
            State::Ready { layout, .. } => Some(layout),
            _ => None,
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let load = kuery_pg::catalog::diagram(self.db.session.clone(), self.schema.clone());
        self.state = State::Loading;
        self.view = None;
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
                        let layout = this.full_layout(&diagram);
                        State::Ready { diagram, layout }
                    }
                    Err(error) => State::Failed(crate::query_tab::describe_error(error)),
                };
                this.refocus();
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The automatic layout, with the saved positions on top.
    fn full_layout(&self, diagram: &Diagram) -> Layout {
        let mut layout = diagram::layout(diagram, self.keys_only);
        for (index, table) in diagram.tables.iter().enumerate() {
            if let Some((x, y)) = self.saved.get(&table.table.name) {
                layout.place(index, *x, *y);
            }
        }
        layout
    }

    /// Lays out the focused part again (after a reload or a change of
    /// columns), or drops the focus if its table is gone.
    fn refocus(&mut self) {
        self.view = match (&self.focus, &self.state) {
            (Some((table, hops)), State::Ready { diagram, .. }) => {
                diagram.index_of(table).map(|index| {
                    let part = diagram.around(index, *hops);
                    let layout = diagram::layout(&part, self.keys_only);
                    (part, layout)
                })
            }
            _ => None,
        };
        if self.view.is_none() {
            self.focus = None;
        }
    }

    fn set_keys_only(&mut self, keys_only: bool, cx: &mut Context<Self>) {
        self.keys_only = keys_only;
        if let State::Ready { diagram, .. } = &self.state {
            let layout = self.full_layout(diagram);
            if let State::Ready {
                layout: current, ..
            } = &mut self.state
            {
                *current = layout;
            }
        }
        self.refocus();
        cx.notify();
    }

    /// Shows only the selected (or focused) table and its neighbors within
    /// `hops`, or the whole schema with `None`.
    fn set_focus(&mut self, hops: Option<usize>, cx: &mut Context<Self>) {
        self.focus = match hops {
            None => None,
            Some(hops) => self
                .selected
                .clone()
                .or_else(|| self.focus.as_ref().map(|(t, _)| t.clone()))
                .map(|table| (table, hops)),
        };
        self.refocus();
        cx.notify();
    }

    fn reset_layout(&mut self, cx: &mut Context<Self>) {
        self.saved.clear();
        let file = self.file();
        if file.exists()
            && let Err(error) = std::fs::remove_file(&file)
        {
            eprintln!("kuery: could not remove {}: {error}", file.display());
        }
        if let State::Ready { diagram, .. } = &self.state {
            let layout = self.full_layout(diagram);
            if let State::Ready {
                layout: current, ..
            } = &mut self.state
            {
                *current = layout;
            }
        }
        self.refocus();
        cx.notify();
    }

    fn step_zoom(&mut self, direction: i32, cx: &mut Context<Self>) {
        let next = if direction > 0 {
            ZOOMS.iter().copied().find(|z| *z > self.zoom + 0.01)
        } else {
            ZOOMS.iter().rev().copied().find(|z| *z < self.zoom - 0.01)
        };
        if let Some(zoom) = next {
            self.zoom = zoom;
            cx.notify();
        }
    }

    /// The zoom at which what is shown fits the visible area.
    fn fit(&mut self, cx: &mut Context<Self>) {
        let (Some(view), Some((_, layout))) = (self.viewport.get(), self.shown()) else {
            return;
        };
        let across = f32::from(view.width) / layout.width.max(1.);
        let down = f32::from(view.height) / layout.height.max(1.);
        self.zoom = across.min(down).clamp(ZOOMS[0], ZOOMS[ZOOMS.len() - 1]);
        cx.notify();
    }

    fn start_drag(&mut self, table: usize, event: &MouseDownEvent) {
        let Some((_, layout)) = self.shown() else {
            return;
        };
        let card = layout.cards[table];
        self.dragged = false;
        self.drag = Some(Drag {
            table,
            start: event.position,
            origin: (card.x, card.y),
            moved: false,
        });
    }

    fn drag_to(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if event.pressed_button != Some(MouseButton::Left) {
            if self.drag.is_some() {
                self.end_drag(cx);
            }
            return;
        }
        let zoom = self.zoom;
        let Some(drag) = &mut self.drag else {
            return;
        };
        let dx = f32::from(event.position.x - drag.start.x) / zoom;
        let dy = f32::from(event.position.y - drag.start.y) / zoom;
        if !drag.moved && dx.hypot(dy) * zoom < DRAG_START {
            return;
        }
        drag.moved = true;
        let (table, (x, y)) = (drag.table, drag.origin);
        if let Some(layout) = self.shown_layout_mut() {
            layout.place(table, x + dx, y + dy);
        }
        cx.notify();
    }

    /// Ends a drag; a card moved in the whole-schema view keeps its place.
    fn end_drag(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        if !drag.moved {
            return;
        }
        self.dragged = true;
        if self.view.is_some() {
            return;
        }
        if let State::Ready { diagram, layout } = &self.state {
            let card = layout.cards[drag.table];
            self.saved.insert(
                diagram.tables[drag.table].table.name.clone(),
                (card.x, card.y),
            );
            if let Err(error) = save_layout(&self.file(), &self.saved) {
                eprintln!("kuery: could not save the diagram layout: {error}");
            }
        }
        cx.notify();
    }

    fn click_table(&mut self, table: usize, event: &ClickEvent, cx: &mut Context<Self>) {
        // The card's click can arrive before or after the drag ends.
        let moved = self.drag.as_ref().is_some_and(|drag| drag.moved);
        if std::mem::take(&mut self.dragged) || moved {
            return;
        }
        let Some((diagram, _)) = self.shown() else {
            return;
        };
        let found = diagram.tables[table].clone();
        if event.click_count() == 2 {
            cx.emit(DiagramEvent::OpenTable(TableInfo {
                table: found.table,
                kind: TableKind::Table,
                estimated_rows: found.rows,
                comment: None,
            }));
            return;
        }
        self.selected = if self.selected.as_ref() == Some(&found.table) {
            None
        } else {
            Some(found.table)
        };
        cx.notify();
    }

    /// The text of an export of what is shown.
    fn export_text(&self, export: Export) -> Option<String> {
        let (diagram, layout) = self.shown()?;
        Some(match export {
            Export::Svg => diagram::to_svg(diagram, layout, self.keys_only),
            Export::Mermaid => diagram.to_mermaid(),
            Export::Dbml => diagram.to_dbml(),
        })
    }

    fn export(&mut self, export: Export, cx: &mut Context<Self>) {
        let Some(text) = self.export_text(export) else {
            return;
        };
        let suggested = format!("{}-diagram.{}", self.schema, export.extension());
        let chosen = cx.prompt_for_new_path(&self.project, Some(&suggested));
        cx.spawn(async move |_, _| {
            if let Ok(Ok(Some(mut path))) = chosen.await {
                if path.extension().is_none() {
                    path.set_extension(export.extension());
                }
                if let Err(error) = std::fs::write(&path, text) {
                    eprintln!("kuery: could not export the diagram: {error}");
                }
            }
        })
        .detach();
    }

    fn copy(&mut self, export: Export, cx: &mut Context<Self>) {
        if let Some(text) = self.export_text(export) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ready = matches!(self.state, State::Ready { .. });
        let can_focus = self.selected.is_some() || self.focus.is_some();
        let hops = self.focus.as_ref().map(|(_, hops)| *hops);
        let weak = cx.entity().downgrade();
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
                        ("diagram-whole", "Whole schema", None),
                        ("diagram-near", "Neighbors", Some(1)),
                        ("diagram-wider", "2 hops", Some(2)),
                    ]
                    .map(|(id, label, choice)| {
                        let active = hops == choice;
                        let enabled = choice.is_none() || can_focus;
                        div()
                            .id(id)
                            .flex()
                            .items_center()
                            .px(px(9.))
                            .rounded(px(5.))
                            .when(active, |s| {
                                s.bg(color(theme::ACTIVE_STRONG))
                                    .text_color(color(theme::TEXT))
                                    .font_weight(FontWeight::MEDIUM)
                            })
                            .when(!active && enabled, |s| {
                                s.text_color(color(theme::TEXT_TERTIARY))
                                    .hover(|s| s.text_color(color(theme::TEXT)))
                            })
                            .when(!enabled, |s| s.text_color(color(0x4a4a50)))
                            .child(label)
                            .when(!enabled, |s| {
                                s.tooltip(|window, cx| {
                                    Tooltip::new("Select a table to focus on it").build(window, cx)
                                })
                            })
                            .when(enabled, |s| {
                                s.on_click(
                                    cx.listener(move |this, _, _, cx| this.set_focus(choice, cx)),
                                )
                            })
                    }),
                ),
            )
            .child(
                chrome::segmented().children(
                    [
                        ("diagram-all", AppIcon::Table, "All columns", false),
                        ("diagram-keys", AppIcon::Key, "Keys", true),
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
            .child(
                chrome::icon_button("diagram-fit", IconName::Maximize, "Fit to view", ready)
                    .when(ready, |b| {
                        b.on_click(cx.listener(|this, _, _, cx| this.fit(cx)))
                    }),
            )
            .child(chrome::toolbar_divider())
            .when(!self.saved.is_empty(), |bar| {
                bar.child(
                    chrome::icon_button(
                        "diagram-reset",
                        IconName::Undo,
                        "Reset layout: forget where cards were moved",
                        true,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.reset_layout(cx))),
                )
            })
            .child(
                Button::new("diagram-export")
                    .ghost()
                    .compact()
                    .h(px(30.))
                    .px(px(8.))
                    .rounded(px(7.))
                    .disabled(!ready)
                    .child(
                        h_flex()
                            .gap(px(5.))
                            .text_color(color(theme::TEXT_SECONDARY))
                            .child(Icon::new(AppIcon::Download).size(px(15.)))
                            .child(Icon::new(IconName::ChevronDown).size(px(11.))),
                    )
                    .dropdown_menu(move |menu, _, _| {
                        menu.label("Export what is shown")
                            .item(item("SVG Image…", &weak, |this, _, cx| {
                                this.export(Export::Svg, cx)
                            }))
                            .item(item("Mermaid…", &weak, |this, _, cx| {
                                this.export(Export::Mermaid, cx)
                            }))
                            .item(item("DBML…", &weak, |this, _, cx| {
                                this.export(Export::Dbml, cx)
                            }))
                            .separator()
                            .item(item("Copy as Mermaid", &weak, |this, _, cx| {
                                this.copy(Export::Mermaid, cx)
                            }))
                            .item(item("Copy as DBML", &weak, |this, _, cx| {
                                this.copy(Export::Dbml, cx)
                            }))
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
        let selected = self
            .selected
            .as_ref()
            .and_then(|table| diagram.index_of(table));
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
            move |bounds: Bounds<Pixels>, _, window, _| {
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
        let dragging = self.drag.as_ref().filter(|d| d.moved).map(|d| d.table);
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
                        .border_color(color(if is_selected || dragging == Some(index) {
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
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &MouseDownEvent, _, _| {
                                this.start_drag(index, event)
                            }),
                        )
                        .on_click(
                            cx.listener(move |this, event, _, cx| {
                                this.click_table(index, event, cx)
                            }),
                        )
                });

        let viewport = self.viewport.clone();
        div()
            .relative()
            .size_full()
            .child(
                // Measures the visible area, for Fit.
                canvas(
                    move |bounds: Bounds<Pixels>, _, _| viewport.set(Some(bounds.size)),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(
                div()
                    .id("diagram-scroll")
                    .size_full()
                    .overflow_scroll()
                    .on_mouse_move(
                        cx.listener(|this, event: &MouseMoveEvent, _, cx| this.drag_to(event, cx)),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.end_drag(cx)),
                    )
                    .child(
                        div()
                            .relative()
                            .flex_shrink_0()
                            .w(px(layout.width * zoom))
                            .h(px(layout.height * zoom))
                            .child(lines)
                            .children(cards),
                    ),
            )
            .into_any_element()
    }

    /// What the selected table refers to and what refers to it, or how to
    /// read the diagram.
    fn render_footer(&self, diagram: &Diagram) -> impl IntoElement {
        let focus = self.focus.as_ref().map(|(table, hops)| {
            format!(
                "Focused on {} · {} · ",
                table.name,
                if *hops == 1 { "neighbors" } else { "2 hops" }
            )
        });
        let selected = self
            .selected
            .as_ref()
            .and_then(|table| diagram.index_of(table));
        let text = match selected {
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
                parts.extend(
                    diagram
                        .relations
                        .iter()
                        .filter(|r| r.from == *name && diagram.index_of(&r.to).is_none())
                        .map(|r| format!("→ {} (not shown)", r.to)),
                );
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
            None => "Three prongs: many · two bars: one · dashed: optional · drag cards to \
                     arrange them · click a table to follow its relations"
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
            .child(
                div()
                    .truncate()
                    .child(format!("{}{text}", focus.unwrap_or_default())),
            )
    }
}

/// `<project>/.kuery/diagrams/<connection>-<schema>.toml`.
fn layout_path(project: &Path, connection: &str, schema: &str) -> PathBuf {
    let safe = |text: &str| -> String {
        text.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    project.join(project::DIR).join("diagrams").join(format!(
        "{}-{}.toml",
        safe(connection),
        safe(schema)
    ))
}

fn load_layout(path: &Path) -> HashMap<String, (f32, f32)> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<SavedLayout>(&text).ok())
        .map(|saved| {
            saved
                .tables
                .into_iter()
                .map(|(name, [x, y])| (name, (x, y)))
                .collect()
        })
        .unwrap_or_default()
}

fn save_layout(path: &Path, positions: &HashMap<String, (f32, f32)>) -> std::io::Result<()> {
    let saved = SavedLayout {
        tables: positions
            .iter()
            .map(|(name, (x, y))| (name.clone(), [x.round(), y.round()]))
            .collect(),
    };
    let text = toml::to_string(&saved).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(
        path,
        format!("# Kuery diagram layout: where each table's card was moved.\n\n{text}"),
    )
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
        let body: Vec<AnyElement> = match (&self.state, self.shown()) {
            (State::Loading, _) => {
                vec![message("Reading the schema…".into(), theme::TEXT_TERTIARY)]
            }
            (State::Failed(error), _) => vec![message(error.clone(), theme::RED)],
            (_, Some((diagram, _))) if diagram.tables.is_empty() => vec![message(
                format!("{} has no tables", self.schema),
                theme::TEXT_TERTIARY,
            )],
            (_, Some((diagram, layout))) => {
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
            (_, None) => Vec::new(),
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

    #[test]
    fn saves_and_loads_card_positions() {
        let dir = std::env::temp_dir().join(format!("kuery-diagram-{}", std::process::id()));
        let path = layout_path(&dir, "local dev", "public");
        assert!(path.ends_with(".kuery/diagrams/local_dev-public.toml"));
        let positions = HashMap::from([
            ("orders".to_string(), (410.4, 96.0)),
            ("my table".to_string(), (40.0, 300.0)),
        ]);
        save_layout(&path, &positions).unwrap();
        let loaded = load_layout(&path);
        assert_eq!(loaded.get("orders"), Some(&(410.0, 96.0)));
        assert_eq!(loaded.get("my table"), Some(&(40.0, 300.0)));
        assert!(load_layout(&dir.join("missing.toml")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
