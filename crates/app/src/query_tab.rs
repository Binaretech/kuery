//! Query tab: SQL editor and streaming results. Read-only unless write mode
//! is on (see [`crate::write_mode`]): then the tab has a connection of its
//! own, and changes stay in a transaction until Commit or Roll Back. A tab can
//! be bound to a saved query file in the project; its first line,
//! `-- connection: <name>`, is kept in the file but not shown in the editor.
//!
//! The editor and the bottom panel are split by a draggable divider. The
//! panel has a Results tab (the grid) and an Output tab (a log of runs), and
//! can be collapsed down to its tab strip.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{
    Copy, Cut, Editor, EditorState, Enter, InputEvent, Paste, SelectAll,
};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::native_menu::NativeMenu;
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, Context, CursorStyle, DragMoveEvent, Empty, Entity, EventEmitter,
    FocusHandle, Focusable, FontWeight, InteractiveElement as _, IntoElement, KeyBinding,
    ParentElement as _, Pixels, PromptLevel, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Task, Window, actions, div, px,
};
use kuery_core::export::{self, Format};
use kuery_core::{format, script};
use kuery_pg::{Access, Event, QueryError, RunOptions, Session};

use crate::assets::AppIcon;
use crate::audit::{self, AuditAction, AuditRecord};
use crate::connection::{Db, WriteKind, WritePolicy};
use crate::history::HistoryEntry;
use crate::project::with_connection_header;
use crate::results::ResultsDelegate;
use crate::theme::{self, color};
use crate::ui::{format_duration, thousands};
use crate::write_mode::{self, Review};

actions!(
    kuery,
    [
        RunQuery,
        RunAll,
        FormatSql,
        CancelQuery,
        SaveQuery,
        SaveQueryAs,
        ToggleResults,
        UseReadOnly,
        UseWrite,
        UseFullWrite,
        ExportResults,
        PreviewImpact,
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
    kind: WriteKind,
    /// Statements and rows changed since the transaction opened.
    statements: usize,
    affected: u64,
    opened: Option<Instant>,
    last_run: Instant,
}

impl WriteSession {
    fn new(session: Session, kind: WriteKind) -> Self {
        Self {
            session,
            kind,
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
-- ⌘↵ runs the statement under the cursor (or the selection), ⌥⌘↵ runs it all.\n\
-- ⌘⇧↵ inserts a new line, ⌘⇧F formats, ⌘S saves it in the project.\n\
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
        /// Rows changed, for runs that write.
        changed: Option<u64>,
        /// Whether those changes are already kept (full write).
        applied: bool,
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

    /// Runs the selection, or else the statement under the cursor.
    fn run_query(&mut self, _: &RunQuery, window: &mut Window, cx: &mut Context<Self>) {
        let sql = self.sql_to_run(cx);
        self.run_sql(sql, window, cx);
    }

    /// Runs everything in the editor.
    fn run_all(&mut self, _: &RunAll, window: &mut Window, cx: &mut Context<Self>) {
        let sql = self.editor.read(cx).text().to_string();
        self.run_sql(sql, window, cx);
    }

    /// The selection if there is one; otherwise the statement under the
    /// cursor, or the whole text when it holds a single statement (or cannot
    /// be split).
    fn sql_to_run(&self, cx: &App) -> String {
        let editor = self.editor.read(cx);
        let selected = editor.selected_text().to_string();
        if !selected.trim().is_empty() {
            return selected;
        }
        let text = editor.text().to_string();
        match script::statements(&text) {
            Some(ranges) if ranges.len() > 1 => script::statement_at(&text, editor.cursor())
                .map(|range| text[range].to_string())
                .unwrap_or(text),
            _ => text,
        }
    }

    /// Formats the selection, or the whole text; undo brings it back.
    fn format_sql(&mut self, _: &FormatSql, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let selected = editor.selected_text().to_string();
            if !selected.trim().is_empty() {
                let formatted = format::format_sql(&selected);
                if formatted != selected {
                    editor.replace(formatted, window, cx);
                }
            } else {
                let text = editor.text().to_string();
                let formatted = format::format_sql(&text);
                if formatted != text {
                    editor.replace_all(formatted, window, cx);
                }
            }
        });
    }

    fn run_sql(&mut self, sql: String, window: &mut Window, cx: &mut Context<Self>) {
        match &self.write {
            WriteState::Off | WriteState::Failed(_) => self.execute(sql, false, cx),
            WriteState::Connecting => {}
            WriteState::On(write) if !self.db.allows(write.kind) => {
                let reason = format!(
                    "{} on this connection is no longer allowed: switch to Read-only, or \
                     allow it again",
                    write.kind.label()
                );
                self.refuse(sql, reason, cx);
            }
            WriteState::On(write) => {
                match write_mode::review(&sql, &self.db.name, self.db.environment, write.kind) {
                    Review::Run { writes } => self.execute(sql, writes, cx),
                    Review::Refuse(reason) => self.refuse(sql, reason, cx),
                    Review::Confirm { message, detail } => {
                        let answer = window.prompt(
                            PromptLevel::Warning,
                            &message,
                            Some(&detail),
                            &["Run", "Preview Impact", "Cancel"],
                            cx,
                        );
                        cx.spawn_in(window, async move |this, cx| match answer.await {
                            Ok(0) => {
                                this.update(cx, |this, cx| this.execute(sql, true, cx)).ok();
                            }
                            Ok(1) => {
                                this.update_in(cx, |this, window, cx| {
                                    this.preview(sql, true, window, cx)
                                })
                                .ok();
                            }
                            _ => {}
                        })
                        .detach();
                    }
                }
            }
        }
    }

    /// Previews the selection or the statement under the cursor, without
    /// asking: the result goes to the Output tab.
    fn preview_impact(&mut self, _: &PreviewImpact, window: &mut Window, cx: &mut Context<Self>) {
        let sql = self.sql_to_run(cx);
        let WriteState::On(write) = &self.write else {
            self.refuse(
                sql,
                "Turn on Write or Full write to preview what a script changes".into(),
                cx,
            );
            return;
        };
        match write_mode::review(&sql, &self.db.name, self.db.environment, write.kind) {
            Review::Refuse(reason) => self.refuse(sql, reason, cx),
            Review::Run { .. } | Review::Confirm { .. } => self.preview(sql, false, window, cx),
        }
    }

    /// Runs `sql` and undoes it, to see what it would change. With
    /// `then_ask`, shows the result and offers to run it for real; otherwise
    /// only logs it.
    fn preview(
        &mut self,
        sql: String,
        then_ask: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let WriteState::On(write) = &self.write else {
            return;
        };
        let preview = write.session.preview(
            sql.clone(),
            RunOptions {
                statement_timeout: Duration::from_secs(60),
                ..RunOptions::default()
            },
        );
        let (name, environment) = (self.db.name.clone(), self.db.environment);
        cx.spawn_in(window, async move |this, cx| {
            let result = preview.await;
            this.update_in(cx, |this, window, cx| {
                let lines = match &result {
                    Ok(impacts) => write_mode::describe_impact(&sql, impacts),
                    Err(error) => vec![describe_error(error.clone())],
                };
                let failed = result.is_err();
                let outcome = if failed {
                    format!("Preview failed: {}", lines.join(" · "))
                } else {
                    format!("Preview (undone): {}", lines.join(" · "))
                };
                this.audit(AuditAction::Preview, &sql, &outcome, None, failed, cx);
                this.log(&sql, outcome, failed, cx);
                if !then_ask {
                    this.panel_collapsed = false;
                    this.panel_tab = PanelTab::Output;
                    cx.notify();
                    return;
                }
                let (message, buttons): (String, &[&str]) = if failed {
                    ("The preview failed".into(), &["OK"])
                } else {
                    (
                        format!("Preview on {name} ({})", environment.label()),
                        &["Run", "Cancel"],
                    )
                };
                let mut detail = lines
                    .iter()
                    .map(|line| format!("• {line}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !failed {
                    detail.push_str("\n\nThe preview was undone. Run the script for real?");
                }
                let answer = window.prompt(
                    if failed {
                        PromptLevel::Critical
                    } else {
                        PromptLevel::Warning
                    },
                    &message,
                    Some(&detail),
                    buttons,
                    cx,
                );
                cx.spawn(async move |this, cx| {
                    if let (Ok(0), false) = (answer.await, failed) {
                        this.update(cx, |this, cx| this.execute(sql, true, cx)).ok();
                    }
                })
                .detach();
            })
            .ok();
        })
        .detach();
    }

    /// Logs a script that write mode will not run.
    fn refuse(&mut self, sql: String, reason: String, cx: &mut Context<Self>) {
        self.last_sql = sql;
        self.record(reason.clone(), true, cx);
        self.run = RunState::Failed(reason);
        self.panel_collapsed = false;
        cx.notify();
    }

    /// Runs `sql`. If `writes`, in the write transaction or applied right
    /// away, depending on the write mode; otherwise read-only (on the write
    /// connection when write mode is on, so it sees uncommitted changes).
    fn execute(&mut self, sql: String, writes: bool, cx: &mut Context<Self>) {
        let (session, kind) = match &self.write {
            WriteState::On(write) => (write.session.clone(), Some(write.kind)),
            _ => (self.db.session.clone(), None),
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
        let write_options = RunOptions {
            statement_timeout: Duration::from_secs(60),
            ..RunOptions::default()
        };
        let rx = match (writes, kind) {
            (true, Some(WriteKind::Full)) => session.run_autocommit(sql, write_options),
            (true, _) => session.run_in_transaction(sql, write_options),
            (false, _) => session.run_read_only(sql, RunOptions::default()),
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
                let mut applied = false;
                if let (true, WriteState::On(write)) = (self.run_writes, &mut self.write) {
                    write.last_run = Instant::now();
                    match write.kind {
                        WriteKind::Transactional => {
                            write.statements += summary.statements;
                            write.affected += summary.affected;
                            write.opened.get_or_insert_with(Instant::now);
                        }
                        WriteKind::Full => applied = true,
                    }
                }
                let outcome = match changed {
                    Some(changed) => format!(
                        "{} · {} changed · {} · {}",
                        plural(summary.statements, "statement"),
                        plural_rows(changed),
                        format_duration(summary.elapsed),
                        if applied { "applied" } else { "not committed" }
                    ),
                    None => format!(
                        "{} rows · {}",
                        thousands(summary.rows as u64),
                        format_duration(summary.elapsed)
                    ),
                };
                if self.run_writes {
                    let sql = self.last_sql.clone();
                    let changed = Some((summary.statements, summary.affected));
                    self.audit(AuditAction::Run, &sql, &outcome, changed, false, cx);
                }
                self.record(outcome, false, cx);
                self.run = RunState::Done {
                    rows: summary.rows,
                    elapsed: summary.elapsed,
                    first_batch_after: summary.first_batch_after,
                    changed,
                    applied,
                };
            }
            Event::Failed(error) => {
                self.finish_table(cx);
                let mut message = describe_error(error);
                if let WriteState::On(write) = &mut self.write {
                    write.last_run = Instant::now();
                    if self.run_writes && write.kind == WriteKind::Full {
                        message.push_str(" · nothing was applied");
                    } else if self.run_writes {
                        if write.session.in_transaction() {
                            message.push_str(" · this script was undone, earlier changes are kept");
                        } else {
                            write.reset();
                            message.push_str(" · the transaction ended");
                        }
                    }
                }
                if self.run_writes {
                    let sql = self.last_sql.clone();
                    self.audit(AuditAction::Run, &sql, &message, None, true, cx);
                }
                self.record(message.clone(), true, cx);
                self.run = RunState::Failed(message);
            }
        }
        cx.notify();
    }

    /// Appends a write-mode event to the audit log, off the UI thread.
    fn audit(
        &self,
        action: AuditAction,
        sql: &str,
        outcome: &str,
        changed: Option<(usize, u64)>,
        failed: bool,
        cx: &mut Context<Self>,
    ) {
        let mode = match &self.write {
            WriteState::On(write) => write.kind.label(),
            _ => WriteKind::Transactional.label(),
        };
        let record = AuditRecord {
            at: AuditRecord::now_at(),
            // `<project>/.kuery/queries` → `<project>`.
            project: self
                .queries_dir
                .ancestors()
                .nth(2)
                .unwrap_or(&self.queries_dir)
                .display()
                .to_string(),
            connection: self.db.name.to_string(),
            environment: self.db.environment.label().to_string(),
            mode: mode.to_string(),
            action,
            sql: sql.to_string(),
            outcome: outcome.to_string(),
            changed,
            failed,
        };
        cx.background_spawn(async move {
            if let Err(error) = audit::append(&record) {
                eprintln!("kuery: could not write the audit log: {error}");
            }
        })
        .detach();
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
        cx.emit(QueryRan(HistoryEntry::now(
            &self.db.name,
            &self.last_sql,
            outcome,
            failed,
        )));
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

    fn use_read_only(&mut self, _: &UseReadOnly, window: &mut Window, cx: &mut Context<Self>) {
        self.set_mode(None, window, cx);
    }

    fn use_write(&mut self, _: &UseWrite, window: &mut Window, cx: &mut Context<Self>) {
        self.set_mode(Some(WriteKind::Transactional), window, cx);
    }

    fn use_full_write(&mut self, _: &UseFullWrite, window: &mut Window, cx: &mut Context<Self>) {
        self.set_mode(Some(WriteKind::Full), window, cx);
    }

    /// Switches between read-only (`None`) and the write modes. On
    /// production, asks to allow what is missing first (full write needs
    /// writes allowed, then full writes allowed).
    fn set_mode(&mut self, target: Option<WriteKind>, window: &mut Window, cx: &mut Context<Self>) {
        let current = match &self.write {
            WriteState::Connecting => return,
            WriteState::On(write) => Some(write.kind),
            WriteState::Off | WriteState::Failed(_) => None,
        };
        if target == current {
            return;
        }
        let Some(kind) = target else {
            self.leave_write_mode(window, cx);
            return;
        };
        if !self.db.allows(kind) {
            let missing = if self.db.allows(WriteKind::Transactional) {
                WriteKind::Full
            } else {
                WriteKind::Transactional
            };
            let this = cx.weak_entity();
            write_mode::allow_writes(self.db.clone(), missing, window, cx, move |window, cx| {
                // Once the dialog is gone: this may open the next one.
                let this = this.clone();
                window.defer(cx, move |window, cx| {
                    this.update(cx, |this, cx| this.set_mode(Some(kind), window, cx))
                        .ok();
                });
            });
            return;
        }
        match &mut self.write {
            WriteState::On(write) if kind == WriteKind::Full && write.session.in_transaction() => {
                self.settle_transaction_then(
                    "Full write keeps no transaction open. The changes are only kept if you \
                     commit them.",
                    window,
                    cx,
                    |this, cx| {
                        if let WriteState::On(write) = &mut this.write {
                            write.kind = WriteKind::Full;
                        }
                        cx.notify();
                    },
                );
            }
            WriteState::On(write) => {
                write.kind = kind;
                cx.notify();
            }
            WriteState::Off | WriteState::Failed(_) | WriteState::Connecting => {
                self.enter_write_mode(kind, cx)
            }
        }
    }

    /// Opens the tab's own read-write connection.
    fn enter_write_mode(&mut self, kind: WriteKind, cx: &mut Context<Self>) {
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
                        WriteState::On(WriteSession::new(session, kind))
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
        self.settle_transaction_then(
            "Write mode is turning off. The changes are only kept if you commit them.",
            window,
            cx,
            |this, cx| this.drop_write_session(cx),
        );
    }

    /// Asks to commit or roll back the open transaction, then runs `then`
    /// (unless the user cancels or the commit fails).
    fn settle_transaction_then(
        &mut self,
        detail: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) {
        let answer = window.prompt(
            PromptLevel::Warning,
            "Commit the open transaction?",
            Some(detail),
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
                this.update(cx, then).ok();
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
        self.db.revoke_writes();
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
                let action = if commit {
                    AuditAction::Commit
                } else {
                    AuditAction::Rollback
                };
                this.audit(
                    action,
                    "",
                    &outcome,
                    Some((statements, affected)),
                    failed,
                    cx,
                );
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

    /// Asks where to save the results (the extension picks CSV, TSV or JSON)
    /// and writes them off the UI thread.
    fn export_results(&mut self, _: &ExportResults, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_running() {
            return;
        }
        let set = self.table.read(cx).delegate().results().clone();
        if set.columns().is_empty() {
            self.log("EXPORT", "There are no results to export".into(), true, cx);
            cx.notify();
            return;
        }
        // `<project>/.kuery/queries` → `<project>`.
        let directory = self
            .queries_dir
            .ancestors()
            .nth(2)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.queries_dir.clone());
        let suggested = format!("{}.csv", self.title.to_lowercase().replace(' ', "_"));
        let chosen = cx.prompt_for_new_path(&directory, Some(&suggested));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(mut path))) = chosen.await else {
                return;
            };
            if path.extension().is_none() {
                path.set_extension(Format::Csv.extension());
            }
            let format = Format::from_path(&path);
            let rows = set.rows();
            let started = Instant::now();
            let target = path.clone();
            let written = cx
                .background_spawn(async move {
                    let file = std::fs::File::create(&target)?;
                    let mut out = std::io::BufWriter::new(file);
                    export::write(&set, format, &mut out)?;
                    out.flush()
                })
                .await;
            this.update(cx, |this, cx| {
                let (outcome, failed) = match written {
                    Ok(()) => (
                        format!(
                            "Exported {} as {} to {} · {}",
                            plural_rows(rows as u64),
                            format.extension().to_uppercase(),
                            path.display(),
                            format_duration(started.elapsed())
                        ),
                        false,
                    ),
                    Err(error) => (
                        format!("Could not export to {}: {error}", path.display()),
                        true,
                    ),
                };
                this.log("EXPORT", outcome, failed, cx);
                this.panel_collapsed = false;
                this.panel_tab = PanelTab::Output;
                cx.notify();
            })
            .ok();
        })
        .detach();
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
            .child(self.render_mode_selector(cx))
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

    /// The SQL editor's right-click menu: run, edit, save and mode.
    fn editor_menu(
        &self,
        cx: &mut Context<Self>,
    ) -> impl Fn(NativeMenu, &mut Window, &mut App) -> NativeMenu + 'static {
        let this = cx.weak_entity();
        let editor = self.editor.clone();
        move |menu, _, cx| {
            let Some(tab) = this.upgrade() else {
                return menu;
            };
            let tab = tab.read(cx);
            let running = tab.is_running();
            let mode = match &tab.write {
                WriteState::On(write) => Some(write.kind),
                _ => None,
            };
            let selection = !editor
                .read(cx)
                .selected_text()
                .to_string()
                .trim()
                .is_empty();
            menu.menu(
                if selection {
                    "Run Selection"
                } else {
                    "Run Statement"
                },
                Box::new(RunQuery),
            )
            .menu("Run All", Box::new(RunAll))
            .menu_with_disabled(
                "Preview Impact",
                mode.is_none() || running,
                Box::new(PreviewImpact),
            )
            .menu_with_disabled("Cancel Query", !running, Box::new(CancelQuery))
            .separator()
            .menu("Cut", Box::new(Cut))
            .menu("Copy", Box::new(Copy))
            .menu("Paste", Box::new(Paste))
            .menu("Select All", Box::new(SelectAll))
            .separator()
            .menu("Format SQL", Box::new(FormatSql))
            .menu("Save Query", Box::new(SaveQuery))
            .menu("Save Query As…", Box::new(SaveQueryAs))
            .menu_with_disabled("Export Results…", running, Box::new(ExportResults))
            .separator()
            .submenu(
                "Mode",
                NativeMenu::new()
                    .menu_with_check("Read-only", mode.is_none(), Box::new(UseReadOnly))
                    .menu_with_check(
                        "Write",
                        mode == Some(WriteKind::Transactional),
                        Box::new(UseWrite),
                    )
                    .menu_with_check(
                        "Full Write",
                        mode == Some(WriteKind::Full),
                        Box::new(UseFullWrite),
                    ),
            )
        }
    }

    /// Read-only / Write / Full write selector in the toolbar.
    fn render_mode_selector(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let environment = self.db.environment;
        let current = match &self.write {
            WriteState::On(write) => Some(write.kind),
            _ => None,
        };
        let (label, tone): (SharedString, u32) = match &self.write {
            WriteState::Off => ("Read-only".into(), 0x4da3ff),
            WriteState::Connecting => ("Connecting…".into(), theme::TEXT_SECONDARY),
            WriteState::On(write) => (
                format!("{} · {}", write.kind.label(), environment.short_label()).into(),
                match write.kind {
                    WriteKind::Transactional => environment.color(),
                    WriteKind::Full => theme::RED,
                },
            ),
            WriteState::Failed(_) => ("Write mode failed".into(), theme::RED),
        };
        let on = current.is_some();
        let pill = h_flex()
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
            .child(Icon::new(if on { AppIcon::Pencil } else { AppIcon::Lock }).size(px(12.)))
            .child(label)
            .child(Icon::new(IconName::ChevronDown).size(px(11.)));

        let this = cx.weak_entity();
        Button::new("write-mode")
            .ghost()
            .compact()
            .child(pill)
            .dropdown_menu(move |menu, _, _| {
                let item = |label: &'static str, mode: Option<WriteKind>, hint: &'static str| {
                    let this = this.clone();
                    PopupMenuItem::new(format!("{label}  ·  {hint}"))
                        .checked(current == mode)
                        .on_click(move |_, window, cx| {
                            this.update(cx, |this, cx| this.set_mode(mode, window, cx))
                                .ok();
                        })
                };
                menu.item(item("Read-only", None, "nothing can change"))
                    .item(item(
                        "Write",
                        Some(WriteKind::Transactional),
                        "in a transaction you commit",
                    ))
                    .item(item(
                        "Full write",
                        Some(WriteKind::Full),
                        "each script applied right away",
                    ))
            })
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
            WriteState::On(write) => {
                let policy = environment.write_policy();
                let hint = write_mode::policy_hint(policy);
                let (tone, text) = match write.kind {
                    WriteKind::Transactional => (
                        environment.color(),
                        format!(
                            "Write mode on {} ({}) · changes stay in a transaction until you \
                             commit · {hint}",
                            self.db.name,
                            environment.label()
                        ),
                    ),
                    WriteKind::Full => (
                        theme::RED,
                        format!(
                            "Full write on {} ({}) · each script is applied when it finishes, \
                             with no Roll Back · {hint}",
                            self.db.name,
                            environment.label()
                        ),
                    ),
                };
                banner
                    .bg(color(tone).opacity(0.14))
                    .text_color(color(tone))
                    .child(Icon::new(AppIcon::Pencil).size(px(13.)))
                    .child(div().flex_1().truncate().child(text))
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
                applied,
                ..
            } => (
                format!(
                    "{} changed · {} · {}",
                    plural_rows(*changed),
                    format_duration(*elapsed),
                    if *applied { "applied" } else { "not committed" }
                ),
                if *applied { theme::GREEN } else { theme::AMBER },
            ),
            RunState::Done {
                rows,
                elapsed,
                first_batch_after,
                changed: None,
                ..
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
        let has_results = !self
            .table
            .read(cx)
            .delegate()
            .results()
            .columns()
            .is_empty();
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
                Button::new("export-results")
                    .ghost()
                    .xsmall()
                    .label("Export…")
                    .tooltip("Save the results as CSV, TSV or JSON")
                    .disabled(self.is_running() || !has_results)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.export_results(&ExportResults, window, cx)
                    })),
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
            .on_action(cx.listener(Self::use_read_only))
            .on_action(cx.listener(Self::use_write))
            .on_action(cx.listener(Self::use_full_write))
            .on_action(cx.listener(Self::export_results))
            .on_action(cx.listener(Self::preview_impact))
            .on_action(cx.listener(Self::run_all))
            .on_action(cx.listener(Self::format_sql))
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
                            .child(
                                Editor::new(&self.editor)
                                    .h_full()
                                    .bordered(false)
                                    .context_menu(self.editor_menu(cx)),
                            ),
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
