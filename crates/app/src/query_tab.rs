//! Query tab: SQL editor and streaming results, always read-only.

use std::time::{Duration, Instant};

use dbadmin_pg::{Event, QueryError, RunOptions};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Render, SharedString,
    Styled as _, Task, Window, actions, div, px,
};

use crate::assets::AppIcon;
use crate::connection::Db;
use crate::explorer::HistoryEntry;
use crate::results::ResultsDelegate;
use crate::theme::{self, color};
use crate::ui::{format_duration, thousands};

actions!(dbadmin, [RunQuery, CancelQuery]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-enter", RunQuery, None),
        KeyBinding::new("secondary-.", CancelQuery, None),
    ]);
}

pub const EXAMPLE_SQL: &str = "-- Read-only: runs inside BEGIN TRANSACTION READ ONLY.\n\
-- ⌘↵ / Ctrl+↵ to run.\n\
select o.id, o.status, o.total, o.currency, o.created_at, c.email, o.shipping\n\
from orders o\n\
join customers c on c.id = o.customer_id\n\
order by o.created_at desc;\n";

enum RunState {
    Idle,
    Running {
        started: Instant,
    },
    Done {
        rows: usize,
        elapsed: Duration,
        first_batch_after: Option<Duration>,
    },
    Failed(String),
}

/// Emitted after every run, for the sidebar's History.
pub struct QueryRan(pub HistoryEntry);

pub struct QueryTab {
    db: Db,
    /// SQL of the run in progress or the last one.
    last_sql: String,
    title: SharedString,
    editor: Entity<EditorState>,
    table: Entity<TableState<ResultsDelegate>>,
    run: RunState,
    run_task: Option<Task<()>>,
    focus_handle: FocusHandle,
}

impl QueryTab {
    pub fn new(
        db: Db,
        title: impl Into<SharedString>,
        sql: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let sql = sql.to_string();
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .default_value(sql)
        });
        let table = cx.new(|cx| TableState::new(ResultsDelegate::default(), window, cx));
        Self {
            db,
            last_sql: String::new(),
            title: title.into(),
            editor,
            table,
            run: RunState::Idle,
            run_task: None,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn run_query(&mut self, _: &RunQuery, _: &mut Window, cx: &mut Context<Self>) {
        let sql = {
            let editor = self.editor.read(cx);
            let selected = editor.selected_text().to_string();
            if selected.trim().is_empty() {
                editor.text().to_string()
            } else {
                selected
            }
        };

        // Dropping the previous task drops its receiver and cancels that query.
        self.run_task = None;
        self.table.update(cx, |table, cx| {
            table.delegate_mut().start();
            table.refresh(cx);
        });
        self.run = RunState::Running {
            started: Instant::now(),
        };
        cx.notify();

        self.last_sql = sql.clone();
        let rx = self.db.session.run_read_only(sql, RunOptions::default());
        self.run_task = Some(cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                if this
                    .update(cx, |this, cx| this.on_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    fn on_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Columns(names) => self.table.update(cx, |table, cx| {
                table.delegate_mut().set_columns(names);
                table.refresh(cx);
            }),
            Event::Batch(batch) => self.table.update(cx, |table, cx| {
                table.delegate_mut().push(batch);
                cx.notify();
            }),
            Event::Finished(summary) => {
                self.finish_table(cx);
                self.record(
                    format!(
                        "{} rows · {}",
                        thousands(summary.rows as u64),
                        format_duration(summary.elapsed)
                    ),
                    false,
                    cx,
                );
                self.run = RunState::Done {
                    rows: summary.rows,
                    elapsed: summary.elapsed,
                    first_batch_after: summary.first_batch_after,
                };
            }
            Event::Failed(error) => {
                self.finish_table(cx);
                let message = describe_error(error);
                self.record(message.clone(), true, cx);
                self.run = RunState::Failed(message);
            }
        }
        cx.notify();
    }

    fn record(&mut self, outcome: String, failed: bool, cx: &mut Context<Self>) {
        cx.emit(QueryRan(HistoryEntry {
            connection: self.db.name.clone(),
            sql: self.last_sql.clone(),
            outcome,
            failed,
        }));
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    fn finish_table(&mut self, cx: &mut Context<Self>) {
        self.table.update(cx, |table, cx| {
            table.delegate_mut().finish();
            cx.notify();
        });
    }

    fn cancel_query(&mut self, _: &CancelQuery, _: &mut Window, cx: &mut Context<Self>) {
        if let RunState::Running { .. } = self.run {
            self.db.session.cancel();
            cx.notify();
        }
    }

    fn is_running(&self) -> bool {
        matches!(self.run, RunState::Running { .. })
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.is_running();
        let run_shortcut = if cfg!(target_os = "macos") {
            "⌘↵"
        } else {
            "Ctrl+↵"
        };
        h_flex()
            .h(px(44.))
            .px_4()
            .gap_2()
            .flex_shrink_0()
            .bg(color(theme::TOOLBAR))
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                Icon::new(AppIcon::Code)
                    .size(px(16.))
                    .text_color(color(theme::TEXT_SECONDARY)),
            )
            .child(
                v_flex()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.title.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(color(theme::TEXT_TERTIARY))
                            .child(format!("{} — Read-only", self.db.target)),
                    ),
            )
            .child(div().flex_1())
            .child(
                h_flex()
                    .gap_1()
                    .px_2()
                    .py(px(3.))
                    .rounded(px(6.))
                    .bg(color(theme::ACCENT).opacity(0.16))
                    .text_color(color(0x4da3ff))
                    .text_size(px(12.))
                    .child(Icon::new(AppIcon::Lock).size(px(12.)))
                    .child("Read-only"),
            )
            .when(running, |row| {
                row.child(Button::new("cancel").small().label("Cancel").on_click(
                    cx.listener(|this, _, window, cx| this.cancel_query(&CancelQuery, window, cx)),
                ))
            })
            .child(
                Button::new("run")
                    .small()
                    .primary()
                    .label(format!("Run  {run_shortcut}"))
                    .loading(running)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.run_query(&RunQuery, window, cx)),
                    ),
            )
    }

    fn render_read_only_banner(&self) -> impl IntoElement {
        h_flex()
            .h(px(28.))
            .px_4()
            .gap_2()
            .flex_shrink_0()
            .bg(color(theme::READ_ONLY_BG))
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .text_size(px(12.))
            .text_color(color(theme::READ_ONLY_FG))
            .child(Icon::new(AppIcon::Lock).size(px(13.)))
            .child(
                "Read-only query: runs in a READ ONLY transaction. INSERT, UPDATE, DELETE and DDL are blocked.",
            )
    }

    fn render_results_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let loaded = self.table.read(cx).delegate().rows();
        let memory = self.table.read(cx).delegate().memory_size() as f64 / 1_048_576.0;
        let (text, tone) = match &self.run {
            RunState::Idle => ("Ready".to_string(), theme::TEXT_SECONDARY),
            RunState::Running { started } => (
                format!(
                    "{} rows · {:.1} s · streaming…",
                    thousands(loaded as u64),
                    started.elapsed().as_secs_f32()
                ),
                theme::TEXT_SECONDARY,
            ),
            RunState::Done {
                rows,
                elapsed,
                first_batch_after,
            } => (
                format!(
                    "{} rows · {} · first rows in {} · {:.0} MB",
                    thousands(*rows as u64),
                    format_duration(*elapsed),
                    first_batch_after
                        .map(format_duration)
                        .unwrap_or_else(|| "—".into()),
                    memory
                ),
                theme::TEXT_SECONDARY,
            ),
            RunState::Failed(message) => (message.clone(), theme::RED),
        };

        h_flex()
            .h(px(36.))
            .px_4()
            .gap_3()
            .flex_shrink_0()
            .bg(color(0x1c1c1e))
            .border_t_1()
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Results"))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(color(tone))
                    .truncate()
                    .child(text),
            )
    }
}

pub fn describe_error(error: QueryError) -> String {
    match error {
        QueryError::Database { code, message } => format!("{message} [{code}]"),
        other => other.to_string(),
    }
}

impl EventEmitter<QueryRan> for QueryTab {}

impl Focusable for QueryTab {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for QueryTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.is_running() {
            // Refreshes the elapsed-time counter while results stream in.
            window.request_animation_frame();
        }

        v_flex()
            .size_full()
            .key_context("QueryTab")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::run_query))
            .on_action(cx.listener(Self::cancel_query))
            .child(self.render_toolbar(cx))
            .child(self.render_read_only_banner())
            .child(
                div()
                    .h(px(220.))
                    .flex_shrink_0()
                    .child(Editor::new(&self.editor).h_full().bordered(false)),
            )
            .child(self.render_results_header(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(DataTable::new(&self.table).stripe(true).bordered(false)),
            )
    }
}
