//! A Redis key in a tab: its type, time to live, size and encoding, and its
//! value. A string shows as text (JSON pretty-printed, if it is JSON);
//! hashes, lists, sets, sorted sets and streams fill the results grid a page
//! at a time, so the Profile of their values works like any other result.

use std::time::Duration;

use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{ActiveTheme as _, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, FocusHandle, Focusable,
    FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, Window, div, px,
};
use kuery_redis::{Content, KeyInfo, KeyKind, Page, Table, display_bytes};

use crate::assets::AppIcon;
use crate::chrome;
use crate::connection::RedisDb;
use crate::results::ResultsDelegate;
use crate::theme::{self, color};
use crate::ui::{format_bytes, thousands};

/// Entries read per page.
const PAGE: usize = 500;

enum Value {
    Loading,
    Text { bytes: Vec<u8>, truncated: bool },
    Rows,
    Unsupported(String),
    Failed(String),
}

pub struct KeyTab {
    db: RedisDb,
    database: i64,
    key: String,
    info: Option<KeyInfo>,
    value: Value,
    /// Every entry read so far.
    rows: Table,
    table: Entity<TableState<ResultsDelegate>>,
    next: Option<Page>,
    /// JSON strings indented, or as stored.
    pretty: bool,
    task: Option<Task<()>>,
    focus_handle: FocusHandle,
}

impl KeyTab {
    pub fn new(
        db: RedisDb,
        database: i64,
        key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let table = cx.new(|cx| TableState::new(ResultsDelegate::default(), window, cx));
        let mut tab = Self {
            db,
            database,
            key,
            info: None,
            value: Value::Loading,
            rows: Table::default(),
            table,
            next: None,
            pretty: true,
            task: None,
            focus_handle: cx.focus_handle(),
        };
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
            .when(json, |bar| {
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
            .when(matches!(self.value, Value::Text { .. }), |bar| {
                bar.child(
                    chrome::icon_button(
                        "key-copy-value",
                        gpui_kit::component::IconName::Copy,
                        "Copy value",
                        true,
                    )
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
            .child(div().flex_1().min_h_0().child(self.render_body(cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
