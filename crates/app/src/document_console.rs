//! A document database's console: statements in the engine's own language
//! (mongosh's commands for MongoDB), each read by the language before
//! anything runs, so the console knows what a statement does without
//! running it. `secondary-enter` runs the statement under the cursor (or
//! the selection), `secondary-alt-enter` all of them, `secondary-.` stops
//! what runs.
//!
//! Each run starts in the database the toolbar shows, the connection's own
//! when it shows none: what a `use` changed lasts until the run ends. The
//! documents of the last statement that returned any fill a grid like the
//! collection tab's, with the selected one beside it; every statement's
//! outcome goes to the Output log. A statement's reading stops after the
//! documents the grid holds, and Export All runs it again, in the database
//! it ran in, to write every one of them into a file.
//!
//! The console only reads for now: statements that write are refused
//! before they reach the server. It has its own key context
//! (`DocumentConsole`), so the query tab's Save, Format, Explain and Commit
//! are not bound here to do nothing.

use std::cell::RefCell;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::highlighter::{Diagnostic, DiagnosticSeverity};
use gpui_kit::component::input::{
    Editor, EditorState, Enter, InputEvent, RopeExt as _, ShowCompletions,
};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu};
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, WeakEntity, Window, div, px,
};
use kuery_core::document::adapter::Outcome;
use kuery_core::document::export::{Ejson, document_ejson};
use kuery_core::document::language::{Language, ParseError, Statement, Style};
use kuery_core::document::session::{DocEvent, DocumentSession, Heavy, RunMode};
use kuery_core::document::value::{Document, Value};
use kuery_core::session::{QueryError, RunOptions};

use crate::assets::AppIcon;
use crate::chrome;
use crate::collection_tab::{open_export, plural_documents, write_batch};
use crate::completion::{DocumentCompletions, SharedNames};
use crate::connection::{DocDb, Environment};
use crate::context_menus::{Cell, copy, item};
use crate::document_detail::{DetailEvent, DocumentDetail};
use crate::document_grid::{DocumentRows, Flattened};
use crate::grid::BrowseDelegate;
use crate::query_tab::{CancelQuery, RunAll, RunQuery, describe_error};
use crate::theme::{self, color};
use crate::ui::{format_duration, thousands};

const EDITOR_HEIGHT: f32 = 220.;

/// Documents of a statement the grid holds; Export All reads every one.
const SHOWN: usize = 1_000;

/// How long typing pauses before the editor's text is read for errors.
const CHECK_AFTER: Duration = Duration::from_millis(150);

/// How long Export All may read for.
const EXPORT_TIME: Duration = Duration::from_secs(60 * 60);

/// The editor's own keys in the console, as in a query tab: a new line
/// without running, and completion on demand.
pub fn bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new(
            "secondary-shift-enter",
            Enter {
                secondary: false,
                shift: true,
            },
            Some("DocumentConsole > Input"),
        ),
        KeyBinding::new(
            "ctrl-space",
            ShowCompletions,
            Some("DocumentConsole > Input"),
        ),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Panel {
    Results,
    Output,
}

/// One statement in the Output log.
struct OutputEntry {
    statement: String,
    text: String,
    failed: bool,
    elapsed: Option<Duration>,
}

enum SessionState {
    Connecting,
    Ready(DocumentSession),
    Failed(String),
}

enum Status {
    Idle,
    Running,
    Done {
        statements: usize,
        elapsed: Duration,
    },
    Failed(String),
}

enum ExportState {
    Running { documents: usize },
    Done(String),
    Failed(String),
}

/// The statement whose documents the grid shows, as Export All runs it
/// again: on its own, in the database it ran in.
#[derive(Clone, Debug)]
struct Shown {
    /// Where it ran: where the run started, or where a `use` before it
    /// in the script went.
    path: Vec<String>,
    statement: String,
    collection: String,
}

pub struct DocumentConsole {
    db: DocDb,
    /// The project folder, where exports are offered.
    project: PathBuf,
    /// The container each run starts in (`["shop"]`); empty for the
    /// connection's own ([`DocumentSession::home`]).
    path: Vec<String>,
    /// The containers the toolbar offers, as the catalog lists them.
    containers: Vec<String>,
    session: SessionState,
    editor: Entity<EditorState>,
    /// Reads the editor's text for errors once typing pauses.
    check_task: Option<Task<()>>,
    names: SharedNames,
    grid: Entity<TableState<BrowseDelegate>>,
    rows: Rc<RefCell<DocumentRows>>,
    /// The row whose document shows beside the grid.
    selected: Option<usize>,
    detail: Entity<DocumentDetail>,
    output: Vec<OutputEntry>,
    panel: Panel,
    status: Status,
    shown: Option<Shown>,
    run_task: Option<Task<()>>,
    /// Cancel was asked for while the run had not reached its statements.
    cancelled: bool,
    export: Option<ExportState>,
    /// The session the running export reads through, so Stop can cancel it.
    export_session: Option<DocumentSession>,
    export_task: Option<Task<()>>,
    focus_handle: FocusHandle,
}

impl DocumentConsole {
    pub fn new(
        db: DocDb,
        path: Vec<String>,
        project: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let language = db.catalog.language();
        let names = SharedNames::default();
        let editor = cx.new(|cx| {
            let mut editor = EditorState::new(window, cx)
                .language(language.editor)
                .line_number(true)
                .placeholder(format!(
                    "Statements in {}, one per line or ended with ;. ⌘↵ runs the one under the cursor, ⌥⌘↵ all of them.",
                    language.name
                ));
            editor.lsp_mut().completion_provider =
                Some(Rc::new(DocumentCompletions::new(language, names.clone())));
            editor
        });
        cx.subscribe(&editor, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.check(cx);
            }
        })
        .detach();

        let rows = Rc::new(RefCell::new(DocumentRows::default()));
        let grid = cx.new(|cx| TableState::new(BrowseDelegate::default(), window, cx));
        let weak = cx.entity().downgrade();
        grid.update(cx, |grid, _| {
            let absent = rows.clone();
            grid.delegate_mut()
                .set_absent(Some(Rc::new(move |row, column| {
                    absent.borrow().is_absent(row, column)
                })));
            let documents = rows.clone();
            grid.delegate_mut().set_row_json(Some(Rc::new(move |row| {
                let documents = documents.borrow();
                let json = document_ejson(documents.document(row)?, Ejson::Relaxed);
                serde_json::to_string(&json).ok()
            })));
            let menu = weak.clone();
            grid.delegate_mut()
                .set_on_menu(Rc::new(move |popup, cell, cx| {
                    row_menu(&menu, popup, cell, cx)
                }));
        });
        cx.subscribe_in(&grid, window, |this, _, event: &TableEvent, window, cx| {
            if let TableEvent::SelectRow(row) = event {
                this.select_row(*row, window, cx);
            }
        })
        .detach();
        let detail = cx.new(|cx| DocumentDetail::new(language, window, cx));
        cx.subscribe_in(&detail, window, |this, _, event: &DetailEvent, _, cx| {
            // There is no filter bar to filter by here.
            if let DetailEvent::Close = event {
                this.selected = None;
                cx.notify();
            }
        })
        .detach();

        let opening = db.catalog.reopen(db.catalog.access());
        cx.spawn(async move |this, cx| {
            let result = opening.await;
            this.update(cx, |this, cx| {
                this.session = match result {
                    Ok(session) => SessionState::Ready(session),
                    Err(error) => SessionState::Failed(error),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
        let listing = db.catalog.children(Vec::new());
        cx.spawn(async move |this, cx| {
            let Ok(children) = listing.await else {
                return;
            };
            this.update(cx, |this, cx| {
                this.containers = children
                    .into_iter()
                    .filter(|child| !child.leaf)
                    .map(|child| child.name)
                    .collect();
                cx.notify();
            })
            .ok();
        })
        .detach();

        let mut console = Self {
            db,
            project,
            path: Vec::new(),
            containers: Vec::new(),
            session: SessionState::Connecting,
            editor,
            check_task: None,
            names,
            grid,
            rows,
            selected: None,
            detail,
            output: Vec::new(),
            panel: Panel::Results,
            status: Status::Idle,
            shown: None,
            run_task: None,
            cancelled: false,
            export: None,
            export_session: None,
            export_task: None,
            focus_handle: cx.focus_handle(),
        };
        console.set_path(path, cx);
        console
    }

    pub fn db(&self) -> &DocDb {
        &self.db
    }

    /// The container each run starts in.
    pub fn path(&self) -> &[String] {
        &self.path
    }

    /// The statements in the editor.
    pub fn text(&self, cx: &App) -> String {
        self.editor.read(cx).text().to_string()
    }

    pub fn set_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.set_value(text, window, cx));
    }

    pub fn title(&self) -> SharedString {
        match self.path.last() {
            Some(name) => format!("Console · {name}").into(),
            None => "Console".into(),
        }
    }

    pub fn focus_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.editor.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// For the status bar.
    pub fn status(&self) -> String {
        if let Some(ExportState::Running { documents }) = &self.export {
            return format!(
                "Exporting · {} written",
                plural_documents(*documents as u64)
            );
        }
        match (&self.session, &self.status) {
            (SessionState::Connecting, _) => "Connecting…".into(),
            (SessionState::Failed(error), _) => error.clone(),
            (_, Status::Idle) => "Ready".into(),
            (_, Status::Running) => "Running…".into(),
            (
                _,
                Status::Done {
                    statements,
                    elapsed,
                },
            ) => format!(
                "{statements} statement{} · {}",
                if *statements == 1 { "" } else { "s" },
                format_duration(*elapsed)
            ),
            (_, Status::Failed(error)) => error.clone(),
        }
    }

    fn language(&self) -> &'static Language {
        self.db.catalog.language()
    }

    /// Work the language marks heavy runs on development connections only.
    fn heavy(&self) -> Heavy {
        match self.db.environment {
            Environment::Development => Heavy::Allow,
            Environment::Staging | Environment::Production => Heavy::Refuse,
        }
    }

    /// Where runs start, and the collections completion offers there.
    fn set_path(&mut self, path: Vec<String>, cx: &mut Context<Self>) {
        self.path = path.clone();
        self.names.borrow_mut().collections.clear();
        if !path.is_empty() {
            let listing = self.db.catalog.children(path.clone());
            cx.spawn(async move |this, cx| {
                let Ok(children) = listing.await else {
                    return;
                };
                this.update(cx, |this, cx| {
                    if this.path == path {
                        this.names.borrow_mut().collections = children
                            .into_iter()
                            .filter(|child| child.leaf)
                            .map(|child| child.name)
                            .collect();
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }

    /// Marks what the language cannot read once typing pauses, reading it
    /// off the UI thread. An edit before then starts over.
    fn check(&mut self, cx: &mut Context<Self>) {
        let analyze = self.language().analyze;
        let text = self.editor.read(cx).text().to_string();
        self.check_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CHECK_AFTER).await;
            let (text, error) = cx
                .background_spawn(async move {
                    let error = analyze(&text).err();
                    (text, error)
                })
                .await;
            this.update(cx, |this, cx| this.mark(&text, error, cx)).ok();
        }));
    }

    /// Shows the error the language found in `text`, if the editor still
    /// holds it.
    fn mark(&mut self, text: &str, error: Option<ParseError>, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            let rope = editor.text().clone();
            if rope != text {
                return;
            }
            let Some(diagnostics) = editor.diagnostics_mut() else {
                return;
            };
            diagnostics.reset(&rope);
            if let Some(error) = error {
                let start = error.span.start.min(text.len());
                let end = error.span.end.clamp(start, text.len());
                // An error at the very end still gets a character to mark.
                let end = match end == start {
                    true => text[start..]
                        .chars()
                        .next()
                        .map_or(start, |c| start + c.len_utf8()),
                    false => end,
                };
                diagnostics.push(
                    Diagnostic::new(
                        rope.offset_to_position(start)..rope.offset_to_position(end),
                        error.message,
                    )
                    .with_severity(DiagnosticSeverity::Error),
                );
            }
            cx.notify();
        });
    }

    fn run_query(&mut self, _: &RunQuery, _: &mut Window, cx: &mut Context<Self>) {
        let editor = self.editor.read(cx);
        let text = editor.text().to_string();
        let selected = editor.selected_range();
        let range = match selected.is_empty() {
            false => selected.start.min(text.len())..selected.end.min(text.len()),
            true => match statement_at(self.language(), &text, editor.cursor()) {
                Some(range) => range,
                None => return,
            },
        };
        let script = text[range].to_string();
        self.run(script, cx);
    }

    fn run_all(&mut self, _: &RunAll, _: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text().to_string();
        self.run(text, cx);
    }

    fn cancel(&mut self, _: &CancelQuery, _: &mut Window, cx: &mut Context<Self>) {
        self.cancel_now(cx);
    }

    /// Stops what runs: on the server, or before it gets there.
    fn cancel_now(&mut self, cx: &mut Context<Self>) {
        if self.run_task.is_some()
            && let SessionState::Ready(session) = &self.session
        {
            self.cancelled = true;
            session.cancel();
            cx.notify();
        }
    }

    /// Reads the script first: what the language cannot read is logged and
    /// nothing runs. Then each run starts in the toolbar's database, and
    /// the documents and outcomes stream in.
    fn run(&mut self, script: String, cx: &mut Context<Self>) {
        let SessionState::Ready(session) = &self.session else {
            return;
        };
        if self.run_task.is_some() || script.trim().is_empty() {
            return;
        }
        let session = session.clone();
        let statements = match (self.language().analyze)(&script) {
            Ok(statements) if statements.is_empty() => return,
            Ok(statements) => statements,
            Err(error) => {
                let message = error.located(&script);
                self.push_output(script.trim(), message.clone(), true, None);
                self.status = Status::Failed(message);
                self.panel = Panel::Output;
                cx.notify();
                return;
            }
        };
        // The toolbar's database, or the connection's own entered again: a
        // `use` from an earlier run does not last.
        let start = match self.path.is_empty() {
            true => session.home().to_vec(),
            false => self.path.clone(),
        };
        let enter = (self.language().enter)(&start);
        let heavy = self.heavy();
        let render = self.language().render;
        self.clear_grid(cx);
        self.shown = None;
        self.cancelled = false;
        self.status = Status::Running;
        self.run_task = Some(cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let entered = match enter {
                Some(enter) => {
                    drain(session.run(enter, RunMode::Read, heavy, RunOptions::default()))
                        .await
                        .map_err(describe_error)
                }
                None => Ok(()),
            };
            // A cancel that came before the statements found nothing to stop.
            let entered = entered.and_then(|()| match this.update(cx, |this, _| this.cancelled) {
                Ok(false) => Ok(()),
                _ => Err(QueryError::Cancelled.to_string()),
            });
            if let Err(error) = entered {
                this.update(cx, |this, cx| this.run_failed(error, cx)).ok();
                return;
            }
            // One past what the grid holds, to tell that there were more.
            let options = RunOptions {
                max_documents: Some(SHOWN + 1),
                ..RunOptions::default()
            };
            let events = session.run(script.clone(), RunMode::Read, heavy, options);
            let mut run = Run::new(script, statements, start);
            let mut columns: Vec<String> = Vec::new();
            let mut statement_started = Instant::now();
            while let Ok(event) = events.recv().await {
                match event {
                    DocEvent::Documents(batch) => {
                        let fresh = run.documents == 0;
                        run.documents += batch.len();
                        let room = SHOWN.saturating_sub(run.documents - batch.len());
                        let batch: Vec<Document> = batch.into_iter().take(room).collect();
                        if batch.is_empty() {
                            continue;
                        }
                        if fresh {
                            columns.clear();
                        }
                        let known = columns.clone();
                        let (batch, flattened) = cx
                            .background_spawn(async move {
                                let flattened = DocumentRows::flatten(&known, &batch, render);
                                (batch, flattened)
                            })
                            .await;
                        columns.extend(flattened.new_columns.iter().map(|info| info.name.clone()));
                        let shown = run.shown();
                        let shown = this.update(cx, |this, cx| {
                            if fresh {
                                this.clear_grid(cx);
                                this.shown = Some(shown);
                            }
                            this.show_batch(batch, flattened, cx)
                        });
                        if shown.is_err() {
                            return;
                        }
                    }
                    DocEvent::Outcome(outcome) => {
                        let (statement, text) = run.ended(outcome);
                        let elapsed = statement_started.elapsed();
                        statement_started = Instant::now();
                        this.update(cx, |this, cx| {
                            this.push_output(&statement, text, false, Some(elapsed));
                            cx.notify();
                        })
                        .ok();
                    }
                    DocEvent::Finished(summary) => {
                        this.update(cx, |this, cx| {
                            this.run_task = None;
                            this.status = Status::Done {
                                statements: summary.statements,
                                elapsed: started.elapsed(),
                            };
                            this.panel = match this.rows.borrow().document(0) {
                                Some(_) => Panel::Results,
                                None => Panel::Output,
                            };
                            this.grid.update(cx, |grid, cx| {
                                grid.delegate_mut().set_loading(false);
                                cx.notify();
                            });
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                    DocEvent::Failed(error) => {
                        let error = describe_error(error);
                        let statement = run.current();
                        this.update(cx, |this, cx| {
                            this.push_output(&statement, error.clone(), true, None);
                            this.run_failed(error, cx);
                        })
                        .ok();
                        return;
                    }
                }
            }
            this.update(cx, |this, cx| this.run_failed("The run stopped".into(), cx))
                .ok();
        }));
        cx.notify();
    }

    fn run_failed(&mut self, error: String, cx: &mut Context<Self>) {
        self.run_task = None;
        self.status = Status::Failed(error);
        self.panel = Panel::Output;
        self.grid.update(cx, |grid, cx| {
            grid.delegate_mut().set_loading(false);
            cx.notify();
        });
        cx.notify();
    }

    fn clear_grid(&mut self, cx: &mut Context<Self>) {
        self.selected = None;
        self.rows.borrow_mut().clear();
        self.grid.update(cx, |grid, cx| {
            let delegate = grid.delegate_mut();
            delegate.set_columns(Vec::new(), None);
            delegate.set_page(Default::default());
            grid.clear_selection(cx);
            grid.refresh(cx);
        });
    }

    fn show_batch(&mut self, batch: Vec<Document>, flattened: Flattened, cx: &mut Context<Self>) {
        if let Some(shown) = &self.shown {
            self.names.borrow_mut().learn(&shown.collection, &batch);
        }
        self.rows.borrow_mut().push(batch, &flattened);
        let Flattened { new_columns, batch } = flattened;
        let widened = !new_columns.is_empty();
        self.grid.update(cx, |grid, cx| {
            let delegate = grid.delegate_mut();
            if widened {
                delegate.extend_columns(new_columns);
            }
            delegate.append(batch);
            if widened {
                grid.refresh(cx);
            } else {
                cx.notify();
            }
        });
    }

    fn push_output(
        &mut self,
        statement: &str,
        text: String,
        failed: bool,
        elapsed: Option<Duration>,
    ) {
        self.output.push(OutputEntry {
            statement: statement.to_string(),
            text,
            failed,
            elapsed,
        });
    }

    /// Shows a row's document beside the grid.
    fn select_row(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let document = self.rows.borrow().document(row).cloned();
        if document.is_none() {
            return;
        }
        self.selected = Some(row);
        self.detail
            .update(cx, |detail, cx| detail.show(document, window, cx));
        cx.notify();
    }

    // Export ---------------------------------------------------------------

    /// Asks where to write every document of the shown statement (`.json`
    /// for one array, NDJSON otherwise) and reads its cursor again there.
    fn export_all(&mut self, cx: &mut Context<Self>) {
        let Some(shown) = self.shown.clone() else {
            return;
        };
        if matches!(self.export, Some(ExportState::Running { .. })) {
            return;
        }
        let suggested = format!("{}.ndjson", shown.collection);
        let chosen = cx.prompt_for_new_path(&self.project, Some(&suggested));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(mut path))) = chosen.await else {
                return;
            };
            if path.extension().is_none() {
                path.set_extension("ndjson");
            }
            this.update(cx, |this, cx| this.export_to(shown, path, cx))
                .ok();
        })
        .detach();
    }

    /// Runs the statement again through a session of its own, in the
    /// database it ran in, and writes all its documents to `path` as they
    /// stream, off the UI thread.
    fn export_to(&mut self, shown: Shown, path: PathBuf, cx: &mut Context<Self>) {
        let opening = self.db.catalog.reopen(self.db.catalog.access());
        let enter = (self.language().enter)(&shown.path);
        let heavy = self.heavy();
        self.export = Some(ExportState::Running { documents: 0 });
        cx.notify();
        self.export_task = Some(cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let failed = |error: String| format!("Could not export to {}: {error}", path.display());
            let session = match opening.await {
                Ok(session) => session,
                Err(error) => {
                    this.update(cx, |this, cx| this.export_ended(Err(failed(error)), cx))
                        .ok();
                    return;
                }
            };
            let shared = session.clone();
            if this
                .update(cx, |this, _| this.export_session = Some(shared))
                .is_err()
            {
                return;
            }
            let options = RunOptions {
                statement_timeout: EXPORT_TIME,
                ..RunOptions::default()
            };
            if let Some(enter) = enter
                && let Err(error) =
                    drain(session.run(enter, RunMode::Read, heavy, options.clone())).await
            {
                let error = failed(describe_error(error));
                this.update(cx, |this, cx| this.export_ended(Err(error), cx))
                    .ok();
                return;
            }
            let events = session.run(shown.statement, RunMode::Read, heavy, options);
            let target = path.clone();
            let opened = cx
                .background_spawn(async move { open_export(&target) })
                .await;
            let mut writer = match opened {
                Ok(writer) => writer,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.export_ended(Err(failed(error.to_string())), cx)
                    })
                    .ok();
                    return;
                }
            };
            let mut written = 0;
            let ended: Result<(), String> = loop {
                match events.recv().await {
                    Ok(DocEvent::Documents(batch)) => {
                        let (back, result) = cx
                            .background_spawn(async move {
                                let result = write_batch(&mut writer, &batch);
                                (writer, result)
                            })
                            .await;
                        writer = back;
                        if let Err(error) = result {
                            break Err(error.to_string());
                        }
                        written = writer.written;
                        this.update(cx, |this, cx| {
                            this.export = Some(ExportState::Running { documents: written });
                            cx.notify();
                        })
                        .ok();
                    }
                    Ok(DocEvent::Outcome(_)) => {}
                    Ok(DocEvent::Finished(_)) => {
                        break cx
                            .background_spawn(async move { writer.finish() })
                            .await
                            .map_err(|error| error.to_string());
                    }
                    Ok(DocEvent::Failed(error)) => break Err(describe_error(error)),
                    Err(_) => break Err("The export stopped".to_string()),
                }
            };
            let outcome = ended.map_err(failed).map(|()| {
                format!(
                    "Exported {} to {} · {}",
                    plural_documents(written as u64),
                    path.display(),
                    format_duration(started.elapsed())
                )
            });
            this.update(cx, |this, cx| this.export_ended(outcome, cx))
                .ok();
        }));
    }

    /// Stops the running export where it is: the file keeps what was
    /// written so far.
    fn stop_export(&mut self, cx: &mut Context<Self>) {
        let Some(ExportState::Running { documents }) = self.export else {
            return;
        };
        if let Some(session) = self.export_session.take() {
            session.cancel();
        }
        self.export_task = None;
        self.export = Some(ExportState::Failed(format!(
            "Stopped the export after {}: the file is incomplete",
            plural_documents(documents as u64)
        )));
        cx.notify();
    }

    fn export_ended(&mut self, outcome: Result<String, String>, cx: &mut Context<Self>) {
        self.export_session = None;
        self.export_task = None;
        self.export = Some(match outcome {
            Ok(done) => ExportState::Done(done),
            Err(error) => ExportState::Failed(error),
        });
        cx.notify();
    }

    // Render ---------------------------------------------------------------

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let ready = matches!(self.session, SessionState::Ready(_));
        let running = self.run_task.is_some();
        let weak = cx.entity().downgrade();
        let containers = self.containers.clone();
        let current = self.path.clone();
        let level = self
            .db
            .catalog
            .capabilities()
            .levels
            .first()
            .copied()
            .unwrap_or("Database");
        h_flex()
            .h(px(44.))
            .flex_shrink_0()
            .px_3()
            .gap_2()
            .bg(color(theme::TOOLBAR))
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                chrome::primary_button("doc-console-run", IconName::Play, "Run", Some("⌘↵"))
                    .when(ready && !running, |button| {
                        button.on_click(
                            cx.listener(|this, _, window, cx| {
                                this.run_query(&RunQuery, window, cx)
                            }),
                        )
                    })
                    .when(!ready || running, |button| button.opacity(0.5)),
            )
            .child(
                chrome::icon_button(
                    "doc-console-run-all",
                    AppIcon::RunAll,
                    "Run all statements  ⌥⌘↵",
                    ready && !running,
                )
                .when(ready && !running, |button| {
                    button.on_click(
                        cx.listener(|this, _, window, cx| this.run_all(&RunAll, window, cx)),
                    )
                }),
            )
            .child(
                chrome::icon_button(
                    "doc-console-cancel",
                    IconName::Square,
                    "Cancel  ⌘.",
                    running,
                )
                .when(running, |button| {
                    button.on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&CancelQuery, window, cx)),
                    )
                }),
            )
            .child(chrome::toolbar_divider())
            .when(!containers.is_empty() || !current.is_empty(), |row| {
                row.child(
                    Button::new("doc-console-database")
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
                                .child(match current.last() {
                                    Some(name) => name.clone(),
                                    None => format!("Connection's {}", level.to_lowercase()),
                                })
                                .child(
                                    Icon::new(IconName::ChevronDown)
                                        .size(px(12.))
                                        .text_color(color(theme::TEXT_TERTIARY)),
                                ),
                        )
                        .dropdown_menu(move |mut menu, _, _| {
                            for name in &containers {
                                let path = vec![name.clone()];
                                let checked = current == path;
                                menu = menu.item(
                                    item(name.clone(), &weak, move |this, _, cx| {
                                        this.set_path(path.clone(), cx)
                                    })
                                    .checked(checked),
                                );
                            }
                            menu
                        }),
                )
            })
            .child(div().flex_1())
            .child(
                chrome::pill()
                    .gap(px(5.))
                    .child(Icon::new(AppIcon::Lock).size(px(11.)))
                    .child("Read-only"),
            )
            .child(
                chrome::pill()
                    .pr(px(10.))
                    .child(chrome::dot(self.db.environment.color()))
                    .child(self.db.name.clone()),
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
            Panel::Results => h_flex()
                .size_full()
                .child(
                    div()
                        .flex_1()
                        .h_full()
                        .min_w_0()
                        .child(DataTable::new(&self.grid).stripe(true).bordered(false)),
                )
                .when(self.selected.is_some(), |row| {
                    row.child(
                        div()
                            .w(px(380.))
                            .h_full()
                            .flex_shrink_0()
                            .bg(color(theme::TOOLBAR))
                            .border_l_1()
                            .border_color(color(theme::HAIRLINE))
                            .child(self.detail.clone()),
                    )
                })
                .into_any_element(),
            Panel::Output if self.output.is_empty() => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(color(theme::TEXT_TERTIARY))
                .child("Run a statement to see what it did here")
                .into_any_element(),
            Panel::Output => div()
                .id("doc-console-output")
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
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .child(format!("> {}", entry.statement)),
                                )
                                .children(entry.elapsed.map(|elapsed| {
                                    div()
                                        .flex_shrink_0()
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
        let exporting = matches!(self.export, Some(ExportState::Running { .. }));
        let can_export = self.shown.is_some() && !exporting && self.run_task.is_none();
        let export_note = match &self.export {
            Some(ExportState::Done(done)) => Some((done.clone(), theme::TEXT_TERTIARY)),
            Some(ExportState::Failed(error)) => Some((error.clone(), theme::RED)),
            _ => None,
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
                    .child(tab("doc-console-results", "Results", Panel::Results, cx))
                    .child(tab("doc-console-output-tab", "Output", Panel::Output, cx))
                    .child(div().flex_1())
                    .children(export_note.map(|(note, tone)| {
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(color(tone))
                            .child(note)
                    }))
                    .child(
                        div()
                            .pr_2()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(color(match self.status {
                                Status::Failed(_) => theme::RED,
                                _ => theme::TEXT_TERTIARY,
                            }))
                            .child(self.status()),
                    )
                    .child(match exporting {
                        true => chrome::icon_button(
                            "doc-console-stop-export",
                            IconName::Square,
                            "Stop Export",
                            true,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.stop_export(cx))),
                        false => chrome::icon_button(
                            "doc-console-export",
                            AppIcon::Download,
                            "Export All…  every document the statement returns",
                            can_export,
                        )
                        .when(can_export, |button| {
                            button.on_click(cx.listener(|this, _, _, cx| this.export_all(cx)))
                        }),
                    }),
            )
            .child(div().flex_1().min_h_0().child(body))
    }
}

/// Drains a run that returns nothing worth showing, such as entering a
/// database.
async fn drain(events: async_channel::Receiver<DocEvent>) -> Result<(), QueryError> {
    while let Ok(event) = events.recv().await {
        match event {
            DocEvent::Finished(_) => return Ok(()),
            DocEvent::Failed(error) => return Err(error),
            DocEvent::Documents(_) | DocEvent::Outcome(_) => {}
        }
    }
    Err(QueryError::Cancelled)
}

/// A run under way: its statements, which one the next events are about,
/// and where it works.
struct Run {
    script: String,
    statements: Vec<Statement>,
    /// The container the statement that runs now works in: where the run
    /// started, then where each `use` went.
    place: Vec<String>,
    /// Statements that ended so far.
    ended: usize,
    /// Documents of the statement that runs now.
    documents: usize,
}

impl Run {
    fn new(script: String, statements: Vec<Statement>, start: Vec<String>) -> Self {
        Run {
            script,
            statements,
            place: start,
            ended: 0,
            documents: 0,
        }
    }

    /// The text of the statement that runs now.
    fn current(&self) -> String {
        self.statements
            .get(self.ended)
            .and_then(|statement| self.script.get(statement.span.clone()))
            .unwrap_or_default()
            .to_string()
    }

    /// What Export All runs for the statement that runs now.
    fn shown(&self) -> Shown {
        let statement = self.statements.get(self.ended);
        Shown {
            path: self.place.clone(),
            statement: self.current(),
            collection: statement
                .and_then(|statement| statement.info.target.as_ref())
                .and_then(|target| target.path.last().cloned())
                .unwrap_or_else(|| "documents".into()),
        }
    }

    /// The statement that ended, and what to log for it.
    fn ended(&mut self, outcome: Outcome) -> (String, String) {
        let statement = self.current();
        if let Some(entered) = self
            .statements
            .get(self.ended)
            .and_then(|statement| statement.info.enters.clone())
        {
            self.place = entered;
        }
        let text = describe_outcome(outcome, self.documents);
        self.ended += 1;
        self.documents = 0;
        (statement, text)
    }
}

/// What a statement did, in one line: the documents it returned, or the
/// counts of what it changed. Its reading stopped one past [`SHOWN`], so
/// more than that says only that there were more.
fn describe_outcome(outcome: Outcome, documents: usize) -> String {
    if documents > SHOWN {
        return format!(
            "{}+ documents · the grid shows the first {}; Export All reads them all",
            thousands(SHOWN as u64),
            thousands(SHOWN as u64)
        );
    }
    let returned = documents.max(outcome.returned as usize);
    if returned > 0 {
        return plural_documents(returned as u64);
    }
    let counts = [
        ("matched", outcome.matched),
        ("modified", outcome.modified),
        ("inserted", outcome.inserted),
        ("deleted", outcome.deleted),
        ("upserted", outcome.upserted),
    ];
    let parts: Vec<String> = counts
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(what, n)| format!("{what} {}", thousands(*n)))
        .collect();
    match parts.is_empty() {
        true => "Done".into(),
        false => parts.join(" · "),
    }
}

/// The statement the cursor is in, or the nearest one before it on its
/// line. With text the language cannot read elsewhere, the statement is
/// found in the paragraph around the cursor alone, so one bad statement
/// does not keep the others from running; only when the paragraph itself
/// cannot be read is it all of it, which then fails as a whole.
pub fn statement_at(language: &Language, text: &str, cursor: usize) -> Option<Range<usize>> {
    let cursor = cursor.min(text.len());
    if let Ok(statements) = (language.analyze)(text) {
        return statement_in(text, &statements, cursor);
    }
    let paragraph = paragraph(text, cursor)?;
    let inside = &text[paragraph.clone()];
    match (language.analyze)(inside) {
        Ok(statements) => statement_in(inside, &statements, cursor - paragraph.start)
            .map(|found| paragraph.start + found.start..paragraph.start + found.end),
        Err(_) => Some(paragraph),
    }
}

/// Of the statements of `text`, the one at `cursor` (see
/// [`statement_at`]).
fn statement_in(text: &str, statements: &[Statement], cursor: usize) -> Option<Range<usize>> {
    let line_start = text[..cursor].rfind('\n').map_or(0, |i| i + 1);
    statements
        .iter()
        .find(|statement| statement.span.start <= cursor && cursor <= statement.span.end)
        .or_else(|| {
            statements
                .iter()
                .rev()
                .find(|statement| statement.span.end <= cursor && statement.span.end >= line_start)
        })
        .or_else(|| {
            statements.iter().find(|statement| {
                statement.span.start >= cursor
                    && text[cursor..statement.span.start].trim().is_empty()
            })
        })
        .map(|statement| statement.span.clone())
}

/// The lines around `cursor` up to the blank lines before and after.
fn paragraph(text: &str, cursor: usize) -> Option<Range<usize>> {
    let blank = |line: &str| line.trim().is_empty();
    let mut start = text[..cursor].rfind('\n').map_or(0, |i| i + 1);
    let mut end = text[cursor..].find('\n').map_or(text.len(), |i| cursor + i);
    if blank(&text[start..end]) {
        return None;
    }
    while start > 0 {
        let previous = text[..start - 1].rfind('\n').map_or(0, |i| i + 1);
        if blank(&text[previous..start - 1]) {
            break;
        }
        start = previous;
    }
    while end < text.len() {
        let next = text[end + 1..]
            .find('\n')
            .map_or(text.len(), |i| end + 1 + i);
        if blank(&text[end + 1..next]) {
            break;
        }
        end = next;
    }
    Some(start..end)
}

/// A row's menu, after the copy items: its document shown, and copied as
/// the engine's literal or as Extended JSON.
fn row_menu(
    console: &WeakEntity<DocumentConsole>,
    menu: PopupMenu,
    cell: &Cell,
    cx: &App,
) -> PopupMenu {
    let Some(state) = console.upgrade() else {
        return menu;
    };
    let state = state.read(cx);
    let rows = state.rows.borrow();
    let Some(document) = rows.document(cell.row) else {
        return menu;
    };
    let literal = (state.language().render)(&Value::Object(document.clone()), Style::Pretty);
    let ejson =
        |mode| serde_json::to_string_pretty(&document_ejson(document, mode)).unwrap_or_default();
    let row = cell.row;
    menu.item(item(
        "Show Document",
        console,
        move |console, window, cx| console.select_row(row, window, cx),
    ))
    .item(copy("Copy Document", literal))
    .item(copy(
        "Copy Document as Relaxed Extended JSON",
        ejson(Ejson::Relaxed),
    ))
    .item(copy(
        "Copy Document as Canonical Extended JSON",
        ejson(Ejson::Canonical),
    ))
}

impl Focusable for DocumentConsole {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DocumentConsole {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            // Its own context: the query tab's shortcuts that mean nothing
            // here (save, format, explain, commit) stay unbound.
            .key_context("DocumentConsole")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::run_query))
            .on_action(cx.listener(Self::run_all))
            .on_action(cx.listener(Self::cancel))
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

    fn at(text: &str, cursor: usize) -> Option<&str> {
        statement_at(&kuery_mongo::LANGUAGE, text, cursor).map(|range| &text[range])
    }

    #[test]
    fn runs_the_statement_under_the_cursor() {
        let text = "use shop\ndb.orders.find({status: 'paid'})\n  .sort({_id: -1})\n\ndb.customers.find(); db.products.find()";
        let cursor = |needle: &str| text.find(needle).unwrap();
        assert_eq!(at(text, 0), Some("use shop"));
        assert_eq!(
            at(text, cursor("paid")),
            Some("db.orders.find({status: 'paid'})\n  .sort({_id: -1})")
        );
        assert_eq!(
            at(text, cursor(".sort") + 2),
            Some("db.orders.find({status: 'paid'})\n  .sort({_id: -1})")
        );
        assert_eq!(at(text, cursor("customers")), Some("db.customers.find()"));
        // After a statement on its line, before the next.
        assert_eq!(
            at(text, cursor("; db.products")),
            Some("db.customers.find()")
        );
        assert_eq!(at(text, cursor("products")), Some("db.products.find()"));
        assert_eq!(at(text, text.len()), Some("db.products.find()"));
        // On the blank line, the statement after it.
        assert_eq!(
            at(text, cursor("\n\ndb.customers") + 1),
            Some("db.customers.find()")
        );
        assert_eq!(at("", 0), None);
    }

    #[test]
    fn with_text_it_cannot_read_runs_the_paragraph() {
        let text = "db.orders.find({\n  a: 1\n})\n\nlet x = 1\n";
        assert_eq!(at(text, 3), Some("db.orders.find({\n  a: 1\n})"));
        assert_eq!(at(text, text.find("let").unwrap()), Some("let x = 1"));
        assert_eq!(at(text, text.find("\n\n").unwrap() + 1), None);
    }

    #[test]
    fn with_text_it_cannot_read_elsewhere_runs_only_the_statement_under_the_cursor() {
        let text = "db.orders.find()\ndb.orders.deleteMany({})\n\nlet x = 1";
        assert_eq!(at(text, 3), Some("db.orders.find()"));
        assert_eq!(
            at(text, text.find("delete").unwrap()),
            Some("db.orders.deleteMany({})")
        );
        // Statements that share a line, past a paragraph that fails.
        let shared = "let x = 1\n\ndb.a.find(); db.b.find()";
        assert_eq!(
            at(shared, shared.find("b.find").unwrap()),
            Some("db.b.find()")
        );
        assert_eq!(
            at(shared, shared.find("a.find").unwrap()),
            Some("db.a.find()")
        );
    }

    #[test]
    fn knows_the_database_each_statement_ran_in() {
        let script = "db.a.find()\nuse sales\ndb.b.find()\ndb.getSiblingDB('x').c.find()";
        let statements = (kuery_mongo::LANGUAGE.analyze)(script).unwrap();
        let mut run = Run::new(script.into(), statements, vec!["shop".into()]);
        let shown = run.shown();
        assert_eq!(
            (shown.path, shown.statement, shown.collection),
            (vec!["shop".into()], "db.a.find()".into(), "a".into())
        );
        run.ended(Outcome::default());
        run.ended(Outcome::default());
        let shown = run.shown();
        assert_eq!(
            (shown.path, shown.statement, shown.collection),
            (vec!["sales".into()], "db.b.find()".into(), "b".into())
        );
        run.ended(Outcome::default());
        // Naming another database for one statement leaves the run's.
        let shown = run.shown();
        assert_eq!(
            (shown.path, shown.collection),
            (vec!["sales".into()], "c".into())
        );
    }

    #[test]
    fn says_what_a_statement_did() {
        assert_eq!(describe_outcome(Outcome::default(), 3), "3 documents");
        // Its reading stopped one past what the grid holds.
        let capped = Outcome {
            returned: 1_100,
            ..Outcome::default()
        };
        assert_eq!(
            describe_outcome(capped, SHOWN + 1),
            "1,000+ documents · the grid shows the first 1,000; Export All reads them all"
        );
        assert_eq!(
            describe_outcome(Outcome::default(), SHOWN),
            "1,000 documents"
        );
        let update = Outcome {
            matched: 2,
            modified: 1,
            ..Outcome::default()
        };
        assert_eq!(describe_outcome(update, 0), "matched 2 · modified 1");
        assert_eq!(describe_outcome(Outcome::default(), 0), "Done");
    }

    /// A console on `path` of the dev MongoDB, in a window, connected
    /// without a database of its own.
    fn open_console(
        cx: &mut gpui::TestAppContext,
        environment: Environment,
        path: &[&str],
    ) -> Entity<DocumentConsole> {
        let path: Vec<String> = path.iter().map(|name| name.to_string()).collect();
        use std::sync::Arc;

        use futures::executor::block_on;
        use gpui_kit::{Bounds, Point, WindowBounds, WindowOptions, size};
        use kuery_core::document::session::DocumentOpener;

        // The sessions answer from their own threads.
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::apply(cx);
        });
        let options = kuery_core::driver::ConnectOptions {
            host: "127.0.0.1".into(),
            port: 27019,
            user: "kuery".into(),
            password: Some("kuery".into()),
            options: [("authSource".to_string(), "admin".to_string())].into(),
            ..Default::default()
        };
        let opener: DocumentOpener =
            Arc::new(move |access| (kuery_mongo::DRIVER.open)(&options, access));
        let catalog = block_on(DocumentSession::open(opener, environment.access())).unwrap();
        let db = DocDb::new(
            "local".into(),
            environment,
            catalog,
            "127.0.0.1:27019/".into(),
            Vec::new(),
            None,
        );
        let console = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(1200.), px(800.)),
                })),
                ..Default::default()
            };
            let (_, console) = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| DocumentConsole::new(db, path, std::env::temp_dir(), window, cx))
            })
            .unwrap();
            console
        });
        run_until(cx, &console, |console| {
            matches!(console.session, SessionState::Ready(_))
        });
        console
    }

    /// Runs the UI until `done` holds or a few seconds pass.
    fn run_until(
        cx: &mut gpui::TestAppContext,
        console: &Entity<DocumentConsole>,
        done: impl Fn(&DocumentConsole) -> bool,
    ) {
        for _ in 0..1000 {
            cx.run_until_parked();
            if console.read_with(cx, |console, _| done(console)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the console never got there");
    }

    fn ended(console: &DocumentConsole) -> bool {
        console.run_task.is_none() && !matches!(console.status, Status::Idle)
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn runs_a_script_in_its_database_and_shows_the_last_documents(cx: &mut gpui::TestAppContext) {
        let console = open_console(cx, Environment::Development, &["shop"]);
        console.update(cx, |console, cx| {
            console.run(
                "db.orders.find({status: 'paid'}).sort({number: 1}).limit(3)\ndb.orders.countDocuments({status: 'paid'})\nuse admin".into(),
                cx,
            )
        });
        run_until(cx, &console, ended);
        console.read_with(cx, |console, cx| {
            assert!(
                matches!(console.status, Status::Done { statements: 3, .. }),
                "{}",
                console.status()
            );
            let outputs: Vec<&str> = console
                .output
                .iter()
                .map(|entry| entry.text.as_str())
                .collect();
            assert_eq!(outputs, ["3 documents", "1 document", "Done"]);
            // The grid shows the count, the last statement with documents.
            let rows = console.rows.borrow();
            assert_eq!(
                rows.document(0)
                    .and_then(|document| document.get("count"))
                    .cloned(),
                Some(Value::Int32(125))
            );
            assert_eq!(console.grid.read(cx).delegate().page().rows(), 1);
            let shown = console.shown.as_ref().unwrap();
            assert_eq!(
                (
                    shown.statement.as_str(),
                    shown.collection.as_str(),
                    shown.path.as_slice()
                ),
                (
                    "db.orders.countDocuments({status: 'paid'})",
                    "orders",
                    ["shop".to_string()].as_slice()
                )
            );
            // What completion learned from the documents shown.
            let names = console.names.borrow();
            assert!(names.collections.iter().any(|name| name == "orders"));
            assert!(names.fields["orders"].contains("count"));
        });
        // `use admin` changed nothing for the next run: it starts in shop.
        console.update(cx, |console, cx| {
            console.run("db.orders.estimatedDocumentCount()".into(), cx)
        });
        run_until(cx, &console, ended);
        console.read_with(cx, |console, _| {
            let rows = console.rows.borrow();
            assert_eq!(
                rows.document(0)
                    .and_then(|document| document.get("count"))
                    .cloned(),
                Some(Value::Int32(500))
            );
        });
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn stops_reading_a_statement_once_the_grid_is_full(cx: &mut gpui::TestAppContext) {
        let console = open_console(cx, Environment::Development, &["shop"]);
        // A millisecond for each of big's 20 000 documents: 20 s to read
        // them all, against the 30 s limit.
        let started = Instant::now();
        console.update(cx, |console, cx| {
            console.run(r#"db.big.find({$where: "sleep(1) || true"})"#.into(), cx)
        });
        run_until(cx, &console, ended);
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "{:?}",
            started.elapsed()
        );
        console.read_with(cx, |console, cx| {
            assert!(
                matches!(console.status, Status::Done { statements: 1, .. }),
                "{}",
                console.status()
            );
            assert_eq!(
                console.output.last().map(|entry| entry.text.as_str()),
                Some(
                    "1,000+ documents · the grid shows the first 1,000; Export All reads them all"
                )
            );
            assert_eq!(console.grid.read(cx).delegate().page().rows(), SHOWN);
        });
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn starts_each_run_where_the_toolbar_says_and_exports_from_where_it_ran(
        cx: &mut gpui::TestAppContext,
    ) {
        // No database in the toolbar: the connection's own, which is
        // mongosh's `test` for a connection without one.
        let console = open_console(cx, Environment::Development, &[]);
        let run = |cx: &mut gpui::TestAppContext, script: &str| {
            let script = script.to_string();
            console.update(cx, |console, cx| console.run(script, cx));
            run_until(cx, &console, ended);
        };
        run(cx, "use shop");
        // The `use` of the run before does not last.
        run(cx, "db.orders.find({status: 'paid'})");
        console.read_with(cx, |console, _| {
            assert!(console.shown.is_none(), "{:?}", console.shown);
            assert_eq!(
                console.output.last().map(|entry| entry.text.as_str()),
                Some("Done")
            );
        });
        run(
            cx,
            "db.orders.find({status: 'paid'}).limit(1)\nuse shop\ndb.orders.find({status: 'paid'})",
        );
        let shown = console.read_with(cx, |console, _| console.shown.clone().unwrap());
        assert_eq!(
            (shown.path.as_slice(), shown.statement.as_str()),
            (
                ["shop".to_string()].as_slice(),
                "db.orders.find({status: 'paid'})"
            )
        );
        let path = std::env::temp_dir().join(format!(
            "kuery-console-export-{}.ndjson",
            std::process::id()
        ));
        console.update(cx, |console, cx| console.export_to(shown, path.clone(), cx));
        run_until(cx, &console, |console| {
            !matches!(console.export, Some(ExportState::Running { .. }))
        });
        console.read_with(cx, |console, _| {
            assert!(
                matches!(&console.export, Some(ExportState::Done(done)) if done.starts_with("Exported 125 documents")),
                "{:?}",
                console.export.as_ref().map(|export| match export {
                    ExportState::Done(text) | ExportState::Failed(text) => text.clone(),
                    ExportState::Running { .. } => "running".into(),
                })
            );
        });
        let written = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(written.lines().count(), 125);
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn marks_text_nested_too_deep_once_typing_pauses(cx: &mut gpui::TestAppContext) {
        let console = open_console(cx, Environment::Development, &["shop"]);
        let deep = format!("db.orders.find({}", "[".repeat(100_000));
        cx.update(|cx| {
            let window = cx.windows()[0];
            window
                .update(cx, |_, window, cx| {
                    console.update(cx, |console, cx| {
                        console.set_text(deep, window, cx);
                        // As an edit would.
                        console.check(cx);
                    })
                })
                .unwrap();
        });
        let marked = |console: &DocumentConsole, cx: &App| {
            console
                .editor
                .read(cx)
                .diagnostics()
                .and_then(|diagnostics| diagnostics.iter().next())
                .map(|entry| entry.diagnostic.message.to_string())
        };
        // Nothing until typing pauses.
        cx.run_until_parked();
        assert_eq!(
            console.read_with(cx, |console, cx| marked(console, cx)),
            None
        );
        cx.executor().advance_clock(CHECK_AFTER);
        for _ in 0..500 {
            cx.run_until_parked();
            if console.read_with(cx, |console, cx| marked(console, cx).is_some()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let message = console.read_with(cx, |console, cx| marked(console, cx));
        assert_eq!(
            message.as_deref(),
            Some("Values nest deeper than 200 levels here")
        );
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn refuses_writes_and_javascript_before_they_run(cx: &mut gpui::TestAppContext) {
        let console = open_console(cx, Environment::Development, &["shop"]);
        console.update(cx, |console, cx| {
            console.run(
                "db.orders.find().limit(1)\ndb.orders.deleteMany({})".into(),
                cx,
            )
        });
        run_until(cx, &console, ended);
        console.read_with(cx, |console, _| {
            let Status::Failed(error) = &console.status else {
                panic!("ran: {}", console.status());
            };
            assert!(error.contains("deleteMany"), "{error}");
            assert_eq!(console.panel, Panel::Output);
        });
        console.update(cx, |console, cx| {
            console.run("let x = db.orders.find()".into(), cx)
        });
        console.read_with(cx, |console, _| {
            let Status::Failed(error) = &console.status else {
                panic!("ran: {}", console.status());
            };
            assert!(error.contains("not JavaScript"), "{error}");
        });
        // Nothing was deleted.
        console.update(cx, |console, cx| {
            console.run("db.orders.estimatedDocumentCount()".into(), cx)
        });
        run_until(cx, &console, ended);
        console.read_with(cx, |console, _| {
            let rows = console.rows.borrow();
            assert_eq!(
                rows.document(0)
                    .and_then(|document| document.get("count"))
                    .cloned(),
                Some(Value::Int32(500))
            );
        });
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn a_cancel_before_the_statements_run_stops_the_run(cx: &mut gpui::TestAppContext) {
        let console = open_console(cx, Environment::Development, &["shop"]);
        console.update(cx, |console, cx| {
            console.run(r#"db.big.find({$where: "sleep(100) || true"})"#.into(), cx);
            console.cancel_now(cx);
        });
        run_until(cx, &console, ended);
        console.read_with(cx, |console, _| {
            assert!(
                matches!(&console.status, Status::Failed(error) if error.contains("cancelled")),
                "{}",
                console.status()
            );
        });
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn cancels_a_slow_statement(cx: &mut gpui::TestAppContext) {
        let console = open_console(cx, Environment::Development, &["shop"]);
        console.update(cx, |console, cx| {
            console.run(r#"db.big.find({$where: "sleep(100) || true"})"#.into(), cx)
        });
        // Past entering the database, into the slow statement.
        for _ in 0..50 {
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(20));
        }
        let started = Instant::now();
        console.update(cx, |console, cx| console.cancel_now(cx));
        run_until(cx, &console, ended);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        console.read_with(cx, |console, _| {
            assert!(
                matches!(&console.status, Status::Failed(error) if error.contains("cancelled")),
                "{}",
                console.status()
            );
        });
    }
}
