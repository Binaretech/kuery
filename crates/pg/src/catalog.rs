//! PostgreSQL catalog: tables, columns, keys, indexes, constraints and
//! triggers, read from `pg_catalog`.

use kuery_core::adapter::{Rows, fetch};
use kuery_core::catalog::{
    CatalogColumn, CatalogOverview, ColumnInfo, ConstraintInfo, ConstraintKind, EnumInfo,
    ForeignKey, ForeignKeyRef, FunctionInfo, IndexInfo, RoutineKind, TableDefinition, TableInfo,
    TableKind, TableRef, TriggerInfo, TriggerRef,
};
use kuery_core::diagram::OnDelete;
use kuery_core::sql::{POSTGRES, quote_literal};

use crate::{Postgres, QueryError};

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

/// Every column of every table and view in the user schemas (or in one,
/// `{schema}` being a filter on `n.nspname`), with whether it is in the
/// primary key.
const COLUMNS_SQL: &str = r"
select n.nspname,
       c.relname,
       a.attname,
       pg_catalog.format_type(a.atttypid, a.atttypmod),
       not a.attnotnull,
       exists (select 1 from pg_catalog.pg_constraint p
               where p.conrelid = c.oid and p.contype = 'p' and a.attnum = any (p.conkey))
from pg_catalog.pg_class c
join pg_catalog.pg_namespace n on n.oid = c.relnamespace
join pg_catalog.pg_attribute a on a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped
where c.relkind in ('r', 'p', 'v', 'm', 'f')
  and not c.relispartition
  and n.nspname not in ('pg_catalog', 'information_schema')
  and n.nspname !~ '^pg_(toast|temp)'
  and {schema}
order by 1, 2, a.attnum";

/// A schema's foreign keys (not the copies partitions inherit): columns in
/// order, joined with the unit separator.
const KEYS_SQL: &str = r"
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
       con.confdeltype::text
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
pub(crate) fn overview(adapter: &mut Postgres) -> Result<CatalogOverview, QueryError> {
    let schemas = fetch(adapter, SCHEMAS_SQL)?;
    let tables = fetch(adapter, TABLES_SQL)?;
    let tables = rows(&tables)
        .map(|row| TableInfo {
            table: TableRef::new(row.text(0), row.text(1)),
            kind: table_kind(&row.text(2)),
            estimated_rows: row.optional(3).and_then(|v| v.parse().ok()),
            comment: row.optional(4),
        })
        .collect();
    let functions = fetch(adapter, FUNCTIONS_SQL)?;
    let enums = fetch(adapter, ENUMS_SQL)?;
    let triggers = fetch(adapter, TRIGGERS_SQL)?;
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

/// `CREATE OR REPLACE FUNCTION …` for a function or procedure. Aggregates have
/// no such definition, so they get a short description instead.
pub(crate) fn function_definition(
    adapter: &mut Postgres,
    function: &FunctionInfo,
) -> Result<String, QueryError> {
    if function.kind == RoutineKind::Aggregate {
        return Ok(format!(
            "-- {}.{}({}) is an aggregate; PostgreSQL has no CREATE statement to show for it.\n",
            function.schema, function.name, function.arguments
        ));
    }
    let set = fetch(
        adapter,
        &format!("select pg_catalog.pg_get_functiondef({})", function.oid),
    )?;
    Ok(set.text(0, 0))
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
pub(crate) fn definition(
    adapter: &mut Postgres,
    table: &TableRef,
) -> Result<TableDefinition, QueryError> {
    let relation =
        quote_literal(&POSTGRES.quote_qualified(&table.schema, &table.name)).map_err(|error| {
            QueryError::Database {
                code: "22021".into(),
                message: error.to_string(),
            }
        })?;
    let relation = format!("{relation}::regclass");

    let columns = fetch(adapter, &columns_sql(&relation))?;
    let indexes = fetch(adapter, &indexes_sql(&relation))?;
    let constraints = fetch(adapter, &constraints_sql(&relation))?;
    let triggers = fetch(adapter, &triggers_sql(&relation))?;

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
    set: &'a Rows,
    index: usize,
}

impl Row<'_> {
    pub(crate) fn optional(&self, column: usize) -> Option<String> {
        self.set.get(self.index, column).map(str::to_owned)
    }

    pub(crate) fn text(&self, column: usize) -> String {
        self.optional(column).unwrap_or_default()
    }

    pub(crate) fn flag(&self, column: usize) -> bool {
        self.set.get(self.index, column) == Some("t")
    }
}

pub(crate) fn rows(set: &Rows) -> impl Iterator<Item = Row<'_>> {
    (0..set.rows.len()).map(move |index| Row { set, index })
}

fn literal(value: &str) -> Result<String, QueryError> {
    quote_literal(value).map_err(|error| QueryError::Database {
        code: "22021".into(),
        message: error.to_string(),
    })
}

/// Every column of the tables and views in `schema`, or in every user schema.
pub(crate) fn catalog_columns(
    adapter: &mut Postgres,
    schema: Option<&str>,
) -> Result<Vec<CatalogColumn>, QueryError> {
    let filter = match schema {
        Some(schema) => format!("n.nspname = {}", literal(schema)?),
        None => "true".into(),
    };
    let set = fetch(adapter, &COLUMNS_SQL.replace("{schema}", &filter))?;
    Ok(rows(&set)
        .map(|row| CatalogColumn {
            table: TableRef::new(row.text(0), row.text(1)),
            name: row.text(2),
            data_type: row.text(3),
            nullable: row.flag(4),
            primary_key: row.flag(5),
        })
        .collect())
}

/// A schema's foreign keys (not the copies partitions inherit).
pub(crate) fn catalog_foreign_keys(
    adapter: &mut Postgres,
    schema: &str,
) -> Result<Vec<ForeignKey>, QueryError> {
    let set = fetch(adapter, &KEYS_SQL.replace("{schema}", &literal(schema)?))?;
    let split = |text: String| -> Vec<String> {
        text.split('\u{1f}')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    };
    Ok(rows(&set)
        .map(|row| ForeignKey {
            name: row.text(0),
            from: TableRef::new(schema, row.text(1)),
            to: TableRef::new(row.text(2), row.text(3)),
            from_columns: split(row.text(4)),
            to_columns: split(row.text(5)),
            on_delete: OnDelete::from_code(&row.text(6)),
        })
        .collect())
}

/// The schemas in `search_path` that exist, in order.
pub(crate) fn search_path(adapter: &mut Postgres) -> Result<Vec<String>, QueryError> {
    let set = fetch(adapter, SEARCH_PATH_SQL)?;
    Ok(rows(&set).map(|row| row.text(0)).collect())
}
