//! Redis connections in the sidebar: the server's databases, and each one's
//! keys, read page by page with `SCAN` (never `KEYS`, which blocks the
//! server) and grouped by `:` into folders (`user:1:name` → `user ▸ 1 ▸
//! name`). The sidebar's filter becomes the `SCAN` pattern.

use std::collections::HashSet;

use gpui_kit::{AnyElement, Task};
use kuery_core::redis::KeyTree;
use kuery_redis::DbInfo;

use super::*;
use crate::ui::thousands;

/// What splits key names into folders.
const DELIMITER: &str = ":";
/// Keys read per page.
const PAGE: usize = 1_000;
/// Rows a folder shows before "N more".
const MAX_CHILDREN: usize = 500;
/// While filtering, folders open by themselves up to this many keys.
const AUTO_EXPAND: usize = 200;

pub(super) enum Keyspace {
    Loading,
    Loaded(Vec<DbInfo>),
    Failed(String),
}

/// The keys of one database read so far.
#[derive(Default)]
pub(super) struct KeyBrowser {
    seen: HashSet<String>,
    tree: KeyTree,
    /// `None` until the first page arrives; 0 once the scan is complete.
    cursor: Option<u64>,
    loading: Option<Task<()>>,
    error: Option<String>,
    /// The `SCAN` pattern the keys were read with.
    pattern: Option<String>,
}

/// The filter as a `SCAN` pattern: as typed when it is already a glob,
/// else "contains", with the glob's special characters escaped.
fn scan_pattern(filter: &str) -> Option<String> {
    let filter = filter.trim();
    if filter.is_empty() {
        return None;
    }
    if filter.contains(['*', '?', '[']) {
        return Some(filter.to_string());
    }
    let escaped: String = filter
        .chars()
        .flat_map(|c| match c {
            '\\' | ']' => vec!['\\', c],
            c => vec![c],
        })
        .collect();
    Some(format!("*{escaped}*"))
}

fn database_key(index: usize, database: i64) -> String {
    format!("{index}/db{database}")
}

fn folder_key(index: usize, database: i64, path: &[String]) -> String {
    format!("{index}/db{database}/k:{}", path.join(DELIMITER))
}

fn selected_key(index: usize, database: i64, key: &str) -> String {
    format!("{index}/db{database}/key:{key}")
}

impl Explorer {
    fn redis_db(&self, index: usize) -> Option<RedisDb> {
        match &self.connections.get(index)?.state {
            ConnectionState::RedisReady { db, .. } => Some(db.clone()),
            _ => None,
        }
    }

    /// Reads the databases and their key counts again.
    pub(super) fn load_keyspace(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(db) = self.redis_db(index) else {
            return;
        };
        if let Some(ConnectionState::RedisReady { keyspace, .. }) =
            self.connections.get_mut(index).map(|n| &mut n.state)
        {
            *keyspace = Keyspace::Loading;
        }
        let load = db.session.keyspace();
        cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                if let Some(ConnectionState::RedisReady { keyspace, .. }) =
                    this.connections.get_mut(index).map(|n| &mut n.state)
                {
                    *keyspace = match result {
                        Ok(databases) => Keyspace::Loaded(databases),
                        Err(error) => Keyspace::Failed(error.to_string()),
                    };
                }
                // Key lists may be stale too: read the open ones again.
                let open: Vec<i64> = this
                    .redis_keys
                    .keys()
                    .filter(|(i, _)| *i == index)
                    .map(|(_, database)| *database)
                    .collect();
                for database in open {
                    this.load_keys(index, database, false, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn toggle_database(&mut self, index: usize, database: i64, cx: &mut Context<Self>) {
        let key = database_key(index, database);
        if self.expanded.remove(&key) {
            cx.notify();
            return;
        }
        self.expanded.insert(key);
        if !self.redis_keys.contains_key(&(index, database)) {
            self.load_keys(index, database, false, cx);
        }
        cx.notify();
    }

    /// Reads the first page of keys (or the next one, with `more`).
    fn load_keys(&mut self, index: usize, database: i64, more: bool, cx: &mut Context<Self>) {
        let Some(db) = self.redis_db(index) else {
            return;
        };
        let pattern = scan_pattern(&self.search.read(cx).value());
        let browser = self.redis_keys.entry((index, database)).or_default();
        if more {
            if browser.loading.is_some() || browser.cursor == Some(0) {
                return;
            }
        } else {
            *browser = KeyBrowser {
                pattern: pattern.clone(),
                ..KeyBrowser::default()
            };
        }
        let cursor = browser.cursor.unwrap_or(0);
        let scan = db.session.scan(database, cursor, pattern, PAGE);
        browser.loading = Some(cx.spawn(async move |this, cx| {
            let result = scan.await;
            this.update(cx, |this, cx| {
                let Some(browser) = this.redis_keys.get_mut(&(index, database)) else {
                    return;
                };
                browser.loading = None;
                match result {
                    Ok(page) => {
                        browser.cursor = Some(page.cursor);
                        for key in page.keys {
                            if browser.seen.insert(key.clone()) {
                                browser.tree.insert(&key, DELIMITER);
                            }
                        }
                    }
                    Err(error) => browser.error = Some(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The filter changed: key lists read with another pattern start over.
    pub(super) fn rescan_redis(&mut self, cx: &mut Context<Self>) {
        let pattern = scan_pattern(&self.search.read(cx).value());
        let stale: Vec<(usize, i64)> = self
            .redis_keys
            .iter()
            .filter(|(_, browser)| browser.pattern != pattern)
            .map(|(place, _)| *place)
            .collect();
        for (index, database) in stale {
            if self.expanded.contains(&database_key(index, database)) {
                self.load_keys(index, database, false, cx);
            } else {
                self.redis_keys.remove(&(index, database));
            }
        }
    }

    fn open_key(&mut self, index: usize, database: i64, key: String, cx: &mut Context<Self>) {
        let Some(db) = self.redis_db(index) else {
            return;
        };
        self.selected = Some(selected_key(index, database, &key));
        cx.emit(ExplorerEvent::OpenRedisKey { db, database, key });
        cx.notify();
    }

    fn open_console(&mut self, index: usize, database: i64, cx: &mut Context<Self>) {
        if let Some(db) = self.redis_db(index) {
            cx.emit(ExplorerEvent::OpenRedisConsole { db, database });
        }
    }

    pub(super) fn open_default_console(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(db) = self.redis_db(index) {
            let database = db.session.default_db();
            cx.emit(ExplorerEvent::OpenRedisConsole { db, database });
        }
    }

    pub(super) fn redis_rows(
        &self,
        index: usize,
        db: &RedisDb,
        keyspace: &Keyspace,
        rows: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        let databases = match keyspace {
            Keyspace::Loading => {
                rows.push(note_row(1, "Loading databases…", theme::TEXT_TERTIARY));
                return;
            }
            Keyspace::Failed(error) => {
                rows.push(note_row(1, error.clone(), theme::RED));
                return;
            }
            Keyspace::Loaded(databases) => databases,
        };
        let default = db.session.default_db();
        let shown: Vec<&DbInfo> = databases
            .iter()
            .filter(|info| info.keys > 0 || info.index == default)
            .collect();
        for info in &shown {
            self.database_rows(index, info, rows, cx);
        }
        let empty = databases.len() - shown.len();
        if empty > 0 {
            rows.push(note_row(
                1,
                format!(
                    "{empty} empty database{}",
                    if empty == 1 { "" } else { "s" }
                ),
                theme::TEXT_QUIET,
            ));
        }
    }

    fn database_rows(
        &self,
        index: usize,
        info: &DbInfo,
        rows: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        let database = info.index;
        let expanded = self.expanded.contains(&database_key(index, database));
        let group = SharedString::from(format!("redis-db-{index}-{database}"));
        rows.push(
            tree_row(
                SharedString::from(format!("redis-db-row-{index}-{database}")),
                1,
                Some(expanded),
                false,
            )
            .group(group.clone())
            .child(
                Icon::new(AppIcon::Database)
                    .size(px(13.))
                    .text_color(color(theme::TEXT_TERTIARY)),
            )
            .child(div().flex_1().truncate().child(format!("db{database}")))
            .child(
                div()
                    .id(SharedString::from(format!(
                        "redis-console-{index}-{database}"
                    )))
                    .px_1()
                    .rounded(px(4.))
                    .invisible()
                    .group_hover(group, |s| s.visible())
                    .text_color(color(theme::TEXT_TERTIARY))
                    .hover(|s| s.bg(color(0x3a3a3e)).text_color(color(theme::TEXT)))
                    .child(Icon::new(AppIcon::Code).size(px(11.)))
                    .tooltip(|window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new("Open console").build(window, cx)
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.open_console(index, database, cx);
                    })),
            )
            .child(counter(compact(info.keys)))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_database(index, database, cx)))
            .context_menu({
                let this = cx.entity().downgrade();
                move |menu, _, _| {
                    menu.item(item_icon(
                        "Open Console",
                        AppIcon::Code,
                        &this,
                        move |this, _, cx| this.open_console(index, database, cx),
                    ))
                    .item(item_icon(
                        "Refresh Keys",
                        AppIcon::Reload,
                        &this,
                        move |this, _, cx| {
                            this.expanded.insert(database_key(index, database));
                            this.load_keys(index, database, false, cx)
                        },
                    ))
                    .separator()
                    .item(copy("Copy Name", format!("db{database}")))
                }
            })
            .into_any_element(),
        );
        if !expanded {
            return;
        }
        let Some(browser) = self.redis_keys.get(&(index, database)) else {
            rows.push(note_row(2, "Loading keys…", theme::TEXT_TERTIARY));
            return;
        };
        if let Some(error) = &browser.error {
            rows.push(note_row(2, error.clone(), theme::RED));
        }
        if browser.cursor.is_none() {
            if browser.loading.is_some() {
                rows.push(note_row(2, "Loading keys…", theme::TEXT_TERTIARY));
            }
            return;
        }
        if browser.tree.is_empty() {
            let text = if browser.pattern.is_some() {
                "No keys match the filter"
            } else {
                "No keys"
            };
            rows.push(note_row(2, text, theme::TEXT_TERTIARY));
        }
        let auto_expand = browser.pattern.is_some() && browser.tree.count <= AUTO_EXPAND;
        self.key_rows(
            index,
            database,
            &browser.tree,
            &mut Vec::new(),
            2,
            auto_expand,
            rows,
            cx,
        );
        if browser.loading.is_some() {
            rows.push(note_row(2, "Loading more keys…", theme::TEXT_TERTIARY));
        } else if browser.cursor != Some(0) {
            rows.push(
                div()
                    .id(SharedString::from(format!("redis-more-{index}-{database}")))
                    .pl(px(26. + 2. * 14.))
                    .py_1()
                    .text_size(px(12.))
                    .text_color(color(0x4da3ff))
                    .hover(|s| s.text_color(color(0x80bdff)))
                    .child(format!(
                        "Load more keys · {} so far",
                        thousands(browser.tree.count as u64)
                    ))
                    .on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.load_keys(index, database, true, cx)
                        }),
                    )
                    .into_any_element(),
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn key_rows(
        &self,
        index: usize,
        database: i64,
        node: &KeyTree,
        path: &mut Vec<String>,
        depth: usize,
        auto_expand: bool,
        rows: &mut Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) {
        let children = node.children();
        for (position, (name, child)) in children.iter().enumerate() {
            if position == MAX_CHILDREN {
                rows.push(note_row(
                    depth,
                    format!(
                        "{} more · filter to narrow",
                        thousands((children.len() - MAX_CHILDREN) as u64)
                    ),
                    theme::TEXT_QUIET,
                ));
                break;
            }
            path.push(name.to_string());
            if child.has_children() {
                let key = folder_key(index, database, path);
                let open = auto_expand || self.expanded.contains(&key);
                rows.push(
                    tree_row(
                        SharedString::from(format!("redis-folder-{key}")),
                        depth,
                        Some(open),
                        false,
                    )
                    .child(
                        Icon::new(IconName::FolderClosed)
                            .size(px(13.))
                            .text_color(color(theme::ACCENT)),
                    )
                    .child(div().flex_1().truncate().child(display_segment(name)))
                    .child(counter(compact(child.count as u64)))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(key.clone(), cx)))
                    .into_any_element(),
                );
                if open {
                    if let Some(full) = &child.key {
                        rows.push(self.key_row(index, database, full, full, depth + 1, cx));
                    }
                    self.key_rows(
                        index,
                        database,
                        child,
                        path,
                        depth + 1,
                        auto_expand,
                        rows,
                        cx,
                    );
                }
            } else if let Some(full) = &child.key {
                rows.push(self.key_row(index, database, name, full, depth, cx));
            }
            path.pop();
        }
    }

    fn key_row(
        &self,
        index: usize,
        database: i64,
        label: &str,
        key: &str,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = self.selected.as_deref() == Some(&selected_key(index, database, key));
        let key = key.to_string();
        tree_row(
            SharedString::from(format!("redis-key-{index}-{database}-{key}")),
            depth,
            None,
            selected,
        )
        .child(icon(AppIcon::Key, theme::TEXT_TERTIARY, selected))
        .child(div().flex_1().truncate().child(display_segment(label)))
        .on_click({
            let key = key.clone();
            cx.listener(move |this, _, _, cx| this.open_key(index, database, key.clone(), cx))
        })
        .context_menu({
            let this = cx.entity().downgrade();
            move |menu, _, _| {
                let open = key.clone();
                menu.item(item_icon(
                    "Open",
                    AppIcon::Key,
                    &this,
                    move |this, _, cx| this.open_key(index, database, open.clone(), cx),
                ))
                .separator()
                .item(copy("Copy Key Name", key.clone()))
            }
        })
        .into_any_element()
    }
}

/// An empty segment (`a::b`) is shown so it can be told apart.
fn display_segment(name: &str) -> String {
    if name.is_empty() {
        "(empty)".into()
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_the_filter_into_a_scan_pattern() {
        assert_eq!(scan_pattern("  "), None);
        assert_eq!(scan_pattern("user"), Some("*user*".into()));
        assert_eq!(scan_pattern("user:*"), Some("user:*".into()));
        assert_eq!(scan_pattern(r"a\b]"), Some(r"*a\\b\]*".into()));
    }
}
