//! SQLite catalog: each attached database (`main`, and any other) is a
//! schema; tables, views, columns, keys, indexes and triggers come from
//! `sqlite_master` and the `pragma_*` table functions. SQLite has no stored
//! functions or enums.

use kuery_core::catalog::{
    CatalogOverview, ColumnInfo, ConstraintInfo, ConstraintKind, ForeignKeyRef, IndexInfo,
    TableDefinition, TableInfo, TableKind, TableRef, TriggerInfo, TriggerRef,
};
use kuery_core::completion;
use kuery_core::diagram::{self, Diagram, OnDelete};
use kuery_core::session::QueryError;
use kuery_core::sql::quote_ident;
use rusqlite::Connection;

use crate::{Session, Worker};

/// A column as `pragma_table_xinfo` describes it.
struct Column {
    name: String,
    data_type: String,
    not_null: bool,
    default: Option<String>,
    /// Position in the primary key, 0 when not in it.
    key_position: i64,
    generated: bool,
}

/// One column of a foreign key.
struct ForeignKey {
    id: i64,
    table: String,
    from: String,
    /// `None` when it refers to the other table's primary key.
    to: Option<String>,
    on_update: String,
    on_delete: String,
}

fn failed(error: rusqlite::Error) -> QueryError {
    QueryError::Database {
        code: String::new(),
        message: error.to_string(),
    }
}

/// Attached databases, `main` first; `temp` only when it has something.
fn schemas(connection: &Connection) -> Result<Vec<String>, QueryError> {
    let mut statement = connection
        .prepare("select name from pragma_database_list order by seq")
        .map_err(failed)?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(failed)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(failed)?;
    let mut schemas = Vec::new();
    for name in names {
        if name == "temp" && objects(connection, &name)?.is_empty() {
            continue;
        }
        schemas.push(name);
    }
    Ok(schemas)
}

/// Tables and views of a schema, by name, without SQLite's own.
fn objects(connection: &Connection, schema: &str) -> Result<Vec<(String, TableKind)>, QueryError> {
    let sql = format!(
        "select name, type from {}.sqlite_master \
         where type in ('table', 'view') and name not like 'sqlite\\_%' escape '\\' \
         order by name",
        quote_ident(schema)
    );
    let mut statement = connection.prepare(&sql).map_err(failed)?;
    statement
        .query_map([], |row| {
            let kind = if row.get::<_, String>(1)? == "view" {
                TableKind::View
            } else {
                TableKind::Table
            };
            Ok((row.get::<_, String>(0)?, kind))
        })
        .map_err(failed)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(failed)
}

fn columns(connection: &Connection, table: &TableRef) -> Result<Vec<Column>, QueryError> {
    let mut statement = connection
        .prepare(
            "select name, type, \"notnull\", dflt_value, pk, hidden \
             from pragma_table_xinfo(?1, ?2) where hidden <> 1 order by cid",
        )
        .map_err(failed)?;
    statement
        .query_map([&table.name, &table.schema], |row| {
            Ok(Column {
                name: row.get(0)?,
                data_type: row.get(1)?,
                not_null: row.get::<_, i64>(2)? != 0,
                default: row.get(3)?,
                key_position: row.get(4)?,
                generated: row.get::<_, i64>(5)? >= 2,
            })
        })
        .map_err(failed)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(failed)
}

fn foreign_keys(connection: &Connection, table: &TableRef) -> Result<Vec<ForeignKey>, QueryError> {
    let mut statement = connection
        .prepare(
            "select id, \"table\", \"from\", \"to\", on_update, on_delete \
             from pragma_foreign_key_list(?1, ?2) order by id, seq",
        )
        .map_err(failed)?;
    statement
        .query_map([&table.name, &table.schema], |row| {
            Ok(ForeignKey {
                id: row.get(0)?,
                table: row.get(1)?,
                from: row.get(2)?,
                to: row.get(3)?,
                on_update: row.get(4)?,
                on_delete: row.get(5)?,
            })
        })
        .map_err(failed)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(failed)
}

/// The primary key's columns of a table, in key order.
fn primary_key(connection: &Connection, table: &TableRef) -> Result<Vec<String>, QueryError> {
    let mut keyed: Vec<(i64, String)> = columns(connection, table)?
        .into_iter()
        .filter(|c| c.key_position > 0)
        .map(|c| (c.key_position, c.name))
        .collect();
    keyed.sort();
    Ok(keyed.into_iter().map(|(_, name)| name).collect())
}

/// What a foreign key's column refers to: its own `to`, or the column of
/// the other table's primary key at the same position.
fn referenced(
    connection: &Connection,
    schema: &str,
    keys: &[&ForeignKey],
) -> Result<Vec<String>, QueryError> {
    if keys.iter().all(|k| k.to.is_some()) {
        return Ok(keys.iter().filter_map(|k| k.to.clone()).collect());
    }
    let parent = primary_key(connection, &TableRef::new(schema, &keys[0].table))?;
    Ok(keys
        .iter()
        .enumerate()
        .map(|(index, key)| {
            key.to
                .clone()
                .or_else(|| parent.get(index).cloned())
                .unwrap_or_default()
        })
        .collect())
}

/// Foreign keys grouped by constraint, as (columns, referenced columns,
/// the first column's row).
fn grouped(keys: &[ForeignKey]) -> Vec<Vec<&ForeignKey>> {
    let mut groups: Vec<Vec<&ForeignKey>> = Vec::new();
    for key in keys {
        match groups.last_mut() {
            Some(group) if group[0].id == key.id => group.push(key),
            _ => groups.push(vec![key]),
        }
    }
    groups
}

impl Worker {
    fn overview(&self) -> Result<CatalogOverview, QueryError> {
        let connection = self.connection();
        let schemas = schemas(connection)?;
        let mut overview = CatalogOverview {
            schemas: schemas.clone(),
            ..CatalogOverview::default()
        };
        for schema in &schemas {
            for (name, kind) in objects(connection, schema)? {
                overview.tables.push(TableInfo {
                    table: TableRef::new(schema.clone(), name),
                    kind,
                    estimated_rows: None,
                    comment: None,
                });
            }
            let sql = format!(
                "select tbl_name, name from {}.sqlite_master where type = 'trigger' order by 1, 2",
                quote_ident(schema)
            );
            let mut statement = connection.prepare(&sql).map_err(failed)?;
            let triggers = statement
                .query_map([], |row| {
                    Ok(TriggerRef {
                        table: TableRef::new(schema.clone(), row.get::<_, String>(0)?),
                        name: row.get(1)?,
                    })
                })
                .map_err(failed)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(failed)?;
            overview.triggers.extend(triggers);
        }
        Ok(overview)
    }

    fn definition(&self, table: &TableRef) -> Result<TableDefinition, QueryError> {
        let connection = self.connection();
        let keys = foreign_keys(connection, table)?;
        let groups = grouped(&keys);
        let mut definition = TableDefinition::default();
        for column in columns(connection, table)? {
            let foreign_key = groups.iter().find_map(|group| {
                let position = group.iter().position(|k| k.from == column.name)?;
                let to = referenced(connection, &table.schema, group).ok()?;
                Some(ForeignKeyRef {
                    table: TableRef::new(table.schema.clone(), group[0].table.clone()),
                    column: to.get(position).cloned().unwrap_or_default(),
                })
            });
            definition.columns.push(ColumnInfo {
                name: column.name,
                data_type: column.data_type,
                nullable: !column.not_null && column.key_position == 0,
                default: column.default,
                primary_key: column.key_position > 0,
                foreign_key,
                generated: column.generated,
                comment: None,
            });
        }

        let primary = definition.primary_key();
        if !primary.is_empty() {
            definition.constraints.push(ConstraintInfo {
                name: "primary key".into(),
                kind: ConstraintKind::PrimaryKey,
                definition: format!(
                    "PRIMARY KEY ({})",
                    primary
                        .iter()
                        .map(|c| quote_ident(c))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
        for group in &groups {
            let to = referenced(connection, &table.schema, group)?;
            let from: Vec<String> = group.iter().map(|k| quote_ident(&k.from)).collect();
            let to: Vec<String> = to.iter().map(|c| quote_ident(c)).collect();
            let mut text = format!(
                "FOREIGN KEY ({}) REFERENCES {} ({})",
                from.join(", "),
                quote_ident(&group[0].table),
                to.join(", ")
            );
            for (event, rule) in [
                ("DELETE", &group[0].on_delete),
                ("UPDATE", &group[0].on_update),
            ] {
                if rule != "NO ACTION" {
                    text.push_str(&format!(" ON {event} {rule}"));
                }
            }
            definition.constraints.push(ConstraintInfo {
                name: format!("foreign key {}", group[0].id),
                kind: ConstraintKind::ForeignKey,
                definition: text,
            });
        }

        let mut statement = connection
            .prepare(
                "select l.name, l.\"unique\", l.origin, m.sql \
                 from pragma_index_list(?1, ?2) l \
                 left join sqlite_master m on m.type = 'index' and m.name = l.name \
                 order by l.name",
            )
            .map_err(failed)?;
        let indexes = statement
            .query_map([&table.name, &table.schema], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? != 0,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(failed)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(failed)?;
        for (name, unique, origin, sql) in indexes {
            let mut columns = connection
                .prepare("select name from pragma_index_info(?1, ?2) order by seqno")
                .map_err(failed)?;
            let names = columns
                .query_map([&name, &table.schema], |row| {
                    row.get::<_, Option<String>>(0)
                })
                .map_err(failed)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(failed)?
                .into_iter()
                .map(|c| c.map_or_else(|| "(expression)".into(), |c| quote_ident(&c)))
                .collect::<Vec<_>>()
                .join(", ");
            let definition_text = sql.unwrap_or_else(|| {
                format!(
                    "{} ({names})",
                    if origin == "pk" {
                        "PRIMARY KEY"
                    } else {
                        "UNIQUE"
                    }
                )
            });
            if origin == "u" {
                definition.constraints.push(ConstraintInfo {
                    name: name.clone(),
                    kind: ConstraintKind::Unique,
                    definition: format!("UNIQUE ({names})"),
                });
            }
            definition.indexes.push(IndexInfo {
                name,
                definition: definition_text,
                unique,
                primary: origin == "pk",
                size_bytes: None,
            });
        }

        let sql = format!(
            "select name, sql from {}.sqlite_master where type = 'trigger' and tbl_name = ?1 \
             order by name",
            quote_ident(&table.schema)
        );
        let mut statement = connection.prepare(&sql).map_err(failed)?;
        definition.triggers = statement
            .query_map([&table.name], |row| {
                Ok(TriggerInfo {
                    name: row.get(0)?,
                    definition: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    enabled: true,
                })
            })
            .map_err(failed)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(failed)?;
        Ok(definition)
    }

    fn completion_catalog(&self) -> Result<completion::Catalog, QueryError> {
        let connection = self.connection();
        let schemas = schemas(connection)?;
        let mut tables = Vec::new();
        for schema in &schemas {
            for (name, kind) in objects(connection, schema)? {
                let table = TableRef::new(schema.clone(), name);
                let columns = columns(connection, &table)?
                    .into_iter()
                    .map(|c| completion::Column {
                        name: c.name,
                        data_type: c.data_type,
                    })
                    .collect();
                tables.push(completion::Table {
                    table,
                    kind,
                    columns,
                });
            }
        }
        Ok(completion::Catalog {
            search_path: vec!["main".into()],
            schemas,
            tables,
        })
    }

    fn diagram(&self, schema: &str) -> Result<Diagram, QueryError> {
        let connection = self.connection();
        let mut tables = Vec::new();
        let mut relations = Vec::new();
        for (name, kind) in objects(connection, schema)? {
            if kind != TableKind::Table {
                continue;
            }
            let table = TableRef::new(schema, &name);
            let keys = foreign_keys(connection, &table)?;
            let in_keys: Vec<&str> = keys.iter().map(|k| k.from.as_str()).collect();
            let columns = columns(connection, &table)?;
            let nullable = |column: &str| {
                columns
                    .iter()
                    .any(|c| c.name == column && !c.not_null && c.key_position == 0)
            };
            for group in grouped(&keys) {
                let from_columns: Vec<String> = group.iter().map(|k| k.from.clone()).collect();
                relations.push(diagram::Relation {
                    name: format!("{name}_fk_{}", group[0].id),
                    from: table.clone(),
                    optional: from_columns.iter().any(|c| nullable(c)),
                    from_columns,
                    to: TableRef::new(schema, &group[0].table),
                    to_columns: referenced(connection, schema, &group)?,
                    on_delete: OnDelete::from_rule(&group[0].on_delete),
                });
            }
            tables.push(diagram::Table {
                table,
                rows: None,
                columns: columns
                    .iter()
                    .map(|c| diagram::Column {
                        name: c.name.clone(),
                        data_type: c.data_type.to_lowercase(),
                        nullable: !c.not_null && c.key_position == 0,
                        primary_key: c.key_position > 0,
                        foreign_key: in_keys.contains(&c.name.as_str()),
                    })
                    .collect(),
            });
        }
        Ok(Diagram {
            schema: schema.to_string(),
            tables,
            relations,
        })
    }
}

/// Schemas with their tables, views and triggers.
pub async fn overview(session: Session) -> Result<CatalogOverview, QueryError> {
    session.call(|worker| worker.overview()).await
}

pub async fn definition(session: Session, table: TableRef) -> Result<TableDefinition, QueryError> {
    session.call(move |worker| worker.definition(&table)).await
}

pub async fn completion_catalog(session: Session) -> Result<completion::Catalog, QueryError> {
    session.call(|worker| worker.completion_catalog()).await
}

pub async fn diagram(session: Session, schema: String) -> Result<Diagram, QueryError> {
    session.call(move |worker| worker.diagram(&schema)).await
}
