//! A Redis key in a tab: its type, time to live, size and encoding, and its
//! value. A string shows as text (JSON pretty-printed, if it is JSON);
//! hashes, lists, sets, sorted sets and streams fill the results grid a page
//! at a time, so the Profile of their values works like any other result.
//!
//! Changes are staged first, like row changes in a table tab: a new string
//! value, the time to live, a new name, deleting the key, and adding,
//! changing or removing entries (from the grid's row menu). Apply runs them
//! as Redis commands once the environment's policy is met (see
//! [`redis_write::approve`]), each one logged to the audit log. On
//! production nothing can be staged until writes are allowed for the
//! connection session.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::input::{Editor, EditorState, Input, InputState};
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, FocusHandle, Focusable,
    FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, Window, div, px,
};
use kuery_core::redis::{join_command, split_command};
use kuery_redis::{Content, KeyInfo, KeyKind, Page, Reply, Table, display_bytes};

use crate::assets::AppIcon;
use crate::chrome;
use crate::connection::RedisDb;
use crate::context_menus::item_icon;
use crate::redis_write::{self, Planned};
use crate::results::{ResultsDelegate, RowMenu};
use crate::theme::{self, color};
use crate::ui::{format_bytes, thousands};

/// Entries read per page.
const PAGE: usize = 500;
/// List items being removed are first set to this, then removed together
/// with `LREM`, so the other indexes do not move in between.
const TOMBSTONE: &str = "__kuery_removed__";

enum Value {
    Loading,
    Text { bytes: Vec<u8>, truncated: bool },
    Rows,
    Unsupported(String),
    Failed(String),
}

/// A staged change.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Edit {
    /// A command that runs in the order it was staged.
    Command { args: Vec<String>, label: String },
    /// A list item to remove: set to [`TOMBSTONE`] before any push, then
    /// removed (see [`commands`]).
    RemoveListItem { index: u64, value: String },
}

impl Edit {
    fn label(&self) -> String {
        match self {
            Edit::Command { label, .. } => label.clone(),
            Edit::RemoveListItem { index, value } => format!("Remove item {index} ({value})"),
        }
    }
}

/// The form open under the toolbar, if any.
enum Form {
    /// A new string value.
    Value(Entity<EditorState>),
    /// Seconds to live.
    Ttl(Entity<InputState>),
    Rename(Entity<InputState>),
    /// Adds an entry, or changes the one in row `editing`.
    Entry {
        editing: Option<usize>,
        first: Entity<InputState>,
        second: Option<Entity<InputState>>,
        at_head: bool,
    },
}

pub struct KeyTab {
    db: RedisDb,
    database: i64,
    key: String,
    /// The project folder, for the audit log.
    project: PathBuf,
    info: Option<KeyInfo>,
    value: Value,
    /// Every entry read so far.
    rows: Table,
    table: Entity<TableState<ResultsDelegate>>,
    next: Option<Page>,
    /// JSON strings indented, or as stored.
    pretty: bool,
    pending: Vec<Edit>,
    form: Option<Form>,
    /// The last Apply's failure.
    apply_error: Option<String>,
    applying: bool,
    task: Option<Task<()>>,
    focus_handle: FocusHandle,
}

impl KeyTab {
    pub fn new(
        db: RedisDb,
        database: i64,
        key: String,
        project: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let table = cx.new(|cx| TableState::new(ResultsDelegate::default(), window, cx));
        let mut tab = Self {
            db,
            database,
            key,
            project,
            info: None,
            value: Value::Loading,
            rows: Table::default(),
            table,
            next: None,
            pretty: true,
            pending: Vec::new(),
            form: None,
            apply_error: None,
            applying: false,
            task: None,
            focus_handle: cx.focus_handle(),
        };
        tab.install_row_menu(cx);
        tab.reload(cx);
        tab
    }

    pub fn db(&self) -> &RedisDb {
        &self.db
    }

    pub fn database(&self) -> i64 {
        self.database
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    /// Whether there are staged changes not applied yet.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// For the status bar: how much of the value is loaded.
    pub fn status(&self) -> Option<String> {
        let total = self.info.as_ref()?.length?;
        match self.value {
            Value::Rows => Some(format!(
                "{} of {} loaded",
                thousands(self.rows.rows.len() as u64),
                thousands(total)
            )),
            _ => None,
        }
    }

    fn kind(&self) -> Option<KeyKind> {
        self.info.as_ref().map(|info| info.kind)
    }

    /// Whether changes can be staged: always, except on production before
    /// writes are allowed for the session.
    fn can_edit(&self) -> bool {
        self.db.allows_writes() && self.kind().is_some_and(|kind| kind != KeyKind::Missing)
    }

    /// Edit and Remove in the grid's row menu, while changes can be staged.
    fn install_row_menu(&mut self, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let menu: RowMenu = Rc::new(move |menu, row, cx| {
            let Some(tab) = this.upgrade() else {
                return menu;
            };
            let tab = tab.read(cx);
            if !tab.can_edit() {
                return menu;
            }
            // Stream entries can be removed, not changed.
            let changeable = tab.kind() != Some(KeyKind::Stream);
            menu.when(changeable, |menu| {
                menu.item(item_icon(
                    "Edit…",
                    AppIcon::Pencil,
                    &this,
                    move |this, window, cx| this.open_entry_form(Some(row), window, cx),
                ))
            })
            .item(item_icon(
                "Remove",
                AppIcon::Trash,
                &this,
                move |this, _, cx| this.remove_row(row, cx),
            ))
        });
        self.table
            .update(cx, |table, _| table.delegate_mut().set_row_menu(Some(menu)));
    }

    /// Reads the key's details and the first page of its value again.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let session = self.db.session.clone();
        let (database, key) = (self.database, self.key.clone());
        self.value = Value::Loading;
        self.task = Some(cx.spawn(async move |this, cx| {
            let info = session.key_info(database, key.clone()).await;
            let page = match &info {
                Ok(info) => Some(
                    session
                        .read_value(database, key, info.kind, Page::Start, PAGE)
                        .await,
                ),
                Err(_) => None,
            };
            this.update(cx, |this, cx| {
                this.task = None;
                this.rows = Table::default();
                this.next = None;
                match (info, page) {
                    (Ok(info), Some(Ok(page))) => {
                        this.info = Some(info);
                        this.next = page.next;
                        this.show(page.content, cx);
                    }
                    (Ok(info), Some(Err(error))) => {
                        this.info = Some(info);
                        this.value = Value::Failed(error.to_string());
                    }
                    (Err(error), _) => this.value = Value::Failed(error.to_string()),
                    (Ok(_), None) => {}
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn load_more(&mut self, cx: &mut Context<Self>) {
        let (Some(next), Some(info)) = (self.next.clone(), self.info.as_ref()) else {
            return;
        };
        if self.task.is_some() {
            return;
        }
        let read =
            self.db
                .session
                .read_value(self.database, self.key.clone(), info.kind, next, PAGE);
        self.task = Some(cx.spawn(async move |this, cx| {
            let page = read.await;
            this.update(cx, |this, cx| {
                this.task = None;
                match page {
                    Ok(page) => {
                        this.next = page.next;
                        this.show(page.content, cx);
                    }
                    Err(error) => this.value = Value::Failed(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn show(&mut self, content: Content, cx: &mut Context<Self>) {
        match content {
            Content::Text { bytes, truncated } => self.value = Value::Text { bytes, truncated },
            Content::Unsupported(text) => self.value = Value::Unsupported(text),
            Content::Rows(page) => {
                merge(&mut self.rows, page);
                let (columns, rows) = (self.rows.columns.clone(), &self.rows.rows);
                self.table.update(cx, |table, cx| {
                    table.delegate_mut().show(columns, rows);
                    table.refresh(cx);
                });
                self.value = Value::Rows;
            }
        }
    }

    /// The string value as shown: indented when it is JSON and `pretty`.
    fn text(&self) -> Option<(String, bool)> {
        let Value::Text { bytes, .. } = &self.value else {
            return None;
        };
        let json = serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .filter(|value| value.is_object() || value.is_array());
        Some(match json {
            Some(value) if self.pretty => (
                serde_json::to_string_pretty(&value).unwrap_or_default(),
                true,
            ),
            Some(_) => (display_bytes(bytes), true),
            None => (display_bytes(bytes), false),
        })
    }

    /// The string value as stored, when it can be edited as text: valid
    /// UTF-8 and read whole.
    fn editable_text(&self) -> Option<String> {
        match &self.value {
            Value::Text {
                bytes,
                truncated: false,
            } => String::from_utf8(bytes.clone()).ok(),
            _ => None,
        }
    }

    /// The name new changes are staged for: the key's, or the one a staged
    /// rename gives it (staged changes run in order).
    fn target_key(&self) -> String {
        self.pending
            .iter()
            .rev()
            .find_map(|edit| match edit {
                Edit::Command { args, .. } if args[0] == "RENAME" => args.get(2).cloned(),
                _ => None,
            })
            .unwrap_or_else(|| self.key.clone())
    }

    fn cell(&self, row: usize, column: &str) -> Option<String> {
        let index = self.rows.columns.iter().position(|c| c == column)?;
        self.rows.rows.get(row)?.get(index)?.clone()
    }

    // Staging -----------------------------------------------------------

    fn stage(&mut self, args: Vec<String>, label: String, cx: &mut Context<Self>) {
        self.pending.push(Edit::Command { args, label });
        self.apply_error = None;
        cx.notify();
    }

    fn input(
        &self,
        value: &str,
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let (value, placeholder) = (value.to_string(), placeholder.to_string());
        cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(placeholder)
                .default_value(value)
        })
    }

    fn open_value_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.editable_text() else {
            return;
        };
        let editor = cx.new(|cx| EditorState::new(window, cx).default_value(text));
        self.form = Some(Form::Value(editor));
        cx.notify();
    }

    fn open_ttl_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self
            .info
            .as_ref()
            .and_then(|info| info.ttl_ms)
            .map(|ms| (ms / 1000).to_string())
            .unwrap_or_default();
        self.form = Some(Form::Ttl(self.input(&current, "seconds", window, cx)));
        cx.notify();
    }

    fn open_rename_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.target_key();
        self.form = Some(Form::Rename(self.input(&key, "new key name", window, cx)));
        cx.notify();
    }

    /// The form to add an entry, or to change the one in `row`.
    fn open_entry_form(&mut self, row: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(kind) = self.kind() else {
            return;
        };
        if !self.can_edit() {
            return;
        }
        let value = |column: &str| {
            row.and_then(|row| self.cell(row, column))
                .unwrap_or_default()
        };
        let (first, second) = match kind {
            KeyKind::Hash => (
                self.input(&value("field"), "field", window, cx),
                Some(self.input(&value("value"), "value", window, cx)),
            ),
            KeyKind::List => (self.input(&value("value"), "value", window, cx), None),
            KeyKind::Set => (self.input(&value("member"), "member", window, cx), None),
            KeyKind::ZSet => (
                self.input(&value("member"), "member", window, cx),
                Some(self.input(&value("score"), "score", window, cx)),
            ),
            // Entries cannot be changed, only added or removed.
            KeyKind::Stream if row.is_none() => (
                self.input("", "field=value field=value …", window, cx),
                None,
            ),
            _ => return,
        };
        self.form = Some(Form::Entry {
            editing: row,
            first,
            second,
            at_head: false,
        });
        cx.notify();
    }

    /// Stages what the open form says, and closes it.
    fn stage_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = self.form.take() else {
            return;
        };
        let key = self.target_key();
        let text = |input: &Entity<InputState>| input.read(cx).value().to_string();
        match &form {
            Form::Value(editor) => {
                let value = editor.read(cx).text().to_string();
                self.stage(
                    vec!["SET".into(), key, value, "KEEPTTL".into()],
                    "Set the value".into(),
                    cx,
                );
            }
            Form::Ttl(input) => {
                let seconds = text(input).trim().to_string();
                if seconds.is_empty() {
                    self.stage(vec!["PERSIST".into(), key], "Remove the expiry".into(), cx);
                } else if seconds.parse::<u64>().is_ok_and(|s| s > 0) {
                    let label = format!(
                        "Expire in {}",
                        ttl_text(Some(seconds.parse::<i64>().unwrap_or(0) * 1000))
                            .trim_start_matches("expires in ")
                    );
                    self.stage(vec!["EXPIRE".into(), key, seconds], label, cx);
                } else {
                    self.apply_error = Some("The time to live is a number of seconds".into());
                    self.form = Some(form);
                }
            }
            Form::Rename(input) => {
                let name = text(input);
                if name.is_empty() || name == key {
                    self.form = Some(form);
                    return;
                }
                self.stage(
                    vec!["RENAME".into(), key, name.clone()],
                    format!("Rename to {name}"),
                    cx,
                );
            }
            Form::Entry {
                editing,
                first,
                second,
                at_head,
            } => match self.entry_commands(
                *editing,
                &text(first),
                second.as_ref().map(text),
                *at_head,
            ) {
                Ok(edits) => {
                    self.pending.extend(edits);
                    self.apply_error = None;
                }
                Err(error) => {
                    self.apply_error = Some(error);
                    self.form = Some(form);
                }
            },
        }
        cx.notify();
    }

    /// The staged changes an entry form makes.
    fn entry_commands(
        &self,
        editing: Option<usize>,
        first: &str,
        second: Option<String>,
        at_head: bool,
    ) -> Result<Vec<Edit>, String> {
        let key = self.target_key();
        let command = |args: Vec<&str>, label: String| Edit::Command {
            args: args.into_iter().map(str::to_string).collect(),
            label,
        };
        let old = |column: &str| editing.and_then(|row| self.cell(row, column));
        Ok(match self.kind() {
            Some(KeyKind::Hash) => {
                if first.is_empty() {
                    return Err("The field needs a name".into());
                }
                let value = second.unwrap_or_default();
                let mut edits = Vec::new();
                if let Some(old) = old("field").filter(|old| old != first) {
                    edits.push(command(
                        vec!["HDEL", &key, &old],
                        format!("Remove field {old}"),
                    ));
                }
                edits.push(command(
                    vec!["HSET", &key, first, &value],
                    format!("Set field {first} = {value}"),
                ));
                edits
            }
            Some(KeyKind::List) => match editing.and_then(|row| self.cell(row, "index")) {
                Some(index) => vec![command(
                    vec!["LSET", &key, &index, first],
                    format!("Set item {index} = {first}"),
                )],
                None if at_head => vec![command(
                    vec!["LPUSH", &key, first],
                    format!("Add {first} at the head"),
                )],
                None => vec![command(
                    vec!["RPUSH", &key, first],
                    format!("Add {first} at the tail"),
                )],
            },
            Some(KeyKind::Set) => {
                let mut edits = Vec::new();
                if let Some(old) = old("member").filter(|old| old != first) {
                    edits.push(command(vec!["SREM", &key, &old], format!("Remove {old}")));
                }
                edits.push(command(vec!["SADD", &key, first], format!("Add {first}")));
                edits
            }
            Some(KeyKind::ZSet) => {
                let score = second.unwrap_or_default();
                if score.trim().parse::<f64>().is_err() {
                    return Err("The score is a number".into());
                }
                let mut edits = Vec::new();
                if let Some(old) = old("member").filter(|old| old != first) {
                    edits.push(command(vec!["ZREM", &key, &old], format!("Remove {old}")));
                }
                edits.push(command(
                    vec!["ZADD", &key, score.trim(), first],
                    format!("Set {first} to score {}", score.trim()),
                ));
                edits
            }
            Some(KeyKind::Stream) => {
                let mut args = vec!["XADD".to_string(), key, "*".into()];
                for pair in split_command(first)? {
                    let Some((field, value)) = pair.split_once('=') else {
                        return Err(format!("{pair} is not field=value"));
                    };
                    args.push(field.into());
                    args.push(value.into());
                }
                if args.len() == 3 {
                    return Err("Add at least one field=value".into());
                }
                vec![Edit::Command {
                    label: format!("Add an entry with {} fields", (args.len() - 3) / 2),
                    args,
                }]
            }
            _ => Vec::new(),
        })
    }

    /// Stages removing the entry in `row`.
    fn remove_row(&mut self, row: usize, cx: &mut Context<Self>) {
        if !self.can_edit() {
            return;
        }
        let key = self.target_key();
        let edit = match self.kind() {
            Some(KeyKind::Hash) => self.cell(row, "field").map(|field| Edit::Command {
                label: format!("Remove field {field}"),
                args: vec!["HDEL".into(), key, field],
            }),
            Some(KeyKind::Set) => self.cell(row, "member").map(|member| Edit::Command {
                label: format!("Remove {member}"),
                args: vec!["SREM".into(), key, member],
            }),
            Some(KeyKind::ZSet) => self.cell(row, "member").map(|member| Edit::Command {
                label: format!("Remove {member}"),
                args: vec!["ZREM".into(), key, member],
            }),
            Some(KeyKind::Stream) => self.cell(row, "id").map(|id| Edit::Command {
                label: format!("Remove entry {id}"),
                args: vec!["XDEL".into(), key, id],
            }),
            Some(KeyKind::List) => {
                let index = self.cell(row, "index").and_then(|i| i.parse().ok());
                index.map(|index| Edit::RemoveListItem {
                    index,
                    value: self.cell(row, "value").unwrap_or_default(),
                })
            }
            _ => None,
        };
        if let Some(edit) = edit.filter(|edit| !self.pending.contains(edit)) {
            self.pending.push(edit);
            self.apply_error = None;
            cx.notify();
        }
    }

    fn stage_delete_key(&mut self, cx: &mut Context<Self>) {
        let edit = Edit::Command {
            args: vec!["DEL".into(), self.target_key()],
            label: "Delete the key".into(),
        };
        if !self.pending.contains(&edit) {
            self.pending.push(edit);
            cx.notify();
        }
    }

    fn discard(&mut self, cx: &mut Context<Self>) {
        self.pending.clear();
        self.form = None;
        self.apply_error = None;
        cx.notify();
    }

    /// Runs the staged changes once approved, in order, stopping at the
    /// first that fails; the ones that ran leave the list.
    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending.is_empty() || self.applying {
            return;
        }
        let planned: Vec<Planned> = commands(&self.key, &self.pending)
            .into_iter()
            .map(Planned::new)
            .collect();
        let this = cx.entity().downgrade();
        let db = self.db.clone();
        redis_write::approve(
            db,
            self.database,
            planned.clone(),
            window,
            cx,
            move |_, cx| {
                let planned = planned.clone();
                this.update(cx, |this, cx| this.run_planned(planned, cx))
                    .ok();
            },
        );
    }

    fn run_planned(&mut self, planned: Vec<Planned>, cx: &mut Context<Self>) {
        let session = self.db.session.clone();
        let database = self.database;
        self.applying = true;
        self.apply_error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let mut ran = 0;
            let mut failure = None;
            for command in &planned {
                let result = session.command(database, command.args.clone()).await;
                let (outcome, failed) = match &result {
                    Ok(Reply::Error(error)) => (error.clone(), true),
                    Ok(reply) => (reply.inline(), false),
                    Err(error) => (error.to_string(), true),
                };
                this.update(cx, |this, cx| {
                    redis_write::audit(
                        &this.db,
                        &this.project,
                        database,
                        &command.line,
                        outcome.clone(),
                        failed,
                        cx,
                    );
                })
                .ok();
                if failed {
                    failure = Some(format!("{}: {outcome}", command.line));
                    break;
                }
                ran += 1;
                // A rename moves the tab to the new name.
                if command.args[0] == "RENAME"
                    && let Some(name) = command.args.get(2).cloned()
                {
                    this.update(cx, |this, _| this.key = name).ok();
                }
            }
            this.update(cx, |this, cx| {
                this.applying = false;
                if failure.is_some() {
                    // Keep what did not run; the list items' commands are
                    // all or nothing, so they stay staged as a whole.
                    let total = planned.len();
                    if ran < total {
                        this.pending = remaining(&this.pending, ran);
                    }
                } else {
                    this.pending.clear();
                }
                this.apply_error = failure;
                this.reload(cx);
            })
            .ok();
        })
        .detach();
    }

    // Render ---------------------------------------------------------------

    fn render_toolbar(&self, json: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let detail = |text: String| {
            div()
                .flex_shrink_0()
                .text_size(px(12.))
                .text_color(color(theme::TEXT_TERTIARY))
                .child(text)
        };
        let info = self.info.as_ref();
        let editable = self.can_edit();
        let locked = !self.db.allows_writes();
        let collection = matches!(
            self.kind(),
            Some(KeyKind::Hash | KeyKind::List | KeyKind::Set | KeyKind::ZSet | KeyKind::Stream)
        );
        h_flex()
            .h(px(44.))
            .flex_shrink_0()
            .px_3()
            .gap(px(10.))
            .bg(color(theme::TOOLBAR))
            .border_b_1()
            .border_color(color(theme::HAIRLINE))
            .child(
                Icon::new(AppIcon::Key)
                    .size(px(14.))
                    .text_color(color(theme::TEXT_TERTIARY)),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(mono)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.key.clone()),
            )
            .children(info.map(|info| {
                chrome::pill()
                    .pr(px(10.))
                    .text_color(color(theme::TEXT_SECONDARY))
                    .child(info.kind.label())
            }))
            .children(info.map(|info| detail(ttl_text(info.ttl_ms))))
            .children(info.and_then(|info| info.length.map(|n| detail(length_text(info.kind, n)))))
            .children(info.and_then(|info| {
                info.memory
                    .map(|m| detail(format!("{} in memory", format_bytes(m))))
            }))
            .children(info.and_then(|info| info.encoding.clone().map(detail)))
            .child(div().flex_1())
            .when(json && !matches!(self.form, Some(Form::Value(_))), |bar| {
                bar.child(
                    chrome::segmented().children([("Pretty", true), ("Raw", false)].map(
                        |(label, pretty)| {
                            chrome::segment(
                                SharedString::from(format!("key-json-{label}")),
                                AppIcon::Format,
                                label,
                                self.pretty == pretty,
                            )
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.pretty = pretty;
                                    cx.notify();
                                },
                            ))
                        },
                    )),
                )
            })
            .when(locked, |bar| {
                bar.child(
                    chrome::pill_button("key-allow-writes", "Allow writes…", false).on_click(
                        cx.listener(|this, _, window, cx| {
                            let tab = cx.entity().downgrade();
                            crate::write_mode::allow_redis_writes(
                                this.db.clone(),
                                window,
                                cx,
                                move |_, cx| {
                                    tab.update(cx, |_, cx| cx.notify()).ok();
                                },
                            );
                        }),
                    ),
                )
            })
            .when(editable && self.editable_text().is_some(), |bar| {
                bar.child(
                    chrome::icon_button("key-edit-value", AppIcon::Pencil, "Edit value", true)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_value_form(window, cx)),
                        ),
                )
            })
            .when(editable && collection, |bar| {
                bar.child(
                    chrome::icon_button("key-add-entry", IconName::Plus, "Add an entry", true)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_entry_form(None, window, cx)
                        })),
                )
            })
            .when(editable, |bar| {
                bar.child(
                    chrome::icon_button("key-ttl", AppIcon::History, "Time to live", true)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_ttl_form(window, cx)),
                        ),
                )
                .child(
                    chrome::icon_button("key-rename", AppIcon::Format, "Rename", true).on_click(
                        cx.listener(|this, _, window, cx| this.open_rename_form(window, cx)),
                    ),
                )
                .child(
                    chrome::icon_button("key-delete", AppIcon::Trash, "Delete key", true)
                        .on_click(cx.listener(|this, _, _, cx| this.stage_delete_key(cx))),
                )
                .child(chrome::toolbar_divider())
            })
            .when(matches!(self.value, Value::Text { .. }), |bar| {
                bar.child(
                    chrome::icon_button("key-copy-value", IconName::Copy, "Copy value", true)
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some((text, _)) = this.text() {
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                            }
                        })),
                )
            })
            .child(
                chrome::icon_button("key-copy-name", AppIcon::Key, "Copy key name", true).on_click(
                    cx.listener(|this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(this.key.clone()))
                    }),
                ),
            )
            .child(
                chrome::icon_button("key-reload", AppIcon::Reload, "Reload", true)
                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            )
    }

    /// The open form's fields and its Stage / Cancel buttons.
    fn render_form(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let form = self.form.as_ref()?;
        let label = |text: &'static str| {
            div()
                .flex_shrink_0()
                .text_size(px(12.))
                .text_color(color(theme::TEXT_TERTIARY))
                .child(text)
        };
        let fields: AnyElement = match form {
            // The value editor takes the body's place.
            Form::Value(_) => label("Editing the value below").into_any_element(),
            Form::Ttl(input) => h_flex()
                .gap_2()
                .child(label("Expire in"))
                .child(div().w(px(140.)).child(Input::new(input)))
                .child(label("seconds · empty removes the expiry"))
                .into_any_element(),
            Form::Rename(input) => h_flex()
                .gap_2()
                .child(label("New name"))
                .child(div().w(px(360.)).child(Input::new(input)))
                .into_any_element(),
            Form::Entry {
                editing,
                first,
                second,
                at_head,
            } => {
                let list_add = self.kind() == Some(KeyKind::List) && editing.is_none();
                h_flex()
                    .gap_2()
                    .child(label(if editing.is_some() { "Change" } else { "Add" }))
                    .child(div().w(px(260.)).child(Input::new(first)))
                    .children(
                        second
                            .as_ref()
                            .map(|second| div().w(px(200.)).child(Input::new(second))),
                    )
                    .when(list_add, |row| {
                        row.child(chrome::segmented().children(
                            [("Tail", false), ("Head", true)].map(|(text, head)| {
                                chrome::segment(
                                    SharedString::from(format!("key-list-{text}")),
                                    AppIcon::RunAll,
                                    text,
                                    *at_head == head,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        if let Some(Form::Entry { at_head, .. }) = &mut this.form {
                                            *at_head = head;
                                        }
                                        cx.notify();
                                    },
                                ))
                            }),
                        ))
                    })
                    .into_any_element()
            }
        };
        Some(
            h_flex()
                .min_h(px(44.))
                .flex_shrink_0()
                .flex_wrap()
                .px_3()
                .py_2()
                .gap_3()
                .bg(color(theme::SIDEBAR))
                .border_b_1()
                .border_color(color(theme::HAIRLINE))
                .child(fields)
                .child(div().flex_1())
                .child(
                    chrome::pill_button("key-form-cancel", "Cancel", false).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.form = None;
                            this.apply_error = None;
                            cx.notify();
                        },
                    )),
                )
                .child(
                    chrome::pill_button("key-form-stage", "Stage", true)
                        .on_click(cx.listener(|this, _, _, cx| this.stage_form(cx))),
                )
                .into_any_element(),
        )
    }

    /// The staged changes, with Discard and Apply.
    fn render_pending(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.pending.is_empty() && self.apply_error.is_none() {
            return None;
        }
        let mono = cx.theme().mono_font_family.clone();
        let count = self.pending.len();
        Some(
            v_flex()
                .flex_shrink_0()
                .px_3()
                .py_2()
                .gap(px(6.))
                .bg(color(theme::GROUND))
                .border_b_1()
                .border_color(color(theme::HAIRLINE))
                .children(self.pending.iter().enumerate().map(|(index, edit)| {
                    let line = match edit {
                        Edit::Command { args, .. } => join_command(args),
                        Edit::RemoveListItem { index, .. } => {
                            format!("LSET {} {index} {TOMBSTONE} · LREM", self.key)
                        }
                    };
                    h_flex()
                        .gap_3()
                        .text_size(px(12.5))
                        .child(chrome::dot(theme::AMBER))
                        .child(div().flex_shrink_0().child(edit.label()))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .font_family(mono.clone())
                                .text_size(px(11.5))
                                .text_color(color(theme::TEXT_QUIET))
                                .child(line),
                        )
                        .child(div().flex_1())
                        .child(
                            div()
                                .id(("key-unstage", index))
                                .p(px(3.))
                                .rounded(px(4.))
                                .text_color(color(theme::TEXT_TERTIARY))
                                .hover(|s| s.bg(color(theme::ACTIVE)).text_color(color(theme::TEXT)))
                                .child(Icon::new(IconName::Close).size(px(11.)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if index < this.pending.len() {
                                        this.pending.remove(index);
                                    }
                                    cx.notify();
                                })),
                        )
                }))
                .children(self.apply_error.clone().map(|error| {
                    div()
                        .text_size(px(12.))
                        .text_color(color(theme::RED))
                        .child(error)
                }))
                .when(count > 0, |panel| {
                    panel.child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(color(theme::TEXT_TERTIARY))
                                    .child(format!(
                                        "{} staged · Redis has no rollback: Apply runs them one by one",
                                        if count == 1 {
                                            "1 change".to_string()
                                        } else {
                                            format!("{count} changes")
                                        }
                                    )),
                            )
                            .child(div().flex_1())
                            .child(
                                chrome::pill_button("key-discard", "Discard", false)
                                    .on_click(cx.listener(|this, _, _, cx| this.discard(cx))),
                            )
                            .child(
                                chrome::pill_button(
                                    "key-apply",
                                    if self.applying { "Applying…" } else { "Apply" },
                                    true,
                                )
                                .on_click(cx.listener(|this, _, window, cx| this.apply(window, cx))),
                            ),
                    )
                })
                .into_any_element(),
        )
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let mono = cx.theme().mono_font_family.clone();
        let message = |text: String, tone: u32| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .px_6()
                .text_color(color(tone))
                .child(text)
                .into_any_element()
        };
        if let Some(Form::Value(editor)) = &self.form {
            return Editor::new(editor)
                .h_full()
                .bordered(false)
                .into_any_element();
        }
        match &self.value {
            Value::Loading => message("Loading…".into(), theme::TEXT_TERTIARY),
            Value::Failed(error) => message(error.clone(), theme::RED),
            Value::Unsupported(text) => message(text.clone(), theme::TEXT_TERTIARY),
            Value::Text { truncated, .. } => {
                let (text, _) = self.text().unwrap_or_default();
                v_flex()
                    .size_full()
                    .child(
                        div()
                            .id("key-text")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p_4()
                            .font_family(mono)
                            .text_size(px(12.5))
                            .child(text),
                    )
                    .when(*truncated, |body| {
                        body.child(footer().child(format!(
                            "Showing the first {} of the value",
                            format_bytes(kuery_redis::MAX_STRING as u64)
                        )))
                    })
                    .into_any_element()
            }
            Value::Rows => {
                let loaded = self.rows.rows.len();
                let total = self.info.as_ref().and_then(|info| info.length);
                v_flex()
                    .size_full()
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .child(DataTable::new(&self.table).stripe(true).bordered(false)),
                    )
                    .child(
                        footer()
                            .child(match total {
                                Some(total) => {
                                    format!("{} of {}", thousands(loaded as u64), thousands(total))
                                }
                                None => thousands(loaded as u64),
                            })
                            .when(self.can_edit(), |row| {
                                row.child(
                                    div()
                                        .text_color(color(theme::TEXT_QUIET))
                                        .child("Right-click a row to change or remove it"),
                                )
                            })
                            .child(div().flex_1())
                            .when(self.task.is_some(), |row| row.child("Loading…"))
                            .when(self.next.is_some() && self.task.is_none(), |row| {
                                row.child(
                                    div()
                                        .id("key-load-more")
                                        .px_2()
                                        .rounded(px(4.))
                                        .text_color(color(theme::ACCENT))
                                        .hover(|s| s.bg(color(theme::ACTIVE)))
                                        .child(format!("Load {} more", thousands(PAGE as u64)))
                                        .on_click(cx.listener(|this, _, _, cx| this.load_more(cx))),
                                )
                            }),
                    )
                    .into_any_element()
            }
        }
    }
}

/// The staged changes as commands. Command edits keep their order; list
/// items to remove are first set to [`TOMBSTONE`] (before anything, so
/// pushes do not move them) and removed together right after.
fn commands(key: &str, pending: &[Edit]) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    let mut removals = false;
    for edit in pending {
        if let Edit::RemoveListItem { index, .. } = edit {
            commands.push(vec![
                "LSET".to_string(),
                key.to_string(),
                index.to_string(),
                TOMBSTONE.to_string(),
            ]);
            removals = true;
        }
    }
    if removals {
        commands.push(vec![
            "LREM".to_string(),
            key.to_string(),
            "0".to_string(),
            TOMBSTONE.to_string(),
        ]);
    }
    for edit in pending {
        if let Edit::Command { args, .. } = edit {
            commands.push(args.clone());
        }
    }
    commands
}

/// The staged changes left after the first `ran` commands ran. List item
/// removals ran first; if they did not all run, they all stay.
fn remaining(pending: &[Edit], ran: usize) -> Vec<Edit> {
    let removals = pending
        .iter()
        .filter(|edit| matches!(edit, Edit::RemoveListItem { .. }))
        .count();
    let removal_commands = if removals > 0 { removals + 1 } else { 0 };
    let others: Vec<&Edit> = pending
        .iter()
        .filter(|edit| matches!(edit, Edit::Command { .. }))
        .collect();
    if ran < removal_commands {
        return pending.to_vec();
    }
    others
        .into_iter()
        .skip(ran - removal_commands)
        .cloned()
        .collect()
}

fn footer() -> gpui_kit::Div {
    h_flex()
        .h(px(30.))
        .flex_shrink_0()
        .px_3()
        .gap_3()
        .bg(color(theme::TOOLBAR))
        .border_t_1()
        .border_color(color(theme::HAIRLINE))
        .text_size(px(12.))
        .text_color(color(theme::TEXT_TERTIARY))
}

/// Adds a page to the rows read so far. Stream entries may bring fields the
/// earlier ones lacked: those become new columns.
pub fn merge(rows: &mut Table, page: Table) {
    if rows.columns.is_empty() {
        *rows = page;
        return;
    }
    for column in &page.columns {
        if !rows.columns.contains(column) {
            rows.columns.push(column.clone());
            for row in &mut rows.rows {
                row.push(None);
            }
        }
    }
    let positions: Vec<usize> = page
        .columns
        .iter()
        .map(|column| rows.columns.iter().position(|c| c == column).unwrap_or(0))
        .collect();
    for values in page.rows {
        let mut row = vec![None; rows.columns.len()];
        for (value, position) in values.into_iter().zip(&positions) {
            row[*position] = value;
        }
        rows.rows.push(row);
    }
}

/// "expires in 2h 5m", or "no expiry".
pub fn ttl_text(ttl_ms: Option<i64>) -> String {
    let Some(ms) = ttl_ms else {
        return "no expiry".into();
    };
    let total = Duration::from_millis(ms.max(0) as u64).as_secs();
    let (days, hours, minutes, seconds) = (
        total / 86_400,
        total % 86_400 / 3_600,
        total % 3_600 / 60,
        total % 60,
    );
    let text = match (days, hours, minutes) {
        (0, 0, 0) => format!("{seconds}s"),
        (0, 0, _) => format!("{minutes}m {seconds}s"),
        (0, _, _) => format!("{hours}h {minutes}m"),
        _ => format!("{days}d {hours}h"),
    };
    format!("expires in {text}")
}

fn length_text(kind: KeyKind, length: u64) -> String {
    let count = |one: &str, many: &str| {
        format!(
            "{} {}",
            thousands(length),
            if length == 1 { one } else { many }
        )
    };
    match kind {
        KeyKind::String => format_bytes(length),
        KeyKind::Hash => count("field", "fields"),
        KeyKind::List => count("item", "items"),
        KeyKind::Set | KeyKind::ZSet => count("member", "members"),
        KeyKind::Stream => count("entry", "entries"),
        KeyKind::Missing | KeyKind::Other => thousands(length),
    }
}

impl Focusable for KeyTab {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for KeyTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let json = self.text().is_some_and(|(_, json)| json);
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .child(self.render_toolbar(json, cx))
            .children(self.render_form(cx))
            .children(self.render_pending(cx))
            .child(div().flex_1().min_h_0().child(self.render_body(cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(args: &[&str]) -> Edit {
        Edit::Command {
            args: args.iter().map(|a| a.to_string()).collect(),
            label: String::new(),
        }
    }

    #[test]
    fn merges_pages_with_new_columns() {
        let mut rows = Table {
            columns: vec!["id".into(), "a".into()],
            rows: vec![vec![Some("1".into()), Some("x".into())]],
        };
        merge(
            &mut rows,
            Table {
                columns: vec!["id".into(), "b".into()],
                rows: vec![vec![Some("2".into()), Some("y".into())]],
            },
        );
        assert_eq!(rows.columns, ["id", "a", "b"]);
        assert_eq!(rows.rows[0], [Some("1".into()), Some("x".into()), None]);
        assert_eq!(rows.rows[1], [Some("2".into()), None, Some("y".into())]);
    }

    #[test]
    fn describes_time_to_live() {
        assert_eq!(ttl_text(None), "no expiry");
        assert_eq!(ttl_text(Some(42_000)), "expires in 42s");
        assert_eq!(ttl_text(Some(7_500_000)), "expires in 2h 5m");
        assert_eq!(ttl_text(Some(90_000_000)), "expires in 1d 1h");
        assert_eq!(length_text(KeyKind::Hash, 1), "1 field");
        assert_eq!(length_text(KeyKind::Stream, 500), "500 entries");
    }

    #[test]
    fn removes_list_items_before_pushes_move_them() {
        let pending = vec![
            command(&["RPUSH", "q", "new"]),
            Edit::RemoveListItem {
                index: 2,
                value: "b".into(),
            },
            Edit::RemoveListItem {
                index: 5,
                value: "e".into(),
            },
            command(&["LPUSH", "q", "first"]),
        ];
        let lines: Vec<String> = commands("q", &pending)
            .iter()
            .map(|args| join_command(args))
            .collect();
        assert_eq!(
            lines,
            [
                format!("LSET q 2 {TOMBSTONE}"),
                format!("LSET q 5 {TOMBSTONE}"),
                format!("LREM q 0 {TOMBSTONE}"),
                "RPUSH q new".into(),
                "LPUSH q first".into(),
            ]
        );
        // The removals and the first push ran: only the last push is left.
        assert_eq!(remaining(&pending, 4), [command(&["LPUSH", "q", "first"])]);
        // A removal failed: everything stays.
        assert_eq!(remaining(&pending, 1), pending);
    }
}
