//! Query tab: SQL editor and streaming results. Read-only unless write mode
//! is on (see [`crate::write_mode`]): then the tab has a connection of its
//! own, and changes stay in a transaction until Commit or Roll Back. A tab can
//! be bound to a saved query file in the project; its first line,
//! `-- connection: <name>`, is kept in the file but not shown in the editor.
//!
//! The editor and the bottom panel are split by a draggable divider. The
//! panel has a Results tab (the grid) and an Output tab (a log of runs), and
//! can be collapsed down to its tab strip.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Enter, InputEvent};
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, CursorStyle, DragMoveEvent, Empty, Entity, EventEmitter,
    FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement, KeyBinding,
    ParentElement as _, Pixels, PromptLevel, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Task, Window, actions, div, px,
};
use kuery_pg::{Access, Event, QueryError, RunOptions, Session};

use crate::assets::AppIcon;
use crate::connection::{Db, WritePolicy};
use crate::explorer::HistoryEntry;
use crate::project::with_connection_header;
use crate::results::ResultsDelegate;
use crate::theme::{self, color};
use crate::ui::{format_duration, thousands};
use crate::write_mode::{self, Review};

actions!(
    kuery,
    [
        RunQuery,
        CancelQuery,
        SaveQuery,
        SaveQueryAs,
        ToggleResults,
        ToggleWriteMode,
        CommitTransaction,
        RollbackTransaction,
    ]
);

/// An open transaction with no runs for this long is rolled back.
const IDLE_ROLLBACK: Duration = Duration::from_secs(10 * 60);
/// The transaction bar counts down over the last stretch.
const IDLE_WARNING: Duration = Duration::from_secs(2 * 60);

/// Write mode of a tab.
enum WriteState {
    Off,
    Connecting,
    On(WriteSession),
    Failed(String),
}

/// The tab's own read-write connection and its open transaction, if any.
struct WriteSession {
    session: Session,
    /// Statements and rows changed since the transaction opened.
    statements: usize,
    affected: u64,
    opened: Option<Instant>,
    last_run: Instant,
}

impl WriteSession {
    fn new(session: Session) -> Self {
        Self {
            session,
            statements: 0,
            affected: 0,
            opened: None,
            last_run: Instant::now(),
        }
    }

    fn reset(&mut self) {
        self.statements = 0;
        self.affected = 0;
        self.opened = None;
    }
}

/// Editor keys that are not commands: `secondary-enter` runs the query (it is
/// the `query.run` shortcut), so a new line moves to `secondary-shift-enter`.
pub fn bindings() -> Vec<KeyBinding> {
    vec![KeyBinding::new(
        "secondary-shift-enter",
        Enter {
            secondary: false,
            shift: true,
        },
        Some("QueryTab > Input"),
    )]
}

const DEFAULT_EDITOR_HEIGHT: Pixels = px(240.);
const MIN_EDITOR_HEIGHT: Pixels = px(60.);
/// Room kept for the bottom panel when the divider is dragged down.
const MIN_PANEL_HEIGHT: Pixels = px(110.);

#[derive(Clone, Copy, PartialEq, Eq)]
enum PanelTab {
    Results,
    Output,
}

/// One run, as the Output tab lists it.
struct OutputEntry {
    run: usize,
    connection: SharedString,
    sql: String,
    outcome: String,
    failed: bool,
}

/// Dragged value for the divider between the editor and the panel.
struct SplitDrag;

impl Render for SplitDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

pub const EXAMPLE_SQL: &str = "-- Read-only: runs inside BEGIN TRANSACTION READ ONLY.\n\
-- ⌘↵ / Ctrl+↵ to run, ⌘⇧↵ for a new line, ⌘S to save it in the project.\n\
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
        /// Rows changed, for runs in the write transaction.
        changed: Option<u64>,
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
    editor_height: Pixels,
    panel_collapsed: bool,
    panel_tab: PanelTab,
    output: Vec<OutputEntry>,
    write: WriteState,
    /// Whether the run in progress goes into the write transaction.
    run_writes: bool,
    /// Checks the open transaction for idleness while write mode is on.
    idle_watch: Option<Task<()>>,
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
            editor_height: DEFAULT_EDITOR_HEIGHT,
            panel_collapsed: false,
            panel_tab: PanelTab::Results,
            output: Vec::new(),
            write: WriteState::Off,
            run_writes: false,
            idle_watch: None,
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

    fn run_query(&mut self, _: &RunQuery, window: &mut Window, cx: &mut Context<Self>) {
        let sql = {
            let editor = self.editor.read(cx);
            let selected = editor.selected_text().to_string();
            if selected.trim().is_empty() {
                editor.text().to_string()
            } else {
                selected
            }
        };
        match &self.write {
            WriteState::Off | WriteState::Failed(_) => self.execute(sql, false, cx),
            WriteState::Connecting => {}
            WriteState::On(_) if !self.db.writes_allowed() => {
                self.refuse(
                    sql,
                    "Writes on this connection are no longer allowed: turn write mode off, or \
                     allow writes again"
                        .into(),
                    cx,
                );
            }
            WriteState::On(_) => {
                match write_mode::review(&sql, &self.db.name, self.db.environment) {
                    Review::Run { writes } => self.execute(sql, writes, cx),
                    Review::Refuse(reason) => self.refuse(sql, reason, cx),
                    Review::Confirm { message, detail } => {
                        let answer = window.prompt(
                            PromptLevel::Warning,
                            &message,
                            Some(&detail),
                            &["Run", "Cancel"],
                            cx,
                        );
                        cx.spawn(async move |this, cx| {
                            if let Ok(0) = answer.await {
                                this.update(cx, |this, cx| this.execute(sql, true, cx)).ok();
                            }
                        })
                        .detach();
                    }
                }
            }
        }
    }

    /// Logs a script that write mode will not run.
    fn refuse(&mut self, sql: String, reason: String, cx: &mut Context<Self>) {
        self.last_sql = sql;
        self.record(reason.clone(), true, cx);
        self.run = RunState::Failed(reason);
        self.panel_collapsed = false;
        cx.notify();
    }

    /// Runs `sql`: in the write transaction if `writes`, otherwise read-only
    /// (on the write connection when write mode is on, so it sees the
    /// uncommitted changes).
    fn execute(&mut self, sql: String, writes: bool, cx: &mut Context<Self>) {
        let session = match &self.write {
            WriteState::On(write) => write.session.clone(),
            _ => self.db.session.clone(),
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
        self.run_writes = writes;
        // A run brings its results into view.
        self.panel_collapsed = false;
        self.panel_tab = PanelTab::Results;
        cx.notify();

        self.last_sql = sql.clone();
        let rx = if writes {
            session.run_in_transaction(
                sql,
                RunOptions {
                    statement_timeout: Duration::from_secs(60),
                    ..RunOptions::default()
                },
            )
        } else {
            session.run_read_only(sql, RunOptions::default())
        };
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
                let changed = self.run_writes.then_some(summary.affected);
                if let (true, WriteState::On(write)) = (self.run_writes, &mut self.write) {
                    write.statements += summary.statements;
                    write.affected += summary.affected;
                    write.opened.get_or_insert_with(Instant::now);
                    write.last_run = Instant::now();
                }
                let outcome = match changed {
                    Some(changed) => format!(
                        "{} · {} changed · {} · not committed",
                        plural(summary.statements, "statement"),
                        plural_rows(changed),
                        format_duration(summary.elapsed)
                    ),
                    None => format!(
                        "{} rows · {}",
                        thousands(summary.rows as u64),
                        format_duration(summary.elapsed)
                    ),
                };
                self.record(outcome, false, cx);
                self.run = RunState::Done {
                    rows: summary.rows,
                    elapsed: summary.elapsed,
                    first_batch_after: summary.first_batch_after,
                    changed,
                };
            }
            Event::Failed(error) => {
                self.finish_table(cx);
                let mut message = describe_error(error);
                if let WriteState::On(write) = &mut self.write {
                    write.last_run = Instant::now();
                    if self.run_writes {
                        if write.session.in_transaction() {
                            message.push_str(" · this script was undone, earlier changes are kept");
                        } else {
                            write.reset();
                            message.push_str(" · the transaction ended");
                        }
                    }
                }
                self.record(message.clone(), true, cx);
                self.run = RunState::Failed(message);
            }
        }
        cx.notify();
    }

    /// Adds a line to the Output tab for something that is not a script run.
    fn log(&mut self, sql: &str, outcome: String, failed: bool, cx: &mut Context<Self>) {
        let previous = std::mem::replace(&mut self.last_sql, sql.to_string());
        self.record(outcome, failed, cx);
        self.last_sql = previous;
    }

    fn record(&mut self, outcome: String, failed: bool, cx: &mut Context<Self>) {
        self.output.push(OutputEntry {
            run: self.output.len() + 1,
            connection: self.db.name.clone(),
            sql: self.last_sql.clone(),
            outcome: outcome.clone(),
            failed,
        });
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
            match &self.write {
                WriteState::On(write) => write.session.cancel(),
                _ => self.db.session.cancel(),
            }
            cx.notify();
        }
    }

    /// Whether a transaction is waiting for Commit or Roll Back.
    pub fn in_transaction(&self) -> bool {
        matches!(&self.write, WriteState::On(write) if write.session.in_transaction())
    }

    fn toggle_write_mode(
        &mut self,
        _: &ToggleWriteMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &self.write {
            WriteState::Connecting => {}
            WriteState::On(_) => self.leave_write_mode(window, cx),
            WriteState::Off | WriteState::Failed(_) if self.db.writes_allowed() => {
                self.enter_write_mode(cx)
            }
            WriteState::Off | WriteState::Failed(_) => {
                let this = cx.weak_entity();
                write_mode::allow_writes(self.db.clone(), window, cx, move |_, cx| {
                    this.update(cx, |this, cx| this.enter_write_mode(cx)).ok();
                });
            }
        }
    }

    /// Opens the tab's own read-write connection.
    fn enter_write_mode(&mut self, cx: &mut Context<Self>) {
        self.write = WriteState::Connecting;
        cx.notify();
        let engine = crate::app::state(cx).engine.clone();
        let connecting = self.db.reconnect.open(&engine, Access::ReadWrite);
        cx.spawn(async move |this, cx| {
            let result = connecting.await;
            this.update(cx, |this, cx| {
                if !matches!(this.write, WriteState::Connecting) {
                    return;
                }
                this.write = match result {
                    Ok(session) => {
                        this.watch_idle(cx);
                        WriteState::On(WriteSession::new(session))
                    }
                    Err(error) => WriteState::Failed(error.to_string()),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Back to read-only; asks first if a transaction is open.
    fn leave_write_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.in_transaction() {
            self.drop_write_session(cx);
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            "Commit the open transaction?",
            Some("Write mode is turning off. The changes are only kept if you commit them."),
            &["Commit", "Roll Back", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            let commit = match answer.await {
                Ok(0) => true,
                Ok(1) => false,
                _ => return,
            };
            let Ok(ended) = this.update(cx, |this, cx| this.end_transaction(commit, cx)) else {
                return;
            };
            if ended.await.is_ok() {
                this.update(cx, |this, cx| this.drop_write_session(cx)).ok();
            }
        })
        .detach();
    }

    fn drop_write_session(&mut self, cx: &mut Context<Self>) {
        // Dropping the session closes its connection.
        self.write = WriteState::Off;
        self.idle_watch = None;
        cx.notify();
    }

    /// Production: stops allowing writes on this connection session.
    fn stop_allowing_writes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.db.set_writes_allowed(false);
        self.leave_write_mode(window, cx);
    }

    fn commit(&mut self, _: &CommitTransaction, _: &mut Window, cx: &mut Context<Self>) {
        self.end_transaction(true, cx).detach();
    }

    fn rollback(&mut self, _: &RollbackTransaction, _: &mut Window, cx: &mut Context<Self>) {
        self.end_transaction(false, cx).detach();
    }

    /// Commits or rolls back the open transaction, and logs it. Resolves to
    /// an error if the commit failed (the transaction is over either way).
    pub fn end_transaction(
        &mut self,
        commit: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), ()>> {
        self.finish_transaction(commit, None, cx)
    }

    fn finish_transaction(
        &mut self,
        commit: bool,
        note: Option<&'static str>,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), ()>> {
        let WriteState::On(write) = &self.write else {
            return Task::ready(Ok(()));
        };
        if !write.session.in_transaction() {
            return Task::ready(Ok(()));
        }
        let session = write.session.clone();
        let (statements, affected) = (write.statements, write.affected);
        cx.spawn(async move |this, cx| {
            let result = if commit {
                session.commit().await
            } else {
                session.rollback().await
            };
            this.update(cx, |this, cx| {
                if let WriteState::On(write) = &mut this.write {
                    write.reset();
                }
                let what = format!(
                    "{} · {} changed",
                    plural(statements, "statement"),
                    plural_rows(affected)
                );
                let (outcome, failed) = match (&result, commit) {
                    (Ok(()), true) => (format!("Committed · {what}"), false),
                    (Ok(()), false) => (
                        format!("Rolled back{} · {what} undone", note.unwrap_or_default()),
                        false,
                    ),
                    (Err(error), _) => (
                        format!(
                            "{} failed: {} · nothing was committed",
                            if commit { "Commit" } else { "Rollback" },
                            describe_error(error.clone())
                        ),
                        true,
                    ),
                };
                this.log(
                    if commit { "COMMIT" } else { "ROLLBACK" },
                    outcome,
                    failed,
                    cx,
                );
                cx.notify();
            })
            .ok();
            result.map_err(|_| ())
        })
    }

    /// Rolls back a transaction left open with no runs for [`IDLE_ROLLBACK`],
    /// so it does not hold locks forever; refreshes the countdown meanwhile.
    fn watch_idle(&mut self, cx: &mut Context<Self>) {
        self.idle_watch = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |this, cx| {
                    let WriteState::On(write) = &this.write else {
                        return;
                    };
                    if !write.session.in_transaction() || this.is_running() {
                        return;
                    }
                    let idle = write.last_run.elapsed();
                    if idle >= IDLE_ROLLBACK {
                        this.finish_transaction(false, Some(" after 10 minutes idle"), cx)
                            .detach();
                    } else if idle + IDLE_WARNING >= IDLE_ROLLBACK {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }

    fn toggle_results(&mut self, _: &ToggleResults, _: &mut Window, cx: &mut Context<Self>) {
        self.panel_collapsed = !self.panel_collapsed;
        cx.notify();
    }

    /// Clicking a panel tab selects it, and expands a collapsed panel.
    fn select_panel_tab(&mut self, tab: PanelTab, cx: &mut Context<Self>) {
        self.panel_tab = tab;
        self.panel_collapsed = false;
        cx.notify();
    }

    fn on_split_drag(&mut self, event: &DragMoveEvent<SplitDrag>, cx: &mut Context<Self>) {
        let bounds = event.bounds;
        let max = (bounds.size.height - MIN_PANEL_HEIGHT).max(MIN_EDITOR_HEIGHT);
        let height = (event.event.position.y - bounds.top()).clamp(MIN_EDITOR_HEIGHT, max);
        if height != self.editor_height {
            self.editor_height = height;
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
            .child(self.render_mode_toggle(cx))
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

    /// Read-only / write mode switch in the toolbar.
    fn render_mode_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let environment = self.db.environment;
        let (label, tone, tooltip): (SharedString, u32, &str) = match &self.write {
            WriteState::Off => ("Read-only".into(), 0x4da3ff, "Turn on write mode"),
            WriteState::Connecting => ("Connecting…".into(), theme::TEXT_SECONDARY, ""),
            WriteState::On(_) => (
                format!("Write · {}", environment.short_label()).into(),
                environment.color(),
                "Back to read-only",
            ),
            WriteState::Failed(_) => ("Write mode failed".into(), theme::RED, "Try again"),
        };
        let on = matches!(self.write, WriteState::On(_));
        h_flex()
            .id("write-mode")
            .gap_1()
            .px_2()
            .py(px(3.))
            .rounded(px(6.))
            .bg(color(tone).opacity(if on { 0.22 } else { 0.14 }))
            .text_color(color(tone))
            .text_size(px(12.))
            .font_weight(if on {
                FontWeight::SEMIBOLD
            } else {
                FontWeight::NORMAL
            })
            .hover(|s| s.bg(color(tone).opacity(0.3)))
            .child(Icon::new(if on { AppIcon::Pencil } else { AppIcon::Lock }).size(px(12.)))
            .child(label)
            .when(!tooltip.is_empty(), |toggle| {
                toggle.tooltip(move |window, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(tooltip).build(window, cx)
                })
            })
            .on_click(cx.listener(|this, _, window, cx| {
                this.toggle_write_mode(&ToggleWriteMode, window, cx)
            }))
    }

    fn render_banner(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let banner = h_flex()
            .h(px(28.))
            .px_4()
            .gap_2()
            .flex_shrink_0()
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .text_size(px(12.));
        let environment = self.db.environment;
        match &self.write {
            WriteState::On(_) => {
                let policy = environment.write_policy();
                banner
                    .bg(color(environment.color()).opacity(0.14))
                    .text_color(color(environment.color()))
                    .child(Icon::new(AppIcon::Pencil).size(px(13.)))
                    .child(div().flex_1().truncate().child(format!(
                        "Write mode on {} ({}) · changes stay in a transaction until you \
                             commit · {}",
                        self.db.name,
                        environment.label(),
                        write_mode::policy_hint(policy)
                    )))
                    .when(policy == WritePolicy::SessionUnlock, |banner| {
                        banner.child(
                            Button::new("stop-allowing-writes")
                                .ghost()
                                .xsmall()
                                .label("Stop Allowing Writes")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.stop_allowing_writes(window, cx)
                                })),
                        )
                    })
                    .into_any_element()
            }
            WriteState::Failed(error) => banner
                .bg(color(theme::RED).opacity(0.14))
                .text_color(color(theme::RED))
                .child(
                    div()
                        .truncate()
                        .child(format!("Could not open a write connection: {error}")),
                )
                .into_any_element(),
            WriteState::Off | WriteState::Connecting => banner
                .bg(color(theme::READ_ONLY_BG))
                .text_color(color(theme::READ_ONLY_FG))
                .child(Icon::new(AppIcon::Lock).size(px(13.)))
                .child(
                    "Read-only query: runs in a READ ONLY transaction. INSERT, UPDATE, DELETE and \
                     DDL are blocked.",
                )
                .into_any_element(),
        }
    }

    /// Shown while a transaction is open: what it holds, and Commit / Roll Back.
    fn render_transaction_bar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let WriteState::On(write) = &self.write else {
            return None;
        };
        if !write.session.in_transaction() {
            return None;
        }
        let idle = write.last_run.elapsed();
        let countdown = (idle + IDLE_WARNING >= IDLE_ROLLBACK).then(|| {
            let left = IDLE_ROLLBACK.saturating_sub(idle).as_secs();
            format!(" · rolls back in {}:{:02} if idle", left / 60, left % 60)
        });
        let open_for = write
            .opened
            .map(|opened| format!(" · open {}", format_duration(opened.elapsed())))
            .unwrap_or_default();
        let busy = self.is_running();
        Some(
            h_flex()
                .h(px(34.))
                .px_4()
                .gap_2()
                .flex_shrink_0()
                .bg(color(theme::AMBER).opacity(0.1))
                .border_b_1()
                .border_color(color(theme::HAIRLINE))
                .text_size(px(12.5))
                .child(div().size(px(7.)).rounded_full().bg(color(theme::AMBER)))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Transaction open"),
                )
                .child(
                    div()
                        .flex_1()
                        .truncate()
                        .text_color(color(theme::TEXT_SECONDARY))
                        .child(format!(
                            "{} · {} changed{open_for}{}",
                            plural(write.statements, "statement"),
                            plural_rows(write.affected),
                            countdown.unwrap_or_default()
                        )),
                )
                .child(
                    Button::new("rollback")
                        .small()
                        .label("Roll Back")
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.rollback(&RollbackTransaction, window, cx)
                        })),
                )
                .child(
                    Button::new("commit")
                        .small()
                        .primary()
                        .label("Commit")
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.commit(&CommitTransaction, window, cx)
                        })),
                ),
        )
    }

    /// The run's status, shown next to the panel tabs.
    fn status(&self, cx: &App) -> (String, u32) {
        let loaded = self.table.read(cx).delegate().rows();
        let memory = self.table.read(cx).delegate().memory_size() as f64 / 1_048_576.0;
        match &self.run {
            RunState::Idle => ("Ready".to_string(), theme::TEXT_TERTIARY),
            RunState::Running { started } => (
                format!(
                    "{} rows · {:.1} s · streaming…",
                    thousands(loaded as u64),
                    started.elapsed().as_secs_f32()
                ),
                theme::TEXT_SECONDARY,
            ),
            RunState::Done {
                changed: Some(changed),
                elapsed,
                ..
            } => (
                format!(
                    "{} changed · {} · not committed",
                    plural_rows(*changed),
                    format_duration(*elapsed)
                ),
                theme::AMBER,
            ),
            RunState::Done {
                rows,
                elapsed,
                first_batch_after,
                changed: None,
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
        }
    }

    fn render_panel_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (status, tone) = self.status(cx);
        let toggle_shortcut = if cfg!(target_os = "macos") {
            "⌘J"
        } else {
            "Ctrl+J"
        };
        let tab = |id: &'static str, label: SharedString, tab: PanelTab, cx: &mut Context<Self>| {
            let active = self.panel_tab == tab && !self.panel_collapsed;
            div()
                .id(id)
                .h_full()
                .px_3()
                .flex()
                .items_center()
                .gap_1()
                .border_b_2()
                .border_color(if active {
                    color(theme::ACCENT)
                } else {
                    color(theme::ACCENT).opacity(0.)
                })
                .text_color(color(if active {
                    theme::TEXT
                } else {
                    theme::TEXT_SECONDARY
                }))
                .font_weight(FontWeight::MEDIUM)
                .hover(|s| s.text_color(color(theme::TEXT)))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.select_panel_tab(tab, cx)))
        };
        let output_label: SharedString = if self.output.is_empty() {
            "Output".into()
        } else {
            format!("Output · {}", self.output.len()).into()
        };

        h_flex()
            .id("panel-header")
            .h(px(34.))
            .pl_1()
            .pr_2()
            .gap_1()
            .flex_shrink_0()
            .bg(color(0x1c1c1e))
            .border_t_1()
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .text_size(px(12.5))
            .child(tab(
                "panel-results",
                "Results".into(),
                PanelTab::Results,
                cx,
            ))
            .child(tab("panel-output", output_label, PanelTab::Output, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl_2()
                    .text_size(px(12.))
                    .text_color(color(tone))
                    .truncate()
                    .child(status),
            )
            .child(
                Button::new("toggle-results")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(if self.panel_collapsed {
                        AppIcon::PanelBottomOpen
                    } else {
                        AppIcon::PanelBottomClose
                    }))
                    .tooltip(format!(
                        "{} panel  {toggle_shortcut}",
                        if self.panel_collapsed { "Show" } else { "Hide" }
                    ))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_results(&ToggleResults, window, cx)
                    })),
            )
            // Double-clicking the strip collapses or expands the panel.
            .on_click(
                cx.listener(|this, event: &gpui_kit::ClickEvent, window, cx| {
                    if event.click_count() == 2 {
                        this.toggle_results(&ToggleResults, window, cx);
                    }
                }),
            )
    }

    fn render_output(&self, mono: SharedString) -> impl IntoElement {
        let mut list = v_flex();
        for entry in self.output.iter().rev() {
            list = list.child(
                v_flex()
                    .px_4()
                    .py_2()
                    .gap_1()
                    .border_b_1()
                    .border_color(color(theme::HAIRLINE))
                    .child(
                        h_flex()
                            .gap_2()
                            .text_size(px(11.5))
                            .child(
                                div()
                                    .text_color(color(theme::TEXT_TERTIARY))
                                    .child(format!("#{} · {}", entry.run, entry.connection)),
                            )
                            .child(
                                div()
                                    .text_color(color(if entry.failed {
                                        theme::RED
                                    } else {
                                        theme::GREEN
                                    }))
                                    .child(entry.outcome.clone()),
                            ),
                    )
                    .child(
                        div()
                            .font_family(mono.clone())
                            .text_size(px(11.5))
                            .text_color(color(theme::TEXT_SECONDARY))
                            .line_clamp(4)
                            .child(entry.sql.trim().to_string()),
                    ),
            );
        }
        if self.output.is_empty() {
            list = list.child(
                div()
                    .p_4()
                    .text_color(color(theme::TEXT_TERTIARY))
                    .child("Each run is logged here: its SQL, rows and time, or the error."),
            );
        }
        div()
            .id("query-output")
            .size_full()
            .overflow_y_scroll()
            .child(list)
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

        let collapsed = self.panel_collapsed;
        let mono = cx.theme().mono_font_family.clone();

        v_flex()
            .size_full()
            .key_context("QueryTab")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::run_query))
            .on_action(cx.listener(Self::cancel_query))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::save_as))
            .on_action(cx.listener(Self::toggle_results))
            .on_action(cx.listener(Self::toggle_write_mode))
            .on_action(cx.listener(Self::commit))
            .on_action(cx.listener(Self::rollback))
            .child(self.render_toolbar(cx))
            .child(self.render_banner(cx))
            .children(self.render_transaction_bar(cx))
            .child(
                v_flex()
                    .id("query-split")
                    .flex_1()
                    .min_h_0()
                    .on_drag_move(cx.listener(|this, event, _, cx| this.on_split_drag(event, cx)))
                    .child(
                        div()
                            .when(collapsed, |editor| editor.flex_1().min_h_0())
                            .when(!collapsed, |editor| {
                                editor.h(self.editor_height).flex_shrink_0()
                            })
                            .child(Editor::new(&self.editor).h_full().bordered(false)),
                    )
                    .when(!collapsed, |split| {
                        split.child(
                            // A thin band above the panel, easy to grab.
                            div()
                                .id("query-divider")
                                .h(px(5.))
                                .w_full()
                                .flex_shrink_0()
                                .cursor(CursorStyle::ResizeRow)
                                .hover(|s| s.bg(color(theme::ACCENT).opacity(0.45)))
                                .on_drag(SplitDrag, |_, _, _, cx| cx.new(|_| SplitDrag))
                                .on_click(cx.listener(
                                    |this, event: &gpui_kit::ClickEvent, _, cx| {
                                        // Double-click puts the divider back where it started.
                                        if event.click_count() == 2 {
                                            this.editor_height = DEFAULT_EDITOR_HEIGHT;
                                            cx.notify();
                                        }
                                    },
                                )),
                        )
                    })
                    .child(self.render_panel_header(cx))
                    .when(!collapsed, |split| {
                        split.child(
                            div().flex_1().min_h_0().child(match self.panel_tab {
                                PanelTab::Results => DataTable::new(&self.table)
                                    .stripe(true)
                                    .bordered(false)
                                    .into_any_element(),
                                PanelTab::Output => self.render_output(mono).into_any_element(),
                            }),
                        )
                    }),
            )
    }
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{} {noun}s", thousands(count as u64))
    }
}

fn plural_rows(count: u64) -> String {
    if count == 1 {
        "1 row".into()
    } else {
        format!("{} rows", thousands(count))
    }
}

/// `reports/monthly_sales.sql` → `monthly_sales`.
fn file_title(path: &Path) -> SharedString {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Query".into())
        .into()
}
