//! The explorer tree as a flat list of rows, top to bottom. Each row names
//! what it shows by position (the connection, the object's index in the
//! catalog) rather than holding a copy, so the list is cheap to build for a
//! catalog of thousands of objects. The sidebar keeps the list and builds an
//! element only for the rows on screen.

use std::collections::HashSet;

use gpui_kit::SharedString;
use kuery_core::catalog::CatalogOverview;

use super::redis_keys::{self, KeyBrowsers, Keyspace};
use super::{CatalogState, ConnectionNode, ConnectionState, Group};
use crate::theme;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Row {
    Connection {
        index: usize,
        expanded: bool,
    },
    /// A line of text under a node: "Connecting…", "Loading catalog…".
    Note {
        depth: usize,
        text: SharedString,
        tone: u32,
    },
    /// An error under a node, cut to one line; its tooltip has all of it.
    Error {
        depth: usize,
        text: SharedString,
    },
    /// The link under a connection that failed to open.
    EditConnection {
        index: usize,
    },
    /// `schema` is its position in the catalog's schemas.
    Schema {
        index: usize,
        schema: usize,
        expanded: bool,
    },
    Group {
        index: usize,
        schema: usize,
        group: Group,
        count: usize,
        open: bool,
    },
    /// A table or view, by its position in the catalog's tables.
    Table {
        index: usize,
        table: usize,
    },
    Function {
        index: usize,
        function: usize,
    },
    Trigger {
        index: usize,
        trigger: usize,
    },
    Enum {
        index: usize,
        item: usize,
    },
    RedisDatabase {
        index: usize,
        database: i64,
        keys: u64,
        expanded: bool,
    },
    /// A folder of keys: `key` is its tree key, `name` its last segment.
    RedisFolder {
        index: usize,
        database: i64,
        key: String,
        name: String,
        count: usize,
        depth: usize,
        open: bool,
    },
    RedisKey {
        index: usize,
        database: i64,
        label: String,
        key: String,
        depth: usize,
    },
    /// "Load more keys", with how many were read so far.
    RedisMore {
        index: usize,
        database: i64,
        count: usize,
    },
}

/// What the rows need to know about one connection.
pub(super) enum NodeView<'a> {
    Idle,
    Connecting,
    Failed(&'a str),
    Catalog(&'a CatalogState),
    Redis {
        id: &'a str,
        keyspace: &'a Keyspace,
        default_db: i64,
    },
}

impl<'a> NodeView<'a> {
    pub(super) fn of(node: &'a ConnectionNode) -> Self {
        match &node.state {
            ConnectionState::Idle => NodeView::Idle,
            ConnectionState::Connecting => NodeView::Connecting,
            ConnectionState::Failed(error) => NodeView::Failed(error),
            ConnectionState::Ready { catalog, .. } => NodeView::Catalog(catalog),
            ConnectionState::RedisReady { db, keyspace } => NodeView::Redis {
                id: &node.spec.id,
                keyspace,
                default_db: db.session.default_db(),
            },
        }
    }

    fn is_open(&self) -> bool {
        matches!(self, NodeView::Catalog(_) | NodeView::Redis { .. })
    }
}

/// The tree's state the rows follow, apart from the connections.
pub(super) struct Tree<'a> {
    /// Keys of expanded nodes, as in [`super::Explorer`].
    pub expanded: &'a HashSet<String>,
    pub redis_keys: &'a KeyBrowsers,
    /// The filter, trimmed and lowercased; empty for none.
    pub query: &'a str,
}

/// Every row of the tree, top to bottom. While filtering, open connections,
/// their schemas and groups open by themselves, and empty ones are left out.
pub(super) fn tree_rows<'a>(
    nodes: impl IntoIterator<Item = NodeView<'a>>,
    tree: &Tree,
) -> Vec<Row> {
    let searching = !tree.query.is_empty();
    let mut rows = Vec::new();
    for (index, node) in nodes.into_iter().enumerate() {
        let expanded = tree.expanded.contains(&index.to_string()) || (searching && node.is_open());
        rows.push(Row::Connection { index, expanded });
        if !expanded {
            continue;
        }
        match node {
            NodeView::Idle => {}
            NodeView::Connecting => rows.push(note(1, "Connecting…", theme::TEXT_TERTIARY)),
            NodeView::Failed(error) => {
                rows.push(error_row(1, error));
                rows.push(Row::EditConnection { index });
            }
            NodeView::Redis {
                id,
                keyspace,
                default_db,
            } => redis_keys::redis_rows(index, id, keyspace, default_db, tree, &mut rows),
            NodeView::Catalog(CatalogState::Loading) => {
                rows.push(note(1, "Loading catalog…", theme::TEXT_TERTIARY))
            }
            NodeView::Catalog(CatalogState::Failed(error)) => rows.push(error_row(1, error)),
            NodeView::Catalog(CatalogState::Loaded(overview)) => {
                for (position, schema) in overview.schemas.iter().enumerate() {
                    let objects = SchemaObjects::collect(overview, schema, tree.query);
                    if searching && objects.is_empty() {
                        continue;
                    }
                    schema_rows(index, position, schema, &objects, tree, &mut rows);
                }
            }
        }
    }
    rows
}

pub(super) fn note(depth: usize, text: impl Into<SharedString>, tone: u32) -> Row {
    Row::Note {
        depth,
        text: text.into(),
        tone,
    }
}

pub(super) fn error_row(depth: usize, text: &str) -> Row {
    Row::Error {
        depth,
        text: text.to_owned().into(),
    }
}

fn schema_rows(
    index: usize,
    position: usize,
    schema: &str,
    objects: &SchemaObjects,
    tree: &Tree,
    rows: &mut Vec<Row>,
) {
    let searching = !tree.query.is_empty();
    let schema_key = format!("{index}/{schema}");
    let expanded = searching || tree.expanded.contains(&schema_key);
    rows.push(Row::Schema {
        index,
        schema: position,
        expanded,
    });
    if !expanded {
        return;
    }
    for group in Group::ALL {
        let items = objects.of(group);
        if items.is_empty()
            && (searching || matches!(group, Group::Triggers | Group::Enums | Group::Views))
        {
            continue;
        }
        let open = searching
            || tree
                .expanded
                .contains(&format!("{schema_key}/{}", group.key()));
        rows.push(Row::Group {
            index,
            schema: position,
            group,
            count: items.len(),
            open,
        });
        if !open {
            continue;
        }
        rows.extend(items.iter().map(|&item| match group {
            Group::Tables | Group::Views => Row::Table { index, table: item },
            Group::Functions => Row::Function {
                index,
                function: item,
            },
            Group::Triggers => Row::Trigger {
                index,
                trigger: item,
            },
            Group::Enums => Row::Enum { index, item },
        }));
    }
}

/// One schema's objects that match the filter, as positions in the catalog.
#[derive(Default)]
struct SchemaObjects {
    tables: Vec<usize>,
    views: Vec<usize>,
    functions: Vec<usize>,
    triggers: Vec<usize>,
    enums: Vec<usize>,
}

impl SchemaObjects {
    fn collect(overview: &CatalogOverview, schema: &str, query: &str) -> Self {
        let matches = |name: &str| query.is_empty() || name.to_lowercase().contains(query);
        let (tables, views) = overview
            .tables
            .iter()
            .enumerate()
            .filter(|(_, t)| t.table.schema == schema && matches(&t.table.name))
            .partition::<Vec<_>, _>(|(_, t)| t.kind.is_editable());
        let positions = |list: Vec<(usize, _)>| list.into_iter().map(|(n, _)| n).collect();
        Self {
            tables: positions(tables),
            views: positions(views),
            functions: overview
                .functions
                .iter()
                .enumerate()
                .filter(|(_, f)| f.schema == schema && matches(&f.name))
                .map(|(n, _)| n)
                .collect(),
            triggers: overview
                .triggers
                .iter()
                .enumerate()
                .filter(|(_, t)| {
                    t.table.schema == schema && (matches(&t.name) || matches(&t.table.name))
                })
                .map(|(n, _)| n)
                .collect(),
            enums: overview
                .enums
                .iter()
                .enumerate()
                .filter(|(_, e)| e.schema == schema && matches(&e.name))
                .map(|(n, _)| n)
                .collect(),
        }
    }

    fn of(&self, group: Group) -> &[usize] {
        match group {
            Group::Tables => &self.tables,
            Group::Views => &self.views,
            Group::Functions => &self.functions,
            Group::Triggers => &self.triggers,
            Group::Enums => &self.enums,
        }
    }

    fn is_empty(&self) -> bool {
        Group::ALL.iter().all(|g| self.of(*g).is_empty())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use kuery_core::catalog::{
        EnumInfo, FunctionInfo, RoutineKind, TableInfo, TableKind, TableRef, TriggerRef,
    };

    use super::*;

    fn table(schema: &str, name: &str, kind: TableKind) -> TableInfo {
        TableInfo {
            table: TableRef {
                schema: schema.into(),
                name: name.into(),
            },
            kind,
            estimated_rows: None,
            comment: None,
        }
    }

    fn function(schema: &str, name: &str) -> FunctionInfo {
        FunctionInfo {
            schema: schema.into(),
            name: name.into(),
            arguments: String::new(),
            result: "void".into(),
            kind: RoutineKind::Function,
            language: "sql".into(),
            oid: 0,
        }
    }

    /// `public`: tables 0 orders, 1 customers, 3 order_totals (a view);
    /// functions 0 refresh_orders, 1 audit; trigger 0 orders_audit; enum 0
    /// status. `audit`: table 2 log, function 2 purge.
    fn overview() -> CatalogOverview {
        CatalogOverview {
            schemas: vec!["public".into(), "audit".into()],
            tables: vec![
                table("public", "orders", TableKind::Table),
                table("public", "customers", TableKind::Table),
                table("audit", "log", TableKind::Table),
                table("public", "order_totals", TableKind::View),
            ],
            functions: vec![
                function("public", "refresh_orders"),
                function("public", "audit"),
                function("audit", "purge"),
            ],
            enums: vec![EnumInfo {
                schema: "public".into(),
                name: "status".into(),
                values: vec!["new".into(), "paid".into()],
            }],
            triggers: vec![TriggerRef {
                table: TableRef {
                    schema: "public".into(),
                    name: "orders".into(),
                },
                name: "orders_audit".into(),
            }],
        }
    }

    fn rows(state: &CatalogState, expanded: &[&str], query: &str) -> Vec<Row> {
        let expanded = expanded.iter().map(|key| key.to_string()).collect();
        let redis_keys = HashMap::new();
        let tree = Tree {
            expanded: &expanded,
            redis_keys: &redis_keys,
            query,
        };
        tree_rows([NodeView::Catalog(state)], &tree)
    }

    fn group(schema: usize, group: Group, count: usize, open: bool) -> Row {
        Row::Group {
            index: 0,
            schema,
            group,
            count,
            open,
        }
    }

    fn schema(schema: usize, expanded: bool) -> Row {
        Row::Schema {
            index: 0,
            schema,
            expanded,
        }
    }

    const CONNECTION: Row = Row::Connection {
        index: 0,
        expanded: true,
    };

    #[test]
    fn a_collapsed_connection_is_one_row() {
        let loaded = CatalogState::Loaded(overview());
        assert_eq!(
            rows(&loaded, &["0/public", "0/public/tables"], ""),
            [Row::Connection {
                index: 0,
                expanded: false
            }]
        );
    }

    #[test]
    fn collapsed_groups_show_their_counts_and_hide_when_empty() {
        let loaded = CatalogState::Loaded(overview());
        assert_eq!(
            rows(&loaded, &["0", "0/public", "0/audit"], ""),
            [
                CONNECTION,
                schema(0, true),
                group(0, Group::Tables, 2, false),
                group(0, Group::Views, 1, false),
                group(0, Group::Functions, 2, false),
                group(0, Group::Triggers, 1, false),
                group(0, Group::Enums, 1, false),
                // No views, triggers or enums: those groups are left out.
                schema(1, true),
                group(1, Group::Tables, 1, false),
                group(1, Group::Functions, 1, false),
            ]
        );
        assert_eq!(
            rows(&loaded, &["0"], ""),
            [CONNECTION, schema(0, false), schema(1, false)]
        );
    }

    #[test]
    fn open_groups_list_their_objects_by_catalog_position() {
        let loaded = CatalogState::Loaded(overview());
        let expanded = [
            "0",
            "0/public",
            "0/public/tables",
            "0/public/views",
            "0/public/functions",
            "0/public/triggers",
            "0/public/enums",
        ];
        assert_eq!(
            rows(&loaded, &expanded, ""),
            [
                CONNECTION,
                schema(0, true),
                group(0, Group::Tables, 2, true),
                Row::Table { index: 0, table: 0 },
                Row::Table { index: 0, table: 1 },
                group(0, Group::Views, 1, true),
                Row::Table { index: 0, table: 3 },
                group(0, Group::Functions, 2, true),
                Row::Function {
                    index: 0,
                    function: 0
                },
                Row::Function {
                    index: 0,
                    function: 1
                },
                group(0, Group::Triggers, 1, true),
                Row::Trigger {
                    index: 0,
                    trigger: 0
                },
                group(0, Group::Enums, 1, true),
                Row::Enum { index: 0, item: 0 },
                schema(1, false),
            ]
        );
    }

    #[test]
    fn the_filter_opens_matching_groups_and_drops_the_rest() {
        let loaded = CatalogState::Loaded(overview());
        // Nothing is expanded: the filter opens the connection, schemas and
        // groups. A trigger matches by its table's name too.
        assert_eq!(
            rows(&loaded, &[], "order"),
            [
                CONNECTION,
                schema(0, true),
                group(0, Group::Tables, 1, true),
                Row::Table { index: 0, table: 0 },
                group(0, Group::Views, 1, true),
                Row::Table { index: 0, table: 3 },
                group(0, Group::Functions, 1, true),
                Row::Function {
                    index: 0,
                    function: 0
                },
                group(0, Group::Triggers, 1, true),
                Row::Trigger {
                    index: 0,
                    trigger: 0
                },
            ]
        );
        // A schema with no match is left out.
        assert_eq!(
            rows(&loaded, &[], "purge"),
            [
                CONNECTION,
                schema(1, true),
                group(1, Group::Functions, 1, true),
                Row::Function {
                    index: 0,
                    function: 2
                },
            ]
        );
        assert_eq!(rows(&loaded, &[], "nothing like it"), [CONNECTION]);
    }

    #[test]
    fn a_connection_that_is_not_open_shows_its_state() {
        let expanded: HashSet<String> = ["0", "1", "2", "3"].map(String::from).into();
        let redis_keys = HashMap::new();
        let tree = Tree {
            expanded: &expanded,
            redis_keys: &redis_keys,
            query: "",
        };
        let (loading, failed) = (
            CatalogState::Loading,
            CatalogState::Failed("permission denied".into()),
        );
        let nodes = [
            NodeView::Idle,
            NodeView::Connecting,
            NodeView::Failed("connection refused"),
            NodeView::Catalog(&loading),
            NodeView::Catalog(&failed),
        ];
        let open = |index| Row::Connection {
            index,
            expanded: true,
        };
        assert_eq!(
            tree_rows(nodes, &tree),
            [
                open(0),
                open(1),
                note(1, "Connecting…", theme::TEXT_TERTIARY),
                open(2),
                error_row(1, "connection refused"),
                Row::EditConnection { index: 2 },
                open(3),
                note(1, "Loading catalog…", theme::TEXT_TERTIARY),
                // Not expanded.
                Row::Connection {
                    index: 4,
                    expanded: false
                },
            ]
        );
    }

    #[test]
    fn the_filter_opens_only_open_connections() {
        let expanded = HashSet::new();
        let redis_keys = HashMap::new();
        let tree = Tree {
            expanded: &expanded,
            redis_keys: &redis_keys,
            query: "x",
        };
        let failed = CatalogState::Failed("permission denied".into());
        assert_eq!(
            tree_rows([NodeView::Idle, NodeView::Catalog(&failed)], &tree),
            [
                Row::Connection {
                    index: 0,
                    expanded: false
                },
                Row::Connection {
                    index: 1,
                    expanded: true
                },
                error_row(1, "permission denied"),
            ]
        );
    }
}
