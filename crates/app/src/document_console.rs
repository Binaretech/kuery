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
//! The console reads unless it is switched to Write, through a session
//! that may only read: in Read-only, a statement that writes is refused
//! before it reaches the server. In Write, the console runs on a session of
//! its own that may write (on production once writes are allowed for the
//! connection), and a script that writes runs once the write review
//! approves it, with a preview of what each statement would change (see
//! [`document_write`]): in one transaction where the server has them, else
//! stopping at the first failure. One script is on its way at a time: Run
//! waits while a script is read, previewed or run, and Cancel stops each.
//! Every write, every one refused or failed, and every preview goes to the
//! audit log. Scripts are read off the UI thread. It has its own key
//! context (`DocumentConsole`), so the query tab's Save, Format and Commit
//! are not bound here to do nothing.
//!
//! Where the driver offers another language (read-only SQL), a picker
//! switches the editor to it. Statements in it never run as they are: each
//! is translated into the engine's own language, which the Translation
//! strip under the editor shows once typing pauses, and Run translates
//! again and runs exactly that native text through the same checks as one
//! typed by hand. What the translation warns of (from the sampled schema)
//! asks before it runs, and so does a full collection scan on production,
//! where Run reads the collection's indexes first when it does not know
//! them, and asks when they cannot be read. The strip follows the cursor,
//! and its actions act on the statement under it.
//! Its rows show as a table by the columns the SQL names; the Documents
//! view shows what the native statement returned, without running again.
//!
//! Explain (`secondary-e`) and Explain with Statistics (`secondary-alt-e`)
//! show the plan of the statement under the cursor in the Plan tab, the
//! query tab's plan view. Only a read is measured, by running it; a write
//! is only planned, never run, and only in Write, through the session that
//! may write. Measuring work the language marks heavy follows the same
//! rule as running it.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::highlighter::{Diagnostic, DiagnosticSeverity};
use gpui_kit::component::input::{
    CompletionProvider, Editor, EditorState, Enter, InputEvent, RopeExt as _, ShowCompletions,
};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu};
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, PromptLevel,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Task, WeakEntity, Window,
    div, px,
};
use kuery_core::document::adapter::Outcome;
use kuery_core::document::capabilities::ExplainKind;
use kuery_core::document::catalog::{CollectionInfo, ContainerKind};
use kuery_core::document::export::{Ejson, document_ejson};
use kuery_core::document::language::{Language, ParseError, Statement, Style};
use kuery_core::document::session::{DocEvent, DocumentSession, Heavy, Left, RunMode};
use kuery_core::document::value::{CollectionRef, Document, Value};
use kuery_core::driver::{OutputColumn, QueryFrontend, TranslateContext, Translation};
use kuery_core::read_only::Rejection;
use kuery_core::session::{Access, QueryError, RunOptions};

use crate::assets::AppIcon;
use crate::audit::{self, AuditAction, AuditRecord};
use crate::chrome;
use crate::collection_tab::{open_export, plural_documents, write_batch};
use crate::completion::{DocumentCompletions, SharedNames, WantFields};
use crate::connection::{DocDb, Environment};
use crate::context_menus::{Cell, copy, item};
use crate::document_detail::{DetailEvent, DocumentDetail};
use crate::document_grid::{DocumentRows, Flattened};
use crate::document_write::{self, Reviewing, Script};
use crate::grid::BrowseDelegate;
use crate::history::HistoryEntry;
use crate::plan_view::{PlanEvent, PlanLabels, PlanPanel};
use crate::query_tab::{
    CancelQuery, ExplainAnalyze, ExplainQuery, QueryRan, RunAll, RunQuery, describe_error,
};
use crate::session::ConsoleView;
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
    Plan,
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

/// Whether the console writes, and the session it writes through.
enum Writing {
    Off,
    /// Its session that may write is opening.
    Connecting,
    On(DocumentSession),
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
    /// The columns of the SQL statement it was translated from, which the
    /// Table view shows; `None` for one in the engine's own language, or a
    /// `SELECT *`.
    columns: Option<Vec<OutputColumn>>,
}

/// The Translation strip: the statement under the cursor, in a front
/// end's language, as the native statement it runs as.
#[derive(Debug, Clone, PartialEq)]
enum Strip {
    /// No statement to translate.
    Empty,
    Translated {
        sql: String,
        translation: Box<Translation>,
    },
    /// Why it has no translation, located in the editor's text.
    Refused(String),
}

/// The strip of each statement of a text, by where it is.
type Strips = Vec<(Range<usize>, Strip)>;

/// What a line of the strip is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    /// The native statement.
    Native,
    /// The sample says it may not be what was meant: Run asks first.
    Warning,
    Note,
    Error,
}

/// Opens the native statement in a console of its own (Open in mongosh),
/// starting where this one's runs start.
pub struct OpenNative {
    pub text: String,
    pub path: Vec<String>,
}

/// The statements of a run translated from a front end, for the history.
struct RunHistory {
    language: &'static str,
    /// Where the run started.
    path: Vec<String>,
    /// Each statement as written and as it ran, in order.
    statements: Vec<(String, String)>,
}

impl RunHistory {
    /// The history's entry for the statement at `index`, once it ended.
    fn entry(
        &self,
        connection: &str,
        index: usize,
        outcome: String,
        failed: bool,
    ) -> Option<HistoryEntry> {
        let (sql, native) = self.statements.get(index)?;
        Some(HistoryEntry {
            language: Some(self.language.to_string()),
            native: Some(native.clone()),
            path: Some(self.path.clone()),
            ..HistoryEntry::now(connection, sql, outcome, failed)
        })
    }
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
    writing: Writing,
    /// The session the running script runs on, so Cancel stops it there.
    run_session: Option<DocumentSession>,
    /// Reads the script Run was asked for, off the UI thread, before it
    /// runs or goes to the review.
    checking: Option<Task<()>>,
    /// The last script that went to the review; Run waits while its
    /// preview runs.
    reviewing: Option<Rc<Reviewing>>,
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
    plan: Entity<PlanPanel>,
    /// The explain on its way, and the session it runs on, for Cancel.
    plan_task: Option<Task<()>>,
    plan_session: Option<DocumentSession>,
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
    /// The language the editor holds: the engine's own, or a front end
    /// (read-only SQL) whose statements are translated before they run.
    frontend: Option<&'static QueryFrontend>,
    /// The engine's completion, set aside while the editor holds a front
    /// end's language.
    native_completion: Option<Rc<dyn CompletionProvider>>,
    /// How results show, once chosen; the language's default until then.
    view: Option<ConsoleView>,
    strip: Strip,
    /// What the console learned about the collections a front end's
    /// statements read (sampled schema, indexes, collation), for the
    /// translation's warnings and notes; and those asked for.
    contexts: HashMap<CollectionRef, TranslateContext>,
    asked_contexts: HashSet<CollectionRef>,
    /// Collections whose context could not be read: the next run asks
    /// again.
    failed_contexts: HashSet<CollectionRef>,
    /// The strip of each statement of the text last translated, so moving
    /// the cursor shows the statement under it without translating again.
    strips: Option<(String, Strips)>,
    /// Where the cursor was, to tell when it moves.
    cursor: usize,
    /// The SQL columns of each statement of the next run, in order; empty
    /// for a script in the engine's language.
    run_columns: Vec<Option<Vec<OutputColumn>>>,
    /// The statements of the run under way, for the history, when they
    /// were translated.
    run_history: Option<RunHistory>,
    /// The documents of the shown statement as they came, which the grid
    /// is filled from again when the view changes.
    raw: Vec<Document>,
    /// Counts the times the grid was filled again, so a batch read for an
    /// earlier filling is not added to a later one.
    filling: u64,
    /// The grid is being filled again: a batch that comes meanwhile only
    /// joins `raw`, and the filling shows it after the rest.
    refilling: bool,
    /// What the audit log was given, for the tests.
    #[cfg(test)]
    audited: Vec<crate::audit::AuditRecord>,
    /// What the history was given, for the tests.
    #[cfg(test)]
    recorded: Vec<HistoryEntry>,
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
        let wanting = cx.entity().downgrade();
        let want_fields: WantFields = Rc::new(move |collection, cx| {
            wanting
                .update(cx, |console, cx| console.want_fields(collection, cx))
                .ok();
        });
        let editor = cx.new(|cx| {
            let mut editor = EditorState::new(window, cx)
                .language(language.editor)
                .line_number(true)
                .placeholder(placeholder(language, None));
            editor.lsp_mut().completion_provider = Some(Rc::new(
                DocumentCompletions::new(language, names.clone()).wanting_fields(want_fields),
            ));
            editor
        });
        cx.subscribe(&editor, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.check(cx);
            }
        })
        .detach();
        // The editor reports no event when only the cursor moves.
        cx.observe(&editor, |this, _, cx| this.follow_cursor(cx))
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
        let measures = db.catalog.capabilities().explain == Some(ExplainKind::Measured);
        let plan = cx.new(|_| {
            PlanPanel::with_labels(PlanLabels::DOCUMENTS)
                .measuring(measures)
                .cancellable()
        });
        cx.subscribe_in(
            &plan,
            window,
            |this, _, event: &PlanEvent, window, cx| match event {
                PlanEvent::Explain { analyze } => this.explain(*analyze, window, cx),
                PlanEvent::Cancel => this.cancel_now(cx),
            },
        )
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

        // Write mode has a session of its own: this one only ever reads,
        // and the protocol guard checks each command it sends.
        let opening = db.catalog.reopen(Access::ReadOnly);
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
            writing: Writing::Off,
            run_session: None,
            checking: None,
            reviewing: None,
            editor,
            check_task: None,
            names,
            grid,
            rows,
            selected: None,
            detail,
            output: Vec::new(),
            plan,
            plan_task: None,
            plan_session: None,
            panel: Panel::Results,
            status: Status::Idle,
            shown: None,
            run_task: None,
            cancelled: false,
            export: None,
            export_session: None,
            export_task: None,
            focus_handle: cx.focus_handle(),
            frontend: None,
            native_completion: None,
            view: None,
            strip: Strip::Empty,
            contexts: HashMap::new(),
            asked_contexts: HashSet::new(),
            failed_contexts: HashSet::new(),
            strips: None,
            cursor: 0,
            run_columns: Vec::new(),
            run_history: None,
            raw: Vec::new(),
            filling: 0,
            refilling: false,
            #[cfg(test)]
            audited: Vec::new(),
            #[cfg(test)]
            recorded: Vec::new(),
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

    /// Replaces the statements, and reads them once typing would pause
    /// (setting the text is no edit the editor reports).
    pub fn set_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.set_value(text, window, cx));
        self.check(cx);
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
        if matches!(self.writing, Writing::Connecting) && matches!(self.status, Status::Idle) {
            return "Opening a session that may write…".into();
        }
        if self.previewing() {
            return "Previewing what the script would change…".into();
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

    /// The name of the language the editor holds, when it is a front
    /// end's (`SQL`).
    pub fn frontend_name(&self) -> Option<&'static str> {
        self.frontend.map(|frontend| frontend.name)
    }

    /// How results show: as chosen, else the language's default.
    pub fn view(&self) -> ConsoleView {
        self.view
            .unwrap_or(ConsoleView::default_for(self.frontend.is_some()))
    }

    /// The view chosen for this tab, if one was.
    pub fn chosen_view(&self) -> Option<ConsoleView> {
        self.view
    }

    /// Switches the editor to the front end named (`SQL`), or to the
    /// engine's own language for any other name.
    pub fn set_language_named(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let frontend = self
            .db
            .frontends
            .iter()
            .find(|frontend| frontend.name == name);
        self.set_frontend(frontend, window, cx);
    }

    /// Switches the editor's language: its highlighting, placeholder and
    /// completion (the engine's, which a front end goes without), and the
    /// view when none was chosen. The text stays as it is.
    fn set_frontend(
        &mut self,
        frontend: Option<&'static QueryFrontend>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.frontend_name() == frontend.map(|frontend| frontend.name) {
            return;
        }
        let before = self.view();
        let language = self.language();
        self.frontend = frontend;
        let native = &mut self.native_completion;
        self.editor.update(cx, |editor, cx| {
            editor.set_highlighter(frontend.map_or(language.editor, |f| f.editor), cx);
            editor.set_placeholder(placeholder(language, frontend), window, cx);
            let lsp = editor.lsp_mut();
            match frontend {
                Some(_) => {
                    if let Some(provider) = lsp.completion_provider.take() {
                        *native = Some(provider);
                    }
                }
                None => {
                    if let Some(provider) = native.take() {
                        lsp.completion_provider = Some(provider);
                    }
                }
            }
        });
        self.strip = Strip::Empty;
        let text = self.text(cx);
        self.mark(&text, None, cx);
        self.check(cx);
        if self.view() != before {
            self.refill(cx);
        }
        cx.notify();
    }

    /// Shows results as `view`, or as the language's default for `None`,
    /// without running anything again.
    pub fn set_view(&mut self, view: Option<ConsoleView>, cx: &mut Context<Self>) {
        let before = self.view();
        self.view = view;
        if self.view() != before {
            self.refill(cx);
        }
        cx.notify();
    }

    /// Where runs start: the toolbar's container, or the connection's own.
    fn start_path(&self) -> Vec<String> {
        match (&self.session, self.path.is_empty()) {
            (_, false) => self.path.clone(),
            (SessionState::Ready(reader), true) => reader.home().to_vec(),
            _ => self.db.home.clone(),
        }
    }

    /// Learns what a translation's warnings and notes need about a
    /// collection (its sampled schema, kept on the connection, and its
    /// indexes and collation), once, and translates again with it.
    fn want_context(&mut self, collection: CollectionRef, cx: &mut Context<Self>) {
        if !self.asked_contexts.insert(collection.clone()) {
            return;
        }
        let loading = self.load_context(collection.clone());
        cx.spawn(async move |this, cx| {
            let (context, failed) = loading.await;
            this.update(cx, |this, cx| {
                this.learned(collection, context, failed);
                this.check(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Reads what a translation needs about a collection, and whether
    /// reading any of it failed.
    fn load_context(
        &self,
        collection: CollectionRef,
    ) -> impl Future<Output = (TranslateContext, bool)> + 'static {
        let schema = self.db.schema(collection.clone(), false);
        let info = self.db.catalog.collection(collection);
        async move {
            let (schema, info) = (schema.await, info.await);
            let failed = schema.is_err() || info.is_err();
            let context = TranslateContext {
                schema: schema.ok().map(|sampled| sampled.schema.clone()),
                collection: info.ok(),
            };
            (context, failed)
        }
    }

    /// Keeps what was read about a collection, over what was known; a
    /// failure is read again by the next run, not by typing.
    fn learned(&mut self, collection: CollectionRef, context: TranslateContext, failed: bool) {
        self.asked_contexts.insert(collection.clone());
        match failed {
            true => self.failed_contexts.insert(collection.clone()),
            false => self.failed_contexts.remove(&collection),
        };
        let known = self.contexts.remove(&collection).unwrap_or_default();
        self.contexts.insert(
            collection,
            TranslateContext {
                schema: context.schema.or(known.schema),
                collection: context.collection.or(known.collection),
            },
        );
    }

    /// The SQL columns the grid shows, in the Table view of a statement
    /// translated from a front end that names them.
    fn table_columns(&self) -> Option<Vec<OutputColumn>> {
        match self.view() {
            ConsoleView::Table => self.shown.as_ref().and_then(|shown| shown.columns.clone()),
            ConsoleView::Documents => None,
        }
    }

    /// Fills the grid again from the shown statement's documents, in the
    /// view it has now, off the UI thread.
    fn refill(&mut self, cx: &mut Context<Self>) {
        self.filling += 1;
        self.clear_grid(cx);
        self.fill_from(0, cx);
    }

    /// Adds the shown statement's documents from `from` on to the grid,
    /// off the UI thread, and then those that came meanwhile.
    fn fill_from(&mut self, from: usize, cx: &mut Context<Self>) {
        let raw = self.raw[from.min(self.raw.len())..].to_vec();
        self.refilling = !raw.is_empty();
        if raw.is_empty() {
            return;
        }
        let (filling, to) = (self.filling, self.raw.len());
        let known = self.rows.borrow().columns().to_vec();
        let columns = self.table_columns();
        let render = self.language().render;
        cx.spawn(async move |this, cx| {
            let (shaped, flattened) = cx
                .background_spawn(async move { fill(&known, &raw, columns.as_deref(), render) })
                .await;
            this.update(cx, |this, cx| {
                if this.filling == filling {
                    this.show_batch(shaped, flattened, cx);
                    this.fill_from(to, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Empties the grid and the documents it is filled from, for another
    /// statement's: a filling on its way is dropped.
    fn reset_results(&mut self, cx: &mut Context<Self>) {
        self.filling += 1;
        self.refilling = false;
        self.clear_grid(cx);
        self.raw.clear();
    }

    /// Whether statements that write may run (once reviewed).
    pub fn is_writing(&self) -> bool {
        !matches!(self.writing, Writing::Off)
    }

    /// Whether the preview of a script on its way to the review runs.
    fn previewing(&self) -> bool {
        self.reviewing
            .as_ref()
            .is_some_and(|reviewing| reviewing.previewing())
    }

    /// Whether a script is on its way: read, previewed for its review, or
    /// run. Run waits for it, and Cancel stops it.
    fn busy(&self) -> bool {
        self.run_task.is_some() || self.checking.is_some() || self.previewing()
    }

    /// Whether Cancel has something to stop: a script on its way, or an
    /// explain.
    fn cancellable(&self) -> bool {
        self.busy() || self.plan_task.is_some()
    }

    /// Whether the engine measures plans, so Explain with Statistics is
    /// offered.
    fn measures(&self) -> bool {
        self.db.catalog.capabilities().explain == Some(ExplainKind::Measured)
    }

    /// Switches to Write (allowing writes for the connection session first
    /// on production) or back to Read-only. Write opens a session of its
    /// own that may write; Read-only drops it.
    fn set_writing(&mut self, writing: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !writing {
            self.writing = Writing::Off;
            cx.notify();
            return;
        }
        if self.is_writing() {
            return;
        }
        if !self.db.allows_writes() {
            let this = cx.entity().downgrade();
            crate::write_mode::allow_document_writes(
                self.db.clone(),
                window,
                cx,
                move |window, cx| {
                    let this = this.clone();
                    window.defer(cx, move |window, cx| {
                        this.update(cx, |this, cx| this.set_writing(true, window, cx))
                            .ok();
                    });
                },
            );
            return;
        }
        self.writing = Writing::Connecting;
        cx.notify();
        let opening = self.db.catalog.reopen(Access::ReadWrite);
        cx.spawn(async move |this, cx| {
            let opened = opening.await;
            this.update(cx, |this, cx| {
                if !matches!(this.writing, Writing::Connecting) {
                    return;
                }
                match opened {
                    Ok(session) => this.writing = Writing::On(session),
                    Err(error) => {
                        this.writing = Writing::Off;
                        this.status = Status::Failed(format!(
                            "Could not open a session that may write: {error}"
                        ));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Production: stops allowing writes on the connection, for every tab,
    /// and goes back to Read-only.
    pub fn stop_allowing_writes(&mut self, cx: &mut Context<Self>) {
        self.db.revoke_writes();
        self.writing = Writing::Off;
        cx.notify();
    }

    /// Work the language marks heavy runs on development connections only.
    fn heavy(&self) -> Heavy {
        match self.db.environment {
            Environment::Development => Heavy::Allow,
            Environment::Staging | Environment::Production => Heavy::Refuse,
        }
    }

    /// Reads the schema of a collection where runs start, sampled once on
    /// the connection, for completion to offer its fields.
    fn want_fields(&mut self, collection: String, cx: &mut Context<Self>) {
        let mut path = match (&self.session, self.path.is_empty()) {
            (_, false) => self.path.clone(),
            (SessionState::Ready(reader), true) => reader.home().to_vec(),
            _ => return,
        };
        path.push(collection.clone());
        let reading = self.db.schema(CollectionRef { path }, false);
        let names = self.names.clone();
        cx.spawn(async move |_, _| {
            if let Ok(sampled) = reading.await {
                names
                    .borrow_mut()
                    .learn_paths(&collection, sampled.schema.query_paths());
            }
        })
        .detach();
    }

    /// Where runs start, and the collections completion offers there.
    fn set_path(&mut self, path: Vec<String>, cx: &mut Context<Self>) {
        self.path = path.clone();
        self.names.borrow_mut().collections.clear();
        self.names.borrow_mut().asked.clear();
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
        if let Some(frontend) = self.frontend {
            self.check_frontend(frontend, cx);
            return;
        }
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

    /// Translates the front end's statements once typing pauses, off the
    /// UI thread: the first that has no translation is marked, and the one
    /// under the cursor fills the Translation strip.
    fn check_frontend(&mut self, frontend: &'static QueryFrontend, cx: &mut Context<Self>) {
        let editor = self.editor.read(cx);
        let text = editor.text().to_string();
        let cursor = editor.cursor();
        let start = self.start_path();
        let contexts = self.contexts.clone();
        self.check_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CHECK_AFTER).await;
            let (text, checked) = cx
                .background_spawn(async move {
                    let checked = check_frontend(frontend, &text, cursor, &start, &contexts);
                    (text, checked)
                })
                .await;
            this.update(cx, |this, cx| {
                if this.frontend_name() != Some(frontend.name) || this.text(cx) != text {
                    return;
                }
                this.mark(&text, checked.error, cx);
                // The statement under the cursor now: it may have moved
                // since.
                this.cursor = this.editor.read(cx).cursor();
                this.strip = strip_at(&text, &checked.strips, this.cursor);
                this.strips = Some((text, checked.strips));
                for collection in checked.read {
                    this.want_context(collection, cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Shows in the strip the statement the cursor moved into, from the
    /// last translation of the text; a translation on its way shows it
    /// when the text changed since.
    fn follow_cursor(&mut self, cx: &mut Context<Self>) {
        if self.frontend.is_none() {
            return;
        }
        let editor = self.editor.read(cx);
        let cursor = editor.cursor();
        if cursor == self.cursor {
            return;
        }
        self.cursor = cursor;
        let rope = editor.text().clone();
        let Some((text, strips)) = &self.strips else {
            return;
        };
        if rope != text.as_str() {
            return;
        }
        let strip = strip_at(text, strips, cursor);
        if strip != self.strip {
            self.strip = strip;
            cx.notify();
        }
    }

    /// The statement under the cursor translated now, which the strip
    /// then shows: what Copy, Open and the estimate act on, so they never
    /// act on a strip a moment old.
    fn translated_at_cursor(&mut self, cx: &mut Context<Self>) -> Option<(String, Translation)> {
        let frontend = self.frontend?;
        let editor = self.editor.read(cx);
        let text = editor.text().to_string();
        let cursor = editor.cursor();
        let start = self.start_path();
        let (strip, found) = match range_at(&text, &(frontend.split)(&text), cursor) {
            None => (Strip::Empty, None),
            Some(range) => {
                let sql = &text[range.clone()];
                match translate_in(frontend, sql, &start, &self.contexts) {
                    Ok((translation, _)) => (
                        Strip::Translated {
                            sql: sql.trim().to_string(),
                            translation: Box::new(translation.clone()),
                        },
                        Some((sql.trim().to_string(), translation)),
                    ),
                    Err(errors) => (
                        Strip::Refused(shifted(&errors[0], range.start).located(&text)),
                        None,
                    ),
                }
            }
        };
        self.strip = strip;
        cx.notify();
        found
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

    /// Runs the selection, or the statement under the cursor, found off the
    /// UI thread: finding it reads the whole text.
    fn run_query(&mut self, _: &RunQuery, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        if let Some(frontend) = self.frontend {
            let editor = self.editor.read(cx);
            let text = editor.text().to_string();
            let selected = editor.selected_range();
            let ranges = match selected.is_empty() {
                false => {
                    let start = selected.start.min(text.len());
                    let end = selected.end.clamp(start, text.len());
                    (frontend.split)(&text[start..end])
                        .into_iter()
                        .map(|range| range.start + start..range.end + start)
                        .collect()
                }
                true => range_at(&text, &(frontend.split)(&text), editor.cursor())
                    .into_iter()
                    .collect(),
            };
            self.run_frontend(frontend, text, ranges, window, cx);
            return;
        }
        self.run_columns.clear();
        self.run_history = None;
        let editor = self.editor.read(cx);
        let text = editor.text().to_string();
        let selected = editor.selected_range();
        if !selected.is_empty() {
            let script =
                text[selected.start.min(text.len())..selected.end.min(text.len())].to_string();
            self.run(script, window, cx);
            return;
        }
        let cursor = editor.cursor();
        let language = self.language();
        self.checking = Some(cx.spawn_in(window, async move |this, cx| {
            let found = cx
                .background_spawn(async move {
                    statement_at(language, &text, cursor).map(|range| text[range].to_string())
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.checking = None;
                match found {
                    Some(script) => this.run(script, window, cx),
                    None => cx.notify(),
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn run_all(&mut self, _: &RunAll, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text().to_string();
        if let Some(frontend) = self.frontend {
            let ranges = (frontend.split)(&text);
            self.run_frontend(frontend, text, ranges, window, cx);
            return;
        }
        self.run_columns.clear();
        self.run_history = None;
        self.run(text, window, cx);
    }

    /// Translates the front end's statements at `ranges` of `text` now,
    /// on the UI thread, and runs what they translate into: the first that
    /// has none is marked and logged, and nothing runs. What the
    /// translations warn of asks first. On production, whether a statement
    /// reads the whole collection waits for the collection's indexes: read
    /// first when they are not known yet, or when reading them failed.
    fn run_frontend(
        &mut self,
        frontend: &'static QueryFrontend,
        text: String,
        ranges: Vec<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_frontend_once_read(frontend, text, ranges, false, window, cx);
    }

    /// [`Self::run_frontend`], once the indexes were `read` for it.
    fn run_frontend_once_read(
        &mut self,
        frontend: &'static QueryFrontend,
        text: String,
        ranges: Vec<Range<usize>>,
        read: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy() || !matches!(self.session, SessionState::Ready(_)) {
            return;
        }
        let start = self.start_path();
        let production = matches!(self.db.environment, Environment::Production);
        let mut translated: Vec<(String, Translation)> = Vec::new();
        let mut collections: Vec<CollectionRef> = Vec::new();
        // The statement whose translation the strip shows: the one under
        // the cursor when it runs, else the last.
        let editor = self.editor.read(cx);
        let under = range_at(&text, &(frontend.split)(&text), editor.cursor());
        let mut shown = None;
        // A statement that may read the whole collection, of one whose
        // indexes are unknown.
        let mut may_scan = false;
        for range in &ranges {
            let sql = &text[range.clone()];
            match translate_in(frontend, sql, &start, &self.contexts) {
                Ok((translation, collection)) => {
                    may_scan |=
                        production && unknown_scan(frontend, sql, &collection, &self.contexts);
                    if !collections.contains(&collection) {
                        collections.push(collection);
                    }
                    if under.as_ref() == Some(range) {
                        shown = Some(translated.len());
                    }
                    translated.push((sql.trim().to_string(), translation));
                }
                Err(errors) => {
                    let error = shifted(&errors[0], range.start);
                    let message = error.located(&text);
                    self.mark(&text, Some(error), cx);
                    self.strip = Strip::Refused(message.clone());
                    self.push_output(sql.trim(), message.clone(), true, None);
                    self.status = Status::Failed(message);
                    self.panel = Panel::Output;
                    cx.notify();
                    return;
                }
            }
        }
        if translated.is_empty() {
            return;
        }
        if production && !read && may_scan {
            let reading: Vec<_> = collections
                .into_iter()
                .filter(|collection| indexes_unknown(collection, &self.contexts))
                .map(|collection| (collection.clone(), self.load_context(collection)))
                .collect();
            self.status = Status::Running;
            self.checking = Some(cx.spawn_in(window, async move |this, cx| {
                let mut learned = Vec::new();
                for (collection, loading) in reading {
                    learned.push((collection, loading.await));
                }
                this.update_in(cx, |this, window, cx| {
                    this.checking = None;
                    this.status = Status::Idle;
                    for (collection, (context, failed)) in learned {
                        this.learned(collection, context, failed);
                    }
                    this.run_frontend_once_read(frontend, text, ranges, true, window, cx);
                })
                .ok();
            }));
            cx.notify();
            return;
        }
        if !read {
            for collection in collections {
                // What could not be read is read again, once per run.
                if self.failed_contexts.remove(&collection) {
                    self.asked_contexts.remove(&collection);
                }
                self.want_context(collection, cx);
            }
        }
        let (sql, translation) = &translated[shown.unwrap_or(translated.len() - 1)];
        self.strip = Strip::Translated {
            sql: sql.clone(),
            translation: Box::new(translation.clone()),
        };
        self.confirm_then_run(translated, may_scan, window, cx);
    }

    /// Runs translations once what they warn of is answered: the sample's
    /// warnings, and on production a read of a whole collection, or one
    /// that may be when its indexes could not be read.
    fn confirm_then_run(
        &mut self,
        translated: Vec<(String, Translation)>,
        may_scan: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let production = matches!(self.db.environment, Environment::Production);
        let Some((title, detail)) = confirmation(&translated, production, may_scan) else {
            self.run_translated(translated, window, cx);
            return;
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &title,
            Some(&detail),
            &["Run Anyway", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if matches!(answer.await, Ok(0)) {
                this.update_in(cx, |this, window, cx| {
                    this.run_translated(translated, window, cx)
                })
                .ok();
            }
        })
        .detach();
    }

    /// Runs the native statements translations stand for, as one script,
    /// keeping their SQL columns for the Table view and both texts for the
    /// history.
    fn run_translated(
        &mut self,
        translated: Vec<(String, Translation)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(frontend) = self.frontend else {
            return;
        };
        self.run_columns = translated
            .iter()
            .map(|(_, translation)| {
                (!translation.columns.is_empty()).then(|| translation.columns.clone())
            })
            .collect();
        self.run_history = Some(RunHistory {
            language: frontend.name,
            path: self.start_path(),
            statements: translated
                .iter()
                .map(|(sql, translation)| (sql.clone(), translation.native.clone()))
                .collect(),
        });
        let script = translated
            .iter()
            .map(|(_, translation)| translation.native.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        self.run(script, window, cx);
    }

    /// Runs the cheaper statement the strip's translation offers (a count
    /// from the engine's bookkeeping).
    fn run_estimate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        let Some((sql, translation)) = self.translated_at_cursor(cx) else {
            return;
        };
        let Some(estimate) = translation.estimate else {
            return;
        };
        self.run_translated(vec![(sql, *estimate)], window, cx);
    }

    /// Puts a statement the run translated in the history once it ended,
    /// with what it ran as and its own outcome.
    fn ran(&mut self, statement: usize, outcome: String, failed: bool, cx: &mut Context<Self>) {
        let Some(entry) = self
            .run_history
            .as_ref()
            .and_then(|history| history.entry(&self.db.name, statement, outcome, failed))
        else {
            return;
        };
        #[cfg(test)]
        self.recorded.push(entry.clone());
        cx.emit(QueryRan(entry));
    }

    fn cancel(&mut self, _: &CancelQuery, _: &mut Window, cx: &mut Context<Self>) {
        self.cancel_now(cx);
    }

    fn explain_query(&mut self, _: &ExplainQuery, window: &mut Window, cx: &mut Context<Self>) {
        self.explain(false, window, cx);
    }

    fn explain_analyze(&mut self, _: &ExplainAnalyze, window: &mut Window, cx: &mut Context<Self>) {
        if self.measures() {
            self.explain(true, window, cx);
        }
    }

    /// Shows the plan of the selection, or of the statement under the
    /// cursor (found off the UI thread), in the Plan tab: planned, or with
    /// `measure` measured by running it, which only a read may do. In
    /// Write it goes through the session that may write, which also plans
    /// writes; in Read-only, through the one that only reads.
    fn explain(&mut self, measure: bool, window: &mut Window, cx: &mut Context<Self>) {
        let SessionState::Ready(reader) = &self.session else {
            return;
        };
        if self.plan_task.is_some() {
            return;
        }
        let session = match &self.writing {
            Writing::On(writer) => writer.clone(),
            Writing::Off | Writing::Connecting => reader.clone(),
        };
        let writing = self.is_writing();
        let start = match self.path.is_empty() {
            true => reader.home().to_vec(),
            false => self.path.clone(),
        };
        let heavy = self.heavy();
        let editor = self.editor.read(cx);
        let text = editor.text().to_string();
        let selected = editor.selected_range();
        let cursor = editor.cursor();
        let language = self.language();
        let frontend = self.frontend;
        let contexts = self.contexts.clone();
        let translate_from = start.clone();
        self.panel = Panel::Plan;
        self.plan.update(cx, |plan, cx| plan.loading(measure, cx));
        self.plan_session = Some(session.clone());
        self.plan_task = Some(cx.spawn_in(window, async move |this, cx| {
            let statement = cx
                .background_spawn(async move {
                    let found = match (selected.is_empty(), frontend) {
                        (true, None) => statement_at(language, &text, cursor)
                            .map(|range| text[range].to_string()),
                        (true, Some(frontend)) => range_at(&text, &(frontend.split)(&text), cursor)
                            .map(|range| text[range].to_string()),
                        (false, _) => text
                            .get(selected.start.min(text.len())..selected.end.min(text.len()))
                            .map(str::to_string),
                    };
                    // A front end's statement is explained as what it runs as.
                    match (found, frontend) {
                        (Some(sql), Some(frontend)) if !sql.trim().is_empty() => Some(
                            translate_in(frontend, &sql, &translate_from, &contexts)
                                .map(|(translation, _)| translation.native)
                                .map_err(|errors| errors[0].located(&sql)),
                        ),
                        (found, _) => found.map(Ok),
                    }
                })
                .await;
            let planned = match statement {
                Some(Err(refused)) => Err(QueryError::Explain(refused)),
                Some(Ok(statement)) if !statement.trim().is_empty() => {
                    let options = RunOptions {
                        statement_timeout: Duration::from_secs(60),
                        ..RunOptions::default()
                    };
                    session
                        .explain_in(&start, statement, measure, heavy, options)
                        .await
                }
                _ => Err(QueryError::Explain(
                    "There is no statement under the cursor to explain".into(),
                )),
            };
            this.update(cx, |this, cx| {
                this.plan_task = None;
                this.plan_session = None;
                this.plan.update(cx, |plan, cx| match planned {
                    Ok(json) => plan.show(json, cx),
                    Err(error) => plan.fail(explain_error(error, writing), cx),
                });
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Stops what is on its way: reading the script, its preview, or its
    /// run (on the server, or before it gets there).
    fn cancel_now(&mut self, cx: &mut Context<Self>) {
        if self.plan_task.is_some()
            && let Some(session) = &self.plan_session
        {
            session.cancel();
        }
        if self.checking.take().is_some() {
            self.status = Status::Failed(QueryError::Cancelled.to_string());
            cx.notify();
            return;
        }
        if let Some(reviewing) = self.reviewing.as_ref().filter(|r| r.previewing()) {
            reviewing.stop();
            self.status = Status::Failed("Preview cancelled: nothing ran".into());
            cx.notify();
            return;
        }
        if self.run_task.is_some()
            && let Some(session) = &self.run_session
        {
            self.cancelled = true;
            session.cancel();
            cx.notify();
        }
    }

    /// Reads the script first, off the UI thread: what the language cannot
    /// read is logged and nothing runs, and what the run may not do is
    /// refused (and audited when it writes). A script that writes, in
    /// Write, runs once the write review approves it; anything else runs
    /// right away.
    fn run(&mut self, script: String, window: &mut Window, cx: &mut Context<Self>) {
        let SessionState::Ready(reader) = &self.session else {
            return;
        };
        if self.busy() || script.trim().is_empty() {
            return;
        }
        let reader = reader.clone();
        let writer = match &self.writing {
            Writing::On(writer) => Some(writer.clone()),
            Writing::Off | Writing::Connecting => None,
        };
        let opening = matches!(self.writing, Writing::Connecting);
        // The toolbar's database, or the connection's own entered again: a
        // `use` from an earlier run does not last.
        let start = match self.path.is_empty() {
            true => reader.home().to_vec(),
            false => self.path.clone(),
        };
        let heavy = self.heavy();
        let analyze = self.language().analyze;
        self.status = Status::Running;
        self.checking = Some(cx.spawn_in(window, async move |this, cx| {
            let checked = cx
                .background_spawn(async move {
                    let place = Place {
                        start,
                        heavy,
                        reader,
                        writer,
                        opening,
                    };
                    check_script(analyze, script, place)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.checking = None;
                this.checked(checked, window, cx);
            })
            .ok();
        }));
        cx.notify();
    }

    /// Goes on with a script once read: logs or refuses it, runs it, or
    /// hands it to the review.
    fn checked(&mut self, checked: Checked, window: &mut Window, cx: &mut Context<Self>) {
        match checked {
            Checked::Empty => {
                self.status = Status::Idle;
                cx.notify();
            }
            Checked::Unreadable { text, message } => {
                self.push_output(text.trim(), message.clone(), true, None);
                self.status = Status::Failed(message);
                self.panel = Panel::Output;
                cx.notify();
            }
            Checked::Refused {
                script,
                mode,
                error,
            } => self.refused(&script, mode, error, cx),
            Checked::Opening(script) => {
                let message = "The session that may write is still opening: nothing ran";
                self.audit(&script, format!("Refused: {message}"), None, true, cx);
                self.push_output(script.text.trim(), message.into(), true, None);
                self.status = Status::Failed(message.into());
                self.panel = Panel::Output;
                cx.notify();
            }
            Checked::Ready {
                script,
                session,
                mode: RunMode::Read,
            } => self.start(script, session, RunMode::Read, cx),
            Checked::Ready {
                script,
                session,
                mode: RunMode::Write,
            } => self.review(script, session, window, cx),
        }
    }

    /// Hands a script that writes to the review, which previews it on
    /// `session` first: Run waits for that preview, and Cancel stops it.
    fn review(
        &mut self,
        script: Script,
        session: DocumentSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity().downgrade();
        let reviewing = Rc::new(Reviewing::new(session.clone(), {
            let this = this.clone();
            move |record, cx| {
                this.update(cx, |this, cx| {
                    this.append(record, cx);
                    cx.notify();
                })
                .ok();
            }
        }));
        self.reviewing = Some(reviewing.clone());
        self.status = Status::Idle;
        let approved = {
            let (script, session) = (script.clone(), session.clone());
            move |_: &mut Window, cx: &mut App| {
                let (script, session) = (script.clone(), session.clone());
                this.update(cx, |this, cx| {
                    this.start(script, session, RunMode::Write, cx)
                })
                .ok();
            }
        };
        let (db, project) = (self.db.clone(), self.project.clone());
        document_write::approve(&db, &project, script, reviewing, window, cx, approved);
        cx.notify();
    }

    /// Logs why a script may not run; one that writes goes to the audit
    /// log too.
    fn refused(
        &mut self,
        script: &Script,
        mode: RunMode,
        error: QueryError,
        cx: &mut Context<Self>,
    ) {
        let read_only = matches!(error, QueryError::Rejected(Rejection::Statement(_)));
        let mut message = describe_error(error);
        if read_only && mode == RunMode::Read && script.writes() {
            message.push_str(". Switch the console to Write to run it");
        }
        self.push_output(script.text.trim(), message.clone(), true, None);
        if script.writes() {
            self.audit(script, format!("Refused: {message}"), None, true, cx);
        }
        self.status = Status::Failed(message);
        self.panel = Panel::Output;
        cx.notify();
    }

    /// Writes a record of a script that writes to the audit log.
    fn audit(
        &mut self,
        script: &Script,
        outcome: String,
        changed: Option<(usize, u64)>,
        failed: bool,
        cx: &mut Context<Self>,
    ) {
        let record = document_write::record(
            &self.db,
            &self.project,
            script,
            AuditAction::Run,
            outcome,
            changed,
            failed,
        );
        self.append(record, cx);
    }

    /// Adds a record to the audit log.
    fn append(&mut self, record: AuditRecord, cx: &mut Context<Self>) {
        #[cfg(test)]
        self.audited.push(record.clone());
        audit::append_in_background(record, cx);
    }

    /// Runs a checked script on `session`: each run starts in the
    /// toolbar's database, entered in the same job, and the documents and
    /// outcomes stream in.
    fn start(
        &mut self,
        script: Script,
        session: DocumentSession,
        mode: RunMode,
        cx: &mut Context<Self>,
    ) {
        if self.run_task.is_some() {
            // One script runs at a time: this one never ran.
            let message = "Another script was running: this one did not run".to_string();
            if script.writes() {
                self.audit(&script, format!("Refused: {message}"), None, true, cx);
            }
            self.push_output(script.text.trim(), message, true, None);
            self.panel = Panel::Output;
            cx.notify();
            return;
        }
        // Writes were turned off, or no longer allowed, while the review
        // was open.
        let writes =
            matches!(&self.writing, Writing::On(writer) if writer.access() == Access::ReadWrite);
        if mode == RunMode::Write && !(writes && self.db.allows_writes()) {
            let message = "Writes were turned off before the script ran: nothing ran".to_string();
            self.audit(&script, format!("Refused: {message}"), None, true, cx);
            self.push_output(script.text.trim(), message.clone(), true, None);
            self.status = Status::Failed(message);
            self.panel = Panel::Output;
            cx.notify();
            return;
        }
        let heavy = script.heavy;
        let render = self.language().render;
        let run_columns = std::mem::take(&mut self.run_columns);
        self.reset_results(cx);
        self.shown = None;
        self.cancelled = false;
        self.status = Status::Running;
        self.run_session = Some(session.clone());
        self.run_task = Some(cx.spawn(async move |this, cx| {
            let started = Instant::now();
            // A cancel that came before the run reached the session found
            // nothing to stop there.
            if !matches!(this.update(cx, |this, _| this.cancelled), Ok(false)) {
                this.update(cx, |this, cx| {
                    this.run_history = None;
                    let error = QueryError::Cancelled.to_string();
                    if script.writes() {
                        let outcome = format!("{error} · nothing ran");
                        this.audit(&script, outcome, Some((0, 0)), true, cx);
                    }
                    this.run_failed(error, cx)
                })
                .ok();
                return;
            }
            // One past what the grid holds, to tell that there were more.
            let options = RunOptions {
                max_documents: Some(SHOWN + 1),
                ..RunOptions::default()
            };
            let events = session.run_in(&script.start, script.text.clone(), mode, heavy, options);
            let mut run = Run::new(
                script.text.clone(),
                script.statements.clone(),
                script.start.clone(),
                run_columns,
            );
            let mut statement_started = Instant::now();
            // What the run said it left, if it fails.
            let mut left = None;
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
                        // The grid as it is filled now: the columns it
                        // has, and those of its view. While it is filled
                        // again, the batch only joins the documents, and
                        // that filling shows it after them.
                        let shown = run.shown();
                        let Ok(snapshot) = this.update(cx, |this, cx| {
                            if fresh {
                                this.reset_results(cx);
                                this.shown = Some(shown);
                            }
                            if this.refilling {
                                this.raw.extend(batch.iter().cloned());
                                return None;
                            }
                            Some((
                                this.rows.borrow().columns().to_vec(),
                                this.table_columns(),
                                this.filling,
                            ))
                        }) else {
                            return;
                        };
                        let Some((known, columns, filling)) = snapshot else {
                            continue;
                        };
                        let (batch, shaped, flattened) = cx
                            .background_spawn(async move {
                                let (shaped, flattened) =
                                    fill(&known, &batch, columns.as_deref(), render);
                                (batch, shaped, flattened)
                            })
                            .await;
                        let shown = this.update(cx, |this, cx| {
                            let from = this.raw.len();
                            this.raw.extend(batch);
                            if this.filling == filling {
                                this.show_batch(shaped, flattened, cx);
                            } else if !this.refilling {
                                // The view changed and was filled again
                                // meanwhile, without this batch.
                                this.fill_from(from, cx);
                            }
                            // Else the filling on its way shows it.
                        });
                        if shown.is_err() {
                            return;
                        }
                    }
                    DocEvent::Outcome(outcome) => {
                        let (statement, text) = run.ended(outcome);
                        let elapsed = statement_started.elapsed();
                        statement_started = Instant::now();
                        let ended = run.ended - 1;
                        this.update(cx, |this, cx| {
                            let outcome = format!("{text} · {}", format_duration(elapsed));
                            this.ran(ended, outcome, false, cx);
                            this.push_output(&statement, text, false, Some(elapsed));
                            cx.notify();
                        })
                        .ok();
                    }
                    DocEvent::Left(said) => left = Some(said),
                    DocEvent::Finished(summary) => {
                        this.update(cx, |this, cx| {
                            if script.writes() {
                                let outcome = format!(
                                    "{} · {} · {}",
                                    plural_statements(summary.statements),
                                    document_write::counts(summary.outcome)
                                        .unwrap_or_else(|| "nothing changed".into()),
                                    format_duration(started.elapsed())
                                );
                                let changed = Some((
                                    summary.statements,
                                    document_write::changed(summary.outcome),
                                ));
                                this.audit(&script, outcome, changed, false, cx);
                            }
                            this.run_task = None;
                            this.run_history = None;
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
                        let mut error = describe_error(error);
                        let mut changed = None;
                        if mode == RunMode::Write {
                            let (note, kept) = kept(left, run.ended, &run.outcome);
                            error.push_str(&note);
                            changed = kept;
                        }
                        let statement = run.current();
                        let failed = run.ended;
                        this.update(cx, |this, cx| {
                            if script.writes() {
                                this.audit(&script, error.clone(), changed, true, cx);
                            }
                            this.push_output(&statement, error.clone(), true, None);
                            this.ran(failed, error.clone(), true, cx);
                            this.run_failed(error, cx);
                        })
                        .ok();
                        return;
                    }
                }
            }
            let stopped = run.ended;
            this.update(cx, |this, cx| {
                if script.writes() {
                    this.audit(&script, "The run stopped".into(), None, true, cx);
                }
                // The statements that ended are in the history already.
                this.ran(stopped, "The run stopped".into(), true, cx);
                this.run_failed("The run stopped".into(), cx)
            })
            .ok();
        }));
        cx.notify();
    }

    fn run_failed(&mut self, error: String, cx: &mut Context<Self>) {
        self.run_task = None;
        self.run_history = None;
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

    /// Shows the selected row's document beside the grid, in the
    /// Documents view: the Table view shows rows, and opens one only when
    /// asked (Show Document).
    fn select_row(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.view() == ConsoleView::Documents {
            self.show_document(row, window, cx);
        }
    }

    /// Shows a row's document beside the grid: in the Table view, the row
    /// by its columns' names.
    fn show_document(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
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
        let opening = self.db.catalog.reopen(Access::ReadOnly);
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
            let events =
                session.run_in(&shown.path, shown.statement, RunMode::Read, heavy, options);
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
                    Ok(DocEvent::Outcome(_) | DocEvent::Left(_)) => {}
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
        let running = self.busy();
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
                    self.cancellable(),
                )
                .when(self.cancellable(), |button| {
                    button.on_click(
                        cx.listener(|this, _, window, cx| this.cancel(&CancelQuery, window, cx)),
                    )
                }),
            )
            .child({
                let can = ready && self.plan_task.is_none();
                let measures = self.measures();
                chrome::icon_button(
                    "doc-console-explain",
                    AppIcon::Plan,
                    match measures {
                        true => "Explain the statement under the cursor  ⌘E (⌥⌘E with statistics)",
                        false => "Explain the statement under the cursor  ⌘E",
                    },
                    can,
                )
                .when(can, |button| {
                    button.on_click(cx.listener(
                        move |this, event: &gpui_kit::ClickEvent, window, cx| {
                            this.explain(measures && event.modifiers().alt, window, cx)
                        },
                    ))
                })
            })
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
            .when(!self.db.frontends.is_empty(), |row| {
                let current = self.frontend_name();
                let native = chrome::segment(
                    "doc-console-language-native",
                    AppIcon::Code,
                    self.language().name,
                    current.is_none(),
                )
                .on_click(cx.listener(|this, _, window, cx| this.set_frontend(None, window, cx)));
                let frontends = self
                    .db
                    .frontends
                    .iter()
                    .enumerate()
                    .map(|(index, frontend)| {
                        chrome::segment(
                            ("doc-console-language", index),
                            AppIcon::Table,
                            frontend.name,
                            current == Some(frontend.name),
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.set_frontend(Some(frontend), window, cx)
                            },
                        ))
                    });
                row.child(chrome::segmented().child(native).children(frontends))
            })
            .child(div().flex_1())
            .child(
                chrome::segmented().children(
                    [
                        ("doc-console-read", AppIcon::Lock, "Read-only", false),
                        ("doc-console-write", AppIcon::Pencil, "Write", true),
                    ]
                    .map(|(id, icon, label, writing)| {
                        chrome::segment(id, icon, label, self.is_writing() == writing).on_click(
                            cx.listener(move |this, _, window, cx| {
                                this.set_writing(writing, window, cx)
                            }),
                        )
                    }),
                ),
            )
            .child(
                chrome::pill()
                    .pr(px(10.))
                    .child(chrome::dot(self.db.environment.color()))
                    .child(self.db.name.clone()),
            )
    }

    /// The Translation strip under the editor: the statement under the
    /// cursor as the native statement it runs as, what the translation
    /// warns of and notes, or why it has none.
    fn render_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let language = self.language().name;
        let (native, estimate) = match &self.strip {
            Strip::Translated { translation, .. } => (
                Some(translation.native.clone()),
                translation.estimate.is_some(),
            ),
            _ => (None, false),
        };
        let lines = strip_lines(&self.strip);
        let button = |id: &'static str, label: String| {
            div()
                .id(id)
                .h(px(20.))
                .px(px(7.))
                .flex()
                .items_center()
                .rounded(px(5.))
                .text_size(px(11.5))
                .text_color(color(theme::TEXT_SECONDARY))
                .hover(|s| {
                    s.bg(color(theme::ACTIVE_STRONG))
                        .text_color(color(theme::TEXT))
                })
                .child(label)
        };
        v_flex()
            .id("doc-console-translation")
            .flex_shrink_0()
            .max_h(px(140.))
            .overflow_y_scroll()
            .px_3()
            .py(px(6.))
            .gap(px(3.))
            .bg(color(theme::TOOLBAR))
            .border_t_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(color(theme::TEXT_TERTIARY))
                            .child(format!("Translation · runs as {language}")),
                    )
                    .child(div().flex_1())
                    .when(estimate, |row| {
                        row.child(
                            button(
                                "doc-console-estimate",
                                "Count from the collection's metadata".into(),
                            )
                            .on_click(
                                cx.listener(|this, _, window, cx| this.run_estimate(window, cx)),
                            ),
                        )
                    })
                    .when(native.is_some(), |row| {
                        row.child(
                            button("doc-console-copy-native", format!("Copy as {language}"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some((_, translation)) = this.translated_at_cursor(cx) {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            translation.native,
                                        ))
                                    }
                                })),
                        )
                        .child(
                            button("doc-console-open-native", format!("Open in {language}"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some((_, translation)) = this.translated_at_cursor(cx) {
                                        cx.emit(OpenNative {
                                            text: translation.native,
                                            path: this.path.clone(),
                                        })
                                    }
                                })),
                        )
                    }),
            )
            .when(lines.is_empty(), |strip| {
                strip.child(
                    div()
                        .text_size(px(12.))
                        .text_color(color(theme::TEXT_QUIET))
                        .child("The statement under the cursor shows here as what it runs as."),
                )
            })
            .children(lines.into_iter().map(|(tone, line)| {
                div()
                    .text_size(px(12.))
                    .when(tone == Tone::Native, |line| line.font_family(mono.clone()))
                    .text_color(color(match tone {
                        Tone::Native => theme::TEXT,
                        Tone::Warning => theme::AMBER,
                        Tone::Note => theme::TEXT_TERTIARY,
                        Tone::Error => theme::RED,
                    }))
                    .child(line)
            }))
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
            Panel::Plan => div()
                .size_full()
                .child(self.plan.clone())
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
        let can_export = self.shown.is_some() && !exporting && !self.busy();
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
                    .child(tab("doc-console-plan-tab", "Plan", Panel::Plan, cx))
                    .when(self.panel == Panel::Results, |row| {
                        row.child(div().w(px(6.))).child(
                            chrome::segmented().children(
                                [
                                    (
                                        "doc-console-view-table",
                                        AppIcon::Table,
                                        "Table",
                                        ConsoleView::Table,
                                    ),
                                    (
                                        "doc-console-view-documents",
                                        AppIcon::Code,
                                        "Documents",
                                        ConsoleView::Documents,
                                    ),
                                ]
                                .map(|(id, icon, label, view)| {
                                    chrome::segment(id, icon, label, self.view() == view).on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            this.set_view(Some(view), cx)
                                        }),
                                    )
                                }),
                            ),
                        )
                    })
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

/// Why a plan could not be shown, in the console's words: a write needs
/// Write to be planned at all.
fn explain_error(error: QueryError, writing: bool) -> String {
    match error {
        QueryError::WritesDisabled if !writing => {
            "This statement writes: switch to Write to see its plan. Explaining a write never \
             runs it."
                .into()
        }
        error => describe_error(error),
    }
}

/// Where a script runs, for reading it off the UI thread.
struct Place {
    /// The container it starts in.
    start: Vec<String>,
    heavy: Heavy,
    /// The console's session that reads.
    reader: DocumentSession,
    /// Its session that may write, in Write.
    writer: Option<DocumentSession>,
    /// The session that may write is still opening.
    opening: bool,
}

/// A script as the console read it, and what comes of it.
enum Checked {
    /// Only comments: nothing to run.
    Empty,
    /// The language cannot read it.
    Unreadable { text: String, message: String },
    /// It may not run as it is.
    Refused {
        script: Script,
        mode: RunMode,
        error: QueryError,
    },
    /// It writes, and the session that may write is still opening.
    Opening(Script),
    /// It may run on `session`; a write once reviewed.
    Ready {
        script: Script,
        session: DocumentSession,
        mode: RunMode,
    },
}

/// Reads a script once, picks the session it runs on, and checks it may
/// run there. Slow for a long script: off the UI thread.
fn check_script(
    analyze: fn(&str) -> Result<Vec<Statement>, ParseError>,
    text: String,
    place: Place,
) -> Checked {
    let statements = match analyze(&text) {
        Ok(statements) if statements.is_empty() => return Checked::Empty,
        Ok(statements) => statements,
        Err(error) => {
            let message = error.located(&text);
            return Checked::Unreadable { text, message };
        }
    };
    let script = Script {
        start: place.start,
        text,
        statements,
        heavy: place.heavy,
    };
    let (session, mode) = match (place.writer, script.writes()) {
        (Some(writer), true) => (writer, RunMode::Write),
        (None, true) if place.opening => return Checked::Opening(script),
        _ => (place.reader, RunMode::Read),
    };
    match session.check_statements(&script.statements, mode, script.heavy) {
        Ok(()) => Checked::Ready {
            script,
            session,
            mode,
        },
        Err(error) => Checked::Refused {
            script,
            mode,
            error,
        },
    }
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
    /// What the statements that ended changed, added up.
    outcome: Outcome,
    /// The SQL columns of each statement, when it was translated from a
    /// front end that names them.
    columns: Vec<Option<Vec<OutputColumn>>>,
}

impl Run {
    fn new(
        script: String,
        statements: Vec<Statement>,
        start: Vec<String>,
        columns: Vec<Option<Vec<OutputColumn>>>,
    ) -> Self {
        Run {
            script,
            statements,
            place: start,
            columns,
            ended: 0,
            documents: 0,
            outcome: Outcome::default(),
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
            columns: self.columns.get(self.ended).cloned().flatten(),
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
        self.outcome.add(outcome);
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
    document_write::counts(outcome).unwrap_or_else(|| "Done".into())
}

/// What a write script that failed left, as the run said: for its message,
/// and for the audit log's statements and documents changed (`None` when
/// unknown). Without a transaction, the statements that ended before the
/// failure stay, and so does what the one that failed changed before it
/// did.
fn kept(left: Option<Left>, ended: usize, outcome: &Outcome) -> (String, Option<(usize, u64)>) {
    let partial = match left {
        Some(Left::Undone) => {
            return (
                " · the transaction was undone: nothing was applied".into(),
                Some((0, 0)),
            );
        }
        Some(Left::Unknown) => {
            return (
                " · the commit's result is unknown: check the data".into(),
                None,
            );
        }
        None => {
            return (" · what it changed is unknown: check the data".into(), None);
        }
        Some(Left::Kept { partial }) => partial,
    };
    let mut notes = Vec::new();
    if ended > 0 {
        notes.push(format!(
            "the {} before it {} applied ({})",
            plural_statements(ended),
            if ended == 1 { "was" } else { "were" },
            document_write::counts(*outcome).unwrap_or_else(|| "nothing changed".into())
        ));
    }
    match partial.and_then(document_write::counts) {
        _ if partial.is_none() => {
            notes.push("the statement that failed may have changed documents: unknown".into())
        }
        Some(counts) => notes.push(format!(
            "the statement that failed applied part of it first ({counts})"
        )),
        None => {}
    }
    if notes.is_empty() {
        notes.push("nothing was applied".into());
    }
    let changed = partial.map(|partial| {
        let failed = document_write::changed(partial);
        (
            ended + usize::from(failed > 0),
            document_write::changed(*outcome) + failed,
        )
    });
    (format!(" · {}", notes.join(" · ")), changed)
}

fn plural_statements(n: usize) -> String {
    match n {
        1 => "1 statement".into(),
        n => format!("{n} statements"),
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
    let spans: Vec<Range<usize>> = statements
        .iter()
        .map(|statement| statement.span.clone())
        .collect();
    range_at(text, &spans, cursor)
}

/// Of the statements at `ranges` of `text`, the one at `cursor`: the one it
/// is in, else the nearest before it on its line, else the next one with
/// only blanks before it.
fn range_at(text: &str, ranges: &[Range<usize>], cursor: usize) -> Option<Range<usize>> {
    let cursor = cursor.min(text.len());
    let line_start = text[..cursor].rfind('\n').map_or(0, |i| i + 1);
    ranges
        .iter()
        .find(|range| range.start <= cursor && cursor <= range.end)
        .or_else(|| {
            ranges
                .iter()
                .rev()
                .find(|range| range.end <= cursor && range.end >= line_start)
        })
        .or_else(|| {
            ranges
                .iter()
                .find(|range| range.start >= cursor && text[cursor..range.start].trim().is_empty())
        })
        .cloned()
}

/// What the editor says before anything is typed, in its language.
fn placeholder(language: &Language, frontend: Option<&QueryFrontend>) -> String {
    match frontend {
        None => format!(
            "Statements in {}, one per line or ended with ;. ⌘↵ runs the one under the cursor, ⌥⌘↵ all of them.",
            language.name
        ),
        Some(frontend) => format!(
            "Read-only {}, statements ended with ;: SELECT … FROM orders WHERE …. ⌘↵ runs the one under the cursor as the {} the strip below shows.",
            frontend.name, language.name
        ),
    }
}

/// The collection a translation reads, named from where runs start: a
/// name alone is in that container.
fn full_ref(start: &[String], target: &CollectionRef) -> CollectionRef {
    match target.path.as_slice() {
        [name] => {
            let mut path = start.to_vec();
            path.push(name.clone());
            CollectionRef { path }
        }
        _ => target.clone(),
    }
}

/// A front end's statement translated with what the console learned about
/// the collection it reads, and that collection.
fn translate_in(
    frontend: &QueryFrontend,
    sql: &str,
    start: &[String],
    contexts: &HashMap<CollectionRef, TranslateContext>,
) -> Result<(Translation, CollectionRef), Vec<ParseError>> {
    let plain = (frontend.translate)(sql, &TranslateContext::default())?;
    let collection = full_ref(start, &plain.target);
    let translation = match contexts.get(&collection) {
        Some(context) => (frontend.translate)(sql, context)?,
        None => plain,
    };
    Ok((translation, collection))
}

/// An error in a statement as an error in the text it starts `at`.
fn shifted(error: &ParseError, at: usize) -> ParseError {
    ParseError {
        span: error.span.start + at..error.span.end + at,
        message: error.message.clone(),
    }
}

/// What a front end's text reads as: its first error, the strip for the
/// statement at the cursor and for each of them, and the collections it
/// reads.
struct FrontendCheck {
    error: Option<ParseError>,
    strip: Strip,
    strips: Strips,
    read: Vec<CollectionRef>,
}

fn check_frontend(
    frontend: &QueryFrontend,
    text: &str,
    cursor: usize,
    start: &[String],
    contexts: &HashMap<CollectionRef, TranslateContext>,
) -> FrontendCheck {
    let ranges = (frontend.split)(text);
    let at = range_at(text, &ranges, cursor);
    let mut checked = FrontendCheck {
        error: None,
        strip: Strip::Empty,
        strips: Vec::new(),
        read: Vec::new(),
    };
    for range in &ranges {
        let sql = &text[range.clone()];
        let result = translate_in(frontend, sql, start, contexts);
        match &result {
            Ok((_, collection)) if !checked.read.contains(collection) => {
                checked.read.push(collection.clone());
            }
            Ok(_) => {}
            Err(errors) => {
                if checked.error.is_none() {
                    checked.error = errors.first().map(|error| shifted(error, range.start));
                }
            }
        }
        let strip = match result {
            Ok((translation, _)) => Strip::Translated {
                sql: sql.trim().to_string(),
                translation: Box::new(translation),
            },
            Err(errors) => Strip::Refused(shifted(&errors[0], range.start).located(text)),
        };
        if at.as_ref() == Some(range) {
            checked.strip = strip.clone();
        }
        checked.strips.push((range.clone(), strip));
    }
    checked
}

/// Of the strips of each statement of `text`, the one of the statement at
/// `cursor`.
fn strip_at(text: &str, strips: &[(Range<usize>, Strip)], cursor: usize) -> Strip {
    let ranges: Vec<Range<usize>> = strips.iter().map(|(range, _)| range.clone()).collect();
    range_at(text, &ranges, cursor)
        .and_then(|at| strips.iter().find(|(range, _)| *range == at))
        .map_or(Strip::Empty, |(_, strip)| strip.clone())
}

/// What to ask before translations run, if anything: what the sample
/// warns of, and on production a read of a whole collection, or one that
/// `may_scan` as far as Kuery can tell without the collection's indexes.
fn confirmation(
    translated: &[(String, Translation)],
    production: bool,
    may_scan: bool,
) -> Option<(String, String)> {
    let warnings: Vec<&str> = translated
        .iter()
        .flat_map(|(_, translation)| translation.warnings.iter())
        .map(|warning| warning.message.as_str())
        .collect();
    let scans = production
        && translated
            .iter()
            .any(|(_, translation)| translation.full_scan);
    let unknown = production && may_scan && !scans;
    let mut detail = warnings.join("\n");
    if scans || unknown {
        if !detail.is_empty() {
            detail.push('\n');
        }
        detail.push_str(if scans { FULL_SCAN } else { UNKNOWN_SCAN });
        detail.push_str(" This is a production connection.");
    }
    let title = match warnings.is_empty() {
        _ if !scans && !unknown && warnings.is_empty() => return None,
        false => "The sample says this may not be what you meant",
        true if scans => "This reads the whole collection",
        true => "This may read the whole collection",
    };
    Some((title.to_string(), detail))
}

/// What the strip says of a full collection scan.
const FULL_SCAN: &str =
    "It reads the whole collection: no index serves its filter or order, and it has no LIMIT.";

/// What a run asks when the collection's indexes could not be read.
const UNKNOWN_SCAN: &str = "Kuery could not read the collection's indexes, so it cannot tell whether one serves the filter or order: without one, it reads the whole collection.";

/// Whether the console knows nothing yet of a collection's indexes.
fn indexes_unknown(
    collection: &CollectionRef,
    contexts: &HashMap<CollectionRef, TranslateContext>,
) -> bool {
    contexts
        .get(collection)
        .is_none_or(|context| context.collection.is_none())
}

/// Whether a statement may read the whole of a collection whose indexes
/// are unknown: whether it would with none.
fn unknown_scan(
    frontend: &QueryFrontend,
    sql: &str,
    collection: &CollectionRef,
    contexts: &HashMap<CollectionRef, TranslateContext>,
) -> bool {
    if !indexes_unknown(collection, contexts) {
        return false;
    }
    let none = TranslateContext {
        schema: None,
        collection: Some(CollectionInfo {
            kind: ContainerKind::Collection,
            indexes: Vec::new(),
            validator: None,
            options: Document::default(),
        }),
    };
    (frontend.translate)(sql, &none).is_ok_and(|translation| translation.full_scan)
}

/// The lines of the Translation strip.
fn strip_lines(strip: &Strip) -> Vec<(Tone, String)> {
    match strip {
        Strip::Empty => Vec::new(),
        Strip::Refused(message) => vec![(Tone::Error, message.clone())],
        Strip::Translated { translation, .. } => {
            let mut lines = vec![(Tone::Native, translation.native.clone())];
            lines.extend(
                translation
                    .warnings
                    .iter()
                    .map(|warning| (Tone::Warning, warning.message.clone())),
            );
            if translation.full_scan {
                lines.push((Tone::Note, FULL_SCAN.to_string()));
            }
            lines.extend(
                translation
                    .notes
                    .iter()
                    .map(|note| (Tone::Note, note.clone())),
            );
            lines
        }
    }
}

/// A statement's documents as the grid shows them: whole, or, for the
/// columns a SQL statement names, as rows of those columns by their names
/// in their order (a column missing from a document is absent from its
/// row).
fn fill(
    known: &[String],
    documents: &[Document],
    columns: Option<&[OutputColumn]>,
    render: fn(&Value, Style) -> String,
) -> (Vec<Document>, Flattened) {
    match columns {
        None => (
            documents.to_vec(),
            DocumentRows::flatten(known, documents, render),
        ),
        Some(columns) => {
            let labels = labels(columns);
            let rows: Vec<Document> = documents
                .iter()
                .map(|document| {
                    Document(
                        columns
                            .iter()
                            .zip(&labels)
                            .filter_map(|(column, label)| {
                                document
                                    .get(&column.field)
                                    .map(|value| (label.clone(), value.clone()))
                            })
                            .collect(),
                    )
                })
                .collect();
            let flattened = DocumentRows::flatten_ordered(known, &rows, &labels, render);
            (rows, flattened)
        }
    }
}

/// Column names a grid can tell apart: a name the SQL gives twice gets
/// its position after it (`total (2)`).
fn labels(columns: &[OutputColumn]) -> Vec<String> {
    let mut labels: Vec<String> = Vec::with_capacity(columns.len());
    for column in columns {
        let mut label = column.label.clone();
        let mut n = 1;
        while labels.contains(&label) {
            n += 1;
            label = format!("{} ({n})", column.label);
        }
        labels.push(label);
    }
    labels
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
        move |console, window, cx| console.show_document(row, window, cx),
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

impl EventEmitter<QueryRan> for DocumentConsole {}
impl EventEmitter<OpenNative> for DocumentConsole {}

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
            .on_action(cx.listener(Self::explain_query))
            .on_action(cx.listener(Self::explain_analyze))
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .h(px(EDITOR_HEIGHT))
                    .flex_shrink_0()
                    .child(Editor::new(&self.editor).h_full().bordered(false)),
            )
            .children(self.frontend.is_some().then(|| self.render_strip(cx)))
            .child(self.render_panel(cx))
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::component::WindowExt as _;

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
        let mut run = Run::new(script.into(), statements, vec!["shop".into()], Vec::new());
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
    fn says_what_a_failed_write_script_left() {
        let nothing = Some(Outcome::default());
        assert_eq!(
            kept(Some(Left::Undone), 2, &Outcome::default()),
            (
                " · the transaction was undone: nothing was applied".into(),
                Some((0, 0))
            )
        );
        // A commit without an answer is never said to be undone.
        assert_eq!(
            kept(Some(Left::Unknown), 2, &Outcome::default()),
            (
                " · the commit's result is unknown: check the data".into(),
                None
            )
        );
        let inserted = Outcome {
            inserted: 2,
            ..Outcome::default()
        };
        assert_eq!(
            kept(Some(Left::Kept { partial: nothing }), 2, &inserted),
            (
                " · the 2 statements before it were applied (inserted 2)".into(),
                Some((2, 2))
            )
        );
        // What the statement that failed changed before it did stays too.
        let modified = Outcome {
            matched: 3,
            modified: 3,
            ..Outcome::default()
        };
        assert_eq!(
            kept(
                Some(Left::Kept {
                    partial: Some(modified)
                }),
                1,
                &inserted
            ),
            (
                " · the 1 statement before it was applied (inserted 2) · the statement that \
                 failed applied part of it first (matched 3 · modified 3)"
                    .into(),
                Some((2, 5))
            )
        );
        // Unknown when it was stopped halfway: never nothing.
        assert_eq!(
            kept(Some(Left::Kept { partial: None }), 0, &Outcome::default()),
            (
                " · the statement that failed may have changed documents: unknown".into(),
                None
            )
        );
        assert_eq!(
            kept(
                Some(Left::Kept { partial: nothing }),
                0,
                &Outcome::default()
            ),
            (" · nothing was applied".into(), Some((0, 0)))
        );
        assert_eq!(
            kept(None, 0, &Outcome::default()),
            (" · what it changed is unknown: check the data".into(), None)
        );
    }

    const SQL: &QueryFrontend = &kuery_mongo::sql::FRONTEND;

    fn shop() -> Vec<String> {
        vec!["shop".into()]
    }

    #[test]
    fn translates_the_sql_under_the_cursor_into_the_strip() {
        let text = "SELECT * FROM orders;\nSELECT a FROM customers WHERE b = NULL";
        let checked = check_frontend(SQL, text, 3, &shop(), &HashMap::new());
        assert_eq!(
            checked.strip,
            Strip::Translated {
                sql: "SELECT * FROM orders".into(),
                translation: Box::new(
                    (SQL.translate)("SELECT * FROM orders", &TranslateContext::default()).unwrap()
                ),
            }
        );
        // The first statement without a translation is marked, in the
        // editor's text.
        let error = checked.error.unwrap();
        assert_eq!(&text[error.span], "NULL");
        assert_eq!(checked.read, [CollectionRef::new(&["shop", "orders"])]);
        // On the statement that has none, the strip says why, and where.
        let checked = check_frontend(SQL, text, text.len(), &shop(), &HashMap::new());
        assert_eq!(
            checked.strip,
            Strip::Refused("line 2, column 35: = NULL is never true: write IS NULL".into())
        );
        assert_eq!(
            strip_lines(&checked.strip),
            [(
                Tone::Error,
                "line 2, column 35: = NULL is never true: write IS NULL".to_string()
            )]
        );
        assert_eq!(
            check_frontend(SQL, "  ", 0, &shop(), &HashMap::new()).strip,
            Strip::Empty
        );
    }

    #[test]
    fn the_strip_shows_what_runs_and_what_it_notes() {
        let collection = CollectionRef::new(&["shop", "orders"]);
        let index = |field: &str| kuery_core::document::catalog::DocIndex {
            name: format!("{field}_1"),
            keys: Document(vec![(field.into(), Value::Int32(1))]),
            unique: false,
            sparse: false,
            partial: None,
            ttl: None,
            hidden: false,
            size: None,
            usage: None,
        };
        let context = TranslateContext {
            schema: None,
            collection: Some(kuery_core::document::catalog::CollectionInfo {
                kind: kuery_core::document::catalog::ContainerKind::Collection,
                indexes: vec![index("_id")],
                validator: None,
                options: Document(vec![(
                    "collation".into(),
                    Value::Object(Document(vec![(
                        "locale".into(),
                        Value::String("fr".into()),
                    )])),
                )]),
            }),
        };
        let contexts = HashMap::from([(collection, context)]);
        let sql = "SELECT name FROM orders WHERE total > 5";
        let (translation, _) = translate_in(SQL, sql, &shop(), &contexts).unwrap();
        let strip = Strip::Translated {
            sql: sql.into(),
            translation: Box::new(translation.clone()),
        };
        assert_eq!(
            strip_lines(&strip),
            [
                (Tone::Native, translation.native.clone()),
                (Tone::Note, FULL_SCAN.to_string()),
                (
                    Tone::Note,
                    "Compares and orders by the collection's collation (fr)".to_string()
                ),
            ]
        );
        assert!(
            translation
                .native
                .starts_with("db.orders.aggregate([{$match: ")
        );
        // A full scan asks only on production.
        let translated = vec![(sql.to_string(), translation)];
        assert_eq!(confirmation(&translated, false, false), None);
        let (title, detail) = confirmation(&translated, true, false).unwrap();
        assert_eq!(title, "This reads the whole collection");
        assert!(detail.starts_with(FULL_SCAN), "{detail}");
    }

    #[test]
    fn the_strip_follows_the_statement_under_the_cursor() {
        let text =
            "SELECT * FROM orders;\nSELECT a FROM customers WHERE b = NULL;\nSELECT b FROM items";
        let checked = check_frontend(SQL, text, 0, &shop(), &HashMap::new());
        assert_eq!(checked.strips.len(), 3);
        let native = |strip: Strip| match strip {
            Strip::Translated { translation, .. } => translation.native,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            native(strip_at(text, &checked.strips, 3)),
            "db.orders.aggregate([])"
        );
        assert!(matches!(
            strip_at(text, &checked.strips, 30),
            Strip::Refused(_)
        ));
        assert_eq!(
            native(strip_at(text, &checked.strips, text.len())),
            r#"db.items.aggregate([{$project: {_id: 0, c0: "$b"}}])"#
        );
        assert_eq!(strip_at("", &[], 0), Strip::Empty);
    }

    #[test]
    fn on_production_a_run_asks_when_the_indexes_are_unknown() {
        let orders = CollectionRef::new(&["shop", "orders"]);
        let none = HashMap::new();
        // Without the indexes, what would read everything with none asks.
        let scan = "SELECT * FROM orders WHERE total > 5";
        assert!(unknown_scan(SQL, scan, &orders, &none));
        assert!(!unknown_scan(
            SQL,
            "SELECT * FROM orders WHERE total > 5 LIMIT 5",
            &orders,
            &none
        ));
        // Indexes that failed to read are as unknown as those not read yet.
        let failed = HashMap::from([(orders.clone(), TranslateContext::default())]);
        assert!(unknown_scan(SQL, scan, &orders, &failed));
        let known = HashMap::from([(
            orders.clone(),
            TranslateContext {
                schema: None,
                collection: Some(CollectionInfo {
                    kind: ContainerKind::Collection,
                    indexes: Vec::new(),
                    validator: None,
                    options: Document::default(),
                }),
            },
        )]);
        assert!(!unknown_scan(SQL, scan, &orders, &known));
        // The translation alone says nothing of a scan it cannot see.
        let (translation, _) = translate_in(SQL, scan, &shop(), &failed).unwrap();
        assert!(!translation.full_scan);
        let translated = vec![(scan.to_string(), translation)];
        assert_eq!(confirmation(&translated, false, true), None);
        assert_eq!(confirmation(&translated, true, false), None);
        let (title, detail) = confirmation(&translated, true, true).unwrap();
        assert_eq!(title, "This may read the whole collection");
        assert!(detail.starts_with(UNKNOWN_SCAN), "{detail}");
    }

    #[test]
    fn the_history_keeps_each_statements_own_outcome() {
        let history = RunHistory {
            language: "SQL",
            path: shop(),
            statements: vec![
                ("SELECT a FROM x".into(), "db.x.aggregate([])".into()),
                ("SELECT b FROM y".into(), "db.y.aggregate([])".into()),
            ],
        };
        let first = history
            .entry("local", 0, "3 documents · 2 ms".into(), false)
            .unwrap();
        assert_eq!(first.sql, "SELECT a FROM x");
        assert_eq!(first.native.as_deref(), Some("db.x.aggregate([])"));
        assert_eq!(first.language.as_deref(), Some("SQL"));
        assert_eq!(first.path, Some(shop()));
        assert_eq!(
            (first.outcome.as_str(), first.failed),
            ("3 documents · 2 ms", false)
        );
        let second = history
            .entry("local", 1, "too many compound keys".into(), true)
            .unwrap();
        assert_eq!(
            (second.sql.as_str(), second.failed),
            ("SELECT b FROM y", true)
        );
        // A statement past those that ran is not recorded.
        assert!(history.entry("local", 2, "x".into(), true).is_none());
    }

    #[test]
    fn names_the_collection_from_where_runs_start() {
        assert_eq!(
            full_ref(&shop(), &CollectionRef::new(&["orders"])),
            CollectionRef::new(&["shop", "orders"])
        );
        assert_eq!(
            full_ref(&shop(), &CollectionRef::new(&["other", "orders"])),
            CollectionRef::new(&["other", "orders"])
        );
    }

    #[test]
    fn the_table_view_shows_the_sql_columns_in_order_by_their_names() {
        let columns = [
            OutputColumn {
                field: "c0".into(),
                label: "total".into(),
            },
            OutputColumn {
                field: "c1".into(),
                label: "COUNT(*)".into(),
            },
            OutputColumn {
                field: "c2".into(),
                label: "total".into(),
            },
        ];
        // The second document lacks c0, the first column: it stays first.
        let documents = [
            Document(vec![
                ("c1".into(), Value::Int32(2)),
                ("c2".into(), Value::Int32(3)),
            ]),
            Document(vec![
                ("c0".into(), Value::Null),
                ("c1".into(), Value::Int32(1)),
                ("c2".into(), Value::Int32(4)),
            ]),
        ];
        let render = kuery_mongo::LANGUAGE.render;
        let (rows, flattened) = fill(&[], &documents, Some(&columns), render);
        let names: Vec<&str> = flattened
            .new_columns
            .iter()
            .map(|column| column.name.as_str())
            .collect();
        assert_eq!(names, ["total", "COUNT(*)", "total (2)"]);
        // Missing stays missing; null stays null.
        assert_eq!(rows[0].get("total"), None);
        assert_eq!(rows[1].get("total"), Some(&Value::Null));
        assert_eq!(rows[0].get("total (2)"), Some(&Value::Int32(3)));
        // Without columns, the documents as they came.
        let (whole, _) = fill(&[], &documents, None, render);
        assert_eq!(whole, documents);
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
        )
        .with_frontends(kuery_mongo::DRIVER.frontends);
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

    /// Calls `f` on the console in its window.
    fn in_window<R>(
        cx: &mut gpui::TestAppContext,
        console: &Entity<DocumentConsole>,
        f: impl FnOnce(&mut DocumentConsole, &mut Window, &mut Context<DocumentConsole>) -> R,
    ) -> R {
        let window = cx.update(|cx| cx.windows()[0]);
        cx.update_window(window, |_, window, cx| {
            console.update(cx, |console, cx| f(console, window, cx))
        })
        .unwrap()
    }

    fn ended(console: &DocumentConsole) -> bool {
        !console.busy() && !matches!(console.status, Status::Idle)
    }

    /// What the audit log was given for `action`.
    fn audited(console: &DocumentConsole, action: AuditAction) -> Vec<AuditRecord> {
        console
            .audited
            .iter()
            .filter(|record| record.action == action)
            .cloned()
            .collect()
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn document_console_runs_sql_as_the_native_query_it_shows(cx: &mut gpui::TestAppContext) {
        let console = open_console(cx, Environment::Development, &["shop"]);
        in_window(cx, &console, |console, window, cx| {
            assert_eq!(console.view(), ConsoleView::Documents);
            console.set_language_named("SQL", window, cx);
            assert_eq!(console.frontend_name(), Some("SQL"));
            // SQL shows rows unless another view was chosen.
            assert_eq!(console.view(), ConsoleView::Table);
            console.set_text(
                "SELECT number, status AS state FROM orders WHERE status = 'paid' ORDER BY number LIMIT 3"
                    .into(),
                window,
                cx,
            );
        });
        // The strip shows the translation once typing pauses.
        cx.run_until_parked();
        cx.executor().advance_clock(CHECK_AFTER);
        run_until(cx, &console, |console| {
            matches!(console.strip, Strip::Translated { .. })
        });
        in_window(cx, &console, |console, window, cx| {
            console.run_all(&RunAll, window, cx)
        });
        run_until(cx, &console, ended);
        console.read_with(cx, |console, cx| {
            assert!(
                matches!(console.status, Status::Done { statements: 1, .. }),
                "{}",
                console.status()
            );
            let Strip::Translated { translation, .. } = &console.strip else {
                panic!("no translation");
            };
            // What ran is what the strip shows.
            assert_eq!(
                console.shown.as_ref().unwrap().statement,
                translation.native
            );
            assert_eq!(console.rows.borrow().columns(), ["number", "state"]);
            assert_eq!(console.grid.read(cx).delegate().page().rows(), 3);
            // The history keeps both texts.
            assert_eq!(console.recorded.len(), 1);
            assert_eq!(
                console.recorded[0].sql,
                "SELECT number, status AS state FROM orders WHERE status = 'paid' ORDER BY number LIMIT 3"
            );
            assert_eq!(console.recorded[0].native.as_ref(), Some(&translation.native));
            assert_eq!(console.recorded[0].language.as_deref(), Some("SQL"));
            // It reopens where it ran, with its own outcome.
            assert_eq!(console.recorded[0].path, Some(shop()));
            assert!(
                console.recorded[0].outcome.starts_with("3 documents · "),
                "{}",
                console.recorded[0].outcome
            );
        });
        // Show Document opens a row of the Table view too.
        in_window(cx, &console, |console, window, cx| {
            console.show_document(0, window, cx);
            assert_eq!(console.selected, Some(0));
        });
        // The Documents view shows what the native statement returned,
        // without running it again.
        console.update(cx, |console, cx| {
            console.set_view(Some(ConsoleView::Documents), cx)
        });
        run_until(cx, &console, |console| {
            console.rows.borrow().columns() == ["c0", "c1"]
        });
        // A write is refused before anything runs.
        in_window(cx, &console, |console, window, cx| {
            console.set_text("UPDATE orders SET status = 'x'".into(), window, cx);
            console.run_all(&RunAll, window, cx);
        });
        console.read_with(cx, |console, _| {
            let last = console.output.last().unwrap();
            assert!(last.failed);
            assert!(
                last.text.contains("Kuery's SQL over MongoDB only reads"),
                "{}",
                last.text
            );
            assert!(matches!(console.strip, Strip::Refused(_)));
            assert_eq!(console.recorded.len(), 1);
        });
        // Explain shows the plan of the native statement.
        in_window(cx, &console, |console, window, cx| {
            console.set_text(
                "SELECT * FROM orders WHERE status = 'paid'".into(),
                window,
                cx,
            );
            console.explain(false, window, cx);
        });
        run_until(cx, &console, |console| console.plan_task.is_none());
        console.read_with(cx, |console, _| assert_eq!(console.panel, Panel::Plan));
        // Back in mongosh, the editor's own completion and view return.
        in_window(cx, &console, |console, window, cx| {
            console.set_view(None, cx);
            console.set_language_named("mongosh", window, cx);
            assert_eq!(console.frontend_name(), None);
            assert_eq!(console.view(), ConsoleView::Documents);
            assert!(console.editor.read(cx).lsp().completion_provider.is_some());
        });
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn document_console_records_each_sql_statement_with_its_own_outcome(
        cx: &mut gpui::TestAppContext,
    ) {
        let console = open_console(cx, Environment::Development, &["shop"]);
        // More sort keys than MongoDB takes: it fails on the server.
        let keys: Vec<String> = (0..33).map(|n| format!("f{n}")).collect();
        let failing = format!("SELECT * FROM orders ORDER BY {}", keys.join(", "));
        in_window(cx, &console, |console, window, cx| {
            console.set_language_named("SQL", window, cx);
            console.set_text(
                format!(
                    "SELECT number FROM orders LIMIT 2;\n{failing};\nSELECT number FROM orders LIMIT 1"
                ),
                window,
                cx,
            );
            console.run_all(&RunAll, window, cx);
        });
        run_until(cx, &console, ended);
        console.read_with(cx, |console, _| {
            let recorded: Vec<(&str, bool)> = console
                .recorded
                .iter()
                .map(|entry| (entry.sql.as_str(), entry.failed))
                .collect();
            // The one before the failure succeeded; the one after never ran.
            assert_eq!(
                recorded,
                [
                    ("SELECT number FROM orders LIMIT 2", false),
                    (failing.as_str(), true)
                ]
            );
            assert!(
                console.recorded[0].outcome.starts_with("2 documents · "),
                "{}",
                console.recorded[0].outcome
            );
            assert!(
                console.recorded[1].outcome.contains("compound keys"),
                "{}",
                console.recorded[1].outcome
            );
        });
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn document_console_reads_the_indexes_before_a_production_run_decides(
        cx: &mut gpui::TestAppContext,
    ) {
        let console = open_console(cx, Environment::Production, &["shop"]);
        // Run before typing pauses: nothing is known of the collection.
        in_window(cx, &console, |console, window, cx| {
            console.set_language_named("SQL", window, cx);
            console.set_text(
                "SELECT number FROM orders WHERE total > 5".into(),
                window,
                cx,
            );
            assert!(console.contexts.is_empty());
            console.run_all(&RunAll, window, cx);
        });
        run_until(cx, &console, |console| !console.busy());
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        console.read_with(cx, |console, _| {
            let orders = CollectionRef::new(&["shop", "orders"]);
            assert!(console.contexts[&orders].collection.is_some());
            assert!(console.shown.is_none());
            assert!(console.recorded.is_empty());
        });
        // A filter an index serves runs without asking, once its indexes
        // are read.
        in_window(cx, &console, |console, window, cx| {
            console.set_text(
                "SELECT name FROM customers WHERE email = 'x@example.com'".into(),
                window,
                cx,
            );
            console.run_all(&RunAll, window, cx);
        });
        run_until(cx, &console, ended);
        assert!(!cx.has_pending_prompt());
        console.read_with(cx, |console, _| {
            assert!(
                matches!(console.status, Status::Done { statements: 1, .. }),
                "{}",
                console.status()
            );
        });
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn runs_a_script_in_its_database_and_shows_the_last_documents(cx: &mut gpui::TestAppContext) {
        let console = open_console(cx, Environment::Development, &["shop"]);
        // Even on development, the console reads through a session that may
        // only read; `use` and the rest run through it.
        console.read_with(cx, |console, _| {
            assert!(matches!(
                &console.session,
                SessionState::Ready(reader) if reader.access() == Access::ReadOnly
            ));
        });
        in_window(cx, &console, |console, window, cx| {
            console.run(
                "db.orders.find({status: 'paid'}).sort({number: 1}).limit(3)\ndb.orders.countDocuments({status: 'paid'})\nuse admin".into(),
                window,
                cx,
            );
            // Read off the UI thread: nothing is known of it yet, and Run
            // waits.
            assert!(console.checking.is_some() && console.busy());
            assert!(console.output.is_empty());
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
        in_window(cx, &console, |console, window, cx| {
            console.run("db.orders.estimatedDocumentCount()".into(), window, cx)
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
        in_window(cx, &console, |console, window, cx| {
            console.run(
                r#"db.big.find({$where: "sleep(1) || true"})"#.into(),
                window,
                cx,
            )
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
            in_window(cx, &console, |console, window, cx| {
                console.run(script, window, cx)
            });
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
        in_window(cx, &console, |console, window, cx| {
            console.run(
                "db.orders.find().limit(1)\ndb.orders.deleteMany({})".into(),
                window,
                cx,
            )
        });
        run_until(cx, &console, ended);
        console.read_with(cx, |console, _| {
            let Status::Failed(error) = &console.status else {
                panic!("ran: {}", console.status());
            };
            assert!(error.contains("deleteMany"), "{error}");
            assert!(error.contains("Switch the console to Write"), "{error}");
            assert_eq!(console.panel, Panel::Output);
            // A refused write is audited too.
            let [refused] = console.audited.as_slice() else {
                panic!("{:?}", console.audited);
            };
            assert!(refused.failed && refused.outcome.starts_with("Refused"));
            assert_eq!(refused.mode, "Document write");
        });
        in_window(cx, &console, |console, window, cx| {
            console.run("let x = db.orders.find()".into(), window, cx)
        });
        run_until(cx, &console, ended);
        console.read_with(cx, |console, _| {
            let Status::Failed(error) = &console.status else {
                panic!("ran: {}", console.status());
            };
            assert!(error.contains("not JavaScript"), "{error}");
        });
        // Nothing was deleted.
        in_window(cx, &console, |console, window, cx| {
            console.run("db.orders.estimatedDocumentCount()".into(), window, cx)
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
        in_window(cx, &console, |console, window, cx| {
            console.run(
                r#"db.big.find({$where: "sleep(100) || true"})"#.into(),
                window,
                cx,
            );
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
        in_window(cx, &console, |console, window, cx| {
            console.run(
                r#"db.big.find({$where: "sleep(100) || true"})"#.into(),
                window,
                cx,
            )
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

    /// A database the write tests may change, dropped when they start and
    /// again when they end, even when they fail.
    struct Scratch(&'static str);

    impl Scratch {
        fn new(name: &'static str) -> Scratch {
            let scratch = Scratch(name);
            scratch.drop_database();
            scratch
        }

        fn drop_database(&self) {
            use std::sync::Arc;

            use futures::executor::block_on;
            use kuery_core::document::session::DocumentOpener;

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
            let session = block_on(DocumentSession::open(opener, Access::ReadWrite)).unwrap();
            let dropped = session.run(
                format!("db.getSiblingDB({:?}).dropDatabase()", self.0),
                RunMode::Write,
                Heavy::Refuse,
                RunOptions::default(),
            );
            block_on(async {
                while let Ok(event) = dropped.recv().await {
                    match event {
                        DocEvent::Finished(_) => return,
                        DocEvent::Failed(error) => panic!("{error}"),
                        _ => {}
                    }
                }
            });
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            self.drop_database();
        }
    }

    fn writing(console: &DocumentConsole) -> bool {
        matches!(&console.writing, Writing::On(session) if session.access() == Access::ReadWrite)
    }

    fn count(cx: &mut gpui::TestAppContext, console: &Entity<DocumentConsole>) -> Option<Value> {
        let audited = console.read_with(cx, |console, _| console.audited.len());
        in_window(cx, console, |console, window, cx| {
            console.run("db.notes.countDocuments({})".into(), window, cx)
        });
        run_until(cx, console, |console| {
            ended(console)
                && console
                    .output
                    .last()
                    .is_some_and(|entry| entry.statement.contains("countDocuments"))
        });
        // A read is never audited.
        assert_eq!(
            console.read_with(cx, |console, _| console.audited.len()),
            audited
        );
        console.read_with(cx, |console, _| {
            console
                .rows
                .borrow()
                .document(0)
                .and_then(|document| document.get("count"))
                .cloned()
        })
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn writes_once_switched_to_write_and_audits_each_write(cx: &mut gpui::TestAppContext) {
        let _scratch = Scratch::new("kuery_console_writes");
        let console = open_console(cx, Environment::Development, &["kuery_console_writes"]);
        in_window(cx, &console, |console, window, cx| {
            console.set_writing(true, window, cx)
        });
        run_until(cx, &console, writing);
        assert!(console.read_with(cx, |console, _| console.is_writing()));
        let runs = |console: &DocumentConsole| audited(console, AuditAction::Run);
        // Development reviews only risky writes: these run once previewed.
        in_window(cx, &console, |console, window, cx| {
            console.run(
                "db.notes.insertOne({_id: 1})\ndb.notes.insertOne({_id: 2})".into(),
                window,
                cx,
            )
        });
        run_until(cx, &console, |console| {
            runs(console).len() == 1 && ended(console)
        });
        console.read_with(cx, |console, _| {
            let record = &runs(console)[0];
            assert!(!record.failed, "{record:?}");
            assert_eq!(record.mode, "Document write");
            assert_eq!(record.connection, "local (kuery_console_writes)");
            assert_eq!(record.changed, Some((2, 2)));
            assert!(record.sql.contains("insertOne({_id: 2})"));
            // Its preview is audited too.
            let previews = audited(console, AuditAction::Preview);
            assert_eq!(
                previews
                    .iter()
                    .map(|record| (record.outcome.as_str(), record.failed))
                    .collect::<Vec<_>>(),
                [("Previewed in a transaction and undone: inserted 2", false)]
            );
        });
        assert_eq!(count(cx, &console), Some(Value::Int32(2)));
        // A script that fails halfway is undone, and audited as failed.
        in_window(cx, &console, |console, window, cx| {
            console.run(
                "db.notes.insertOne({_id: 3})\ndb.notes.insertOne({_id: 1})".into(),
                window,
                cx,
            )
        });
        run_until(cx, &console, |console| {
            runs(console).len() == 2 && ended(console)
        });
        console.read_with(cx, |console, _| {
            let record = &runs(console)[1];
            assert!(record.failed, "{record:?}");
            assert!(
                record
                    .outcome
                    .contains("the transaction was undone: nothing was applied"),
                "{}",
                record.outcome
            );
            assert_eq!(record.changed, Some((0, 0)));
        });
        assert_eq!(count(cx, &console), Some(Value::Int32(2)));
        // So is one statement that fails halfway, as its preview showed.
        in_window(cx, &console, |console, window, cx| {
            console.run(
                "db.notes.insertMany([{_id: 4}, {_id: 1}])".into(),
                window,
                cx,
            )
        });
        run_until(cx, &console, |console| {
            runs(console).len() == 3 && ended(console)
        });
        console.read_with(cx, |console, _| {
            let record = &runs(console)[2];
            assert!(
                record.failed && record.outcome.contains("the transaction was undone"),
                "{record:?}"
            );
        });
        assert_eq!(count(cx, &console), Some(Value::Int32(2)));
        // A risky one waits for the review, and nothing runs meanwhile.
        in_window(cx, &console, |console, window, cx| {
            console.run("db.notes.deleteMany({})".into(), window, cx)
        });
        let reviewing = |cx: &mut gpui::TestAppContext| {
            in_window(cx, &console, |_, window, cx| window.has_active_dialog(cx))
        };
        for _ in 0..500 {
            cx.run_until_parked();
            if reviewing(cx) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(reviewing(cx), "the review never opened");
        in_window(cx, &console, |_, window, cx| window.close_dialog(cx));
        assert_eq!(console.read_with(cx, |console, _| runs(console).len()), 3);
        assert_eq!(count(cx, &console), Some(Value::Int32(2)));
        // Back to Read-only: a write is refused, and audited.
        in_window(cx, &console, |console, window, cx| {
            console.set_writing(false, window, cx)
        });
        assert!(!console.read_with(cx, |console, _| console.is_writing()));
        in_window(cx, &console, |console, window, cx| {
            console.run("db.notes.insertOne({_id: 9})".into(), window, cx)
        });
        run_until(cx, &console, |console| runs(console).len() == 4);
        console.read_with(cx, |console, _| {
            let record = &runs(console)[3];
            assert!(
                record.failed && record.outcome.starts_with("Refused"),
                "{record:?}"
            );
        });
        assert_eq!(count(cx, &console), Some(Value::Int32(2)));
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn run_waits_for_a_preview_and_cancel_stops_it(cx: &mut gpui::TestAppContext) {
        let _scratch = Scratch::new("kuery_console_preview");
        let console = open_console(cx, Environment::Development, &["kuery_console_preview"]);
        in_window(cx, &console, |console, window, cx| {
            console.set_writing(true, window, cx)
        });
        run_until(cx, &console, writing);
        let notes: Vec<String> = (0..40).map(|n| format!("{{_id: {n}}}")).collect();
        let script = format!("db.notes.insertMany([{}])", notes.join(", "));
        in_window(cx, &console, |console, window, cx| {
            console.run(script, window, cx)
        });
        run_until(cx, &console, |console| {
            audited(console, AuditAction::Run).len() == 1 && ended(console)
        });
        // A tenth of a second for each of the 40 notes: its preview takes 4 s.
        in_window(cx, &console, |console, window, cx| {
            console.run(
                r#"db.notes.updateMany({$where: "sleep(100) || true"}, {$set: {x: 1}})"#.into(),
                window,
                cx,
            )
        });
        run_until(cx, &console, |console| console.previewing());
        // Run waits: a second script neither runs nor goes to the review.
        let reviewing = console.read_with(cx, |console, _| console.reviewing.clone().unwrap());
        in_window(cx, &console, |console, window, cx| {
            assert!(console.busy());
            console.run("db.notes.deleteMany({})".into(), window, cx);
            assert!(console.checking.is_none());
        });
        let started = Instant::now();
        console.update(cx, |console, cx| console.cancel_now(cx));
        run_until(cx, &console, |console| {
            audited(console, AuditAction::Preview).len() == 2
        });
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        console.read_with(cx, |console, _| {
            assert!(Rc::ptr_eq(console.reviewing.as_ref().unwrap(), &reviewing));
            assert!(!console.busy());
            let stopped = &audited(console, AuditAction::Preview)[1];
            assert!(
                stopped.failed && stopped.outcome.starts_with("Preview failed"),
                "{stopped:?}"
            );
            assert_eq!(audited(console, AuditAction::Run).len(), 1);
        });
        // Neither the review nor the run follows a stopped preview.
        for _ in 0..20 {
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!in_window(cx, &console, |_, window, cx| window.has_active_dialog(cx)));
        assert_eq!(
            console.read_with(cx, |console, _| audited(console, AuditAction::Run).len()),
            1
        );
        assert_eq!(count(cx, &console), Some(Value::Int32(40)));
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn audits_the_writes_that_never_ran(cx: &mut gpui::TestAppContext) {
        let console = open_console(cx, Environment::Development, &["kuery_console_never"]);
        // The session that may write is still opening.
        console.update(cx, |console, _| console.writing = Writing::Connecting);
        in_window(cx, &console, |console, window, cx| {
            console.run("db.notes.insertOne({_id: 1})".into(), window, cx)
        });
        run_until(cx, &console, |console| console.audited.len() == 1);
        console.read_with(cx, |console, _| {
            let record = &console.audited[0];
            assert!(
                record.failed
                    && record.outcome
                        == "Refused: The session that may write is still opening: nothing ran",
                "{record:?}"
            );
        });
        // An approval that comes while another script runs is dropped, and
        // audited.
        let reader = console.read_with(cx, |console, _| match &console.session {
            SessionState::Ready(reader) => reader.clone(),
            _ => unreachable!(),
        });
        let text = "db.notes.insertOne({_id: 2})";
        let script = Script {
            start: vec!["kuery_console_never".into()],
            text: text.into(),
            statements: (kuery_mongo::LANGUAGE.analyze)(text).unwrap(),
            heavy: Heavy::Refuse,
        };
        console.update(cx, |console, cx| {
            console.run_task = Some(Task::ready(()));
            console.start(script, reader, RunMode::Write, cx);
            console.run_task = None;
        });
        console.read_with(cx, |console, _| {
            let record = &console.audited[1];
            assert!(
                record.failed
                    && record.outcome
                        == "Refused: Another script was running: this one did not run",
                "{record:?}"
            );
        });
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn on_production_writes_only_once_allowed(cx: &mut gpui::TestAppContext) {
        let _scratch = Scratch::new("kuery_console_production");
        let console = open_console(cx, Environment::Production, &["kuery_console_production"]);
        // Writes are not allowed yet: Write asks to type the name first.
        in_window(cx, &console, |console, window, cx| {
            console.set_writing(true, window, cx)
        });
        cx.run_until_parked();
        assert!(!console.read_with(cx, |console, _| console.is_writing()));
        assert!(in_window(cx, &console, |_, window, cx| window.has_active_dialog(cx)));
        in_window(cx, &console, |_, window, cx| window.close_dialog(cx));
        // As typing the name would.
        console.read_with(cx, |console, _| console.db.allow_writes());
        in_window(cx, &console, |console, window, cx| {
            console.set_writing(true, window, cx)
        });
        run_until(cx, &console, writing);
        // Heavy work is refused on production, before any review.
        in_window(cx, &console, |console, window, cx| {
            console.run(
                r#"db.notes.updateMany({$where: "true"}, {$set: {a: 1}})"#.into(),
                window,
                cx,
            )
        });
        run_until(cx, &console, |console| !console.audited.is_empty());
        console.read_with(cx, |console, _| {
            let [record] = console.audited.as_slice() else {
                panic!("{:?}", console.audited);
            };
            assert!(
                record.failed && record.outcome.starts_with("Refused"),
                "{record:?}"
            );
            assert_eq!(record.environment, "PRODUCTION");
        });
        assert!(!in_window(cx, &console, |_, window, cx| window.has_active_dialog(cx)));
        // Stop allowing writes: back to Read-only, for the whole connection.
        console.update(cx, |console, cx| console.stop_allowing_writes(cx));
        console.read_with(cx, |console, _| {
            assert!(!console.is_writing());
            assert!(!console.db.allows_writes());
        });
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn document_console_explains_reads_and_only_plans_writes(cx: &mut gpui::TestAppContext) {
        let console = open_console(cx, Environment::Development, &["shop"]);
        let explain = |cx: &mut gpui::TestAppContext, text: &str, measure: bool| {
            let text = text.to_string();
            in_window(cx, &console, move |console, window, cx| {
                console.set_text(text, window, cx);
                console.explain(measure, window, cx);
            });
            run_until(cx, &console, |console| console.plan_task.is_none());
        };

        // A read, measured by running it, in the Plan tab.
        explain(cx, "db.big.find({n: {$lt: 5}}).sort({label: 1})", true);
        console.read_with(cx, |console, cx| {
            assert_eq!(console.panel, Panel::Plan);
            let plan = console.plan.read(cx);
            let plan = plan
                .plan()
                .unwrap_or_else(|| panic!("{:?}", plan.failure()));
            assert!(plan.analyzed);
            assert_eq!(plan.root.actual_rows(), Some(1000.0));
            assert_eq!(plan.root.children[0].title, "Read all of big");
        });

        // A write, in Read-only: not even planned.
        explain(cx, "db.orders.deleteMany({})", false);
        console.read_with(cx, |console, cx| {
            let failure = console
                .plan
                .read(cx)
                .failure()
                .unwrap_or_default()
                .to_string();
            assert!(failure.contains("switch to Write"), "{failure}");
        });

        // In Write it is planned, and never run, measured or not.
        in_window(cx, &console, |console, window, cx| {
            console.set_writing(true, window, cx)
        });
        run_until(cx, &console, |console| {
            matches!(console.writing, Writing::On(_))
        });
        explain(cx, "db.orders.deleteMany({})", true);
        console.read_with(cx, |console, cx| {
            let failure = console
                .plan
                .read(cx)
                .failure()
                .unwrap_or_default()
                .to_string();
            assert!(failure.contains("deleteMany orders writes"), "{failure}");
        });
        explain(cx, "db.orders.deleteMany({})", false);
        console.read_with(cx, |console, cx| {
            let plan = console.plan.read(cx).plan().expect("the write's plan");
            assert!(!plan.analyzed);
            assert_eq!(plan.root.title, "Delete documents of orders");
        });
        in_window(cx, &console, |console, window, cx| {
            console.set_writing(false, window, cx);
            console.run("db.orders.estimatedDocumentCount()".into(), window, cx)
        });
        run_until(cx, &console, ended);
        console.read_with(cx, |console, _| {
            let rows = console.rows.borrow();
            assert_eq!(
                rows.document(0).and_then(|d| d.get("count")).cloned(),
                Some(Value::Int32(500)),
                "nothing was deleted"
            );
        });
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn document_console_cancels_an_explain_on_its_way(cx: &mut gpui::TestAppContext) {
        let console = open_console(cx, Environment::Development, &["shop"]);
        // MongoDB measures plans, so Explain with Statistics is offered.
        console.read_with(cx, |console, _| assert!(console.measures()));
        // A read of 500 orders at 100 ms each, measured.
        in_window(cx, &console, |console, window, cx| {
            console.set_text(
                "db.orders.find({$where: 'sleep(100); return true'})".into(),
                window,
                cx,
            );
            console.explain(true, window, cx);
        });
        console.read_with(cx, |console, _| {
            assert!(!console.busy());
            assert!(console.cancellable(), "Cancel stops an explain too");
        });
        for _ in 0..50 {
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        // As the Plan tab's Cancel does.
        let plan = console.read_with(cx, |console, _| console.plan.clone());
        plan.update(cx, |_, cx| cx.emit(PlanEvent::Cancel));
        run_until(cx, &console, |console| console.plan_task.is_none());
        console.read_with(cx, |console, cx| {
            let failure = console.plan.read(cx).failure().unwrap_or_default();
            assert!(failure.to_lowercase().contains("cancel"), "{failure}");
            assert!(!console.cancellable());
        });
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn document_console_completes_the_fields_of_a_sampled_schema(cx: &mut gpui::TestAppContext) {
        let console = open_console(cx, Environment::Development, &["shop"]);
        console.update(cx, |console, cx| console.want_fields("orders".into(), cx));
        run_until(cx, &console, |console| {
            console.names.borrow().fields.contains_key("orders")
        });
        console.read_with(cx, |console, _| {
            let names = console.names.borrow();
            let orders = &names.fields["orders"];
            // Four levels deep, arrays named as filters name them.
            assert!(orders.contains("items.sku"), "{orders:?}");
            assert!(orders.contains("coupon"), "{orders:?}");
            assert!(!orders.iter().any(|path| path.contains("[]")), "{orders:?}");
            // Kept for the connection: the collection tab finds it there.
            let cached = console
                .db
                .cached_schema(&CollectionRef::new(&["shop", "orders"]));
            assert!(cached.is_some());
        });
    }
}
