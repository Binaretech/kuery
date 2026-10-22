//! SQL Server catalog, from the `sys` views of the current database: each
//! schema with objects (or the user's default one) is a schema; tables,
//! views, columns, keys, indexes, checks, DML triggers, functions,
//! procedures and user-defined types.

use kuery_core::adapter::{Rows, fetch};
use kuery_core::catalog::{
    CatalogColumn, CatalogOverview, ColumnInfo, ConstraintInfo, ConstraintKind, EnumInfo,
    ForeignKeyRef, FunctionInfo, IndexInfo, RoutineKind, TableDefinition, TableInfo, TableKind,
    TableRef, TriggerInfo, TriggerRef,
};
use kuery_core::diagram::OnDelete;
use kuery_core::session::QueryError;
use kuery_core::sql::MSSQL;

use crate::Mssql;

/// A schema whose name is not one of the server's own (`s` is `sys.schemas`).
const USER_SCHEMA: &str =
    "s.name not in ('sys', 'INFORMATION_SCHEMA', 'guest') and s.name not like 'db[_]%'";

fn literal(value: &str) -> Result<String, QueryError> {
    MSSQL
        .quote_literal(value)
        .map_err(|error| QueryError::Unsupported(error.to_string()))
}

fn ident(value: &str) -> String {
    MSSQL.quote_ident(value)
}

/// A column's type as written in DDL (`nvarchar(40)`, `decimal(10,2)`,
/// `nvarchar(max)`; a user-defined type by its qualified name). `c` is a row of
/// `sys.columns` or `sys.parameters`, `t` its `sys.types`.
fn type_text(c: &str, t: &str) -> String {
    format!(
        "case \
         when {t}.is_user_defined = 1 then schema_name({t}.schema_id) + '.' + {t}.name \
         when {t}.name in ('nvarchar', 'nchar') then {t}.name + '(' + \
           case when {c}.max_length = -1 then 'max' else cast({c}.max_length / 2 as varchar(10)) end + ')' \
         when {t}.name in ('varchar', 'char', 'varbinary', 'binary') then {t}.name + '(' + \
           case when {c}.max_length = -1 then 'max' else cast({c}.max_length as varchar(10)) end + ')' \
         when {t}.name in ('decimal', 'numeric') then {t}.name + '(' + \
           cast({c}.precision as varchar(10)) + ',' + cast({c}.scale as varchar(10)) + ')' \
         when {t}.name in ('datetime2', 'datetimeoffset', 'time') then {t}.name + '(' + \
           cast({c}.scale as varchar(10)) + ')' \
         else {t}.name end"
    )
}

fn text(cell: &Option<String>) -> String {
    cell.clone().unwrap_or_default()
}

fn flag(cell: &Option<String>) -> bool {
    cell.as_deref().is_some_and(|value| value != "0")
}

struct Column {
    schema: String,
    table: String,
    name: String,
    data_type: String,
    nullable: bool,
    default: Option<String>,
    primary_key: bool,
    generated: bool,
    comment: String,
}

fn columns(adapter: &mut Mssql, filter: &str) -> Result<Vec<Column>, QueryError> {
    let sql = format!(
        "select s.name, o.name, c.name, {}, \
         cast(c.is_nullable as int), dc.definition, \
         cast(case when exists (select 1 from sys.indexes i \
           join sys.index_columns ic on ic.object_id = i.object_id and ic.index_id = i.index_id \
           where i.is_primary_key = 1 and ic.object_id = c.object_id and ic.column_id = c.column_id) \
           then 1 else 0 end as int), \
         cast(case when c.is_computed = 1 or c.is_identity = 1 or t.name = 'timestamp' \
           then 1 else 0 end as int), \
         cast(ep.value as nvarchar(max)) \
         from sys.columns c \
         join sys.objects o on o.object_id = c.object_id and o.type in ('U', 'V') \
              and o.is_ms_shipped = 0 \
         join sys.schemas s on s.schema_id = o.schema_id \
         join sys.types t on t.user_type_id = c.user_type_id \
         left join sys.default_constraints dc on dc.object_id = c.default_object_id \
         left join sys.extended_properties ep on ep.class = 1 and ep.name = 'MS_Description' \
              and ep.major_id = c.object_id and ep.minor_id = c.column_id \
         where {USER_SCHEMA} and {filter} \
         order by s.name, o.name, c.column_id",
        type_text("c", "t")
    );
    let set = fetch(adapter, &sql)?;
    Ok(set
        .rows
        .iter()
        .map(|row| Column {
            schema: text(&row[0]),
            table: text(&row[1]),
            name: text(&row[2]),
            data_type: text(&row[3]),
            nullable: flag(&row[4]),
            default: row[5].clone(),
            primary_key: flag(&row[6]),
            generated: flag(&row[7]),
            comment: text(&row[8]),
        })
        .collect())
}

struct ForeignKey {
    schema: String,
    table: String,
    name: String,
    column: String,
    referenced_schema: String,
    referenced_table: String,
    referenced_column: String,
    on_delete: String,
    on_update: String,
}

/// `filter` is over `ps` (the schema of the table the key is on) and `po`
/// (the table).
fn foreign_keys(adapter: &mut Mssql, filter: &str) -> Result<Vec<ForeignKey>, QueryError> {
    let sql = format!(
        "select ps.name, po.name, f.name, pc.name, rs.name, ro.name, rc.name, \
         f.delete_referential_action_desc, f.update_referential_action_desc \
         from sys.foreign_keys f \
         join sys.foreign_key_columns fc on fc.constraint_object_id = f.object_id \
         join sys.objects po on po.object_id = fc.parent_object_id \
         join sys.schemas ps on ps.schema_id = po.schema_id \
         join sys.columns pc on pc.object_id = fc.parent_object_id and pc.column_id = fc.parent_column_id \
         join sys.objects ro on ro.object_id = fc.referenced_object_id \
         join sys.schemas rs on rs.schema_id = ro.schema_id \
         join sys.columns rc on rc.object_id = fc.referenced_object_id and rc.column_id = fc.referenced_column_id \
         where {filter} \
         order by ps.name, po.name, f.name, fc.constraint_column_id"
    );
    let set = fetch(adapter, &sql)?;
    Ok(set
        .rows
        .iter()
        .map(|row| ForeignKey {
            schema: text(&row[0]),
            table: text(&row[1]),
            name: text(&row[2]),
            column: text(&row[3]),
            referenced_schema: text(&row[4]),
            referenced_table: text(&row[5]),
            referenced_column: text(&row[6]),
            on_delete: rule(&text(&row[7])),
            on_update: rule(&text(&row[8])),
        })
        .collect())
}

/// `SET_NULL` → `SET NULL`.
fn rule(desc: &str) -> String {
    desc.replace('_', " ")
}

/// Foreign key columns grouped by constraint.
fn grouped(keys: &[ForeignKey]) -> Vec<Vec<&ForeignKey>> {
    let mut groups: Vec<Vec<&ForeignKey>> = Vec::new();
    for key in keys {
        match groups.last_mut() {
            Some(group)
                if group[0].name == key.name
                    && group[0].table == key.table
                    && group[0].schema == key.schema =>
            {
                group.push(key)
            }
            _ => groups.push(vec![key]),
        }
    }
    groups
}

fn quoted_list<'a>(names: impl Iterator<Item = &'a String>) -> String {
    names.map(|name| ident(name)).collect::<Vec<_>>().join(", ")
}

/// Schemas with their tables, views, functions, procedures, triggers and
/// user-defined types.
pub(crate) fn overview(adapter: &mut Mssql) -> Result<CatalogOverview, QueryError> {
    let schemas = fetch(
        adapter,
        &format!(
            "select s.name from sys.schemas s where {USER_SCHEMA} and ( \
               s.name = schema_name() \
               or exists (select 1 from sys.objects o where o.schema_id = s.schema_id \
                          and o.is_ms_shipped = 0 and o.type in ('U', 'V', 'P', 'FN', 'IF', 'TF')) \
               or exists (select 1 from sys.types t where t.schema_id = s.schema_id \
                          and t.is_user_defined = 1)) \
             order by 1"
        ),
    )?;
    let tables = fetch(
        adapter,
        &format!(
            "select s.name, o.name, o.type, \
             cast((select sum(p.rows) from sys.partitions p \
                   where p.object_id = o.object_id and p.index_id in (0, 1)) as bigint), \
             cast(ep.value as nvarchar(max)) \
             from sys.objects o \
             join sys.schemas s on s.schema_id = o.schema_id \
             left join sys.extended_properties ep on ep.class = 1 and ep.name = 'MS_Description' \
                  and ep.major_id = o.object_id and ep.minor_id = 0 \
             where {USER_SCHEMA} and o.type in ('U', 'V') and o.is_ms_shipped = 0 \
             order by 1, 2"
        ),
    )?;
    let routines = fetch(
        adapter,
        &format!(
            "select s.name, o.name, o.type, \
             case when o.type in ('IF', 'TF') then 'table' else coalesce(( \
               select {} from sys.parameters p join sys.types t on t.user_type_id = p.user_type_id \
               where p.object_id = o.object_id and p.parameter_id = 0), '') end, \
             coalesce(stuff(( \
               select ', ' + p.name + ' ' + {} \
               from sys.parameters p join sys.types t on t.user_type_id = p.user_type_id \
               where p.object_id = o.object_id and p.parameter_id > 0 \
               order by p.parameter_id for xml path(''), type).value('.', 'nvarchar(max)'), 1, 2, ''), '') \
             from sys.objects o join sys.schemas s on s.schema_id = o.schema_id \
             where {USER_SCHEMA} and o.type in ('P', 'FN', 'IF', 'TF') and o.is_ms_shipped = 0 \
             order by 1, 2",
            type_text("p", "t"),
            type_text("p", "t"),
        ),
    )?;
    let triggers = fetch(
        adapter,
        &format!(
            "select s.name, o.name, tr.name \
             from sys.triggers tr \
             join sys.objects o on o.object_id = tr.parent_id \
             join sys.schemas s on s.schema_id = o.schema_id \
             where tr.parent_class = 1 and tr.is_ms_shipped = 0 and {USER_SCHEMA} \
             order by 1, 2, 3"
        ),
    )?;
    let types = fetch(
        adapter,
        &format!(
            "select s.name, t.name, \
             {} + case when t.is_nullable = 1 then '' else ' not null' end \
             from sys.types t join sys.schemas s on s.schema_id = t.schema_id \
             join sys.types b on b.user_type_id = t.system_type_id and b.is_user_defined = 0 \
             where t.is_user_defined = 1 and {USER_SCHEMA} order by 1, 2",
            // The type the alias is built on, sized like a column of it.
            type_text("t", "b"),
        ),
    )?;
    Ok(CatalogOverview {
        schemas: schemas.rows.iter().map(|row| text(&row[0])).collect(),
        tables: tables
            .rows
            .iter()
            .map(|row| {
                let view = text(&row[2]).trim() == "V";
                TableInfo {
                    table: TableRef::new(text(&row[0]), text(&row[1])),
                    kind: if view {
                        TableKind::View
                    } else {
                        TableKind::Table
                    },
                    estimated_rows: row[3]
                        .as_deref()
                        .filter(|_| !view)
                        .and_then(|n| n.parse().ok()),
                    comment: row[4].clone().filter(|c| !c.is_empty()),
                }
            })
            .collect(),
        functions: routines
            .rows
            .iter()
            .map(|row| FunctionInfo {
                schema: text(&row[0]),
                name: text(&row[1]),
                kind: if text(&row[2]).trim() == "P" {
                    RoutineKind::Procedure
                } else {
                    RoutineKind::Function
                },
                result: text(&row[3]),
                arguments: text(&row[4]),
                language: "T-SQL".into(),
                oid: 0,
            })
            .collect(),
        enums: types
            .rows
            .iter()
            .map(|row| EnumInfo {
                schema: text(&row[0]),
                name: text(&row[1]),
                values: vec![text(&row[2])],
            })
            .collect(),
        triggers: triggers
            .rows
            .iter()
            .map(|row| TriggerRef {
                table: TableRef::new(text(&row[0]), text(&row[1])),
                name: text(&row[2]),
            })
            .collect(),
    })
}

/// The source of a function or procedure, as the server keeps it.
pub(crate) fn function_definition(
    adapter: &mut Mssql,
    function: &FunctionInfo,
) -> Result<String, QueryError> {
    let name = MSSQL.quote_qualified(&function.schema, &function.name);
    let set = fetch(
        adapter,
        &format!("select object_definition(object_id({}))", literal(&name)?),
    )?;
    Ok(match set.get(0, 0) {
        Some(source) => format!("{}\n", source.trim_end()),
        None => format!(
            "-- The server keeps no source to show for {name}: it is encrypted, or this login cannot view definitions.\n"
        ),
    })
}

/// One column of an index.
struct IndexColumn {
    name: String,
    unique: bool,
    primary: bool,
    unique_constraint: bool,
    kind: String,
    filter: Option<String>,
    column: String,
    descending: bool,
    included: bool,
}

fn index_columns(adapter: &mut Mssql, table: &TableRef) -> Result<Vec<IndexColumn>, QueryError> {
    let set = fetch(
        adapter,
        &format!(
            "select i.name, cast(i.is_unique as int), cast(i.is_primary_key as int), \
             cast(i.is_unique_constraint as int), i.type_desc, i.filter_definition, \
             c.name, cast(ic.is_descending_key as int), cast(ic.is_included_column as int) \
             from sys.indexes i \
             join sys.index_columns ic on ic.object_id = i.object_id and ic.index_id = i.index_id \
             join sys.columns c on c.object_id = ic.object_id and c.column_id = ic.column_id \
             join sys.objects o on o.object_id = i.object_id \
             join sys.schemas s on s.schema_id = o.schema_id \
             where s.name = {} and o.name = {} and i.type > 0 and i.is_hypothetical = 0 \
             order by i.is_primary_key desc, i.name, ic.is_included_column, ic.key_ordinal, \
                      ic.index_column_id",
            literal(&table.schema)?,
            literal(&table.name)?
        ),
    )?;
    Ok(set
        .rows
        .iter()
        .map(|row| IndexColumn {
            unique: flag(&row[1]),
            primary: flag(&row[2]),
            unique_constraint: flag(&row[3]),
            kind: text(&row[4]),
            filter: row[5].clone(),
            column: text(&row[6]),
            descending: flag(&row[7]),
            included: flag(&row[8]),
            name: text(&row[0]),
        })
        .collect())
}

/// Indexes (and the key constraints behind them), from their columns.
fn add_indexes(definition: &mut TableDefinition, table: &TableRef, rows: &[IndexColumn]) {
    let mut start = 0;
    while start < rows.len() {
        let first = &rows[start];
        let end = rows[start..]
            .iter()
            .position(|row| row.name != first.name)
            .map_or(rows.len(), |n| start + n);
        let group = &rows[start..end];
        start = end;
        let keys: Vec<String> = group
            .iter()
            .filter(|c| !c.included)
            .map(|c| {
                if c.descending {
                    format!("{} DESC", ident(&c.column))
                } else {
                    ident(&c.column)
                }
            })
            .collect();
        let keys = keys.join(", ");
        let included = quoted_list(group.iter().filter(|c| c.included).map(|c| &c.column));
        let mut text_definition = format!(
            "CREATE {}{} INDEX {} ON {} ({keys})",
            if first.unique { "UNIQUE " } else { "" },
            first.kind,
            ident(&first.name),
            MSSQL.quote_qualified(&table.schema, &table.name),
        );
        if !included.is_empty() {
            text_definition.push_str(&format!(" INCLUDE ({included})"));
        }
        if let Some(filter) = &first.filter {
            text_definition.push_str(&format!(" WHERE {filter}"));
        }
        if first.primary {
            definition.constraints.push(ConstraintInfo {
                name: first.name.clone(),
                kind: ConstraintKind::PrimaryKey,
                definition: format!("PRIMARY KEY ({keys})"),
            });
        } else if first.unique_constraint {
            definition.constraints.push(ConstraintInfo {
                name: first.name.clone(),
                kind: ConstraintKind::Unique,
                definition: format!("UNIQUE ({keys})"),
            });
        }
        definition.indexes.push(IndexInfo {
            name: first.name.clone(),
            definition: text_definition,
            unique: first.unique,
            primary: first.primary,
            size_bytes: None,
        });
    }
}

pub(crate) fn definition(
    adapter: &mut Mssql,
    table: &TableRef,
) -> Result<TableDefinition, QueryError> {
    let filter = format!(
        "s.name = {} and o.name = {}",
        literal(&table.schema)?,
        literal(&table.name)?
    );
    let keys = foreign_keys(
        adapter,
        &format!(
            "ps.name = {} and po.name = {}",
            literal(&table.schema)?,
            literal(&table.name)?
        ),
    )?;
    let mut definition = TableDefinition::default();
    for column in columns(adapter, &filter)? {
        let foreign_key = keys
            .iter()
            .find(|k| k.column == column.name)
            .map(|k| ForeignKeyRef {
                table: TableRef::new(k.referenced_schema.clone(), k.referenced_table.clone()),
                column: k.referenced_column.clone(),
            });
        definition.columns.push(ColumnInfo {
            name: column.name,
            data_type: column.data_type,
            nullable: column.nullable,
            default: column.default,
            primary_key: column.primary_key,
            foreign_key,
            generated: column.generated,
            comment: Some(column.comment).filter(|c| !c.is_empty()),
        });
    }

    add_indexes(&mut definition, table, &index_columns(adapter, table)?);

    for group in grouped(&keys) {
        let first = group[0];
        let mut text_definition = format!(
            "FOREIGN KEY ({}) REFERENCES {} ({})",
            quoted_list(group.iter().map(|k| &k.column)),
            MSSQL.quote_qualified(&first.referenced_schema, &first.referenced_table),
            quoted_list(group.iter().map(|k| &k.referenced_column)),
        );
        for (event, rule) in [("DELETE", &first.on_delete), ("UPDATE", &first.on_update)] {
            if rule != "NO ACTION" {
                text_definition.push_str(&format!(" ON {event} {rule}"));
            }
        }
        definition.constraints.push(ConstraintInfo {
            name: first.name.clone(),
            kind: ConstraintKind::ForeignKey,
            definition: text_definition,
        });
    }

    let checks = fetch(
        adapter,
        &format!(
            "select k.name, k.definition from sys.check_constraints k \
             join sys.objects o on o.object_id = k.parent_object_id \
             join sys.schemas s on s.schema_id = o.schema_id \
             where {filter} order by 1"
        ),
    )?;
    for row in &checks.rows {
        definition.constraints.push(ConstraintInfo {
            name: text(&row[0]),
            kind: ConstraintKind::Check,
            definition: format!("CHECK {}", text(&row[1])),
        });
    }

    let triggers = fetch(
        adapter,
        &format!(
            "select tr.name, object_definition(tr.object_id), cast(tr.is_disabled as int) \
             from sys.triggers tr \
             join sys.objects o on o.object_id = tr.parent_id \
             join sys.schemas s on s.schema_id = o.schema_id \
             where tr.parent_class = 1 and {filter} order by 1"
        ),
    )?;
    definition.triggers = triggers_of(&triggers);
    Ok(definition)
}

fn triggers_of(rows: &Rows) -> Vec<TriggerInfo> {
    rows.rows
        .iter()
        .map(|row| TriggerInfo {
            name: text(&row[0]),
            definition: match &row[1] {
                Some(source) => source.trim_end().to_string(),
                None => format!(
                    "-- The server keeps no source to show for {}.",
                    text(&row[0])
                ),
            },
            enabled: !flag(&row[2]),
        })
        .collect()
}

/// Every column of the tables and views in `schema`, or in every schema.
pub(crate) fn catalog_columns(
    adapter: &mut Mssql,
    schema: Option<&str>,
) -> Result<Vec<CatalogColumn>, QueryError> {
    let filter = match schema {
        Some(schema) => format!("s.name = {}", literal(schema)?),
        None => "1 = 1".to_string(),
    };
    Ok(columns(adapter, &filter)?
        .into_iter()
        .map(|c| CatalogColumn {
            table: TableRef::new(c.schema, c.table),
            name: c.name,
            data_type: c.data_type,
            nullable: c.nullable,
            primary_key: c.primary_key,
        })
        .collect())
}

/// The foreign keys of the tables in `schema`.
pub(crate) fn catalog_foreign_keys(
    adapter: &mut Mssql,
    schema: &str,
) -> Result<Vec<kuery_core::catalog::ForeignKey>, QueryError> {
    let keys = foreign_keys(adapter, &format!("ps.name = {}", literal(schema)?))?;
    Ok(grouped(&keys)
        .into_iter()
        .map(|group| {
            let first = group[0];
            kuery_core::catalog::ForeignKey {
                name: first.name.clone(),
                from: TableRef::new(first.schema.clone(), first.table.clone()),
                from_columns: group.iter().map(|k| k.column.clone()).collect(),
                to: TableRef::new(
                    first.referenced_schema.clone(),
                    first.referenced_table.clone(),
                ),
                to_columns: group.iter().map(|k| k.referenced_column.clone()).collect(),
                on_delete: OnDelete::from_rule(&first.on_delete),
            }
        })
        .collect())
}

/// The user's default schema, then `dbo`.
pub(crate) fn search_path(adapter: &mut Mssql) -> Result<Vec<String>, QueryError> {
    let mut path: Vec<String> = fetch(adapter, "select schema_name()")?
        .get(0, 0)
        .map(|schema| vec![schema.to_string()])
        .unwrap_or_default();
    if !path.iter().any(|schema| schema == "dbo") {
        path.push("dbo".into());
    }
    Ok(path)
}
