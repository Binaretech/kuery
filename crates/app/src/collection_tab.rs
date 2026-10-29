//! A collection's tab: its documents a page at a time, as a grid whose
//! columns are the documents' top-level fields, with a filter bar like
//! Compass's (filter, projection, sort and limit, in the engine's own
//! language). The tab opens a session of its own, so a long page never
//! holds up the sidebar.
//!
//! Documents of one collection need not share their fields: a field first
//! seen in a later batch adds a column without starting the grid over, and
//! a field a document lacks shows as a dimmed dash, not as NULL. Without a
//! sort, pages follow the documents' keys; with one, they skip what came
//! before, which the footer says can repeat or miss documents that move.
//! The count is capped and timed, so a big collection never waits on it.
//! Views open read-only, with their pipeline under the bar.
//!
//! The selected row's document shows beside the grid, and the row menu
//! copies it as the engine's literal or as Extended JSON, from the document
//! itself. Export Documents writes every document the bar matches, read
//! through the cursor as it streams, not just the loaded page.
//!
//! Documents are edited in stages, as rows of a table are: Edit opens the
//! document's typed literal beside the grid, Insert a template of the
//! fields loaded, Duplicate a copy without its key, and Delete marks the
//! row; each is staged (see [`crate::document_edits`]) and marked on the
//! grid until Apply sends them through the write review, on a session of
//! their own that may write, and audits them. Where the engine has
//! transactions they apply all or nothing; elsewhere Apply stops at the
//! first failure and keeps the rest staged. Views, and users the server
//! keeps from writing, offer no edits.
//!
//! Besides its documents, the tab shows the schema they hold, sampled on
//! the connection's catalog session and kept for every tab on the
//! connection (see [`DocDb::schema`]), beside the one the engine declares.
//! A new document starts from that schema.

use std::cell::RefCell;
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu};
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, WeakEntity, Window, actions, div, px,
};
use kuery_core::browse::Direction;
use kuery_core::document::capabilities::Transactions;
use kuery_core::document::catalog::{CollectionInfo, ContainerKind};
use kuery_core::document::change::{BrowseSpec, Continuation, Count, DocChange};
use kuery_core::document::export::{DocFormat, DocumentWriter, Ejson, document_ejson};
use kuery_core::document::language::{Language, ParseError, Style};
use kuery_core::document::schema::{self, Sampled};
use kuery_core::document::session::{DocEvent, DocSummary, DocumentSession, Heavy};
use kuery_core::document::value::{CollectionRef, Document, FieldPath, Value};
use kuery_core::export::{self, Format};
use kuery_core::read_only::Rejection;
use kuery_core::result_set::ResultSet;
use kuery_core::session::{Access, RunOptions};

use crate::assets::AppIcon;
use crate::audit;
#[cfg(test)]
use crate::audit::AuditRecord;
use crate::chrome;
use crate::collection_sections;
use crate::connection::{DocDb, Environment};
use crate::context_menus::{Cell, copy, item};
use crate::document_detail::{DetailEvent, DocumentDetail};
use crate::document_edits::{self, Mark, Staging};
use crate::document_grid::{DocumentRows, Flattened};
use crate::document_write;
use crate::grid::BrowseDelegate;
use crate::query_tab::describe_error;
use crate::theme::{self, color};
use crate::ui::{format_duration, placeholder, thousands};

actions!(kuery, [ReloadCollection]);

/// Documents in a page when the limit is left empty.
pub const DEFAULT_LIMIT: usize = 100;

/// Matching documents counted before the count says "more than".
const COUNT_CAP: u64 = 10_000;

/// How long an export may read for: a whole collection can take a while.
const EXPORT_TIME: Duration = Duration::from_secs(60 * 60);

/// The filter bar's texts, as typed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BarText {
    pub filter: String,
    pub projection: String,
    pub sort: String,
    pub limit: String,
}

enum SessionState {
    Connecting,
    Ready(DocumentSession),
    Failed(String),
}

enum Status {
    Idle,
    Loading,
    Loaded { documents: usize, elapsed: Duration },
    Failed(String),
}

enum ExportState {
    Running { documents: usize },
    Done(String),
    Failed(String),
}

/// What the detail's literal becomes when it is staged.
enum Editing {
    /// An edit of this document, as it was loaded.
    Edit(Document),
    /// A new document.
    Insert,
}

/// Where the staged changes stand.
enum ApplyState {
    Idle,
    Applying,
    Applied(String),
    Failed(String),
}

/// What the tab shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Documents,
    /// What the documents hold, sampled, beside what the engine declares.
    Schema,
    Indexes,
}

/// Something a section reads, off the UI thread.
enum Read<T> {
    Idle,
    Pending,
    Ready(T),
    Failed(String),
}

enum CountState {
    /// Not asked for: a view, whose count runs its whole pipeline.
    Skipped,
    Counting,
    Done(Count),
    Failed(String),
}

pub struct CollectionTab {
    db: DocDb,
    /// The project folder, where exports are offered.
    project: PathBuf,
    session: SessionState,
    /// A session of its own for the count, so a page never waits behind a
    /// slow one. `None` until it opens, or if it could not: the count then
    /// queues on `session`, after the page.
    counter: Option<DocumentSession>,
    collection: CollectionRef,
    kind: ContainerKind,
    filter: Entity<InputState>,
    projection: Entity<InputState>,
    sort: Entity<InputState>,
    limit: Entity<InputState>,
    /// What is wrong with the bar's text, as typed.
    bar_error: Option<String>,
    /// The bar the shown page was read with.
    shown: Option<BrowseSpec>,
    grid: Entity<TableState<BrowseDelegate>>,
    rows: Rc<RefCell<DocumentRows>>,
    /// The row whose document shows beside the grid.
    selected: Option<usize>,
    detail: Entity<DocumentDetail>,
    /// Where each page shown so far starts, the current one last; the first
    /// starts at `None`.
    pages: Vec<Option<Continuation>>,
    /// Where the page after the current one starts, if there is one.
    next: Option<Continuation>,
    count: CountState,
    status: Status,
    /// A view's pipeline, as the engine's literal.
    pipeline: Option<String>,
    section: Section,
    /// The collection's schema, as sampled on the connection.
    schema: Read<Arc<Sampled>>,
    /// Its kind, indexes, validator and options, as the engine reports them.
    info: Read<Rc<CollectionInfo>>,
    schema_task: Option<Task<()>>,
    info_task: Option<Task<()>>,
    /// Insert waits for the schema it starts from.
    insert_task: Option<Task<()>>,
    /// Find was asked for before the session opened.
    find_on_open: bool,
    export: Option<ExportState>,
    /// The session the running export reads through, its own so the tab
    /// keeps paging, and so Stop can cancel it.
    export_session: Option<DocumentSession>,
    page_task: Option<Task<()>>,
    count_task: Option<Task<()>>,
    export_task: Option<Task<()>>,
    /// Document changes waiting for Apply.
    staging: Staging,
    /// What the detail's literal is open for, if it is.
    editing: Option<Editing>,
    /// A session of its own that may write, opened on the first Apply; the
    /// tab's own only reads.
    writer: Option<DocumentSession>,
    apply: ApplyState,
    apply_task: Option<Task<()>>,
    /// The last apply did not say whether some changes applied: Apply
    /// waits for a page to be read again, so they are not applied twice.
    reload_first: bool,
    /// The staged changes are listed as the statements they become.
    show_changes: bool,
    focus_handle: FocusHandle,
    /// What the audit log was given, for the tests.
    #[cfg(test)]
    audited: Vec<AuditRecord>,
}

impl CollectionTab {
    pub fn new(
        db: DocDb,
        collection: CollectionRef,
        kind: ContainerKind,
        project: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let rows = Rc::new(RefCell::new(DocumentRows::default()));
        let grid = cx.new(|cx| TableState::new(BrowseDelegate::default(), window, cx));
        let weak = cx.entity().downgrade();
        let window_handle = window.window_handle();
        grid.update(cx, |grid, _| {
            let sort = weak.clone();
            grid.delegate_mut()
                .set_on_sort(Rc::new(move |column, direction, cx| {
                    let sort = sort.clone();
                    window_handle
                        .update(cx, |_, window, cx| {
                            sort.update(cx, |tab, cx| tab.sort_by(column, direction, window, cx))
                        })
                        .ok();
                }));
            let absent = rows.clone();
            grid.delegate_mut()
                .set_absent(Some(Rc::new(move |row, column| {
                    absent.borrow().is_absent(row, column)
                })));
            // Copy Row as JSON copies the document, types and all.
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
        let detail = cx.new(|cx| DocumentDetail::new(db.catalog.language(), window, cx));
        cx.subscribe_in(
            &detail,
            window,
            |this, _, event: &DetailEvent, window, cx| match event {
                DetailEvent::FilterBy(filter) => this.filter_by(filter.clone(), window, cx),
                DetailEvent::Close => {
                    this.selected = None;
                    cx.notify();
                }
                DetailEvent::Edit => {
                    if let Some(row) = this.selected {
                        this.edit_row(row, window, cx);
                    }
                }
                DetailEvent::Stage(text) => this.stage(text, window, cx),
                DetailEvent::CancelEdit => this.cancel_edit(window, cx),
            },
        )
        .detach();

        let input = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            let input =
                cx.new(|cx| InputState::new(window, cx).placeholder(placeholder.to_string()));
            cx.subscribe(&input, |this, _, event: &InputEvent, cx| match event {
                InputEvent::Change => this.check(cx),
                InputEvent::PressEnter { .. } => this.find(cx),
                _ => {}
            })
            .detach();
            input
        };
        let filter = input("{ field: \"value\" }", window, cx);
        let projection = input("{ field: 1 }", window, cx);
        let sort = input("{ field: -1 }", window, cx);
        let limit = input(&DEFAULT_LIMIT.to_string(), window, cx);

        // Both start opening now, side by side.
        let opening = db.catalog.reopen(db.catalog.access());
        let counting = db.catalog.reopen(db.catalog.access());
        cx.spawn(async move |this, cx| {
            let result = opening.await;
            let counter = counting.await.ok();
            this.update(cx, |this, cx| this.opened(result, counter, cx))
                .ok();
        })
        .detach();
        let mut tab = Self {
            db,
            project,
            session: SessionState::Connecting,
            counter: None,
            collection,
            kind,
            filter,
            projection,
            sort,
            limit,
            bar_error: None,
            shown: None,
            grid,
            rows,
            selected: None,
            detail,
            pages: vec![None],
            next: None,
            count: CountState::Skipped,
            status: Status::Idle,
            pipeline: None,
            section: Section::Documents,
            schema: Read::Idle,
            info: Read::Idle,
            schema_task: None,
            info_task: None,
            insert_task: None,
            find_on_open: true,
            export: None,
            export_session: None,
            page_task: None,
            count_task: None,
            export_task: None,
            staging: Staging::default(),
            editing: None,
            writer: None,
            apply: ApplyState::Idle,
            reload_first: false,
            apply_task: None,
            show_changes: false,
            focus_handle: cx.focus_handle(),
            #[cfg(test)]
            audited: Vec::new(),
        };
        // A view's pipeline shows under the bar.
        if kind == ContainerKind::View {
            tab.load_info(cx);
        }
        tab
    }

    /// Whether changes wait to be applied: closing the tab would lose them.
    pub fn has_pending(&self) -> bool {
        !self.staging.is_empty()
    }

    pub fn db(&self) -> &DocDb {
        &self.db
    }

    pub fn collection(&self) -> &CollectionRef {
        &self.collection
    }

    pub fn kind(&self) -> ContainerKind {
        self.kind
    }

    pub fn title(&self) -> SharedString {
        self.collection.name().to_string().into()
    }

    /// The filter, projection and sort as typed, for restoring the tab.
    pub fn bar(&self, cx: &App) -> (String, String, String) {
        (
            self.filter.read(cx).value().to_string(),
            self.projection.read(cx).value().to_string(),
            self.sort.read(cx).value().to_string(),
        )
    }

    /// Puts a bar back (a restored tab) and reads the first page with it.
    pub fn set_bar(
        &mut self,
        filter: String,
        projection: String,
        sort: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for (input, text) in [
            (&self.filter, filter),
            (&self.projection, projection),
            (&self.sort, sort),
        ] {
            input.update(cx, |input, cx| input.set_value(text, window, cx));
        }
        self.check(cx);
        self.find(cx);
    }

    /// The loaded page, or the export under way, for the status bar.
    pub fn page_status(&self) -> Option<String> {
        if let Some(ExportState::Running { documents }) = &self.export {
            return Some(format!(
                "Exporting · {} written",
                plural_documents(*documents as u64)
            ));
        }
        match &self.status {
            Status::Loaded { documents, elapsed } => Some(format!(
                "{} · loaded in {}",
                plural_documents(*documents as u64),
                format_duration(*elapsed)
            )),
            _ => None,
        }
    }

    fn language(&self) -> &'static Language {
        self.db.catalog.language()
    }

    /// Shows a section, reading what it needs the first time.
    fn show_section(&mut self, section: Section, cx: &mut Context<Self>) {
        self.section = section;
        if section == Section::Schema && matches!(self.schema, Read::Idle) {
            self.load_schema(false, cx);
        }
        if section != Section::Documents && matches!(self.info, Read::Idle) {
            self.load_info(cx);
        }
        cx.notify();
    }

    /// Reads the collection's schema: the one sampled on the connection,
    /// or (`again`, or when there is none) a new sample, on the catalog
    /// session.
    fn load_schema(&mut self, again: bool, cx: &mut Context<Self>) {
        let reading = self.db.schema(self.collection.clone(), again);
        self.schema = Read::Pending;
        self.schema_task = Some(cx.spawn(async move |this, cx| {
            let read = reading.await;
            this.update(cx, |this, cx| {
                this.schema = match read {
                    Ok(sampled) => Read::Ready(sampled),
                    Err(error) => Read::Failed(describe_error(error)),
                };
                this.schema_task = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Reads what the engine reports about the collection, on the catalog
    /// session: a view's pipeline, the validator, the indexes.
    fn load_info(&mut self, cx: &mut Context<Self>) {
        let reading = self.db.catalog.collection(self.collection.clone());
        let render = self.language().render;
        self.info = Read::Pending;
        self.info_task = Some(cx.spawn(async move |this, cx| {
            let read = reading.await;
            this.update(cx, |this, cx| {
                this.info = match read {
                    Ok(info) => {
                        this.pipeline = info
                            .options
                            .get("pipeline")
                            .map(|pipeline| render(pipeline, Style::Compact));
                        Read::Ready(Rc::new(info))
                    }
                    Err(error) => Read::Failed(describe_error(error)),
                };
                this.info_task = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Work the language marks heavy runs on development connections only.
    fn heavy(&self) -> Heavy {
        match self.db.environment {
            Environment::Development => Heavy::Allow,
            Environment::Staging | Environment::Production => Heavy::Refuse,
        }
    }

    fn bar_text(&self, cx: &App) -> BarText {
        let (filter, projection, sort) = self.bar(cx);
        BarText {
            filter,
            projection,
            sort,
            limit: self.limit.read(cx).value().to_string(),
        }
    }

    fn opened(
        &mut self,
        result: Result<DocumentSession, String>,
        counter: Option<DocumentSession>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(session) => {
                self.session = SessionState::Ready(session);
                self.counter = counter;
                if self.find_on_open {
                    self.find(cx);
                }
            }
            Err(error) => self.session = SessionState::Failed(error),
        }
        cx.notify();
    }

    /// Checks the bar as it is typed in, so its errors show before Find.
    fn check(&mut self, cx: &mut Context<Self>) {
        self.bar_error = check_bar(self.language(), &self.bar_text(cx), self.heavy());
        cx.notify();
    }

    /// Reads the first page with the bar as it is, and counts what matches.
    fn find(&mut self, cx: &mut Context<Self>) {
        let bar = self.bar_text(cx);
        self.bar_error = check_bar(self.language(), &bar, self.heavy());
        if self.bar_error.is_some() {
            cx.notify();
            return;
        }
        let spec = match browse_spec(&bar, None, self.language().parse_document) {
            Ok(spec) => spec,
            Err(error) => {
                self.bar_error = Some(error);
                cx.notify();
                return;
            }
        };
        let SessionState::Ready(session) = &self.session else {
            self.find_on_open = true;
            cx.notify();
            return;
        };
        self.find_on_open = false;
        let session = session.clone();
        self.pages = vec![None];
        // The page first: without a counter of its own, the count queues
        // behind it.
        let filter = spec.filter.clone();
        self.load_page(&session, spec, cx);
        self.count(&session, filter, cx);
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let (SessionState::Ready(session), Some(spec)) = (&self.session, self.shown.clone()) else {
            self.find(cx);
            return;
        };
        let session = session.clone();
        self.load_page(&session, spec, cx);
    }

    fn go_next(&mut self, cx: &mut Context<Self>) {
        let (SessionState::Ready(session), Some(spec), Some(next)) =
            (&self.session, self.shown.clone(), self.next.clone())
        else {
            return;
        };
        let session = session.clone();
        self.pages.push(Some(next));
        self.load_page(&session, spec, cx);
    }

    fn go_previous(&mut self, cx: &mut Context<Self>) {
        let (SessionState::Ready(session), Some(spec)) = (&self.session, self.shown.clone()) else {
            return;
        };
        if self.pages.len() < 2 {
            return;
        }
        let session = session.clone();
        self.pages.pop();
        self.load_page(&session, spec, cx);
    }

    /// Sorts by a column from its header: the sort bar says it, so it is
    /// what restoring and Find read back.
    fn sort_by(
        &mut self,
        column: String,
        direction: Option<Direction>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = direction.map_or(String::new(), |direction| {
            let order = match direction {
                Direction::Ascending => 1,
                Direction::Descending => -1,
            };
            let sort = Document(vec![(column, Value::Int32(order))]);
            (self.language().render)(&Value::Object(sort), Style::Compact)
        });
        self.sort
            .update(cx, |input, cx| input.set_value(text, window, cx));
        self.find(cx);
    }

    /// Reads one page, where `pages` says the current one starts.
    fn load_page(
        &mut self,
        session: &DocumentSession,
        mut spec: BrowseSpec,
        cx: &mut Context<Self>,
    ) {
        // What the last apply left is on its way to the grid.
        self.reload_first = false;
        spec.after = self.pages.last().cloned().flatten();
        self.shown = Some(spec.clone());
        self.next = None;
        self.status = Status::Loading;
        self.selected = None;
        self.rows.borrow_mut().clear();
        let sort = header_sort(&spec);
        self.grid.update(cx, |grid, cx| {
            let delegate = grid.delegate_mut();
            delegate.set_columns(Vec::new(), sort);
            delegate.set_page(ResultSet::default());
            delegate.clear_marks();
            delegate.set_loading(true);
            grid.clear_selection(cx);
            grid.refresh(cx);
        });
        let editable = self.edits_refused().is_none();
        self.detail
            .update(cx, |detail, cx| detail.set_editable(editable, cx));

        let render = self.language().render;
        let events = session.browse(
            self.collection.clone(),
            spec,
            self.heavy(),
            RunOptions::default(),
        );
        self.page_task = Some(cx.spawn(async move |this, cx| {
            let mut columns: Vec<String> = Vec::new();
            while let Ok(event) = events.recv().await {
                match event {
                    DocEvent::Documents(batch) => {
                        let known = columns.clone();
                        let (batch, flattened) = cx
                            .background_spawn(async move {
                                let flattened = DocumentRows::flatten(&known, &batch, render);
                                (batch, flattened)
                            })
                            .await;
                        columns.extend(flattened.new_columns.iter().map(|info| info.name.clone()));
                        let shown =
                            this.update(cx, |this, cx| this.show_batch(batch, flattened, cx));
                        if shown.is_err() {
                            return;
                        }
                    }
                    DocEvent::Outcome(_) | DocEvent::Left(_) => {}
                    DocEvent::Finished(summary) => {
                        this.update(cx, |this, cx| this.page_loaded(summary, cx))
                            .ok();
                        return;
                    }
                    DocEvent::Failed(error) => {
                        this.update(cx, |this, cx| this.page_failed(describe_error(error), cx))
                            .ok();
                        return;
                    }
                }
            }
        }));
        cx.notify();
    }

    fn show_batch(&mut self, batch: Vec<Document>, flattened: Flattened, cx: &mut Context<Self>) {
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
        if !self.staging.is_empty() {
            self.mark(cx);
        }
    }

    fn page_loaded(&mut self, summary: DocSummary, cx: &mut Context<Self>) {
        self.status = Status::Loaded {
            documents: summary.documents,
            elapsed: summary.elapsed,
        };
        self.next = summary.next;
        self.grid.update(cx, |grid, cx| {
            grid.delegate_mut().set_loading(false);
            cx.notify();
        });
        cx.notify();
    }

    fn page_failed(&mut self, error: String, cx: &mut Context<Self>) {
        self.status = Status::Failed(error);
        self.grid.update(cx, |grid, cx| {
            grid.delegate_mut().set_loading(false);
            cx.notify();
        });
        cx.notify();
    }

    /// Counts what the filter matches, capped and timed by the driver, on
    /// the counter's session when it has one: a count still running for an
    /// earlier Find is stopped first.
    fn count(&mut self, session: &DocumentSession, filter: Option<String>, cx: &mut Context<Self>) {
        if self.kind == ContainerKind::View {
            self.count = CountState::Skipped;
            return;
        }
        self.count = CountState::Counting;
        let session = match &self.counter {
            Some(counter) => {
                counter.cancel();
                counter
            }
            None => session,
        };
        let counting = session.count(self.collection.clone(), filter, COUNT_CAP, self.heavy());
        self.count_task = Some(cx.spawn(async move |this, cx| {
            let result = counting.await;
            this.update(cx, |this, cx| {
                this.count = match result {
                    Ok(count) => CountState::Done(count),
                    Err(error) => CountState::Failed(describe_error(error)),
                };
                cx.notify();
            })
            .ok();
        }));
    }

    fn loading(&self) -> bool {
        matches!(self.status, Status::Loading)
    }

    // Documents ------------------------------------------------------------

    /// Shows a row's document beside the grid.
    fn select_row(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let document = self.rows.borrow().document(row).cloned();
        // An edit under way keeps the detail until it is staged or
        // cancelled.
        if document.is_none() || self.editing.is_some() {
            return;
        }
        self.selected = Some(row);
        self.detail
            .update(cx, |detail, cx| detail.show(document, window, cx));
        cx.notify();
    }

    /// Puts a filter in the bar (one by a field, from a menu) and reads the
    /// first page with it.
    fn filter_by(&mut self, filter: String, window: &mut Window, cx: &mut Context<Self>) {
        self.filter
            .update(cx, |input, cx| input.set_value(filter, window, cx));
        self.find(cx);
    }

    // Edits ----------------------------------------------------------------

    /// Why documents cannot be edited here, if they cannot: a view, a user
    /// the server keeps from writing, or a projection, whose documents are
    /// not whole.
    fn edits_refused(&self) -> Option<&'static str> {
        let refused = document_edits::read_only_reason(self.kind, self.db.catalog.capabilities());
        if refused.is_some() {
            return refused;
        }
        let projected = self
            .shown
            .as_ref()
            .is_some_and(|spec| spec.projection.is_some());
        projected.then_some("Clear the projection to edit: the documents shown are not whole")
    }

    /// Whether the tab offers edits at all: a projection only keeps them
    /// until it is cleared.
    fn edits_offered(&self) -> bool {
        document_edits::read_only_reason(self.kind, self.db.catalog.capabilities()).is_none()
    }

    fn applying(&self) -> bool {
        matches!(self.apply, ApplyState::Applying)
    }

    /// Opens a loaded document for editing, as already edited if it was.
    /// One that changed since its staged edit was made from it is refused:
    /// the edit would show over what it held before.
    fn edit_row(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        // What was asked last wins over a new document still on its way.
        self.insert_task = None;
        if self.refuse_edit(cx) {
            return;
        }
        let Some(loaded) = self.rows.borrow().document(row).cloned() else {
            return;
        };
        if self.staging.stale(&loaded) {
            self.apply = ApplyState::Failed(document_edits::STALE.into());
            cx.notify();
            return;
        }
        let current = self.staging.current(&loaded);
        let title = format!("Edit {}", self.document_name(&loaded));
        self.open_editor(title, &current, Editing::Edit(loaded), window, cx);
        self.selected = Some(row);
    }

    /// Opens a new document, from the collection's schema: the one
    /// sampled on the connection, sampled now if there is none, or, if
    /// that fails, the documents loaded.
    fn insert_document(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.applying() || !self.edits_offered() || self.insert_task.is_some() {
            return;
        }
        let key = self.language().key_name;
        let loaded = schema::infer(self.rows.borrow().documents());
        let reading = self.db.schema(self.collection.clone(), false);
        self.insert_task = Some(cx.spawn_in(window, async move |this, cx| {
            let template = match reading.await {
                Ok(sampled) => document_edits::template(&sampled.schema, key),
                Err(_) => document_edits::template(&loaded, key),
            };
            this.update_in(cx, |this, window, cx| {
                this.insert_task = None;
                // Never over an editor opened meanwhile, or while an apply
                // runs.
                if this.editing.is_some() || this.applying() {
                    return;
                }
                this.open_editor(
                    "New Document".into(),
                    &template,
                    Editing::Insert,
                    window,
                    cx,
                );
            })
            .ok();
        }));
    }

    /// Opens a copy of a loaded document, without its key, as a new one.
    fn duplicate_row(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.insert_task = None;
        if self.refuse_edit(cx) {
            return;
        }
        let Some(loaded) = self.rows.borrow().document(row).cloned() else {
            return;
        };
        let copy = document_edits::duplicate(&self.staging.current(&loaded));
        let title = format!("Copy of {}", self.document_name(&loaded));
        self.open_editor(title, &copy, Editing::Insert, window, cx);
    }

    /// Stages the delete of a loaded document.
    fn delete_row(&mut self, row: usize, cx: &mut Context<Self>) {
        if self.refuse_edit(cx) {
            return;
        }
        let Some(loaded) = self.rows.borrow().document(row).cloned() else {
            return;
        };
        self.staging.delete(&loaded);
        self.staged(cx);
    }

    /// Says why the loaded documents cannot be edited, if they cannot;
    /// nothing is staged while an apply runs.
    fn refuse_edit(&mut self, cx: &mut Context<Self>) -> bool {
        if self.applying() {
            return true;
        }
        match self.edits_refused() {
            Some(reason) => {
                self.apply = ApplyState::Failed(reason.into());
                cx.notify();
                true
            }
            None => false,
        }
    }

    /// `Document 7`, after its key.
    fn document_name(&self, document: &Document) -> String {
        match document.key() {
            Some(key) => format!(
                "Document {}",
                (self.language().render)(&key.0, Style::Compact)
            ),
            None => "Document".into(),
        }
    }

    fn open_editor(
        &mut self,
        title: String,
        document: &Document,
        editing: Editing,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = (self.language().render)(&Value::Object(document.clone()), Style::Pretty);
        // A new document on its way would open over this one.
        self.insert_task = None;
        self.editing = Some(editing);
        self.detail
            .update(cx, |detail, cx| detail.edit(title, text, window, cx));
        cx.notify();
    }

    /// Stages the edited literal: read and diffed against the document as
    /// it was loaded, or read as a new document. What cannot be staged
    /// stays open, saying why.
    fn stage(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.applying() {
            return;
        }
        let language = self.language();
        let staged = match &self.editing {
            Some(Editing::Edit(loaded)) => self.staging.edit(loaded, text, language),
            Some(Editing::Insert) => self.staging.insert(text, language),
            None => return,
        };
        match staged {
            Ok(()) => {
                self.close_editor(window, cx);
                self.staged(cx);
            }
            Err(error) => self
                .detail
                .update(cx, |detail, cx| detail.refuse(error, cx)),
        }
    }

    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_editor(window, cx);
        cx.notify();
    }

    /// Back to the selected document, if any.
    fn close_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        let document = self
            .selected
            .and_then(|row| self.rows.borrow().document(row).cloned());
        self.detail
            .update(cx, |detail, cx| detail.stop_editing(document, window, cx));
    }

    /// The staged changes moved: marks them again, and forgets the last
    /// apply's outcome.
    fn staged(&mut self, cx: &mut Context<Self>) {
        self.apply = ApplyState::Idle;
        if self.staging.is_empty() {
            self.show_changes = false;
        }
        self.mark(cx);
        cx.notify();
    }

    /// Marks the staged changes on the rows of the documents they change.
    fn mark(&mut self, cx: &mut Context<Self>) {
        let marks = {
            let rows = self.rows.borrow();
            self.staging
                .marks(rows.documents(), rows.columns(), self.language().render)
        };
        self.grid.update(cx, |grid, cx| {
            let delegate = grid.delegate_mut();
            delegate.clear_marks();
            for mark in marks {
                match mark {
                    Mark::Edited { row, column, text } => delegate.mark_edit(row, column, text),
                    Mark::Removed { row, column } => delegate.mark_removed(row, column),
                    Mark::Deleted(row) => delegate.mark_deleted(row),
                }
            }
            cx.notify();
        });
    }

    fn unstage(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.applying() {
            return;
        }
        self.staging.remove(index);
        self.staged(cx);
    }

    fn discard(&mut self, cx: &mut Context<Self>) {
        if self.applying() {
            return;
        }
        self.staging.clear();
        self.staged(cx);
    }

    /// Sends the staged changes through the write review, then applies
    /// them on the tab's session that may write.
    fn apply_staged(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.staging.is_empty() || self.applying() || !self.edits_offered() {
            return;
        }
        if self.reload_first {
            self.apply = ApplyState::Failed(document_write::RELOAD_FIRST.into());
            cx.notify();
            return;
        }
        let changes = self.staging.changes();
        let generation = self.staging.generation();
        let this = cx.entity().downgrade();
        let approved = {
            let changes = changes.clone();
            move |_: &mut Window, cx: &mut App| {
                let changes = changes.clone();
                this.update(cx, |this, cx| this.run_apply(changes, generation, cx))
                    .ok();
            }
        };
        document_write::approve_changes(&self.db, &self.collection, &changes, window, cx, approved);
    }

    /// Applies approved changes, all or nothing where the engine has
    /// transactions, and audits them as the statements they became.
    /// `generation` is the staging's when they were reviewed.
    fn run_apply(&mut self, changes: Vec<DocChange>, generation: u64, cx: &mut Context<Self>) {
        if self.applying() {
            return;
        }
        let language = self.language();
        let statements: Vec<String> = changes
            .iter()
            .map(|change| (language.change)(&self.collection, change))
            .collect();
        // The staged changes moved, or writes stopped being allowed, while
        // the review was open.
        let refused = if self.staging.generation() != generation {
            Some("The staged changes moved while they were reviewed: nothing was applied")
        } else if self.reload_first {
            Some(document_write::RELOAD_FIRST)
        } else if !self.db.allows_writes() {
            Some("Writes are no longer allowed on this connection: nothing was applied")
        } else {
            None
        };
        if let Some(refused) = refused {
            self.audit(&statements, format!("Refused: {refused}"), None, true, cx);
            self.apply = ApplyState::Failed(refused.into());
            cx.notify();
            return;
        }
        self.apply = ApplyState::Applying;
        let writer = self.writer.clone();
        let opening = writer
            .is_none()
            .then(|| self.db.catalog.reopen(Access::ReadWrite));
        let collection = self.collection.clone();
        let count = changes.len();
        self.apply_task = Some(cx.spawn(async move |this, cx| {
            let writer = match (writer, opening) {
                (Some(writer), _) => writer,
                (None, Some(opening)) => match opening.await {
                    Ok(writer) => {
                        let kept = writer.clone();
                        this.update(cx, |this, _| this.writer = Some(kept)).ok();
                        writer
                    }
                    Err(error) => {
                        let failure = format!(
                            "Could not open a session that may write: {error}. Nothing was applied."
                        );
                        this.update(cx, |this, cx| {
                            this.audit(&statements, failure.clone(), None, true, cx);
                            this.applied_with(ApplyState::Failed(failure), 0, cx)
                        })
                        .ok();
                        return;
                    }
                },
                (None, None) => return,
            };
            let result = writer.apply(collection, changes).await;
            this.update(cx, |this, cx| match result {
                Ok(outcome) => {
                    let done = document_write::applied(count, outcome);
                    let changed = Some((count, document_write::changed(outcome)));
                    this.audit(&statements, done.clone(), changed, false, cx);
                    this.applied_with(ApplyState::Applied(done), count, cx);
                }
                Err(failure) => {
                    let said = document_write::apply_failed(&failure, &statements);
                    // What changed is unknown when an answer never came.
                    let changed =
                        (failure.unknown == 0).then_some((failure.applied, failure.applied as u64));
                    this.audit(&statements, said.clone(), changed, true, cx);
                    this.reload_first = failure.unknown > 0;
                    this.applied_with(ApplyState::Failed(said), failure.applied, cx);
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// Unstages what applied, and reads the page again to show it, unless
    /// what applied is unknown: the reload is then the user's to ask for.
    fn applied_with(&mut self, state: ApplyState, applied: usize, cx: &mut Context<Self>) {
        self.apply_task = None;
        self.staging.applied(applied);
        if self.staging.is_empty() {
            self.show_changes = false;
        }
        self.apply = state;
        if applied > 0 && !self.reload_first {
            self.reload(cx);
        } else {
            self.mark(cx);
        }
        cx.notify();
    }

    /// Writes a record of applied changes to the audit log.
    fn audit(
        &mut self,
        statements: &[String],
        outcome: String,
        changed: Option<(usize, u64)>,
        failed: bool,
        cx: &mut Context<Self>,
    ) {
        let record = document_write::record_changes(
            &self.db,
            &self.project,
            &self.collection,
            statements,
            outcome,
            changed,
            failed,
        );
        #[cfg(test)]
        self.audited.push(record.clone());
        audit::append_in_background(record, cx);
    }

    /// Whether the connection's server applies a batch all or nothing.
    fn all_or_nothing(&self) -> bool {
        matches!(
            self.db.catalog.capabilities().transactions,
            Transactions::Multi { .. }
        )
    }

    // Exports --------------------------------------------------------------

    /// Asks where to write every document the bar matches (`.json` for one
    /// array, NDJSON otherwise) and streams them there.
    fn export_documents(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.session, SessionState::Ready(_)) || self.shown.is_none() {
            return;
        }
        if matches!(self.export, Some(ExportState::Running { .. })) {
            return;
        }
        let suggested = format!("{}.ndjson", self.collection.name());
        let chosen = cx.prompt_for_new_path(&self.project, Some(&suggested));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(mut path))) = chosen.await else {
                return;
            };
            if path.extension().is_none() {
                path.set_extension("ndjson");
            }
            this.update(cx, |this, cx| this.export_to(path, cx)).ok();
        })
        .detach();
    }

    /// Streams every document the shown bar matches to `path`, off the UI
    /// thread, through a session of its own: the tab keeps paging while it
    /// runs, and Stop cancels it.
    fn export_to(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(shown) = &self.shown else {
            return;
        };
        if matches!(self.export, Some(ExportState::Running { .. })) {
            return;
        }
        let spec = BrowseSpec {
            limit: 0,
            after: None,
            ..shown.clone()
        };
        let (collection, heavy) = (self.collection.clone(), self.heavy());
        let opening = self.db.catalog.reopen(self.db.catalog.access());
        self.export = Some(ExportState::Running { documents: 0 });
        cx.notify();
        self.export_task = Some(cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let session = match opening.await {
                Ok(session) => session,
                Err(error) => {
                    let failure = format!("Could not export to {}: {error}", path.display());
                    this.update(cx, |this, cx| this.export_ended(Err(failure), cx))
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
            let events = session.browse(collection, spec, heavy, options);
            let target = path.clone();
            let opened = cx
                .background_spawn(async move { open_export(&target) })
                .await;
            let mut writer = match opened {
                Ok(writer) => writer,
                Err(error) => {
                    let failure = format!("Could not export to {}: {error}", path.display());
                    this.update(cx, |this, cx| this.export_ended(Err(failure), cx))
                        .ok();
                    return;
                }
            };
            let mut written = 0;
            this.update(cx, |this, cx| this.export_progress(0, cx)).ok();
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
                        this.update(cx, |this, cx| this.export_progress(written, cx))
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
            let outcome = ended
                .map(|()| {
                    format!(
                        "Exported {} to {} · {}",
                        plural_documents(written as u64),
                        path.display(),
                        format_duration(started.elapsed())
                    )
                })
                .map_err(|error| format!("Could not export to {}: {error}", path.display()));
            this.update(cx, |this, cx| this.export_ended(outcome, cx))
                .ok();
        }));
    }

    fn export_progress(&mut self, documents: usize, cx: &mut Context<Self>) {
        self.export = Some(ExportState::Running { documents });
        cx.notify();
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
        self.export = Some(match outcome {
            Ok(done) => ExportState::Done(done),
            Err(error) => ExportState::Failed(error),
        });
        cx.notify();
    }

    /// Asks where to write the loaded rows as the grid shows them (CSV, TSV
    /// or JSON by the extension).
    fn export_grid(&mut self, cx: &mut Context<Self>) {
        let set = self.grid.read(cx).delegate().page().clone();
        if set.columns().is_empty() {
            return;
        }
        let suggested = format!("{}.csv", self.collection.name());
        let chosen = cx.prompt_for_new_path(&self.project, Some(&suggested));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(mut path))) = chosen.await else {
                return;
            };
            if path.extension().is_none() {
                path.set_extension(Format::Csv.extension());
            }
            let format = Format::from_path(&path);
            let Ok(absent) = this.update(cx, |this, _| this.absent_cells(&set)) else {
                return;
            };
            let rows = set.rows();
            let target = path.clone();
            let written = cx
                .background_spawn(async move {
                    let mut out = BufWriter::new(File::create(&target)?);
                    let absent = |row, column| absent.contains(&(row, column));
                    export::write_lacking(&set, format, &absent, &mut out)?;
                    out.flush()
                })
                .await;
            let outcome = match written {
                Ok(()) => Ok(format!(
                    "Exported {} rows as {} to {}",
                    thousands(rows as u64),
                    format.extension().to_uppercase(),
                    path.display()
                )),
                Err(error) => Err(format!("Could not export to {}: {error}", path.display())),
            };
            this.update(cx, |this, cx| {
                // A document export that started meanwhile keeps its place.
                if !matches!(this.export, Some(ExportState::Running { .. })) {
                    this.export_ended(outcome, cx)
                }
            })
            .ok();
        })
        .detach();
    }

    /// The grid's empty cells whose document lacks the field, which a JSON
    /// export leaves out rather than writing null.
    fn absent_cells(&self, set: &ResultSet) -> HashSet<(usize, usize)> {
        let rows = self.rows.borrow();
        (0..set.rows())
            .flat_map(|row| (0..set.columns().len()).map(move |column| (row, column)))
            .filter(|&(row, column)| {
                set.cell(row, column).is_some_and(|value| value.is_none())
                    && rows.is_absent(row, column)
            })
            .collect()
    }

    // Render ---------------------------------------------------------------

    fn render_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let label = |text: &'static str| {
            div()
                .w(px(70.))
                .flex_shrink_0()
                .text_size(px(11.5))
                .text_color(color(theme::TEXT_TERTIARY))
                .child(text)
        };
        let ready = matches!(self.session, SessionState::Ready(_));
        v_flex()
            .px_3()
            .py_2()
            .gap(px(6.))
            .flex_shrink_0()
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .font_family(mono)
            .child(
                h_flex()
                    .gap_2()
                    .child(label("Filter"))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.filter).small()),
                    )
                    .child(
                        chrome::primary_button("find", AppIcon::Filter, "Find", Some("↵"))
                            .when(ready, |b| {
                                b.on_click(cx.listener(|this, _, _, cx| this.find(cx)))
                            }),
                    )
                    .child(
                        chrome::icon_button(
                            "reload",
                            AppIcon::Reload,
                            if cfg!(target_os = "macos") {
                                "Reload  ⌘R"
                            } else {
                                "Reload  Ctrl+R"
                            },
                            ready && !self.loading(),
                        )
                        .when(ready && !self.loading(), |b| {
                            b.on_click(cx.listener(|this, _, _, cx| this.reload(cx)))
                        }),
                    )
                    .when(self.edits_offered(), |row| {
                        let enabled = ready && !self.applying();
                        row.child(
                            chrome::icon_button(
                                "insert-document",
                                IconName::Plus,
                                "Insert Document",
                                enabled,
                            )
                            .when(enabled, |b| {
                                b.on_click(cx.listener(|this, _, window, cx| {
                                    this.insert_document(window, cx)
                                }))
                            }),
                        )
                    })
                    .child(self.render_export_menu(ready, cx)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(label("Projection"))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.projection).small()),
                    )
                    .child(label("Sort"))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.sort).small()),
                    )
                    .child(label("Limit"))
                    .child(div().w(px(90.)).child(Input::new(&self.limit).small())),
            )
            .children(self.bar_error.clone().map(|error| {
                div()
                    .text_size(px(12.))
                    .text_color(color(theme::RED))
                    .child(error)
            }))
            .children(self.pipeline.clone().map(|pipeline| {
                h_flex()
                    .gap_2()
                    .text_size(px(12.))
                    .child(label("Pipeline"))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(color(theme::TEXT_SECONDARY))
                            .child(pipeline),
                    )
            }))
    }

    fn render_export_menu(&self, ready: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.entity().downgrade();
        let exporting = matches!(self.export, Some(ExportState::Running { .. }));
        Button::new("collection-export")
            .ghost()
            .compact()
            .h(px(30.))
            .px(px(8.))
            .rounded(px(7.))
            .disabled(!ready || self.shown.is_none())
            .child(
                h_flex()
                    .gap(px(5.))
                    .text_color(color(theme::TEXT_SECONDARY))
                    .child(Icon::new(AppIcon::Download).size(px(15.)))
                    .child(Icon::new(IconName::ChevronDown).size(px(11.))),
            )
            .dropdown_menu(move |menu, _, _| {
                menu.item(
                    item("Export Documents…", &weak, |this, _, cx| {
                        this.export_documents(cx)
                    })
                    .disabled(exporting),
                )
                .item(item("Export Grid…", &weak, |this, _, cx| {
                    this.export_grid(cx)
                }))
            })
    }

    /// The staged changes and the last apply's outcome, above the footer:
    /// Review lists the changes as the statements they become.
    fn render_changes(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let pending = self.staging.len();
        let outcome = match &self.apply {
            ApplyState::Applied(done) => Some((done.clone(), theme::TEXT_TERTIARY)),
            ApplyState::Failed(error) => Some((error.clone(), theme::RED)),
            ApplyState::Idle | ApplyState::Applying => None,
        };
        if pending == 0 && outcome.is_none() {
            return None;
        }
        let applying = self.applying();
        let language = self.language();
        let mono = cx.theme().mono_font_family.clone();
        let listed = self.show_changes.then(|| {
            v_flex()
                .id("staged-changes")
                .max_h(px(160.))
                .overflow_y_scroll()
                .gap(px(2.))
                .font_family(mono)
                .text_size(px(11.5))
                .children(
                    self.staging
                        .staged()
                        .iter()
                        .enumerate()
                        .map(|(index, staged)| {
                            let statement = (language.change)(&self.collection, &staged.change());
                            h_flex()
                                .gap_2()
                                .child(
                                    chrome::icon_button(
                                        ("unstage", index),
                                        IconName::Close,
                                        "Unstage",
                                        !applying,
                                    )
                                    .when(!applying, |b| {
                                        b.on_click(cx.listener(move |this, _, _, cx| {
                                            this.unstage(index, cx)
                                        }))
                                    }),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_color(color(theme::TEXT_SECONDARY))
                                        .child(statement),
                                )
                        }),
                )
        });
        let atomic = match self.all_or_nothing() {
            true => "Applied all or nothing",
            false => "Applied one by one, stopping at the first failure",
        };
        Some(
            v_flex()
                .px_3()
                .py(px(6.))
                .gap(px(6.))
                .flex_shrink_0()
                .border_t_1()
                .border_color(color(theme::HAIRLINE))
                .child(
                    h_flex()
                        .gap(px(10.))
                        .text_size(px(12.))
                        .when(pending > 0, |row| {
                            row.child(
                                chrome::pill()
                                    .child(chrome::dot(theme::AMBER))
                                    .child(div().text_color(color(0xc7c7cc)).child(match pending {
                                        1 => "1 change staged".to_string(),
                                        n => format!("{n} changes staged"),
                                    }))
                                    .child(
                                        chrome::pill_button(
                                            "review-changes",
                                            if self.show_changes { "Hide" } else { "Review" },
                                            false,
                                        )
                                        .on_click(
                                            cx.listener(|this, _, _, cx| {
                                                this.show_changes = !this.show_changes;
                                                cx.notify();
                                            }),
                                        ),
                                    )
                                    .child(
                                        chrome::pill_button("discard-changes", "Discard", false)
                                            .when(!applying, |b| {
                                                b.on_click(
                                                    cx.listener(|this, _, _, cx| this.discard(cx)),
                                                )
                                            }),
                                    )
                                    .child(
                                        chrome::pill_button(
                                            "apply-changes",
                                            if applying { "Applying…" } else { "Apply" },
                                            true,
                                        )
                                        .when(
                                            !applying,
                                            |b| {
                                                b.on_click(cx.listener(|this, _, window, cx| {
                                                    this.apply_staged(window, cx)
                                                }))
                                            },
                                        ),
                                    ),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_color(color(theme::TEXT_TERTIARY))
                                    .child(atomic),
                            )
                        })
                        .children(outcome.map(|(text, tone)| {
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(color(tone))
                                .child(text)
                        })),
                )
                .children(listed),
        )
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let page = self.pages.len() - 1;
        let limit = self.shown.as_ref().map_or(DEFAULT_LIMIT, |spec| spec.limit);
        let first = (page * limit) as u64;
        let total = match &self.count {
            CountState::Skipped => None,
            CountState::Counting => Some("counting…".to_string()),
            CountState::Done(count) => Some(describe_count(*count)),
            CountState::Failed(error) => Some(format!("count failed: {error}")),
        };
        let shown = match &self.status {
            Status::Loaded { documents, .. } if *documents > 0 => {
                let range = format!(
                    "{}–{}",
                    thousands(first + 1),
                    thousands(first + *documents as u64)
                );
                match total {
                    Some(total) => format!("{range} of {total}"),
                    None => range,
                }
            }
            Status::Loaded { .. } => "No documents".to_string(),
            Status::Loading => "Loading…".to_string(),
            Status::Idle | Status::Failed(_) => total.unwrap_or_default(),
        };
        let failure = match &self.status {
            Status::Failed(error) => Some(error.clone()),
            _ => None,
        };
        let by_position = self
            .shown
            .as_ref()
            .is_some_and(|spec| !spec.sort.is_empty());
        let can_back = !self.loading() && page > 0;
        let can_forward = !self.loading() && self.next.is_some();
        h_flex()
            .h(px(36.))
            .px_3()
            .gap(px(10.))
            .flex_shrink_0()
            .border_t_1()
            .border_color(color(theme::HAIRLINE))
            .text_size(px(12.))
            .text_color(color(theme::TEXT_SECONDARY))
            .child(div().flex_shrink_0().child(shown))
            .children(failure.map(|error| {
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(color(theme::RED))
                    .child(error)
            }))
            .when(by_position, |row| {
                row.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(color(theme::TEXT_TERTIARY))
                        .child(
                            "Paging by position: documents that move while you page can repeat or be skipped",
                        ),
                )
            })
            .child(div().flex_1())
            .children(self.export.as_ref().map(|export| match export {
                ExportState::Running { .. } => {
                    chrome::icon_button("stop-export", IconName::Square, "Stop Export", true)
                        .on_click(cx.listener(|this, _, _, cx| this.stop_export(cx)))
                        .into_any_element()
                }
                ExportState::Done(done) => div()
                    .min_w_0()
                    .truncate()
                    .text_color(color(theme::TEXT_TERTIARY))
                    .child(done.clone())
                    .into_any_element(),
                ExportState::Failed(error) => div()
                    .min_w_0()
                    .truncate()
                    .text_color(color(theme::RED))
                    .child(error.clone())
                    .into_any_element(),
            }))
            .when(self.kind == ContainerKind::View, |row| {
                row.child(
                    h_flex()
                        .gap(px(5.))
                        .child(Icon::new(AppIcon::Lock).size(px(11.)))
                        .child("Read-only view"),
                )
            })
            .when(
                self.kind != ContainerKind::View && !self.edits_offered(),
                |row| {
                    row.child(
                        h_flex()
                            .gap(px(5.))
                            .child(Icon::new(AppIcon::Lock).size(px(11.)))
                            .child("Read-only user"),
                    )
                },
            )
            .child(
                chrome::icon_button("prev-page", IconName::ChevronLeft, "Previous Page", can_back)
                    .when(can_back, |b| {
                        b.on_click(cx.listener(|this, _, _, cx| this.go_previous(cx)))
                    }),
            )
            .child(div().child(format!("Page {}", page + 1)))
            .child(
                chrome::icon_button("next-page", IconName::ChevronRight, "Next Page", can_forward)
                    .when(can_forward, |b| {
                        b.on_click(cx.listener(|this, _, _, cx| this.go_next(cx)))
                    }),
            )
    }
}

impl Focusable for CollectionTab {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl CollectionTab {
    /// The sections, and what the one shown offers.
    fn render_sections(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let section = self.section;
        let note = match (&self.section, &self.schema) {
            (Section::Schema, Read::Ready(sampled)) => Some(sampled.label()),
            (Section::Schema, Read::Pending) => Some("Sampling…".into()),
            _ => None,
        };
        let resample = section == Section::Schema && !matches!(self.schema, Read::Pending);
        let reread = section == Section::Indexes && !matches!(self.info, Read::Pending);
        h_flex()
            .h(px(40.))
            .px_3()
            .gap_2()
            .flex_shrink_0()
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                chrome::segmented().children(
                    [
                        (
                            "collection-documents",
                            AppIcon::Table,
                            "Documents",
                            Section::Documents,
                        ),
                        (
                            "collection-schema",
                            AppIcon::Code,
                            "Schema",
                            Section::Schema,
                        ),
                        (
                            "collection-indexes",
                            AppIcon::Key,
                            "Indexes",
                            Section::Indexes,
                        ),
                    ]
                    .map(|(id, icon, label, target)| {
                        chrome::segment(id, icon, label, section == target).on_click(
                            cx.listener(move |this, _, _, cx| this.show_section(target, cx)),
                        )
                    }),
                ),
            )
            .child(div().flex_1())
            .children(note.map(|note| {
                div()
                    .text_size(px(12.))
                    .text_color(color(theme::TEXT_TERTIARY))
                    .child(note)
            }))
            .when(section == Section::Schema, |row| {
                row.child(
                    chrome::icon_button(
                        "collection-resample",
                        AppIcon::Reload,
                        "Sample Again",
                        resample,
                    )
                    .when(resample, |b| {
                        b.on_click(cx.listener(|this, _, _, cx| {
                            this.load_schema(true, cx);
                            this.load_info(cx);
                        }))
                    }),
                )
            })
            .when(section == Section::Indexes, |row| {
                row.child(
                    chrome::icon_button("collection-reread", AppIcon::Reload, "Reload", reread)
                        .when(reread, |b| {
                            b.on_click(cx.listener(|this, _, _, cx| this.load_info(cx)))
                        }),
                )
            })
    }

    /// The indexes section.
    fn render_indexes(&self, cx: &mut Context<Self>) -> AnyElement {
        let mono = cx.theme().mono_font_family.clone();
        match &self.info {
            Read::Ready(_) if self.kind == ContainerKind::View => {
                placeholder("A view has no indexes of its own: it reads through its collection's")
            }
            Read::Ready(info) => {
                collection_sections::index_view(&info.indexes, self.language(), mono)
            }
            Read::Failed(error) => placeholder(error.clone()),
            Read::Idle | Read::Pending => placeholder("Reading the indexes…"),
        }
    }

    /// The schema section: what the documents hold, beside what the
    /// engine declares.
    fn render_schema(&self, cx: &mut Context<Self>) -> AnyElement {
        let mono = cx.theme().mono_font_family.clone();
        match (&self.schema, &self.info) {
            (Read::Failed(error), _) => placeholder(error.clone()),
            (Read::Ready(sampled), Read::Ready(info)) => collection_sections::schema_view(
                sampled,
                info.validator.as_ref(),
                self.language(),
                mono,
            ),
            // What the engine declares could not be read: the sample stands.
            (Read::Ready(sampled), Read::Failed(_)) => {
                collection_sections::schema_view(sampled, None, self.language(), mono)
            }
            _ => placeholder("Sampling the collection…"),
        }
    }
}

impl Render for CollectionTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.section != Section::Documents {
            let section = match self.section {
                Section::Schema => self.render_schema(cx),
                _ => self.render_indexes(cx),
            };
            return v_flex()
                .size_full()
                .key_context("CollectionTab")
                .track_focus(&self.focus_handle)
                .child(self.render_sections(cx))
                .child(div().flex_1().min_h_0().child(section))
                .into_any_element();
        }
        let content: AnyElement = match &self.session {
            SessionState::Connecting => placeholder("Connecting…"),
            SessionState::Failed(error) => placeholder(error.clone()),
            SessionState::Ready(_) => h_flex()
                .size_full()
                .child(
                    div()
                        .flex_1()
                        .h_full()
                        .min_w_0()
                        .child(DataTable::new(&self.grid).stripe(true).bordered(false)),
                )
                .when(self.selected.is_some() || self.editing.is_some(), |row| {
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
        };
        v_flex()
            .size_full()
            .key_context("CollectionTab")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &ReloadCollection, _, cx| {
                if !this.loading() {
                    this.reload(cx);
                }
            }))
            .child(self.render_sections(cx))
            .child(self.render_bar(cx))
            .child(div().flex_1().min_h_0().child(content))
            .children(self.render_changes(cx))
            .child(self.render_footer(cx))
            .into_any_element()
    }
}

/// The collection tab's items in a row's menu, after the copy items: the
/// row's document copied three ways, and a filter by the cell's value, read
/// from the document so its type is kept.
fn row_menu(tab: &WeakEntity<CollectionTab>, menu: PopupMenu, cell: &Cell, cx: &App) -> PopupMenu {
    let Some(state) = tab.upgrade() else {
        return menu;
    };
    let state = state.read(cx);
    let rows = state.rows.borrow();
    let Some(document) = rows.document(cell.row) else {
        return menu;
    };
    let render = state.language().render;
    let literal = render(&Value::Object(document.clone()), Style::Pretty);
    let ejson =
        |mode| serde_json::to_string_pretty(&document_ejson(document, mode)).unwrap_or_default();
    let row = cell.row;
    let value = document.get(cell.column_name());
    let filter_label = match value {
        None => "Filter by Missing Field",
        Some(_) => "Filter by This Value",
    };
    let filter = (state.language().field_filter)(&FieldPath::field(cell.column_name()), value);
    let editable = state.edits_offered();
    let menu = menu.item(item("Show Document", tab, move |tab, window, cx| {
        tab.select_row(row, window, cx)
    }));
    let menu = match editable {
        true => menu
            .item(
                item("Edit Document", tab, move |tab, window, cx| {
                    tab.edit_row(row, window, cx)
                })
                .icon(AppIcon::Pencil),
            )
            .item(item("Duplicate Document", tab, move |tab, window, cx| {
                tab.duplicate_row(row, window, cx)
            }))
            .item(
                item("Delete Document", tab, move |tab, _, cx| {
                    tab.delete_row(row, cx)
                })
                .icon(AppIcon::Trash),
            )
            .separator(),
        false => menu,
    };
    let menu = menu
        .item(copy("Copy Document", literal))
        .item(copy(
            "Copy Document as Relaxed Extended JSON",
            ejson(Ejson::Relaxed),
        ))
        .item(copy(
            "Copy Document as Canonical Extended JSON",
            ejson(Ejson::Canonical),
        ));
    match filter {
        Some(filter) => menu.separator().item(
            item(filter_label, tab, move |tab, window, cx| {
                tab.filter_by(filter.clone(), window, cx)
            })
            .icon(AppIcon::Filter),
        ),
        None => menu,
    }
}

/// What an export writes, by the file's extension: one JSON array for
/// `.json`, a document a line otherwise.
pub fn export_format(path: &Path) -> DocFormat {
    match path.extension().map(|e| e.to_string_lossy().to_lowercase()) {
        Some(extension) if extension == "json" => DocFormat::JsonArray,
        _ => DocFormat::Ndjson,
    }
}

/// An export being written, and how many documents went in so far.
pub(crate) struct ExportWriter {
    writer: DocumentWriter<BufWriter<File>>,
    pub(crate) written: usize,
}

impl ExportWriter {
    pub(crate) fn finish(self) -> io::Result<()> {
        self.writer.finish()?.flush()
    }
}

pub(crate) fn open_export(path: &Path) -> io::Result<ExportWriter> {
    let out = BufWriter::new(File::create(path)?);
    Ok(ExportWriter {
        writer: DocumentWriter::new(out, export_format(path), Ejson::Relaxed),
        written: 0,
    })
}

pub(crate) fn write_batch(export: &mut ExportWriter, batch: &[Document]) -> io::Result<()> {
    for document in batch {
        export.writer.write(document)?;
        export.written += 1;
    }
    Ok(())
}

/// Writes batches of documents to `path` with the export's own steps, the
/// format read from its extension; returns how many it wrote.
#[cfg(test)]
fn write_export(
    batches: impl IntoIterator<Item = Vec<Document>>,
    path: &Path,
) -> io::Result<usize> {
    let mut export = open_export(path)?;
    for batch in batches {
        write_batch(&mut export, &batch)?;
    }
    let written = export.written;
    export.finish()?;
    Ok(written)
}

/// The page a bar asks for: its filter and projection as typed (the
/// session checks them), its sort read into fields and directions, and its
/// limit, 100 when left empty.
pub fn browse_spec(
    bar: &BarText,
    page: Option<Continuation>,
    parse: fn(&str) -> Result<Document, ParseError>,
) -> Result<BrowseSpec, String> {
    let text = |text: &str| Some(text.trim().to_string()).filter(|text| !text.is_empty());
    let sort = match text(&bar.sort) {
        None => Vec::new(),
        Some(sort) => {
            let document =
                parse(&sort).map_err(|error| format!("Sort: {}", error.located(&sort)))?;
            document
                .0
                .into_iter()
                .map(|(field, value)| match direction(&value) {
                    Some(direction) => Ok((FieldPath::parse(&field), direction)),
                    None => Err(format!("Sort: {field} must be 1 or -1")),
                })
                .collect::<Result<_, _>>()?
        }
    };
    let limit = match bar.limit.trim() {
        "" => DEFAULT_LIMIT,
        limit => match limit.replace([',', '_'], "").parse::<usize>() {
            Ok(limit) if limit > 0 => limit,
            _ => return Err(format!("Limit: {limit} is not a number of documents")),
        },
    };
    Ok(BrowseSpec {
        filter: text(&bar.filter),
        quick: Vec::new(),
        sort,
        projection: text(&bar.projection),
        limit,
        after: page,
    })
}

/// What is wrong with a bar, if anything, as it would be refused: text the
/// language cannot read, heavy work outside development, a sort or a limit
/// that is not one.
pub fn check_bar(language: &Language, bar: &BarText, heavy: Heavy) -> Option<String> {
    for (name, text) in [("Filter", &bar.filter), ("Projection", &bar.projection)] {
        if text.trim().is_empty() {
            continue;
        }
        match (language.check_filter)(text) {
            Err(error) => return Some(format!("{name}: {}", error.located(text))),
            Ok(checked) if checked.heavy && heavy == Heavy::Refuse => {
                return Some(Rejection::Heavy(format!("The {}", name.to_lowercase())).to_string());
            }
            Ok(_) => {}
        }
    }
    browse_spec(bar, None, language.parse_document).err()
}

/// `1` or `-1`, however the number is written.
fn direction(value: &Value) -> Option<Direction> {
    let number = match value {
        Value::Int32(n) => f64::from(*n),
        Value::Int64(n) => *n as f64,
        Value::Double(x) => *x,
        _ => return None,
    };
    if number == 1.0 {
        Some(Direction::Ascending)
    } else if number == -1.0 {
        Some(Direction::Descending)
    } else {
        None
    }
}

/// The column the grid marks as sorted: the sort's only field, when it is
/// a top-level one.
fn header_sort(spec: &BrowseSpec) -> Option<(String, Direction)> {
    match spec.sort.as_slice() {
        [(path, direction)] if path.0.len() == 1 => Some((path.dotted(), *direction)),
        _ => None,
    }
}

fn describe_count(count: Count) -> String {
    match count {
        Count::Exact(n) => plural_documents(n),
        Count::AtLeast(n) => format!("more than {}", plural_documents(n)),
        Count::Unknown => "an unknown number (counting took too long)".to_string(),
    }
}

pub(crate) fn plural_documents(n: u64) -> String {
    match n {
        1 => "1 document".to_string(),
        n => format!("{} documents", thousands(n)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Document, ParseError> {
        (kuery_mongo::LANGUAGE.parse_document)(text)
    }

    fn bar(filter: &str, sort: &str, limit: &str) -> BarText {
        BarText {
            filter: filter.into(),
            projection: String::new(),
            sort: sort.into(),
            limit: limit.into(),
        }
    }

    #[test]
    fn an_empty_bar_reads_the_first_hundred_documents() {
        let spec = browse_spec(&BarText::default(), None, parse).unwrap();
        assert_eq!(
            spec,
            BrowseSpec {
                limit: DEFAULT_LIMIT,
                ..BrowseSpec::default()
            }
        );
        let spaces = browse_spec(&bar("  ", " ", " "), None, parse).unwrap();
        assert_eq!(spaces.filter, None);
    }

    #[test]
    fn reads_the_sort_limit_and_where_the_page_starts() {
        let page = Some(Continuation::Skip(100));
        let spec = browse_spec(
            &bar("{status: \"paid\"}", "{total: -1, \"items.0\": 1}", "1,000"),
            page.clone(),
            parse,
        )
        .unwrap();
        assert_eq!(spec.filter.as_deref(), Some("{status: \"paid\"}"));
        assert_eq!(
            spec.sort,
            [
                (FieldPath::field("total"), Direction::Descending),
                (FieldPath::parse("items.0"), Direction::Ascending),
            ]
        );
        assert_eq!(spec.limit, 1000);
        assert_eq!(spec.after, page);
        assert_eq!(header_sort(&spec), None);
    }

    #[test]
    fn refuses_what_is_not_a_limit_or_a_sort() {
        assert_eq!(
            browse_spec(&bar("", "", "abc"), None, parse).unwrap_err(),
            "Limit: abc is not a number of documents"
        );
        assert!(browse_spec(&bar("", "", "0"), None, parse).is_err());
        assert_eq!(
            browse_spec(&bar("", "{total: \"down\"}", ""), None, parse).unwrap_err(),
            "Sort: total must be 1 or -1"
        );
        assert!(
            browse_spec(&bar("", "{total: ", ""), None, parse)
                .unwrap_err()
                .starts_with("Sort: line 1, column")
        );
    }

    #[test]
    fn checks_the_bar_as_typed() {
        let language = &kuery_mongo::LANGUAGE;
        assert_eq!(
            check_bar(language, &bar("{a: 1}", "", ""), Heavy::Refuse),
            None
        );
        let broken = check_bar(language, &bar("{a: ", "", ""), Heavy::Refuse).unwrap();
        assert!(broken.starts_with("Filter: line 1, column"), "{broken}");
        let javascript = bar("{$where: \"this.a > 1\"}", "", "");
        let refused = check_bar(language, &javascript, Heavy::Refuse).unwrap();
        assert!(
            refused.starts_with("The filter runs heavy work"),
            "{refused}"
        );
        assert_eq!(check_bar(language, &javascript, Heavy::Allow), None);
    }

    #[test]
    fn exports_every_batch_as_ndjson_or_one_array() {
        let dir = std::env::temp_dir().join(format!("kuery-export-{}", crate::project::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let order = |id: i32| {
            Document(vec![
                ("_id".into(), Value::Int32(id)),
                ("total".into(), Value::Decimal("9.99".into())),
                ("big".into(), Value::Int64(9_007_199_254_740_993)),
            ])
        };
        let batches = || vec![vec![order(1), order(2)], vec![order(3)]];

        let lines = dir.join("orders.ndjson");
        assert_eq!(write_export(batches(), &lines).unwrap(), 3);
        let text = std::fs::read_to_string(&lines).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert_eq!(
            text.lines().next(),
            Some(r#"{"_id":1,"total":{"$numberDecimal":"9.99"},"big":9007199254740993}"#)
        );

        let array = dir.join("orders.JSON");
        assert_eq!(export_format(&array), DocFormat::JsonArray);
        assert_eq!(write_export(batches(), &array).unwrap(), 3);
        let read: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&array).unwrap()).unwrap();
        assert_eq!(read.as_array().map(Vec::len), Some(3));
        assert_eq!(export_format(Path::new("orders.txt")), DocFormat::Ndjson);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn counts_and_sorted_columns_read_as_shown() {
        assert_eq!(describe_count(Count::Exact(1)), "1 document");
        assert_eq!(describe_count(Count::Exact(1234)), "1,234 documents");
        assert_eq!(
            describe_count(Count::AtLeast(10_000)),
            "more than 10,000 documents"
        );
        let sorted = BrowseSpec {
            sort: vec![(FieldPath::field("total"), Direction::Ascending)],
            ..BrowseSpec::default()
        };
        assert_eq!(
            header_sort(&sorted),
            Some(("total".to_string(), Direction::Ascending))
        );
    }

    /// A collection tab on `shop.orders` of the dev MongoDB, in a window.
    fn open_orders(
        cx: &mut gpui::TestAppContext,
    ) -> (gpui_kit::AnyWindowHandle, Entity<CollectionTab>) {
        open_collection(cx, &["shop", "orders"])
    }

    /// A session of the dev MongoDB that may write.
    fn dev_session() -> DocumentSession {
        use std::sync::Arc;

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
        futures::executor::block_on(DocumentSession::open(opener, Access::ReadWrite)).unwrap()
    }

    fn open_collection(
        cx: &mut gpui::TestAppContext,
        path: &[&str],
    ) -> (gpui_kit::AnyWindowHandle, Entity<CollectionTab>) {
        use std::sync::Arc;

        use futures::executor::block_on;
        use gpui_kit::{Bounds, Point, WindowBounds, WindowOptions, size};
        use kuery_core::document::session::DocumentOpener;
        use kuery_core::session::Access;

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
        let catalog = block_on(DocumentSession::open(opener, Access::ReadWrite)).unwrap();
        let db = DocDb::new(
            "local".into(),
            Environment::Development,
            catalog,
            "127.0.0.1:27019/".into(),
            Vec::new(),
            None,
        );
        cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(1200.), px(800.)),
                })),
                ..Default::default()
            };
            let (window, tab) = gpui_kit::open_window(options, cx, |window, cx| {
                let collection = CollectionRef::new(path);
                cx.new(|cx| {
                    CollectionTab::new(
                        db,
                        collection,
                        ContainerKind::Collection,
                        std::env::temp_dir(),
                        window,
                        cx,
                    )
                })
            })
            .unwrap();
            (window, tab)
        })
    }

    /// Runs the UI until `done` holds or a few seconds pass.
    fn run_until(
        cx: &mut gpui::TestAppContext,
        tab: &Entity<CollectionTab>,
        done: impl Fn(&CollectionTab) -> bool,
    ) {
        for _ in 0..500 {
            cx.run_until_parked();
            if tab.read_with(cx, |tab, _| done(tab)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the collection tab never got there");
    }

    fn settled(tab: &CollectionTab) -> bool {
        matches!(tab.status, Status::Loaded { .. } | Status::Failed(_))
            && matches!(tab.count, CountState::Done(_) | CountState::Failed(_))
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn pages_through_orders_with_their_late_and_absent_fields(cx: &mut gpui::TestAppContext) {
        let (window, tab) = open_orders(cx);
        run_until(cx, &tab, settled);
        tab.read_with(cx, |tab, cx| {
            assert!(matches!(tab.status, Status::Loaded { documents: 100, .. }));
            assert!(matches!(tab.count, CountState::Done(Count::Exact(500))));
            let rows = tab.rows.borrow();
            let coupon = rows.columns().iter().position(|c| c == "coupon").unwrap();
            let grid = tab.grid.read(cx).delegate();
            assert_eq!(grid.page().rows(), 100);
            assert_eq!(grid.page().columns(), rows.columns());
            // Order 1 has no coupon, order 3 a null one, order 7 a code.
            let number = |row: usize| rows.document(row).and_then(|d| d.get("number")).cloned();
            assert_eq!(number(0), Some(Value::Int32(1)));
            assert_eq!(grid.original(0, coupon), Some(None));
            assert!(rows.is_absent(0, coupon));
            assert_eq!(grid.original(2, coupon), Some(None));
            assert!(!rows.is_absent(2, coupon));
            assert_eq!(grid.original(6, coupon), Some(Some("SAVE7")));
            assert!(tab.next.is_some());
        });

        tab.update(cx, |tab, cx| tab.go_next(cx));
        run_until(cx, &tab, settled);
        tab.read_with(cx, |tab, _| {
            assert_eq!(tab.pages.len(), 2);
            let rows = tab.rows.borrow();
            let first = rows.document(0).and_then(|d| d.get("number")).cloned();
            assert_eq!(first, Some(Value::Int32(101)));
        });

        cx.update_window(window, |_, window, cx| {
            tab.update(cx, |tab, cx| {
                tab.set_bar(
                    "{status: \"paid\"}".into(),
                    "{number: 1, status: 1}".into(),
                    "{number: -1}".into(),
                    window,
                    cx,
                )
            })
        })
        .unwrap();
        run_until(cx, &tab, |tab| {
            settled(tab) && tab.shown.as_ref().is_some_and(|spec| !spec.sort.is_empty())
        });
        tab.read_with(cx, |tab, _| {
            assert_eq!(tab.pages.len(), 1);
            assert!(matches!(tab.count, CountState::Done(Count::Exact(125))));
            let rows = tab.rows.borrow();
            assert_eq!(rows.columns(), ["_id", "number", "status"]);
            let first = rows.document(0).and_then(|d| d.get("number")).cloned();
            assert_eq!(first, Some(Value::Int32(497)));
            assert!(matches!(tab.next, Some(Continuation::Skip(100))));
        });
    }

    /// Puts `filter` in the bar, with no projection or sort, and finds.
    fn find_with(
        cx: &mut gpui::TestAppContext,
        window: gpui_kit::AnyWindowHandle,
        tab: &Entity<CollectionTab>,
        filter: &str,
    ) {
        cx.update_window(window, |_, window, cx| {
            tab.update(cx, |tab, cx| {
                tab.set_bar(filter.into(), String::new(), String::new(), window, cx)
            })
        })
        .unwrap();
    }

    /// Each order takes 10 ms to match: a page of 100 a second, all 500
    /// five seconds.
    const SLOW: &str = "{$where: \"sleep(10); return true\"}";

    fn shown_filter(tab: &CollectionTab) -> Option<&str> {
        tab.shown.as_ref().and_then(|spec| spec.filter.as_deref())
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn a_slow_count_never_holds_up_a_page(cx: &mut gpui::TestAppContext) {
        let (window, tab) = open_orders(cx);
        run_until(cx, &tab, settled);
        find_with(cx, window, &tab, SLOW);
        run_until(cx, &tab, |tab| {
            shown_filter(tab).is_some() && matches!(tab.status, Status::Loaded { .. })
        });
        tab.read_with(cx, |tab, _| {
            assert!(matches!(tab.status, Status::Loaded { documents: 100, .. }));
            assert!(matches!(tab.count, CountState::Counting));
        });
        tab.update(cx, |tab, cx| tab.go_next(cx));
        run_until(cx, &tab, |tab| {
            tab.pages.len() == 2 && matches!(tab.status, Status::Loaded { .. })
        });
        tab.read_with(cx, |tab, _| {
            assert!(matches!(tab.status, Status::Loaded { documents: 100, .. }));
            assert!(matches!(tab.count, CountState::Counting));
        });
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn the_tab_pages_while_an_export_runs_and_stops_it(cx: &mut gpui::TestAppContext) {
        let (window, tab) = open_orders(cx);
        run_until(cx, &tab, settled);
        find_with(cx, window, &tab, SLOW);
        run_until(cx, &tab, |tab| {
            shown_filter(tab).is_some() && matches!(tab.status, Status::Loaded { .. })
        });
        let path =
            std::env::temp_dir().join(format!("kuery-export-{}.ndjson", crate::project::new_id()));
        tab.update(cx, |tab, cx| tab.export_to(path.clone(), cx));
        run_until(cx, &tab, |tab| tab.export_session.is_some());

        // The export reads every slow order; the tab finds the paid ones
        // meanwhile.
        find_with(cx, window, &tab, "{status: \"paid\"}");
        run_until(cx, &tab, |tab| {
            shown_filter(tab).is_some_and(|filter| filter.contains("paid")) && settled(tab)
        });
        tab.read_with(cx, |tab, _| {
            assert!(matches!(tab.count, CountState::Done(Count::Exact(125))));
            assert!(
                matches!(tab.export, Some(ExportState::Running { .. })),
                "the export ended before the page came"
            );
        });

        tab.update(cx, |tab, cx| tab.stop_export(cx));
        tab.read_with(cx, |tab, _| {
            assert!(tab.export_session.is_none() && tab.export_task.is_none());
            match &tab.export {
                Some(ExportState::Failed(stopped)) => {
                    assert!(stopped.starts_with("Stopped the export after"), "{stopped}")
                }
                _ => panic!("the export did not stop"),
            }
        });
        let _ = std::fs::remove_file(path);
    }

    /// Runs a script that writes on `session`, to its end.
    fn write(session: &DocumentSession, script: &str) {
        use kuery_core::document::session::RunMode;

        let events = session.run(
            script.into(),
            RunMode::Write,
            Heavy::Refuse,
            RunOptions::default(),
        );
        futures::executor::block_on(async {
            while let Ok(event) = events.recv().await {
                match event {
                    DocEvent::Finished(_) => return,
                    DocEvent::Failed(error) => panic!("{script}: {error}"),
                    _ => {}
                }
            }
        });
    }

    /// Stages the edit of the loaded row whose `_id` is `id`, with `edit`
    /// applied to its literal.
    fn stage_edit(
        cx: &mut gpui::TestAppContext,
        window: gpui_kit::AnyWindowHandle,
        tab: &Entity<CollectionTab>,
        id: i32,
        edit: impl Fn(&str) -> String,
    ) {
        cx.update_window(window, |_, window, cx| {
            tab.update(cx, |tab, cx| {
                let row = tab
                    .rows
                    .borrow()
                    .documents()
                    .iter()
                    .position(|document| document.get("_id") == Some(&Value::Int32(id)))
                    .unwrap();
                tab.edit_row(row, window, cx);
                let text = tab.detail.read(cx).text(cx);
                tab.stage(&edit(&text), window, cx);
                assert!(tab.editing.is_none(), "the edit was not staged");
            })
        })
        .unwrap();
    }

    fn apply(
        cx: &mut gpui::TestAppContext,
        window: gpui_kit::AnyWindowHandle,
        tab: &Entity<CollectionTab>,
    ) {
        cx.update_window(window, |_, window, cx| {
            tab.update(cx, |tab, cx| tab.apply_staged(window, cx))
        })
        .unwrap();
        run_until(cx, tab, |tab| {
            matches!(tab.apply, ApplyState::Applied(_) | ApplyState::Failed(_))
                && matches!(tab.status, Status::Loaded { .. })
        });
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn collection_tab_edits_keep_types_and_detect_concurrent_changes(
        cx: &mut gpui::TestAppContext,
    ) {
        let setup = dev_session();
        write(
            &setup,
            r#"db.getSiblingDB("kuery_tab_edits").dropDatabase()"#,
        );
        write(
            &setup,
            r#"db.getSiblingDB("kuery_tab_edits").notes.insertMany([{_id: 1, qty: NumberLong("9007199254740993"), price: NumberDecimal("9.99"), note: "old"}, {_id: 2, qty: NumberLong(2)}, {_id: 3}])"#,
        );
        let (window, tab) = open_collection(cx, &["kuery_tab_edits", "notes"]);
        run_until(cx, &tab, settled);

        // An edit, a delete and an insert, staged and marked.
        stage_edit(cx, window, &tab, 1, |text| {
            text.replace(r#""old""#, r#""new""#)
                .replace(r#"NumberDecimal("9.99")"#, r#"NumberDecimal("10.50")"#)
        });
        tab.update(cx, |tab, cx| tab.delete_row(2, cx));
        insert(cx, window, &tab);
        cx.update_window(window, |_, window, cx| {
            tab.update(cx, |tab, cx| {
                tab.stage(r#"{_id: 4, qty: NumberLong(4)}"#, window, cx);
            })
        })
        .unwrap();
        tab.read_with(cx, |tab, _| {
            assert!(tab.has_pending());
            assert_eq!(tab.staging.len(), 3);
        });

        apply(cx, window, &tab);
        tab.read_with(cx, |tab, _| {
            assert!(
                matches!(&tab.apply, ApplyState::Applied(done) if done.starts_with("Applied 3 changes")),
                "{}",
                match &tab.apply {
                    ApplyState::Failed(error) => error.as_str(),
                    _ => "",
                }
            );
            assert!(!tab.has_pending());
            let rows = tab.rows.borrow();
            let ids: Vec<Option<&Value>> =
                rows.documents().iter().map(|d| d.get("_id")).collect();
            assert_eq!(
                ids,
                [Some(&Value::Int32(1)), Some(&Value::Int32(2)), Some(&Value::Int32(4))]
            );
            let edited = rows.document(0).unwrap();
            // Untouched NumberLong stays Int64, the decimal stays Decimal128.
            assert_eq!(edited.get("qty"), Some(&Value::Int64(9_007_199_254_740_993)));
            assert_eq!(edited.get("price"), Some(&Value::Decimal("10.50".into())));
            assert_eq!(edited.get("note"), Some(&Value::String("new".into())));
            // Audited as the statements: the diff, not whole documents.
            let [record] = tab.audited.as_slice() else {
                panic!("{:?}", tab.audited);
            };
            assert!(record.sql.contains("$set"), "{}", record.sql);
            assert!(!record.sql.contains("9007199254740993"), "{}", record.sql);
            assert!(record.sql.contains("deleteOne({_id: 3})"), "{}", record.sql);
            assert!(!record.failed);
        });

        // Someone else changes document 2 after it was loaded.
        stage_edit(cx, window, &tab, 2, |text| {
            text.replace(r#"NumberLong("2")"#, r#"NumberLong("3")"#)
        });
        write(
            &setup,
            r#"db.getSiblingDB("kuery_tab_edits").notes.updateOne({_id: 2}, {$set: {qty: NumberLong(5)}})"#,
        );
        apply(cx, window, &tab);
        tab.read_with(cx, |tab, _| {
            let ApplyState::Failed(error) = &tab.apply else {
                panic!("a stale edit applied");
            };
            assert!(
                error.contains(
                    "This document changed since you loaded it. Reload it and edit again."
                ),
                "{error}"
            );
            assert!(error.contains("Nothing was applied"), "{error}");
            // It stays staged, and the document keeps the other change.
            assert_eq!(tab.staging.len(), 1);
            assert!(tab.audited.last().unwrap().failed);
        });
        let kept = futures::executor::block_on(setup.count(
            CollectionRef::new(&["kuery_tab_edits", "notes"]),
            Some("{_id: 2, qty: NumberLong(5)}".into()),
            0,
            Heavy::Refuse,
        ));
        assert_eq!(kept, Ok(Count::Exact(1)));
        write(
            &setup,
            r#"db.getSiblingDB("kuery_tab_edits").dropDatabase()"#,
        );
    }

    /// The grid's row of the loaded document whose `_id` is `id`.
    /// Opens a new document, once its template is read.
    fn insert(
        cx: &mut gpui::TestAppContext,
        window: gpui_kit::AnyWindowHandle,
        tab: &Entity<CollectionTab>,
    ) {
        cx.update_window(window, |_, window, cx| {
            tab.update(cx, |tab, cx| tab.insert_document(window, cx))
        })
        .unwrap();
        run_until(cx, tab, |tab| matches!(tab.editing, Some(Editing::Insert)));
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn collection_tab_insert_never_opens_over_another_editor(cx: &mut gpui::TestAppContext) {
        let (window, tab) = open_collection(cx, &["shop", "products"]);
        run_until(cx, &tab, settled);
        let in_window =
            |cx: &mut gpui::TestAppContext,
             f: &dyn Fn(&mut CollectionTab, &mut Window, &mut Context<CollectionTab>)| {
                cx.update_window(window, |_, window, cx| {
                    tab.update(cx, |tab, cx| f(tab, window, cx))
                })
                .unwrap();
            };
        // Edit asked for while the new document's schema is read: the
        // edit stays open.
        in_window(cx, &|tab, window, cx| {
            tab.insert_document(window, cx);
            assert!(tab.insert_task.is_some());
            tab.edit_row(0, window, cx);
        });
        tab.read_with(cx, |tab, _| assert!(tab.insert_task.is_none()));
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(300));
        cx.run_until_parked();
        tab.read_with(cx, |tab, _| {
            assert!(matches!(tab.editing, Some(Editing::Edit(_))));
        });
        in_window(cx, &|tab, window, cx| tab.cancel_edit(window, cx));

        // The same for a copy.
        in_window(cx, &|tab, window, cx| {
            tab.insert_document(window, cx);
            tab.duplicate_row(0, window, cx);
            assert!(tab.insert_task.is_none());
        });
        in_window(cx, &|tab, window, cx| tab.cancel_edit(window, cx));

        // An apply that started meanwhile keeps the editor closed.
        in_window(cx, &|tab, window, cx| {
            tab.insert_document(window, cx);
            tab.apply = ApplyState::Applying;
        });
        run_until(cx, &tab, |tab| tab.insert_task.is_none());
        tab.read_with(cx, |tab, _| assert!(tab.editing.is_none()));
        tab.update(cx, |tab, _| tab.apply = ApplyState::Idle);

        // With nothing else open, it opens as before.
        insert(cx, window, &tab);
    }

    fn row_of(tab: &CollectionTab, id: i32) -> usize {
        tab.rows
            .borrow()
            .documents()
            .iter()
            .position(|document| document.get("_id") == Some(&Value::Int32(id)))
            .unwrap()
    }

    fn failed(tab: &CollectionTab) -> &str {
        match &tab.apply {
            ApplyState::Failed(error) => error,
            _ => "",
        }
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn collection_tab_changes_apply_only_to_documents_as_loaded(cx: &mut gpui::TestAppContext) {
        let setup = dev_session();
        let db = r#"db.getSiblingDB("kuery_tab_checks")"#;
        write(&setup, &format!("{db}.dropDatabase()"));
        write(
            &setup,
            &format!(
                "{db}.notes.insertMany([{{_id: 1, n: 1, ratio: NaN}}, {{_id: 2, n: 1}}, {{_id: 3, a: 1, b: 1}}])"
            ),
        );
        let (window, tab) = open_collection(cx, &["kuery_tab_checks", "notes"]);
        run_until(cx, &tab, settled);

        // A document holding NaN, which is not equal to itself, applies.
        stage_edit(cx, window, &tab, 1, |text| text.replace("n: 1,", "n: 2,"));
        apply(cx, window, &tab);
        tab.read_with(cx, |tab, _| {
            assert!(
                matches!(&tab.apply, ApplyState::Applied(_)),
                "{}",
                failed(tab)
            );
        });

        // A delete of a document that gained a field since is refused.
        cx.update_window(window, |_, _, cx| {
            tab.update(cx, |tab, cx| tab.delete_row(row_of(tab, 2), cx))
        })
        .unwrap();
        write(
            &setup,
            &format!("{db}.notes.updateOne({{_id: 2}}, {{$set: {{theirs: true}}}})"),
        );
        apply(cx, window, &tab);
        tab.read_with(cx, |tab, _| {
            assert!(
                failed(tab).contains("This document changed since you loaded it"),
                "{}",
                failed(tab)
            );
            assert_eq!(tab.staging.len(), 1);
            // The audit keeps to the key, not the whole document.
            let record = tab.audited.last().unwrap();
            assert!(
                record.sql.ends_with("deleteOne({_id: 2})"),
                "{}",
                record.sql
            );
        });
        cx.update_window(window, |_, _, cx| tab.update(cx, |tab, cx| tab.discard(cx)))
            .unwrap();

        // Someone else changes a document whose edit is staged, and the
        // page reloads: editing it again is refused, since the editor would
        // show the edit over what it held before. The staged edit still
        // expects only what it changes, so it applies and keeps theirs.
        stage_edit(cx, window, &tab, 3, |text| text.replace("a: 1", "a: 2"));
        write(
            &setup,
            &format!("{db}.notes.updateOne({{_id: 3}}, {{$set: {{b: 5}}}})"),
        );
        tab.update(cx, |tab, cx| tab.reload(cx));
        run_until(cx, &tab, |tab| matches!(tab.status, Status::Loaded { .. }));
        cx.update_window(window, |_, window, cx| {
            tab.update(cx, |tab, cx| {
                let row = row_of(tab, 3);
                tab.edit_row(row, window, cx);
                assert!(tab.editing.is_none(), "a stale document opened to edit");
                assert_eq!(failed(tab), document_edits::STALE);
            })
        })
        .unwrap();
        apply(cx, window, &tab);
        tab.read_with(cx, |tab, _| {
            assert!(
                matches!(&tab.apply, ApplyState::Applied(_)),
                "{}",
                failed(tab)
            );
        });
        let kept = futures::executor::block_on(setup.count(
            CollectionRef::new(&["kuery_tab_checks", "notes"]),
            Some("{_id: 3, a: 2, b: 5}".into()),
            0,
            Heavy::Refuse,
        ));
        assert_eq!(kept, Ok(Count::Exact(1)), "their change was undone");

        // After an apply whose outcome is unknown, Apply waits for a reload.
        insert(cx, window, &tab);
        cx.update_window(window, |_, window, cx| {
            tab.update(cx, |tab, cx| {
                tab.stage("{n: 4}", window, cx);
                tab.reload_first = true;
            })
        })
        .unwrap();
        let audited = tab.read_with(cx, |tab, _| tab.audited.len());
        apply(cx, window, &tab);
        tab.read_with(cx, |tab, _| {
            assert_eq!(failed(tab), document_write::RELOAD_FIRST);
            assert_eq!(tab.audited.len(), audited, "it tried to apply");
        });
        tab.update(cx, |tab, cx| tab.reload(cx));
        run_until(cx, &tab, |tab| matches!(tab.status, Status::Loaded { .. }));
        tab.read_with(cx, |tab, _| assert!(!tab.reload_first));
        write(&setup, &format!("{db}.dropDatabase()"));
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn collection_tab_views_offer_no_edits(cx: &mut gpui::TestAppContext) {
        let (_window, tab) = open_orders(cx);
        run_until(cx, &tab, settled);
        tab.read_with(cx, |tab, _| assert!(tab.edits_offered()));
        tab.update(cx, |tab, _| tab.kind = ContainerKind::View);
        tab.update(cx, |tab, cx| tab.delete_row(0, cx));
        tab.read_with(cx, |tab, _| {
            assert!(!tab.edits_offered());
            assert!(!tab.has_pending());
            assert!(matches!(tab.apply, ApplyState::Failed(_)));
        });
    }

    fn schema_read(tab: &CollectionTab) -> bool {
        !matches!(tab.schema, Read::Idle | Read::Pending)
            && !matches!(tab.info, Read::Idle | Read::Pending)
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn collection_tab_samples_the_schema_and_inserts_from_it(cx: &mut gpui::TestAppContext) {
        let (window, tab) = open_collection(cx, &["shop", "products"]);
        run_until(cx, &tab, settled);
        tab.update(cx, |tab, cx| tab.show_section(Section::Schema, cx));
        run_until(cx, &tab, schema_read);
        tab.read_with(cx, |tab, _| {
            let Read::Ready(sampled) = &tab.schema else {
                panic!("not sampled");
            };
            // Fewer documents than the sample asks for: all of them.
            assert_eq!(sampled.label(), "Sampled from 40 documents");
            let price = sampled
                .schema
                .fields
                .iter()
                .find(|field| field.path == "price")
                .unwrap();
            assert!(price.mixed());
            assert_eq!(price.label(), "Double 90 %, String 10 %");
            let Read::Ready(info) = &tab.info else {
                panic!("no collection info");
            };
            assert!(info.validator.is_none());
            // Kept for every tab on the connection.
            assert!(tab.db.cached_schema(&tab.collection).is_some());
        });

        // A new document starts from the schema, without the key.
        insert(cx, window, &tab);
        tab.read_with(cx, |tab, cx| {
            let text = tab.detail.read(cx).text(cx);
            assert!(text.contains("price: 0"), "{text}");
            assert!(text.contains("stock: 0"), "{text}");
            assert!(!text.contains("_id"), "{text}");
        });
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn collection_tab_samples_a_collection_once_and_keeps_no_empty_sample(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_window, tab) = open_collection(cx, &["shop", "products"]);
        run_until(cx, &tab, settled);
        let db = tab.read_with(cx, |tab, _| tab.db.clone());
        let products = CollectionRef::new(&["shop", "products"]);
        // Asked for twice before the first is done: sampled once.
        let first = db.schema(products.clone(), true);
        let second = db.schema(products.clone(), false);
        let (first, second) = futures::executor::block_on(futures::future::join(first, second));
        let (first, second) = (first.unwrap(), second.unwrap());
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        assert!(db.cached_schema(&products).is_some());
        // A collection that does not exist yet samples nothing, and that
        // is not kept for when it does.
        let missing = CollectionRef::new(&["shop", "kuery_not_there"]);
        let sampled = futures::executor::block_on(db.schema(missing.clone(), false)).unwrap();
        assert_eq!(sampled.schema.documents, 0);
        assert!(db.cached_schema(&missing).is_none());
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn collection_tab_shows_the_declared_schema_beside_the_sample(cx: &mut gpui::TestAppContext) {
        let (_window, tab) = open_collection(cx, &["shop", "customers"]);
        run_until(cx, &tab, settled);
        tab.update(cx, |tab, cx| tab.show_section(Section::Schema, cx));
        run_until(cx, &tab, schema_read);
        tab.read_with(cx, |tab, _| {
            let Read::Ready(sampled) = &tab.schema else {
                panic!("not sampled");
            };
            let tags = sampled
                .schema
                .fields
                .iter()
                .find(|field| field.path == "tags")
                .unwrap();
            assert_eq!(tags.presence(sampled.schema.documents), 0.25);
            let Read::Ready(info) = &tab.info else {
                panic!("no collection info");
            };
            let declared = info.validator.as_ref().expect("the $jsonSchema validator");
            assert!(declared.get("required").is_some());
        });
    }

    #[gpui::test]
    #[ignore = "needs the dev MongoDB"]
    fn collection_tab_lists_indexes_with_their_usage(cx: &mut gpui::TestAppContext) {
        let (_window, tab) = open_orders(cx);
        run_until(cx, &tab, settled);
        tab.update(cx, |tab, cx| tab.show_section(Section::Indexes, cx));
        run_until(cx, &tab, |tab| {
            !matches!(tab.info, Read::Idle | Read::Pending)
        });
        tab.read_with(cx, |tab, _| {
            let Read::Ready(info) = &tab.info else {
                panic!("the indexes were not read");
            };
            let names: Vec<&str> = info.indexes.iter().map(|i| i.name.as_str()).collect();
            assert_eq!(names, ["_id_", "status_1_date_-1", "customer_1"]);
            // As root, $indexStats says how often each is used.
            assert!(info.indexes.iter().all(|index| index.usage.is_some()));
            assert!(info.indexes.iter().all(|index| index.size.is_some()));
            // Indexes need no sample.
            assert!(matches!(tab.schema, Read::Idle));
        });
    }
}
