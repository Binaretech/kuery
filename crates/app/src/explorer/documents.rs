//! Document connections in the sidebar: a lazy tree of what the driver
//! lists under each node (a MongoDB server's databases, then each one's
//! collections), read when a node first opens and named by the levels the
//! driver reports (`Database`, `Collection`). Collections show their kind
//! and how many documents they hold, counted as they are listed, and open
//! (by their chevron) to their indexes. Names are filtered as typed, among
//! those read so far.

use gpui_kit::{AnyElement, Task};
use kuery_core::document::catalog::{Container, ContainerKind, DocIndex};
use kuery_core::document::change::Count;
use kuery_core::document::language::Style;
use kuery_core::document::session::Heavy;
use kuery_core::document::value::{CollectionRef, Value};

use super::rows::{Row, Tree, error_row, note};
use super::*;
use crate::ui::thousands;

/// Children a node shows before "Load more".
pub(super) const PAGE: usize = 500;

/// Children read so far, by connection id and parent path: an id, not a
/// position, since connections move.
pub(super) type DocumentTrees = HashMap<(String, Vec<String>), Children>;

pub(super) enum Children {
    Loading(#[allow(dead_code)] Task<()>),
    Loaded {
        items: Vec<Container>,
        /// How many of them the tree shows.
        shown: usize,
        /// Counts the documents of those shown, one collection at a time.
        counting: Option<Task<()>>,
    },
    Failed(String),
    /// A collection's indexes, read when it first opens.
    Indexes(Vec<DocIndex>),
}

/// The tree key of the node at `path`: `0/d:shop/d:orders`.
pub(super) fn node_key(index: usize, path: &[String]) -> String {
    std::iter::once(index.to_string())
        .chain(path.iter().map(|segment| format!("d:{segment}")))
        .collect::<Vec<_>>()
        .join("/")
}

/// `path` with `name` added.
fn child(path: &[String], name: &str) -> Vec<String> {
    let mut path = path.to_vec();
    path.push(name.to_string());
    path
}

/// What an index row shows besides its name: `unique`, `TTL`…
pub(super) fn index_traits(index: &DocIndex) -> Vec<&'static str> {
    [
        (index.unique, "unique"),
        (index.sparse, "sparse"),
        (index.partial.is_some(), "partial"),
        (index.ttl.is_some(), "TTL"),
        (index.hidden, "hidden"),
    ]
    .into_iter()
    .filter_map(|(has, name)| has.then_some(name))
    .collect()
}

/// Whether a collection opens to its indexes: a view has none of its own.
fn has_indexes(node: &Container) -> bool {
    node.leaf && node.kind != ContainerKind::View
}

/// What the tree shows for a kind besides its name.
fn kind_label(kind: ContainerKind) -> Option<&'static str> {
    match kind {
        ContainerKind::View => Some("view"),
        ContainerKind::TimeSeries => Some("time series"),
        ContainerKind::Capped => Some("capped"),
        ContainerKind::Database | ContainerKind::Scope | ContainerKind::Collection => None,
    }
}

impl Explorer {
    pub(super) fn doc_db(&self, index: usize) -> Option<DocDb> {
        match &self.connections.get(index)?.state {
            ConnectionState::DocumentReady { db } => Some(db.clone()),
            _ => None,
        }
    }

    /// Reads what lies under `parent` (`[]` for the top level) again.
    pub(super) fn load_children(
        &mut self,
        index: usize,
        parent: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(db) = self.doc_db(index) else {
            return;
        };
        let place = (self.connections[index].spec.id.clone(), parent.clone());
        let load = db.catalog.children(parent);
        let key = place.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                // Gone with a disconnect, or read again meanwhile.
                if !matches!(this.documents.get(&key), Some(Children::Loading(_))) {
                    return;
                }
                let children = match result {
                    Ok(items) => Children::Loaded {
                        items,
                        shown: PAGE,
                        counting: None,
                    },
                    Err(error) => Children::Failed(describe_error(error)),
                };
                this.documents.insert(key.clone(), children);
                this.count_documents(key, cx);
                cx.notify();
            })
            .ok();
        });
        self.documents.insert(place, Children::Loading(task));
        cx.notify();
    }

    /// Counts the documents of the collections shown under a node that do
    /// not have a count yet. Views are left out: counting one runs its
    /// query over all of it.
    fn count_documents(&mut self, place: (String, Vec<String>), cx: &mut Context<Self>) {
        let Some(db) = self.index_of(&place.0).and_then(|index| self.doc_db(index)) else {
            return;
        };
        let Some(Children::Loaded {
            items,
            shown,
            counting,
        }) = self.documents.get_mut(&place)
        else {
            return;
        };
        let names: Vec<String> = items
            .iter()
            .take(*shown)
            .filter(|item| {
                item.leaf && item.estimated.is_none() && item.kind != ContainerKind::View
            })
            .map(|item| item.name.clone())
            .collect();
        if names.is_empty() {
            return;
        }
        *counting = Some(cx.spawn(async move |this, cx| {
            for name in names {
                let collection = CollectionRef {
                    path: child(&place.1, &name),
                };
                let counted = db.catalog.count(collection, None, 0, Heavy::Refuse).await;
                let Ok(Count::Exact(count)) = counted else {
                    continue;
                };
                let kept = this.update(cx, |this, cx| {
                    if let Some(Children::Loaded { items, .. }) = this.documents.get_mut(&place)
                        && let Some(item) = items.iter_mut().find(|item| item.name == name)
                    {
                        item.estimated = Some(count);
                        cx.notify();
                    }
                });
                if kept.is_err() {
                    return;
                }
            }
        }));
    }

    /// Reads a collection's indexes again, on the catalog session.
    pub(super) fn load_indexes(
        &mut self,
        index: usize,
        collection: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(db) = self.doc_db(index) else {
            return;
        };
        let place = (self.connections[index].spec.id.clone(), collection.clone());
        let load = db.catalog.collection(CollectionRef { path: collection });
        let key = place.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                if !matches!(this.documents.get(&key), Some(Children::Loading(_))) {
                    return;
                }
                let indexes = match result {
                    Ok(info) => Children::Indexes(info.indexes),
                    Err(error) => Children::Failed(describe_error(error)),
                };
                this.documents.insert(key, indexes);
                cx.notify();
            })
            .ok();
        });
        self.documents.insert(place, Children::Loading(task));
        cx.notify();
    }

    /// Whether `path` names a collection: as deep as the driver's levels.
    fn is_collection(&self, index: usize, path: &[String]) -> bool {
        self.doc_db(index)
            .is_some_and(|db| db.catalog.capabilities().levels.len() == path.len())
    }

    /// Reads every node of a connection read so far again.
    pub(super) fn refresh_documents(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(id) = self.connections.get(index).map(|node| node.spec.id.clone()) else {
            return;
        };
        let mut read: Vec<Vec<String>> = self
            .documents
            .keys()
            .filter(|(other, _)| *other == id)
            .map(|(_, path)| path.clone())
            .collect();
        if read.is_empty() {
            read.push(Vec::new());
        }
        for path in read {
            match self.is_collection(index, &path) {
                true => self.load_indexes(index, path, cx),
                false => self.load_children(index, path, cx),
            }
        }
    }

    fn toggle_document_node(&mut self, index: usize, path: Vec<String>, cx: &mut Context<Self>) {
        let key = node_key(index, &path);
        if self.expanded.remove(&key) {
            cx.notify();
            return;
        }
        self.expanded.insert(key);
        let id = self.connections[index].spec.id.clone();
        if !self.documents.contains_key(&(id, path.clone())) {
            match self.is_collection(index, &path) {
                true => self.load_indexes(index, path, cx),
                false => self.load_children(index, path, cx),
            }
        }
        cx.notify();
    }

    fn open_collection(
        &mut self,
        index: usize,
        path: Vec<String>,
        kind: ContainerKind,
        cx: &mut Context<Self>,
    ) {
        let Some(db) = self.doc_db(index) else {
            return;
        };
        self.selected = Some(node_key(index, &path));
        cx.emit(ExplorerEvent::OpenCollection {
            db,
            collection: CollectionRef { path },
            kind,
        });
        cx.notify();
    }

    /// Opens a console on the connection, its runs starting in `path`, or
    /// in the connection's own database for `None`.
    pub(super) fn open_document_console(
        &mut self,
        index: usize,
        path: Option<Vec<String>>,
        cx: &mut Context<Self>,
    ) {
        let Some(db) = self.doc_db(index) else {
            return;
        };
        let path = path.unwrap_or_else(|| db.home.clone());
        cx.emit(ExplorerEvent::OpenDocumentConsole { db, path });
    }

    fn show_more(&mut self, index: usize, parent: Vec<String>, cx: &mut Context<Self>) {
        let place = (self.connections[index].spec.id.clone(), parent);
        if let Some(Children::Loaded { shown, .. }) = self.documents.get_mut(&place) {
            *shown += PAGE;
            self.count_documents(place, cx);
            cx.notify();
        }
    }

    /// The node `item` of what was read under `parent`.
    fn document_node(&self, index: usize, parent: &[String], item: usize) -> Option<&Container> {
        let id = self.connections.get(index)?.spec.id.clone();
        match self.documents.get(&(id, parent.to_vec()))? {
            Children::Loaded { items, .. } => items.get(item),
            _ => None,
        }
    }

    pub(super) fn document_row(
        &self,
        index: usize,
        parent: &[String],
        item: usize,
        depth: usize,
        expanded: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let node = self.document_node(index, parent, item)?.clone();
        let path = child(parent, &node.name);
        let key = node_key(index, &path);
        let selected = self.selected.as_deref() == Some(key.as_str());
        let level = self
            .doc_db(index)
            .and_then(|db| db.catalog.capabilities().levels.get(parent.len()).copied())
            .unwrap_or("Node");
        let tooltip: SharedString = format!("{level} {}", node.name).into();
        let (glyph, tone) = match node.kind {
            ContainerKind::Database => (Icon::from(AppIcon::Database), theme::TEXT_TERTIARY),
            ContainerKind::Scope => (Icon::from(IconName::FolderClosed), theme::ACCENT),
            ContainerKind::View => (Icon::from(AppIcon::Code), 0xc49bff),
            ContainerKind::Collection | ContainerKind::TimeSeries | ContainerKind::Capped => {
                (Icon::from(AppIcon::Table), 0x4da3ff)
            }
        };
        let id = SharedString::from(format!("document-{key}"));
        let row = match has_indexes(&node) {
            // A collection's chevron opens its indexes; the row opens the
            // collection.
            true => {
                let toggle = path.clone();
                let opens = div()
                    .id(SharedString::from(format!("document-indexes-{key}")))
                    .flex_shrink_0()
                    .child(chevron(expanded))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_document_node(index, toggle.clone(), cx)
                    }));
                tree_row_with(id, depth, opens.into_any_element(), selected)
            }
            false => tree_row(id, depth, (!node.leaf).then_some(expanded), selected),
        }
        .child(
            glyph
                .size(px(13.))
                .text_color(color(if selected { theme::ACCENT } else { tone })),
        )
        .child(div().flex_1().truncate().child(node.name.clone()))
        .children(kind_label(node.kind).map(counter))
        .children(node.estimated.map(|count| counter(compact(count))))
        .tooltip(move |window, cx| {
            gpui_kit::component::tooltip::Tooltip::new(tooltip.clone()).build(window, cx)
        });
        let this = cx.entity().downgrade();
        let name = node.name.clone();
        let dotted = path.join(".");
        let row = if node.leaf {
            let open = path.clone();
            row.on_click(cx.listener(move |this, _, _, cx| {
                this.open_collection(index, open.clone(), node.kind, cx)
            }))
            .context_menu(move |menu, _, _| {
                let open = path.clone();
                menu.item(item_icon(
                    "Open",
                    AppIcon::Table,
                    &this,
                    move |this, _, cx| this.open_collection(index, open.clone(), node.kind, cx),
                ))
                .separator()
                .item(copy("Copy Name", name.clone()))
                .item(copy("Copy Path", dotted.clone()))
            })
        } else {
            let toggle = path.clone();
            row.on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_document_node(index, toggle.clone(), cx)
            }))
            .context_menu(move |menu, _, _| {
                let refresh = path.clone();
                let console = path.clone();
                menu.item(item_icon(
                    "Open Console",
                    AppIcon::Code,
                    &this,
                    move |this, _, cx| this.open_document_console(index, Some(console.clone()), cx),
                ))
                .item(item_icon(
                    "Refresh",
                    AppIcon::Reload,
                    &this,
                    move |this, _, cx| {
                        this.expanded.insert(node_key(index, &refresh));
                        this.load_children(index, refresh.clone(), cx)
                    },
                ))
                .separator()
                .item(copy("Copy Name", name.clone()))
            })
        };
        Some(row.into_any_element())
    }

    /// An index of a collection the tree has open.
    pub(super) fn document_index_row(
        &self,
        index: usize,
        collection: &[String],
        item: usize,
        depth: usize,
    ) -> Option<AnyElement> {
        let id = self.connections.get(index)?.spec.id.clone();
        let Some(Children::Indexes(indexes)) = self.documents.get(&(id, collection.to_vec()))
        else {
            return None;
        };
        let found = indexes.get(item)?.clone();
        let render = self.doc_db(index)?.catalog.language().render;
        let keys = render(&Value::Object(found.keys.clone()), Style::Compact);
        let traits = index_traits(&found);
        let tooltip: SharedString = match traits.is_empty() {
            true => format!("Index {} on {keys}", found.name).into(),
            false => format!("Index {} on {keys} · {}", found.name, traits.join(", ")).into(),
        };
        let key = format!("{}/i:{}", node_key(index, collection), found.name);
        let name = found.name.clone();
        Some(
            tree_row(
                SharedString::from(format!("document-index-{key}")),
                depth,
                None,
                false,
            )
            .child(
                Icon::from(AppIcon::Key)
                    .size(px(12.))
                    .text_color(color(theme::AMBER)),
            )
            .child(div().flex_1().truncate().child(found.name.clone()))
            .children(traits.into_iter().map(counter))
            .tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(tooltip.clone()).build(window, cx)
            })
            .context_menu(move |menu, _, _| menu.item(copy("Copy Name", name.clone())))
            .into_any_element(),
        )
    }

    pub(super) fn document_more_row(
        &self,
        index: usize,
        parent: &[String],
        depth: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = self.connections.get(index).map(|node| node.spec.id.clone());
        let left = id
            .and_then(|id| match self.documents.get(&(id, parent.to_vec())) {
                Some(Children::Loaded { items, shown, .. }) => Some(items.len() - shown),
                _ => None,
            })
            .unwrap_or_default();
        let parent = parent.to_vec();
        h_flex()
            .id(SharedString::from(format!(
                "document-more-{}",
                node_key(index, &parent)
            )))
            .h(px(ROW_HEIGHT))
            .pl(px(26. + depth as f32 * 14.))
            .text_size(px(12.))
            .text_color(color(0x4da3ff))
            .hover(|s| s.text_color(color(0x80bdff)))
            .child(format!("Load more · {} left", thousands(left as u64)))
            .on_click(cx.listener(move |this, _, _, cx| this.show_more(index, parent.clone(), cx)))
            .into_any_element()
    }
}

/// The rows under an open document connection: the nodes read so far,
/// those of expanded nodes below them. While filtering, only nodes whose
/// name matches, or that hold one that does, are listed, and those open by
/// themselves.
pub(super) fn document_rows(index: usize, id: &str, tree: &Tree, rows: &mut Vec<Row>) {
    if !tree.documents.contains_key(&(id.to_string(), Vec::new())) {
        rows.push(note(1, "Loading…", theme::TEXT_TERTIARY));
        return;
    }
    children_rows(index, id, &[], 1, tree, rows);
}

fn children_rows(
    index: usize,
    id: &str,
    parent: &[String],
    depth: usize,
    tree: &Tree,
    rows: &mut Vec<Row>,
) {
    let items = match tree.documents.get(&(id.to_string(), parent.to_vec())) {
        None => return,
        Some(Children::Loading(_)) => {
            rows.push(note(depth, "Loading…", theme::TEXT_TERTIARY));
            return;
        }
        Some(Children::Failed(error)) => {
            rows.push(error_row(depth, error));
            return;
        }
        Some(Children::Indexes(_)) => return,
        Some(Children::Loaded { items, shown, .. }) => (items, *shown),
    };
    let (items, shown) = items;
    if items.is_empty() {
        rows.push(note(depth, "Empty", theme::TEXT_TERTIARY));
        return;
    }
    let searching = !tree.query.is_empty();
    let listed: Vec<(usize, &Container)> = items
        .iter()
        .enumerate()
        .filter(|(_, node)| !searching || matches(id, parent, node, tree))
        .collect();
    for &(item, node) in listed.iter().take(shown) {
        let path = child(parent, &node.name);
        let open = tree.expanded.contains(&node_key(index, &path));
        // A collection opens on its indexes, and only when asked: a
        // filter does not open it by itself.
        let expanded = match has_indexes(node) {
            true => open,
            false => !node.leaf && (open || (searching && holds_match(id, &path, tree))),
        };
        rows.push(Row::DocumentNode {
            index,
            path: parent.to_vec(),
            item,
            depth,
            expanded,
        });
        match (expanded, has_indexes(node)) {
            (false, _) => {}
            (true, true) => index_rows(index, id, &path, depth + 1, tree, rows),
            (true, false) => children_rows(index, id, &path, depth + 1, tree, rows),
        }
    }
    if listed.len() > shown {
        rows.push(Row::DocumentMore {
            index,
            parent: parent.to_vec(),
            depth,
        });
    }
}

/// The indexes of an open collection, as far as they were read.
fn index_rows(
    index: usize,
    id: &str,
    collection: &[String],
    depth: usize,
    tree: &Tree,
    rows: &mut Vec<Row>,
) {
    match tree.documents.get(&(id.to_string(), collection.to_vec())) {
        None | Some(Children::Loading(_)) => {
            rows.push(note(depth, "Loading…", theme::TEXT_TERTIARY));
        }
        Some(Children::Failed(error)) => rows.push(error_row(depth, error)),
        Some(Children::Indexes(indexes)) if indexes.is_empty() => {
            rows.push(note(depth, "No indexes", theme::TEXT_TERTIARY));
        }
        Some(Children::Indexes(indexes)) => {
            rows.extend((0..indexes.len()).map(|item| Row::DocumentIndex {
                index,
                collection: collection.to_vec(),
                item,
                depth,
            }));
        }
        Some(Children::Loaded { .. }) => {}
    }
}

/// Whether a node is listed while filtering: its name matches, or it holds
/// a node read so far that does.
fn matches(id: &str, parent: &[String], node: &Container, tree: &Tree) -> bool {
    node.name.to_lowercase().contains(tree.query)
        || (!node.leaf && holds_match(id, &child(parent, &node.name), tree))
}

fn holds_match(id: &str, path: &[String], tree: &Tree) -> bool {
    match tree.documents.get(&(id.to_string(), path.to_vec())) {
        Some(Children::Loaded { items, .. }) => {
            items.iter().any(|node| matches(id, path, node, tree))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn node(name: &str, kind: ContainerKind) -> Container {
        Container {
            name: name.into(),
            kind,
            leaf: !matches!(kind, ContainerKind::Database | ContainerKind::Scope),
            estimated: None,
            size: None,
        }
    }

    fn loaded(items: Vec<Container>) -> Children {
        Children::Loaded {
            items,
            shown: PAGE,
            counting: None,
        }
    }

    /// `shop` (orders, paid_orders) and `admin`, with `shop` read.
    fn trees() -> DocumentTrees {
        let path = |segments: &[&str]| -> (String, Vec<String>) {
            (
                "m".to_string(),
                segments.iter().map(|s| s.to_string()).collect(),
            )
        };
        [
            (
                path(&[]),
                loaded(vec![
                    node("shop", ContainerKind::Database),
                    node("admin", ContainerKind::Database),
                ]),
            ),
            (
                path(&["shop"]),
                loaded(vec![
                    node("orders", ContainerKind::Collection),
                    node("paid_orders", ContainerKind::View),
                ]),
            ),
        ]
        .into_iter()
        .collect()
    }

    fn rows(trees: &DocumentTrees, expanded: &[&str], query: &str) -> Vec<Row> {
        let expanded: HashSet<String> = expanded.iter().map(|key| key.to_string()).collect();
        let redis_keys = HashMap::new();
        let tree = Tree {
            expanded: &expanded,
            redis_keys: &redis_keys,
            documents: trees,
            query,
        };
        let mut rows = Vec::new();
        document_rows(0, "m", &tree, &mut rows);
        rows
    }

    fn at(path: &[&str], item: usize, depth: usize, expanded: bool) -> Row {
        Row::DocumentNode {
            index: 0,
            path: path.iter().map(|s| s.to_string()).collect(),
            item,
            depth,
            expanded,
        }
    }

    #[test]
    fn keys_nodes_by_their_path() {
        assert_eq!(node_key(0, &[]), "0");
        assert_eq!(
            node_key(2, &["shop".into(), "orders".into()]),
            "2/d:shop/d:orders"
        );
    }

    #[test]
    fn lists_databases_then_the_collections_of_expanded_ones() {
        let trees = trees();
        assert_eq!(
            rows(&trees, &["0/d:shop"], ""),
            [
                at(&[], 0, 1, true),
                at(&["shop"], 0, 2, false),
                at(&["shop"], 1, 2, false),
                at(&[], 1, 1, false),
            ]
        );
        // Collapsed, a database hides its collections.
        assert_eq!(
            rows(&trees, &[], ""),
            [at(&[], 0, 1, false), at(&[], 1, 1, false)]
        );
        // Expanded but not read yet: it is being read.
        let mut reading = trees;
        reading.insert(
            ("m".into(), vec!["admin".into()]),
            Children::Failed("not authorized".into()),
        );
        assert_eq!(
            rows(&reading, &["0/d:admin"], "")[2],
            error_row(2, "not authorized")
        );
    }

    #[test]
    fn many_children_end_in_one_load_more_row() {
        let mut trees = DocumentTrees::new();
        let items = (0..600)
            .map(|n| node(&format!("c{n}"), ContainerKind::Collection))
            .collect();
        trees.insert(
            ("m".into(), Vec::new()),
            loaded(vec![node("big", ContainerKind::Database)]),
        );
        trees.insert(("m".into(), vec!["big".into()]), loaded(items));
        let rows = rows(&trees, &["0/d:big"], "");
        assert_eq!(rows.len(), 1 + PAGE + 1);
        assert_eq!(
            rows.last(),
            Some(&Row::DocumentMore {
                index: 0,
                parent: vec!["big".into()],
                depth: 2,
            })
        );
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, Row::DocumentMore { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn the_filter_keeps_matches_and_the_nodes_that_hold_them() {
        let trees = trees();
        // `paid_orders` is in `shop`, which opens by itself.
        assert_eq!(
            rows(&trees, &[], "paid"),
            [at(&[], 0, 1, true), at(&["shop"], 1, 2, false)]
        );
        assert_eq!(rows(&trees, &[], "adm"), [at(&[], 1, 1, false)]);
        assert!(rows(&trees, &[], "nothing").is_empty());
    }

    fn index(name: &str) -> DocIndex {
        DocIndex {
            name: name.into(),
            keys: Default::default(),
            unique: false,
            sparse: false,
            partial: None,
            ttl: None,
            hidden: false,
            size: None,
            usage: None,
        }
    }

    #[test]
    fn an_open_collection_lists_its_indexes() {
        let mut trees = trees();
        let orders = vec!["shop".to_string(), "orders".to_string()];
        // Opened, while its indexes are read.
        let listed = rows(&trees, &["0/d:shop", "0/d:shop/d:orders"], "");
        assert_eq!(listed[2], note(3, "Loading…", theme::TEXT_TERTIARY));
        trees.insert(
            ("m".into(), orders.clone()),
            Children::Indexes(vec![index("_id_"), index("status_1")]),
        );
        let at_index = |item| Row::DocumentIndex {
            index: 0,
            collection: orders.clone(),
            item,
            depth: 3,
        };
        assert_eq!(
            rows(&trees, &["0/d:shop", "0/d:shop/d:orders"], ""),
            [
                at(&[], 0, 1, true),
                // Its chevron points down.
                at(&["shop"], 0, 2, true),
                at_index(0),
                at_index(1),
                at(&["shop"], 1, 2, false),
                at(&[], 1, 1, false),
            ]
        );
        // Closed, or a view (which has none of its own): no index rows.
        assert_eq!(rows(&trees, &["0/d:shop"], "").len(), 4);
        let view = rows(&trees, &["0/d:shop", "0/d:shop/d:paid_orders"], "");
        assert_eq!(view.len(), 4);
        // While filtering, a collection does not open by itself.
        assert_eq!(
            rows(&trees, &["0/d:shop"], "orders"),
            [
                at(&[], 0, 1, true),
                at(&["shop"], 0, 2, false),
                at(&["shop"], 1, 2, false),
            ]
        );
        trees.insert(("m".into(), orders), Children::Indexes(Vec::new()));
        let empty = rows(&trees, &["0/d:shop", "0/d:shop/d:orders"], "");
        assert_eq!(empty[2], note(3, "No indexes", theme::TEXT_TERTIARY));
    }

    #[test]
    fn index_rows_say_what_kind_of_index() {
        let mut ttl = index("at_1");
        ttl.ttl = Some(60);
        ttl.unique = true;
        assert_eq!(index_traits(&ttl), ["unique", "TTL"]);
        assert!(index_traits(&index("_id_")).is_empty());
    }

    #[test]
    fn says_what_is_still_being_read() {
        assert_eq!(
            rows(&DocumentTrees::new(), &[], ""),
            [note(1, "Loading…", theme::TEXT_TERTIARY)]
        );
        let mut empty = DocumentTrees::new();
        empty.insert(("m".into(), Vec::new()), loaded(Vec::new()));
        assert_eq!(
            rows(&empty, &[], ""),
            [note(1, "Empty", theme::TEXT_TERTIARY)]
        );
    }
}
