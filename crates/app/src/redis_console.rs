//! The Redis console: commands typed one per line, run against one database
//! of the connection. The query tab's shortcuts apply: `secondary-enter`
//! runs the line under the cursor (or the selected lines), `secondary-alt-
//! enter` every line, one after another. The last reply fills the results
//! grid; every reply goes to the Output log the way `redis-cli` prints it.
//!
//! For now the console only reads: commands that write, and commands that
//! would change or hold the connection, are refused with the reason (see
//! [`kuery_core::redis::classify`]). `KEYS` runs on development connections
//! only, since it blocks the server while it walks every key.

use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::menu::DropdownMenu as _;
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Task, Window, div, px,
};
use kuery_core::redis::{Class, classify, command_lines, split_command};

use crate::assets::AppIcon;
use crate::chrome;
use crate::completion::RedisCompletions;
use crate::connection::{Environment, RedisDb};
use crate::context_menus::item;
use crate::query_tab::{RunAll, RunQuery};
use crate::results::ResultsDelegate;
use crate::theme::{self, color};
use crate::ui::format_duration;

const EXAMPLE: &str = "# Redis commands, one per line. ⌘↵ runs the line under the cursor, ⌥⌘↵ all of them.\n\
                       INFO keyspace\n\
                       SCAN 0 COUNT 20\n";
const EDITOR_HEIGHT: f32 = 220.;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Panel {
    Results,
    Output,
}

/// One command in the Output log.
struct OutputEntry {
    command: String,
    text: String,
    failed: bool,
    elapsed: Option<Duration>,
}

enum Status {
    Idle,
    Running,
    Done { commands: usize, elapsed: Duration },
    Failed(String),
}

pub struct ConsoleTab {
    db: RedisDb,
    database: i64,
    editor: Entity<EditorState>,
    table: Entity<TableState<ResultsDelegate>>,
    output: Vec<OutputEntry>,
    panel: Panel,
    status: Status,
    task: Option<Task<()>>,
    focus_handle: FocusHandle,
}

impl ConsoleTab {
    pub fn new(db: RedisDb, database: i64, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = EditorState::new(window, cx)
                .line_number(true)
                .default_value(EXAMPLE);
            editor.lsp_mut().completion_provider = Some(Rc::new(RedisCompletions));
            editor
        });
        let table = cx.new(|cx| TableState::new(ResultsDelegate::default(), window, cx));
        Self {
            db,
            database,
            editor,
            table,
            output: Vec::new(),
            panel: Panel::Results,
            status: Status::Idle,
            task: None,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn db(&self) -> &RedisDb {
        &self.db
    }

    pub fn title(&self) -> SharedString {
        format!("Console · db{}", self.database).into()
    }

    pub fn focus_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.editor.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// For the status bar.
    pub fn status(&self) -> String {
        match &self.status {
            Status::Idle => "Ready".into(),
            Status::Running => "Running…".into(),
            Status::Done { commands, elapsed } => format!(
                "{commands} command{} · {}",
                if *commands == 1 { "" } else { "s" },
                format_duration(*elapsed)
            ),
            Status::Failed(error) => error.clone(),
        }
    }

    fn set_database(&mut self, database: i64, cx: &mut Context<Self>) {
        self.database = database;
        cx.notify();
    }

    fn run_query(&mut self, _: &RunQuery, _: &mut Window, cx: &mut Context<Self>) {
        let lines = self.lines_to_run(cx);
        self.run(lines, cx);
    }

    fn run_all(&mut self, _: &RunAll, _: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text().to_string();
        let lines = command_lines(&text)
            .into_iter()
            .map(|(_, line)| line.to_string())
            .collect();
        self.run(lines, cx);
    }

    /// The selected lines, or the line under the cursor.
    fn lines_to_run(&self, cx: &gpui_kit::App) -> Vec<String> {
        let editor = self.editor.read(cx);
        let selected = editor.selected_text().to_string();
        if !selected.trim().is_empty() {
            return command_lines(&selected)
                .into_iter()
                .map(|(_, line)| line.to_string())
                .collect();
        }
        let text = editor.text().to_string();
        let cursor = editor.cursor().min(text.len());
        let start = text[..cursor].rfind('\n').map_or(0, |i| i + 1);
        let end = text[cursor..].find('\n').map_or(text.len(), |i| cursor + i);
        command_lines(&text[start..end])
            .into_iter()
            .map(|(_, line)| line.to_string())
            .collect()
    }

    /// Runs the commands one after another; a refused or failed one is
    /// logged and the rest still run.
    fn run(&mut self, lines: Vec<String>, cx: &mut Context<Self>) {
        if lines.is_empty() || self.task.is_some() {
            return;
        }
        let session = self.db.session.clone();
        let database = self.database;
        let development = self.db.environment == Environment::Development;
        self.status = Status::Running;
        self.task = Some(cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let mut last_reply = None;
            let mut failed = None;
            for line in &lines {
                let started = Instant::now();
                let args = match split_command(line) {
                    Ok(args) if !args.is_empty() => args,
                    Ok(_) => continue,
                    Err(error) => {
                        failed = Some(error.clone());
                        log(&this, cx, line, error, true, None);
                        continue;
                    }
                };
                if let Some(refusal) = refusal(&args, development) {
                    failed = Some(refusal.clone());
                    log(&this, cx, line, refusal, true, None);
                    continue;
                }
                match session.command(database, args).await {
                    Ok(reply) => {
                        let is_error = matches!(reply, kuery_redis::Reply::Error(_));
                        if is_error {
                            failed = Some(reply.inline());
                        }
                        log(
                            &this,
                            cx,
                            line,
                            reply.format_cli(),
                            is_error,
                            Some(started.elapsed()),
                        );
                        if !is_error {
                            last_reply = Some(reply);
                        }
                    }
                    Err(error) => {
                        failed = Some(error.to_string());
                        log(&this, cx, line, error.to_string(), true, None);
                    }
                }
            }
            this.update(cx, |this, cx| {
                this.task = None;
                if let Some(reply) = last_reply {
                    let table = reply.table();
                    this.table.update(cx, |state, cx| {
                        state.delegate_mut().show(table.columns, &table.rows);
                        state.refresh(cx);
                    });
                    this.panel = if failed.is_some() && lines.len() == 1 {
                        Panel::Output
                    } else {
                        Panel::Results
                    };
                } else {
                    this.panel = Panel::Output;
                }
                this.status = match failed {
                    Some(error) if lines.len() == 1 => Status::Failed(error),
                    _ => Status::Done {
                        commands: lines.len(),
                        elapsed: started.elapsed(),
                    },
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.task.is_some();
        let databases = self.db.session.server().databases.max(1);
        let current = self.database;
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
                chrome::primary_button("console-run", IconName::Play, "Run", Some("⌘↵"))
                    .when(!running, |button| {
                        button.on_click(
                            cx.listener(|this, _, window, cx| {
                                this.run_query(&RunQuery, window, cx)
                            }),
                        )
                    })
                    .when(running, |button| button.opacity(0.5)),
            )
            .child(
                chrome::icon_button(
                    "console-run-all",
                    AppIcon::RunAll,
                    "Run all lines  ⌥⌘↵",
                    !running,
                )
                .when(!running, |button| {
                    button.on_click(
                        cx.listener(|this, _, window, cx| this.run_all(&RunAll, window, cx)),
                    )
                }),
            )
            .child(chrome::toolbar_divider())
            .child(
                Button::new("console-database")
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
                            .child(
                                Icon::new(AppIcon::Database)
                                    .size(px(13.))
                                    .text_color(color(theme::TEXT_TERTIARY)),
                            )
                            .child(format!("db{current}"))
                            .child(
                                Icon::new(IconName::ChevronDown)
                                    .size(px(12.))
                                    .text_color(color(theme::TEXT_TERTIARY)),
                            ),
                    )
                    .dropdown_menu(move |mut menu, _, _| {
                        for database in 0..databases {
                            menu = menu.item(
                                item(format!("db{database}"), &weak, move |this, _, cx| {
                                    this.set_database(database, cx)
                                })
                                .checked(database == current),
                            );
                        }
                        menu
                    }),
            )
            .child(div().flex_1())
            .child(
                chrome::pill()
                    .pr(px(10.))
                    .id("console-read-only")
                    .child(Icon::new(AppIcon::Lock).size(px(12.)))
                    .child("Read-only")
                    .tooltip(|window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(
                            "The Redis console only runs commands that read, for now",
                        )
                        .build(window, cx)
                    }),
            )
            .child(
                chrome::pill()
                    .pr(px(10.))
                    .child(chrome::dot(self.db.environment.color()))
                    .child(format!("{} · db{current}", self.db.name)),
            )
    }

    fn render_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let tab = |id: &'static str, label: &'static str, panel: Panel, cx: &mut Context<Self>| {
            let active = self.panel == panel;
            div()
                .id(id)
                .px(px(10.))
                .h(px(24.))
                .flex()
                .items_center()
                .rounded(px(6.))
                .text_size(px(12.))
                .when(active, |t| {
                    t.bg(color(theme::ACTIVE)).text_color(color(theme::TEXT))
                })
                .when(!active, |t| {
                    t.text_color(color(theme::TEXT_TERTIARY))
                        .hover(|s| s.text_color(color(theme::TEXT)))
                })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.panel = panel;
                    cx.notify();
                }))
        };
        let body: AnyElement = match self.panel {
            Panel::Results => DataTable::new(&self.table)
                .stripe(true)
                .bordered(false)
                .into_any_element(),
            Panel::Output if self.output.is_empty() => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(color(theme::TEXT_TERTIARY))
                .child("Run a command to see its reply here")
                .into_any_element(),
            Panel::Output => div()
                .id("console-output")
                .size_full()
                .overflow_y_scroll()
                .p_3()
                .font_family(mono)
                .text_size(px(12.5))
                .children(self.output.iter().rev().map(|entry| {
                    v_flex()
                        .pb_3()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_2()
                                .text_color(color(theme::TEXT_SECONDARY))
                                .child(format!("> {}", entry.command))
                                .children(entry.elapsed.map(|elapsed| {
                                    div()
                                        .text_color(color(theme::TEXT_QUIET))
                                        .child(format_duration(elapsed))
                                })),
                        )
                        .child(
                            div()
                                .text_color(color(if entry.failed {
                                    theme::RED
                                } else {
                                    theme::TEXT
                                }))
                                .child(entry.text.clone()),
                        )
                }))
                .into_any_element(),
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .border_t_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                h_flex()
                    .h(px(34.))
                    .flex_shrink_0()
                    .px_2()
                    .gap_1()
                    .child(tab("console-results", "Results", Panel::Results, cx))
                    .child(tab("console-output-tab", "Output", Panel::Output, cx))
                    .child(div().flex_1())
                    .child(
                        div()
                            .pr_2()
                            .text_size(px(12.))
                            .text_color(color(match self.status {
                                Status::Failed(_) => theme::RED,
                                _ => theme::TEXT_TERTIARY,
                            }))
                            .child(self.status()),
                    ),
            )
            .child(div().flex_1().min_h_0().child(body))
    }
}

/// Why a command may not run from the console, if it may not.
fn refusal(args: &[String], development: bool) -> Option<String> {
    let name = args
        .first()
        .map(|a| a.to_ascii_uppercase())
        .unwrap_or_default();
    match classify(args) {
        Class::Read => None,
        Class::Heavy if development => None,
        Class::Heavy => Some(format!(
            "{name} walks every key and blocks the server meanwhile; it only runs on development connections. Use SCAN, or the sidebar's filter."
        )),
        Class::Write => Some(format!(
            "{name} is not run: the Redis console only reads for now."
        )),
        Class::Unsupported(reason) => Some(format!("{name} is not run: {reason}.")),
    }
}

fn log(
    this: &gpui_kit::WeakEntity<ConsoleTab>,
    cx: &mut gpui_kit::AsyncApp,
    command: &str,
    text: String,
    failed: bool,
    elapsed: Option<Duration>,
) {
    this.update(cx, |this, cx| {
        this.output.push(OutputEntry {
            command: command.to_string(),
            text,
            failed,
            elapsed,
        });
        cx.notify();
    })
    .ok();
}

impl Focusable for ConsoleTab {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ConsoleTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            // Shares the query tab's run shortcuts (and its editor keys).
            .key_context("QueryTab RedisConsole")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::run_query))
            .on_action(cx.listener(Self::run_all))
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .h(px(EDITOR_HEIGHT))
                    .flex_shrink_0()
                    .child(Editor::new(&self.editor).h_full().bordered(false)),
            )
            .child(self.render_panel(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        split_command(line).unwrap()
    }

    #[test]
    fn refuses_what_the_console_does_not_run() {
        assert_eq!(refusal(&args("HGETALL user:1"), false), None);
        assert!(
            refusal(&args("SET a 1"), true)
                .unwrap()
                .contains("only reads")
        );
        assert_eq!(refusal(&args("KEYS *"), true), None);
        assert!(refusal(&args("keys *"), false).unwrap().starts_with("KEYS"));
        assert!(
            refusal(&args("SELECT 1"), true)
                .unwrap()
                .contains("toolbar")
        );
    }
}
