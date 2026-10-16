//! PostgreSQL catalog: tables, columns, keys, indexes, constraints and
//! triggers, read from `pg_catalog`.

use kuery_core::catalog::{
    CatalogOverview, ColumnInfo, ConstraintInfo, ConstraintKind, EnumInfo, ForeignKeyRef,
    FunctionInfo, IndexInfo, RoutineKind, TableDefinition, TableInfo, TableKind, TableRef,
    TriggerInfo, TriggerRef,
};
use kuery_core::completion;
use kuery_core::diagram::{self, Diagram, OnDelete};
use kuery_core::result_set::ResultSet;
use kuery_core::sql::{quote_literal, quote_qualified};

use crate::{QueryError, Session};

const TABLES_SQL: &str = r"
select n.nspname,
       c.relname,
       c.relkind::text,
       case when c.reltuples < 0 then null else c.reltuples::bigint end,
       pg_catalog.obj_description(c.oid, 'pg_class')
from pg_catalog.pg_class c
join pg_catalog.pg_namespace n on n.oid = c.relnamespace
where c.relkind in ('r', 'p', 'v', 'm', 'f')
  and not c.relispartition
  and n.nspname not in ('pg_catalog', 'information_schema')
  and n.nspname !~ '^pg_(toast|temp)'
order by 1, 2";

/// User schemas: everything except `pg_*` and `information_schema`.
const SCHEMAS_SQL: &str = r"
select nspname
from pg_catalog.pg_namespace
where nspname <> 'information_schema' and nspname !~ '^pg_'
order by 1";

/// Functions and procedures, without those that belong to an extension.
const FUNCTIONS_SQL: &str = r"
select n.nspname,
       p.proname,
       pg_catalog.pg_get_function_identity_arguments(p.oid),
       coalesce(pg_catalog.pg_get_function_result(p.oid), ''),
       p.prokind::text,
       l.lanname,
       p.oid::text
from pg_catalog.pg_proc p
join pg_catalog.pg_namespace n on n.oid = p.pronamespace
join pg_catalog.pg_language l on l.oid = p.prolang
where n.nspname <> 'information_schema' and n.nspname !~ '^pg_'
  and not exists (
      select 1 from pg_catalog.pg_depend d
      where d.classid = 'pg_catalog.pg_proc'::regclass and d.objid = p.oid and d.deptype = 'e')
order by 1, 2, 3";

/// Enum labels are joined with the ASCII unit separator, which cannot appear
/// in a label by accident.
const ENUMS_SQL: &str = r"
select n.nspname, t.typname, string_agg(e.enumlabel, E'\x1f' order by e.enumsortorder)
from pg_catalog.pg_type t
join pg_catalog.pg_enum e on e.enumtypid = t.oid
join pg_catalog.pg_namespace n on n.oid = t.typnamespace
where n.nspname <> 'information_schema' and n.nspname !~ '^pg_'
group by 1, 2
order by 1, 2";

const TRIGGERS_SQL: &str = r"
select n.nspname, c.relname, t.tgname
from pg_catalog.pg_trigger t
join pg_catalog.pg_class c on c.oid = t.tgrelid
join pg_catalog.pg_namespace n on n.oid = c.relnamespace
where not t.tgisinternal and n.nspname <> 'information_schema' and n.nspname !~ '^pg_'
order by 1, 3";

/// Every column of every table and view in a user schema, for completion.
const COLUMNS_SQL: &str = r"
select n.nspname,
       c.relname,
       c.relkind::text,
       a.attname,
       pg_catalog.format_type(a.atttypid, a.atttypmod)
from pg_catalog.pg_class c
join pg_catalog.pg_namespace n on n.oid = c.relnamespace
left join pg_catalog.pg_attribute a
  on a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped
where c.relkind in ('r', 'p', 'v', 'm', 'f')
  and not c.relispartition
  and n.nspname not in ('pg_catalog', 'information_schema')
  and n.nspname !~ '^pg_(toast|temp)'
order by 1, 2, a.attnum";

/// A schema's tables and columns, with which are primary or foreign keys,
/// for a diagram. `{schema}` is a quoted literal.
const DIAGRAM_COLUMNS_SQL: &str = r"
select c.relname,
       case when c.reltuples < 0 then null else c.reltuples::bigint end,
       a.attname,
       pg_catalog.format_type(a.atttypid, a.atttypmod),
       not a.attnotnull,
       exists (select 1 from pg_catalog.pg_constraint p
               where p.conrelid = c.oid and p.contype = 'p' and a.attnum = any (p.conkey)),
       exists (select 1 from pg_catalog.pg_constraint f
               where f.conrelid = c.oid and f.contype = 'f' and a.attnum = any (f.conkey))
from pg_catalog.pg_class c
join pg_catalog.pg_namespace n on n.oid = c.relnamespace
join pg_catalog.pg_attribute a on a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped
where n.nspname = {schema} and c.relkind in ('r', 'p') and not c.relispartition
order by c.relname, a.attnum";

/// A schema's foreign keys (not the copies partitions inherit): columns in
/// order, joined with the unit separator.
const DIAGRAM_KEYS_SQL: &str = r"
select con.conname,
       src.relname,
       tn.nspname,
       tgt.relname,
       (select string_agg(a.attname, E'\x1f' order by k.i)
          from unnest(con.conkey) with ordinality k(n, i)
          join pg_catalog.pg_attribute a on a.attrelid = con.conrelid and a.attnum = k.n),
       (select string_agg(a.attname, E'\x1f' order by k.i)
          from unnest(con.confkey) with ordinality k(n, i)
          join pg_catalog.pg_attribute a on a.attrelid = con.confrelid and a.attnum = k.n),
       con.confdeltype::text,
       (select bool_or(not a.attnotnull)
          from unnest(con.conkey) k(n)
          join pg_catalog.pg_attribute a on a.attrelid = con.conrelid and a.attnum = k.n)
from pg_catalog.pg_constraint con
join pg_catalog.pg_class src on src.oid = con.conrelid
join pg_catalog.pg_namespace sn on sn.oid = src.relnamespace
join pg_catalog.pg_class tgt on tgt.oid = con.confrelid
join pg_catalog.pg_namespace tn on tn.oid = tgt.relnamespace
where con.contype = 'f' and con.conparentid = 0 and not src.relispartition
  and sn.nspname = {schema}
order by 2, 1";

const SEARCH_PATH_SQL: &str = "select pg_catalog.unnest(pg_catalog.current_schemas(false))";

/// Everything the explorer tree needs for one connection.
pub async fn overview(session: Session) -> Result<CatalogOverview, QueryError> {
    let schemas = session.fetch_trusted(SCHEMAS_SQL.into()).await?;
    let tables = tables(session.clone()).await?;
    let functions = session.fetch_trusted(FUNCTIONS_SQL.into()).await?;
    let enums = session.fetch_trusted(ENUMS_SQL.into()).await?;
    let triggers = session.fetch_trusted(TRIGGERS_SQL.into()).await?;
    Ok(CatalogOverview {
        schemas: rows(&schemas).map(|row| row.text(0)).collect(),
        tables,
        functions: rows(&functions)
            .map(|row| FunctionInfo {
                schema: row.text(0),
                name: row.text(1),
                arguments: row.text(2),
                result: row.text(3),
                kind: match row.text(4).as_str() {
                    "p" => RoutineKind::Procedure,
                    "a" => RoutineKind::Aggregate,
                    "w" => RoutineKind::Window,
                    _ => RoutineKind::Function,
                },
                language: row.text(5),
                oid: row.text(6).parse().unwrap_or(0),
            })
            .collect(),
        enums: rows(&enums)
            .map(|row| EnumInfo {
                schema: row.text(0),
                name: row.text(1),
                values: row.text(2).split('\u{1f}').map(str::to_owned).collect(),
            })
            .collect(),
        triggers: rows(&triggers)
            .map(|row| TriggerRef {
                table: TableRef::new(row.text(0), row.text(1)),
                name: row.text(2),
            })
            .collect(),
    })
}

/// A schema's tables, columns and foreign keys, for its diagram.
pub async fn diagram(session: Session, schema: String) -> Result<Diagram, QueryError> {
    let literal = quote_literal(&schema).map_err(|error| QueryError::Database {
        code: "22021".into(),
        message: error.to_string(),
    })?;
    let columns = session
        .fetch_trusted(DIAGRAM_COLUMNS_SQL.replace("{schema}", &literal))
        .await?;
    let keys = session
        .fetch_trusted(DIAGRAM_KEYS_SQL.replace("{schema}", &literal))
        .await?;
    let mut tables: Vec<diagram::Table> = Vec::new();
    for row in rows(&columns) {
        let name = row.text(0);
        if tables.last().is_none_or(|t| t.table.name != name) {
            tables.push(diagram::Table {
                table: TableRef::new(schema.clone(), name),
                rows: row.optional(1).and_then(|r| r.parse().ok()),
                columns: Vec::new(),
            });
        }
        if let Some(table) = tables.last_mut() {
            table.columns.push(diagram::Column {
                name: row.text(2),
                data_type: row.text(3),
                nullable: row.flag(4),
                primary_key: row.flag(5),
                foreign_key: row.flag(6),
            });
        }
    }
    let split = |text: String| -> Vec<String> {
        text.split('\u{1f}')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let relations = rows(&keys)
        .map(|row| diagram::Relation {
            name: row.text(0),
            from: TableRef::new(schema.clone(), row.text(1)),
            to: TableRef::new(row.text(2), row.text(3)),
            from_columns: split(row.text(4)),
            to_columns: split(row.text(5)),
            on_delete: OnDelete::from_code(&row.text(6)),
            optional: row.flag(7),
        })
        .collect();
    Ok(Diagram {
        schema,
        tables,
        relations,
    })
}

/// `CREATE OR REPLACE FUNCTION …` for a function or procedure. Aggregates have
/// no such definition, so they get a short description instead.
pub async fn function_definition(
    session: Session,
    function: FunctionInfo,
) -> Result<String, QueryError> {
    if function.kind == RoutineKind::Aggregate {
        return Ok(format!(
            "-- {}.{}({}) is an aggregate; PostgreSQL has no CREATE statement to show for it.\n",
            function.schema, function.name, function.arguments
        ));
    }
    let set = session
        .fetch_trusted(format!(
            "select pg_catalog.pg_get_functiondef({})",
            function.oid
        ))
        .await?;
    Ok(set.cell(0, 0).flatten().unwrap_or_default().to_owned())
}

/// Tables and views from every user schema.
pub async fn tables(session: Session) -> Result<Vec<TableInfo>, QueryError> {
    let set = session.fetch_trusted(TABLES_SQL.into()).await?;
    Ok(rows(&set)
        .map(|row| TableInfo {
            table: TableRef::new(row.text(0), row.text(1)),
            kind: table_kind(&row.text(2)),
            estimated_rows: row.optional(3).and_then(|v| v.parse().ok()),
            comment: row.optional(4),
        })
        .collect())
}

/// Schemas, tables and columns for the SQL editor's completion.
pub async fn completion_catalog(session: Session) -> Result<completion::Catalog, QueryError> {
    let search_path = session.fetch_trusted(SEARCH_PATH_SQL.into()).await?;
    let schemas = session.fetch_trusted(SCHEMAS_SQL.into()).await?;
    let columns = session.fetch_trusted(COLUMNS_SQL.into()).await?;
    let mut tables: Vec<completion::Table> = Vec::new();
    for row in rows(&columns) {
        let table = TableRef::new(row.text(0), row.text(1));
        if tables.last().is_none_or(|last| last.table != table) {
            tables.push(completion::Table {
                table,
                kind: table_kind(&row.text(2)),
                columns: Vec::new(),
            });
        }
        // A table without columns comes with a NULL one.
        if let (Some(name), Some(last)) = (row.optional(3), tables.last_mut()) {
            last.columns.push(completion::Column {
                name,
                data_type: row.text(4),
            });
        }
    }
    Ok(completion::Catalog {
        search_path: rows(&search_path).map(|row| row.text(0)).collect(),
        schemas: rows(&schemas).map(|row| row.text(0)).collect(),
        tables,
    })
}

/// `pg_class.relkind` of a table or view.
fn table_kind(relkind: &str) -> TableKind {
    match relkind {
        "p" => TableKind::PartitionedTable,
        "v" => TableKind::View,
        "m" => TableKind::MaterializedView,
        "f" => TableKind::ForeignTable,
        _ => TableKind::Table,
    }
}

/// A table's full definition.
pub async fn definition(session: Session, table: TableRef) -> Result<TableDefinition, QueryError> {
    let relation =
        quote_literal(&quote_qualified(&table.schema, &table.name)).map_err(|error| {
            QueryError::Database {
                code: "22021".into(),
                message: error.to_string(),
            }
        })?;
    let relation = format!("{relation}::regclass");

    let columns = session.fetch_trusted(columns_sql(&relation)).await?;
    let indexes = session.fetch_trusted(indexes_sql(&relation)).await?;
    let constraints = session.fetch_trusted(constraints_sql(&relation)).await?;
    let triggers = session.fetch_trusted(triggers_sql(&relation)).await?;

    Ok(TableDefinition {
        columns: rows(&columns)
            .map(|row| ColumnInfo {
                name: row.text(0),
                data_type: row.text(1),
                nullable: row.flag(2),
                default: row.optional(3),
                primary_key: row.flag(4),
                foreign_key: row.optional(5).map(|schema| ForeignKeyRef {
                    table: TableRef::new(schema, row.text(6)),
                    column: row.text(7),
                }),
                generated: row.flag(8),
                comment: row.optional(9),
            })
            .collect(),
        indexes: rows(&indexes)
            .map(|row| IndexInfo {
                name: row.text(0),
                definition: row.text(1),
                unique: row.flag(2),
                primary: row.flag(3),
                size_bytes: row.optional(4).and_then(|v| v.parse().ok()),
            })
            .collect(),
        constraints: rows(&constraints)
            .map(|row| ConstraintInfo {
                name: row.text(0),
                kind: match row.text(1).as_str() {
                    "p" => ConstraintKind::PrimaryKey,
                    "f" => ConstraintKind::ForeignKey,
                    "u" => ConstraintKind::Unique,
                    "c" => ConstraintKind::Check,
                    "x" => ConstraintKind::Exclusion,
                    _ => ConstraintKind::Other,
                },
                definition: row.text(2),
            })
            .collect(),
        triggers: rows(&triggers)
            .map(|row| TriggerInfo {
                name: row.text(0),
                definition: row.text(1),
                enabled: row.flag(2),
            })
            .collect(),
    })
}

fn columns_sql(relation: &str) -> String {
    format!(
        r"
select a.attname,
       pg_catalog.format_type(a.atttypid, a.atttypmod),
       not a.attnotnull,
       pg_catalog.pg_get_expr(d.adbin, d.adrelid),
       coalesce(a.attnum = any(pk.conkey), false),
       fk.ref_schema,
       fk.ref_table,
       fk.ref_column,
       a.attidentity <> '' or a.attgenerated <> '',
       pg_catalog.col_description(a.attrelid, a.attnum)
from pg_catalog.pg_attribute a
left join pg_catalog.pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum
left join pg_catalog.pg_constraint pk on pk.conrelid = a.attrelid and pk.contype = 'p'
left join lateral (
    select rn.nspname as ref_schema, rc.relname as ref_table, ra.attname as ref_column
    from pg_catalog.pg_constraint f
    join pg_catalog.pg_class rc on rc.oid = f.confrelid
    join pg_catalog.pg_namespace rn on rn.oid = rc.relnamespace
    join pg_catalog.pg_attribute ra
      on ra.attrelid = f.confrelid
     and ra.attnum = f.confkey[pg_catalog.array_position(f.conkey, a.attnum)]
    where f.conrelid = a.attrelid and f.contype = 'f' and a.attnum = any(f.conkey)
    limit 1
) fk on true
where a.attrelid = {relation} and a.attnum > 0 and not a.attisdropped
order by a.attnum"
    )
}

fn indexes_sql(relation: &str) -> String {
    format!(
        r"
select i.relname,
       pg_catalog.pg_get_indexdef(x.indexrelid),
       x.indisunique,
       x.indisprimary,
       pg_catalog.pg_relation_size(x.indexrelid)
from pg_catalog.pg_index x
join pg_catalog.pg_class i on i.oid = x.indexrelid
where x.indrelid = {relation}
order by x.indisprimary desc, i.relname"
    )
}

fn constraints_sql(relation: &str) -> String {
    format!(
        r"
select conname, contype::text, pg_catalog.pg_get_constraintdef(oid, true)
from pg_catalog.pg_constraint
where conrelid = {relation}
order by case contype when 'p' then 0 when 'f' then 1 when 'u' then 2 else 3 end, conname"
    )
}

fn triggers_sql(relation: &str) -> String {
    format!(
        r"
select tgname, pg_catalog.pg_get_triggerdef(oid, true), tgenabled <> 'D'
from pg_catalog.pg_trigger
where tgrelid = {relation} and not tgisinternal
order by tgname"
    )
}

pub(crate) struct Row<'a> {
    set: &'a ResultSet,
    index: usize,
}

impl Row<'_> {
    pub(crate) fn optional(&self, column: usize) -> Option<String> {
        self.set
            .cell(self.index, column)
            .flatten()
            .map(str::to_owned)
    }

    pub(crate) fn text(&self, column: usize) -> String {
        self.optional(column).unwrap_or_default()
    }

    pub(crate) fn flag(&self, column: usize) -> bool {
        self.set.cell(self.index, column).flatten() == Some("t")
    }
}

pub(crate) fn rows(set: &ResultSet) -> impl Iterator<Item = Row<'_>> {
    (0..set.rows()).map(move |index| Row { set, index })
}
