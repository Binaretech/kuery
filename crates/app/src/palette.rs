//! Command palette: fuzzy search over every command (with its shortcut) and,
//! once you type, over the tables of the open connections.
//!
//! It does not run anything itself: it tells the workspace which command or
//! table was picked, and the workspace dispatches the same action a shortcut
//! would.

use dbadmin_core::catalog::TableInfo;
use dbadmin_core::fuzzy;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    KeyBinding, MouseButton, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, actions, div, px,
};

use crate::assets::AppIcon;
use crate::commands::{COMMANDS, Command};
use crate::connection::Db;
use crate::keymap;
use crate::theme::{self, color};

actions!(palette, [SelectNext, SelectPrevious, Confirm, Dismiss]);

const CONTEXT: &str = "CommandPalette";
const MAX_RESULTS: usize = 50;

/// Navigation inside the palette. These are not user-configurable: they only
/// apply while the palette is open, including inside its search field.
pub fn bindings() -> Vec<KeyBinding> {
    let mut bindings = Vec::new();
    for context in [CONTEXT, "CommandPalette > Input"] {
        bindings.push(KeyBinding::new("up", SelectPrevious, Some(context)));
        bindings.push(KeyBinding::new("down", SelectNext, Some(context)));
        bindings.push(KeyBinding::new("ctrl-p", SelectPrevious, Some(context)));
        bindings.push(KeyBinding::new("ctrl-n", SelectNext, Some(context)));
        bindings.push(KeyBinding::new("enter", Confirm, Some(context)));
        bindings.push(KeyBinding::new("escape", Dismiss, Some(context)));
    }
    bindings
}

pub enum PaletteEvent {
    Run(&'static Command),
    OpenTable { db: Db, info: TableInfo },
    Dismissed,
}

enum Item {
    Command(&'static Command),
    Table { db: Db, info: TableInfo },
}

struct Entry {
    item: Item,
    title: SharedString,
    detail: SharedString,
    shortcut: Option<String>,
}

pub struct CommandPalette {
    input: Entity<InputState>,
    entries: Vec<Entry>,
    /// Indices into `entries`, best match first.
    matches: Vec<usize>,
    selected: usize,
    problems: Vec<String>,
}

impl EventEmitter<PaletteEvent> for CommandPalette {}

impl CommandPalette {
    pub fn new(tables: Vec<(Db, TableInfo)>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search commands and tables…"));
        cx.subscribe(&input, |this, _, event: &InputEvent, cx| match event {
            InputEvent::Change => this.refilter(cx),
            InputEvent::PressEnter { .. } => this.confirm(cx),
            _ => {}
        })
        .detach();
        input.update(cx, |input, cx| input.focus(window, cx));

        let mut entries: Vec<Entry> = COMMANDS
            .iter()
            .map(|command| Entry {
                item: Item::Command(command),
                title: command.title.into(),
                detail: command.category.into(),
                shortcut: keymap::shortcut(cx, command.id),
            })
            .collect();
        entries.extend(tables.into_iter().map(|(db, info)| Entry {
            title: format!("{}.{}", info.table.schema, info.table.name).into(),
            detail: format!("{} · {}", info.kind.label(), db.name).into(),
            shortcut: None,
            item: Item::Table { db, info },
        }));

        let mut palette = Self {
            input,
            entries,
            matches: Vec::new(),
            selected: 0,
            problems: keymap::problems(cx),
        };
        palette.refilter(cx);
        palette
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).value().to_string();
        let query = query.trim();
        let mut scored: Vec<(i64, usize)> = self
            .entries
            .iter()
            .enumerate()
            // Tables only show up once you type: they would bury the commands.
            .filter(|(_, entry)| !query.is_empty() || matches!(entry.item, Item::Command(_)))
            .filter_map(|(index, entry)| {
                let title = fuzzy::score(query, &entry.title);
                let full = fuzzy::score(query, &format!("{} {}", entry.detail, entry.title));
                title.max(full.map(|s| s - 50)).map(|score| (score, index))
            })
            .collect();
        if !query.is_empty() {
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        self.matches = scored
            .into_iter()
            .take(MAX_RESULTS)
            .map(|(_, index)| index)
            .collect();
        self.selected = 0;
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
            cx.notify();
        }
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + self.matches.len() - 1) % self.matches.len();
            cx.notify();
        }
    }

    fn on_confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        self.confirm(cx);
    }

    fn on_dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PaletteEvent::Dismissed);
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self
            .matches
            .get(self.selected)
            .and_then(|&index| self.entries.get(index))
        else {
            return;
        };
        cx.emit(match &entry.item {
            Item::Command(command) => PaletteEvent::Run(command),
            Item::Table { db, info } => PaletteEvent::OpenTable {
                db: db.clone(),
                info: info.clone(),
            },
        });
    }

    fn pick(&mut self, position: usize, cx: &mut Context<Self>) {
        self.selected = position;
        self.confirm(cx);
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = v_flex().gap(px(1.));
        for (position, &index) in self.matches.iter().enumerate() {
            let entry = &self.entries[index];
            let selected = position == self.selected;
            let (icon, tone) = match &entry.item {
                Item::Command(_) => (AppIcon::Code, theme::TEXT_SECONDARY),
                Item::Table { info, .. } if info.kind.is_editable() => (AppIcon::Table, 0x4da3ff),
                Item::Table { .. } => (AppIcon::Code, 0xc49bff),
            };
            list = list.child(
                h_flex()
                    .id(("palette-entry", position))
                    .h(px(32.))
                    .px_3()
                    .gap_3()
                    .rounded(px(6.))
                    .when(selected, |row| row.bg(color(theme::SELECTION)))
                    .when(!selected, |row| row.hover(|s| s.bg(color(0x2e2e32))))
                    .child(Icon::new(icon).size(px(14.)).text_color(color(if selected {
                        0xffffff
                    } else {
                        tone
                    })))
                    .child(
                        div()
                            .truncate()
                            .text_color(color(if selected { 0xffffff } else { theme::TEXT }))
                            .child(entry.title.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(color(if selected {
                                0xdde9ff
                            } else {
                                theme::TEXT_TERTIARY
                            }))
                            .child(entry.detail.clone()),
                    )
                    .children(entry.shortcut.clone().map(|shortcut| {
                        div()
                            .px_2()
                            .py(px(1.))
                            .rounded(px(4.))
                            .bg(color(if selected { 0x2b73e0 } else { 0x2e2e32 }))
                            .text_size(px(11.5))
                            .text_color(color(if selected {
                                0xffffff
                            } else {
                                theme::TEXT_SECONDARY
                            }))
                            .child(shortcut)
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| this.pick(position, cx))),
            );
        }
        if self.matches.is_empty() {
            list = list.child(
                div()
                    .px_3()
                    .py_3()
                    .text_color(color(theme::TEXT_TERTIARY))
                    .child("No matching commands or tables"),
            );
        }

        let footer = match self.problems.first() {
            Some(problem) => div()
                .text_color(color(theme::AMBER))
                .truncate()
                .child(format!("Keymap: {problem}")),
            None => div().text_color(color(theme::TEXT_TERTIARY)).child(
                "↑↓ to move · ↵ to run · esc to close · \"Open Keymap File\" to change shortcuts",
            ),
        };

        v_flex()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::on_confirm))
            .on_action(cx.listener(Self::on_dismiss))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .w(px(620.))
            .max_h(px(460.))
            .rounded(px(12.))
            .bg(color(0x2a2a2d))
            .border_1()
            .border_color(color(theme::HAIRLINE))
            .shadow_lg()
            .overflow_hidden()
            .child(
                div()
                    .p_2()
                    .border_b_1()
                    .border_color(color(theme::HAIRLINE))
                    .child(Input::new(&self.input).cleanable(false)),
            )
            .child(
                div()
                    .id("palette-results")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_1()
                    .child(list),
            )
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(color(theme::HAIRLINE))
                    .text_size(px(11.5))
                    .child(footer),
            )
    }
}
