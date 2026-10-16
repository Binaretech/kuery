//! Command palette: fuzzy search over every command (with its shortcut) and,
//! once you type, over the tables of the open connections and the recent
//! projects.
//!
//! It does not run anything itself: it tells the workspace which command or
//! table was picked, and the workspace dispatches the same action a shortcut
//! would.

use std::path::PathBuf;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, FontWeight, InteractiveElement as _,
    IntoElement, KeyBinding, MouseButton, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, actions, div, px,
};
use kuery_core::catalog::TableInfo;
use kuery_core::fuzzy;

use crate::assets::AppIcon;
use crate::commands::{COMMANDS, Command};
use crate::connection::Db;
use crate::keymap;
use crate::recent::RecentProject;
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
    OpenProject(PathBuf),
    Dismissed,
}

enum Item {
    Command(&'static Command),
    Table { db: Db, info: TableInfo },
    Project(PathBuf),
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
    /// Nothing typed: commands are grouped under their category.
    browsing: bool,
}

impl EventEmitter<PaletteEvent> for CommandPalette {}

impl CommandPalette {
    pub fn new(
        tables: Vec<(Db, TableInfo)>,
        projects: Vec<RecentProject>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search commands, tables and projects…")
        });
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
        entries.extend(projects.into_iter().map(|project| Entry {
            title: project.name.clone().into(),
            detail: format!("Recent project · {}", project.display_path()).into(),
            shortcut: None,
            item: Item::Project(project.path),
        }));

        let mut palette = Self {
            input,
            entries,
            matches: Vec::new(),
            selected: 0,
            problems: keymap::problems(cx),
            browsing: true,
        };
        palette.refilter(cx);
        palette
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let query = self.input.read(cx).value().to_string();
        let query = query.trim();
        self.browsing = query.is_empty();
        let mut scored: Vec<(i64, usize)> = self
            .entries
            .iter()
            .enumerate()
            // Tables and projects only show up once you type: they would bury
            // the commands.
            .filter(|(_, entry)| !query.is_empty() || matches!(entry.item, Item::Command(_)))
            .filter_map(|(index, entry)| {
                let title = fuzzy::score(query, &entry.title);
                let full = fuzzy::score(query, &format!("{} {}", entry.detail, entry.title));
                title.max(full.map(|s| s - 50)).map(|score| (score, index))
            })
            .collect();
        if !query.is_empty() {
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        } else {
            // Browsing: each category once, in the order it first appears.
            let rank = |index: usize| match &self.entries[index].item {
                Item::Command(command) => COMMANDS
                    .iter()
                    .position(|c| c.category == command.category)
                    .unwrap_or(usize::MAX),
                _ => usize::MAX,
            };
            scored.sort_by_key(|&(_, index)| (rank(index), index));
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
            Item::Project(path) => PaletteEvent::OpenProject(path.clone()),
        });
    }

    fn pick(&mut self, position: usize, cx: &mut Context<Self>) {
        self.selected = position;
        self.confirm(cx);
    }
}

/// The icon a command shows in the palette.
fn command_icon(id: &str) -> Icon {
    match id {
        "palette.open" => Icon::new(IconName::Search),
        "project.open" => Icon::new(IconName::FolderOpen),
        "project.clear_recent" => Icon::new(AppIcon::Trash),
        "connection.new" => Icon::new(AppIcon::Database),
        "tab.new_query" => Icon::new(IconName::Plus),
        "tab.close" | "window.close" | "app.quit" => Icon::new(IconName::Close),
        "tab.next" => Icon::new(IconName::ChevronRight),
        "tab.previous" => Icon::new(IconName::ChevronLeft),
        "sidebar.toggle" => Icon::new(IconName::PanelLeft),
        "query.run" => Icon::new(IconName::Play),
        "query.run_all" => Icon::new(AppIcon::RunAll),
        "query.format" => Icon::new(AppIcon::Format),
        "query.save" | "query.save_as" | "table.save" => Icon::new(AppIcon::Save),
        "query.cancel" => Icon::new(IconName::Square),
        "query.toggle_results" => Icon::new(IconName::PanelBottom),
        "query.export" => Icon::new(AppIcon::Download),
        "query.preview" => Icon::new(IconName::Eye),
        "audit.open" => Icon::new(AppIcon::Audit),
        "query.read_only" => Icon::new(AppIcon::Lock),
        "query.write" => Icon::new(AppIcon::Pencil),
        "query.full_write" => Icon::new(AppIcon::FullWrite),
        "query.commit" => Icon::new(IconName::Check),
        "query.rollback" => Icon::new(IconName::Undo),
        "table.reload" => Icon::new(AppIcon::Reload),
        "keymap.open" | "keymap.reload" => Icon::new(AppIcon::Keyboard),
        "window.minimize" => Icon::new(IconName::Minimize),
        "window.zoom" | "window.fullscreen" => Icon::new(IconName::Maximize),
        _ => Icon::new(AppIcon::Code),
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let mut list = v_flex().gap(px(1.));
        let mut last_category: Option<&str> = None;
        for (position, &index) in self.matches.iter().enumerate() {
            let entry = &self.entries[index];
            let selected = position == self.selected;
            if self.browsing
                && let Item::Command(command) = &entry.item
                && last_category != Some(command.category)
            {
                last_category = Some(command.category);
                list = list.child(
                    div()
                        .px(px(10.))
                        .pt(px(if position == 0 { 6. } else { 10. }))
                        .pb(px(4.))
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color(theme::TEXT_QUIET))
                        .child(command.category.to_uppercase()),
                );
            }
            let (icon, tone) = match &entry.item {
                Item::Command(command) => (command_icon(command.id), theme::TEXT_SECONDARY),
                Item::Table { info, .. } if info.kind.is_editable() => {
                    (Icon::new(AppIcon::Table), 0x4da3ff)
                }
                Item::Table { .. } => (Icon::new(AppIcon::Code), 0xb9a3e6),
                Item::Project(_) => (Icon::new(IconName::FolderClosed), theme::TEXT_SECONDARY),
            };
            list = list.child(
                h_flex()
                    .id(("palette-entry", position))
                    .h(px(34.))
                    .px(px(10.))
                    .gap(px(10.))
                    .rounded(px(7.))
                    .when(selected, |row| row.bg(color(theme::ACCENT)))
                    .when(!selected, |row| row.hover(|s| s.bg(color(theme::ACTIVE))))
                    .child(icon.size(px(14.)).text_color(color(if selected {
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
                            .when(
                                !self.browsing || !matches!(entry.item, Item::Command(_)),
                                |d| d.child(entry.detail.clone()),
                            ),
                    )
                    .children(entry.shortcut.clone().map(|shortcut| {
                        div()
                            .px(px(6.))
                            .py(px(1.))
                            .rounded(px(4.))
                            .bg(if selected {
                                gpui_kit::white().opacity(0.18)
                            } else {
                                color(theme::ACTIVE)
                            })
                            .font_family(mono.clone())
                            .text_size(px(11.))
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
            None => div()
                .text_color(color(theme::TEXT_TERTIARY))
                .child("↑↓ move · ↵ run · esc close · type to search tables and recent projects"),
        };

        v_flex()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::on_confirm))
            .on_action(cx.listener(Self::on_dismiss))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .w(px(620.))
            .max_h(px(480.))
            .rounded(px(14.))
            .bg(color(0x1f1f22))
            .border_1()
            .border_color(color(0x36363b))
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
