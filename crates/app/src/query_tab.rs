//! Query tab: SQL editor and streaming results, always read-only. A tab can
//! be bound to a saved query file in the project; its first line,
//! `-- connection: <name>`, is kept in the file but not shown in the editor.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dbadmin_pg::{Event, QueryError, RunOptions};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, InputEvent};
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Task, Window, actions, div, px,
};

use crate::assets::AppIcon;
use crate::connection::Db;
use crate::explorer::HistoryEntry;
use crate::project::with_connection_header;
use crate::results::ResultsDelegate;
use crate::theme::{self, color};
use crate::ui::{format_duration, thousands};

actions!(dbadmin, [RunQuery, CancelQuery, SaveQuery, SaveQueryAs]);

pub const EXAMPLE_SQL: &str = "-- Read-only: runs inside BEGIN TRANSACTION READ ONLY.\n\
-- ⌘↵ / Ctrl+↵ to run, ⌘S to save it in the project.\n\
select current_database(), current_user, version();\n";

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

/// Emitted after the query is written to a file.
pub struct QuerySaved(pub PathBuf);

pub struct QueryTab {
    db: Db,
    /// SQL of the run in progress or the last one.
    last_sql: String,
    title: SharedString,
    /// The saved query this tab edits, if any.
    file: Option<PathBuf>,
    /// Where "Save As" proposes to put the file.
    queries_dir: PathBuf,
    /// Editor text as last saved (or as opened), to tell unsaved changes.
    saved_text: String,
    dirty: bool,
    save_error: Option<String>,
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
        file: Option<PathBuf>,
        queries_dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let sql = sql.to_string();
        let saved_text = sql.clone();
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .default_value(sql)
        });
        cx.subscribe(&editor, |this, editor, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let dirty = *editor.read(cx).text() != this.saved_text;
                if dirty != this.dirty {
                    this.dirty = dirty;
                    cx.notify();
                }
            }
        })
        .detach();
        let table = cx.new(|cx| TableState::new(ResultsDelegate::default(), window, cx));
        let title = match &file {
            Some(path) => file_title(path),
            None => title.into(),
        };
        Self {
            db,
            last_sql: String::new(),
            title,
            file,
            queries_dir,
            saved_text,
            dirty: false,
            save_error: None,
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

    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Saves to the tab's file, or asks where if it has none.
    pub fn request_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save(&SaveQuery, window, cx);
    }

    fn save(&mut self, _: &SaveQuery, window: &mut Window, cx: &mut Context<Self>) {
        match self.file.clone() {
            Some(path) => self.write(path, cx),
            None => self.save_as(&SaveQueryAs, window, cx),
        }
    }

    fn save_as(&mut self, _: &SaveQueryAs, _: &mut Window, cx: &mut Context<Self>) {
        let _ = std::fs::create_dir_all(&self.queries_dir);
        let directory = self
            .file
            .as_ref()
            .and_then(|f| f.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| self.queries_dir.clone());
        let suggested = format!("{}.sql", self.title.to_lowercase().replace(' ', "_"));
        let chosen = cx.prompt_for_new_path(&directory, Some(&suggested));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(mut path))) = chosen.await {
                if path.extension().is_none() {
                    path.set_extension("sql");
                }
                this.update(cx, |this, cx| this.write(path, cx)).ok();
            }
        })
        .detach();
    }

    /// Writes the editor text, with the connection line on top, to `path`.
    fn write(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text().to_string();
        let contents = with_connection_header(&text, &self.db.name);
        let written = path
            .parent()
            .map(std::fs::create_dir_all)
            .transpose()
            .and_then(|_| std::fs::write(&path, contents));
        match written {
            Ok(()) => {
                self.title = file_title(&path);
                self.file = Some(path.clone());
                self.saved_text = text;
                self.dirty = false;
                self.save_error = None;
                cx.emit(QuerySaved(path));
            }
            Err(error) => {
                self.save_error = Some(format!("Could not save {}: {error}", path.display()))
            }
        }
        cx.notify();
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

    pub fn focus_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.focus(window, cx));
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

    /// `local-dev · reports/monthly_sales.sql · edited`.
    fn subtitle(&self) -> String {
        let place = match &self.file {
            Some(path) => path
                .strip_prefix(&self.queries_dir)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| path.display().to_string()),
            None => "not saved".into(),
        };
        let edited = if self.dirty && self.file.is_some() {
            " · edited"
        } else {
            ""
        };
        format!("{} · {place}{edited}", self.db.name)
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
                            .child(self.subtitle()),
                    ),
            )
            .children(self.save_error.clone().map(|error| {
                div()
                    .text_size(px(12.))
                    .text_color(color(theme::RED))
                    .truncate()
                    .child(error)
            }))
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
impl EventEmitter<QuerySaved> for QueryTab {}

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
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::save_as))
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

/// `reports/monthly_sales.sql` → `monthly_sales`.
fn file_title(path: &Path) -> SharedString {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Query".into())
        .into()
}
