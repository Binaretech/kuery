//! MySQL catalog, from `information_schema`: each database the user can see
//! is a schema (MySQL's own are left out). Tables, views, columns, keys,
//! indexes, checks, triggers, functions and procedures.

use kuery_core::catalog::{
    CatalogColumn, CatalogOverview, ColumnInfo, ConstraintInfo, ConstraintKind, ForeignKeyRef,
    FunctionInfo, IndexInfo, RoutineKind, TableDefinition, TableInfo, TableKind, TableRef,
    TriggerInfo, TriggerRef,
};
use kuery_core::diagram::OnDelete;
use kuery_core::session::QueryError;

use kuery_core::adapter::fetch;

use crate::Mysql;

/// MySQL's and MariaDB's own databases.
const SYSTEM: &str = "('mysql', 'information_schema', 'performance_schema', 'sys')";

fn literal(value: &str) -> Result<String, QueryError> {
    kuery_core::sql::MYSQL
        .quote_literal(value)
        .map_err(|error| QueryError::Unsupported(error.to_string()))
}

fn ident(value: &str) -> String {
    kuery_core::sql::MYSQL.quote_ident(value)
}

fn text(cell: &Option<String>) -> String {
    cell.clone().unwrap_or_default()
}

/// A column as `information_schema.columns` describes it.
struct Column {
    schema: String,
    table: String,
    name: String,
    column_type: String,
    nullable: bool,
    default: Option<String>,
    primary_key: bool,
    generated: bool,
    comment: String,
}

fn columns(adapter: &mut Mysql, filter: &str) -> Result<Vec<Column>, QueryError> {
    let sql = format!(
        "select table_schema, table_name, column_name, column_type, is_nullable, \
         column_default, column_key, extra, column_comment \
         from information_schema.columns where {filter} \
         order by table_schema, table_name, ordinal_position"
    );
    let set = fetch(adapter, &sql)?;
    Ok(set
        .rows
        .iter()
        .map(|row| Column {
            schema: text(&row[0]),
            table: text(&row[1]),
            name: text(&row[2]),
            column_type: text(&row[3]),
            nullable: text(&row[4]) == "YES",
            default: row[5].clone(),
            primary_key: text(&row[6]) == "PRI",
            generated: text(&row[7]).contains("GENERATED"),
            comment: text(&row[8]),
        })
        .collect())
}

/// One column of a foreign key.
struct ForeignKey {
    table: String,
    name: String,
    column: String,
    referenced_schema: String,
    referenced_table: String,
    referenced_column: String,
    on_delete: String,
    on_update: String,
}

fn foreign_keys(adapter: &mut Mysql, filter: &str) -> Result<Vec<ForeignKey>, QueryError> {
    let sql = format!(
        "select k.table_name, k.constraint_name, k.column_name, k.referenced_table_schema, \
         k.referenced_table_name, k.referenced_column_name, r.delete_rule, r.update_rule \
         from information_schema.key_column_usage k \
         join information_schema.referential_constraints r \
           on r.constraint_schema = k.constraint_schema \
          and r.constraint_name = k.constraint_name \
          and r.table_name = k.table_name \
         where k.referenced_table_name is not null and {filter} \
         order by k.table_name, k.constraint_name, k.ordinal_position"
    );
    let set = fetch(adapter, &sql)?;
    Ok(set
        .rows
        .iter()
        .map(|row| ForeignKey {
            table: text(&row[0]),
            name: text(&row[1]),
            column: text(&row[2]),
            referenced_schema: text(&row[3]),
            referenced_table: text(&row[4]),
            referenced_column: text(&row[5]),
            on_delete: text(&row[6]),
            on_update: text(&row[7]),
        })
        .collect())
}

/// Foreign key columns grouped by constraint.
fn grouped(keys: &[ForeignKey]) -> Vec<Vec<&ForeignKey>> {
    let mut groups: Vec<Vec<&ForeignKey>> = Vec::new();
    for key in keys {
        match groups.last_mut() {
            Some(group) if group[0].name == key.name && group[0].table == key.table => {
                group.push(key)
            }
            _ => groups.push(vec![key]),
        }
    }
    groups
}

fn kind(table_type: &str) -> TableKind {
    if table_type == "VIEW" {
        TableKind::View
    } else {
        TableKind::Table
    }
}

/// Databases with their tables, views, functions, procedures and triggers.
pub(crate) fn overview(adapter: &mut Mysql) -> Result<CatalogOverview, QueryError> {
    let schemas = fetch(
        adapter,
        &format!(
            "select schema_name from information_schema.schemata \
             where schema_name not in {SYSTEM} order by 1"
        ),
    )?;
    let tables = fetch(
        adapter,
        &format!(
            "select table_schema, table_name, table_type, table_rows, table_comment \
             from information_schema.tables \
             where table_schema not in {SYSTEM} and table_type in ('BASE TABLE', 'VIEW') \
             order by 1, 2"
        ),
    )?;
    let routines = fetch(
        adapter,
        &format!(
            "select r.routine_schema, r.routine_name, r.routine_type, \
             coalesce(r.dtd_identifier, ''), \
             coalesce((select group_concat(concat_ws(' ', p.parameter_mode, p.parameter_name, p.dtd_identifier) \
                       order by p.ordinal_position separator ', ') \
                       from information_schema.parameters p \
                       where p.specific_schema = r.routine_schema \
                         and p.specific_name = r.specific_name and p.ordinal_position > 0), '') \
             from information_schema.routines r \
             where r.routine_schema not in {SYSTEM} order by 1, 2"
        ),
    )?;
    let triggers = fetch(
        adapter,
        &format!(
            "select trigger_schema, event_object_table, trigger_name \
             from information_schema.triggers where trigger_schema not in {SYSTEM} \
             order by 1, 2, 3"
        ),
    )?;
    Ok(CatalogOverview {
        schemas: schemas.rows.iter().map(|row| text(&row[0])).collect(),
        tables: tables
            .rows
            .iter()
            .map(|row| TableInfo {
                table: TableRef::new(text(&row[0]), text(&row[1])),
                kind: kind(&text(&row[2])),
                estimated_rows: row[3].as_deref().and_then(|n| n.parse().ok()),
                comment: row[4].clone().filter(|c| !c.is_empty()),
            })
            .collect(),
        functions: routines
            .rows
            .iter()
            .map(|row| FunctionInfo {
                schema: text(&row[0]),
                name: text(&row[1]),
                kind: if text(&row[2]) == "PROCEDURE" {
                    RoutineKind::Procedure
                } else {
                    RoutineKind::Function
                },
                result: text(&row[3]),
                arguments: text(&row[4]),
                language: "SQL".into(),
                oid: 0,
            })
            .collect(),
        enums: Vec::new(),
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

/// `CREATE FUNCTION …` or `CREATE PROCEDURE …`, as the server shows it.
pub(crate) fn function_definition(
    adapter: &mut Mysql,
    function: &FunctionInfo,
) -> Result<String, QueryError> {
    let what = match function.kind {
        RoutineKind::Procedure => "procedure",
        _ => "function",
    };
    let set = fetch(
        adapter,
        &format!(
            "show create {what} {}.{}",
            ident(&function.schema),
            ident(&function.name)
        ),
    )?;
    match set.get(0, 2) {
        Some(definition) => Ok(format!("{definition};\n")),
        None => Ok(format!(
            "-- The server shows the body of {}.{} only to its owner or with SELECT on mysql.proc.\n",
            function.schema, function.name
        )),
    }
}

pub(crate) fn definition(
    adapter: &mut Mysql,
    table: &TableRef,
) -> Result<TableDefinition, QueryError> {
    let filter = format!(
        "table_schema = {} and table_name = {}",
        literal(&table.schema)?,
        literal(&table.name)?
    );
    let key_filter = format!(
        "k.table_schema = {} and k.table_name = {}",
        literal(&table.schema)?,
        literal(&table.name)?
    );
    let keys = foreign_keys(adapter, &key_filter)?;
    let groups = grouped(&keys);
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
            data_type: column.column_type,
            nullable: column.nullable,
            default: column.default,
            primary_key: column.primary_key,
            foreign_key,
            generated: column.generated,
            comment: Some(column.comment).filter(|c| !c.is_empty()),
        });
    }

    let indexes = fetch(
        adapter,
        &format!(
            "select index_name, non_unique, \
             group_concat(coalesce(column_name, '(expression)') order by seq_in_index separator ', '), \
             index_type \
             from information_schema.statistics where {filter} \
             group by index_name, non_unique, index_type \
             order by index_name = 'PRIMARY' desc, index_name"
        ),
    )?;
    for row in indexes.rows {
        let name = text(&row[0]);
        let unique = text(&row[1]) == "0";
        let primary = name == "PRIMARY";
        let columns = text(&row[2]);
        let index_type = text(&row[3]);
        let text_definition = if primary {
            format!("PRIMARY KEY ({columns})")
        } else {
            format!(
                "{}INDEX {} ({columns}) USING {index_type}",
                if unique { "UNIQUE " } else { "" },
                ident(&name)
            )
        };
        match (primary, unique) {
            (true, _) => definition.constraints.push(ConstraintInfo {
                name: name.clone(),
                kind: ConstraintKind::PrimaryKey,
                definition: format!("PRIMARY KEY ({columns})"),
            }),
            (false, true) => definition.constraints.push(ConstraintInfo {
                name: name.clone(),
                kind: ConstraintKind::Unique,
                definition: format!("UNIQUE ({columns})"),
            }),
            _ => {}
        }
        definition.indexes.push(IndexInfo {
            name,
            definition: text_definition,
            unique,
            primary,
            size_bytes: None,
        });
    }

    for group in &groups {
        let first = group[0];
        let mut text_definition = format!(
            "FOREIGN KEY ({}) REFERENCES {} ({})",
            group
                .iter()
                .map(|k| ident(&k.column))
                .collect::<Vec<_>>()
                .join(", "),
            kuery_core::sql::MYSQL
                .quote_qualified(&first.referenced_schema, &first.referenced_table),
            group
                .iter()
                .map(|k| ident(&k.referenced_column))
                .collect::<Vec<_>>()
                .join(", ")
        );
        for (event, rule) in [("DELETE", &first.on_delete), ("UPDATE", &first.on_update)] {
            if rule != "NO ACTION" && rule != "RESTRICT" {
                text_definition.push_str(&format!(" ON {event} {rule}"));
            }
        }
        definition.constraints.push(ConstraintInfo {
            name: first.name.clone(),
            kind: ConstraintKind::ForeignKey,
            definition: text_definition,
        });
    }

    // CHECK constraints (MySQL 8.0.16+, MariaDB); older servers have none.
    let checks = fetch(
        adapter,
        &format!(
            "select c.constraint_name, c.check_clause \
             from information_schema.check_constraints c \
             join information_schema.table_constraints t \
               on t.constraint_schema = c.constraint_schema \
              and t.constraint_name = c.constraint_name \
             where t.constraint_type = 'CHECK' and t.table_schema = {} and t.table_name = {} \
             order by 1",
            literal(&table.schema)?,
            literal(&table.name)?
        ),
    );
    if let Ok(checks) = checks {
        for row in checks.rows {
            definition.constraints.push(ConstraintInfo {
                name: text(&row[0]),
                kind: ConstraintKind::Check,
                definition: format!("CHECK ({})", text(&row[1])),
            });
        }
    }

    let triggers = fetch(
        adapter,
        &format!(
            "select trigger_name, action_timing, event_manipulation, action_statement \
             from information_schema.triggers \
             where event_object_schema = {} and event_object_table = {} order by 1",
            literal(&table.schema)?,
            literal(&table.name)?
        ),
    )?;
    definition.triggers = triggers
        .rows
        .iter()
        .map(|row| TriggerInfo {
            name: text(&row[0]),
            definition: format!(
                "CREATE TRIGGER {} {} {} ON {} FOR EACH ROW {}",
                ident(&text(&row[0])),
                text(&row[1]),
                text(&row[2]),
                ident(&table.name),
                text(&row[3])
            ),
            enabled: true,
        })
        .collect();
    Ok(definition)
}

/// Every column of the tables and views in `schema`, or in every schema.
pub(crate) fn catalog_columns(
    adapter: &mut Mysql,
    schema: Option<&str>,
) -> Result<Vec<CatalogColumn>, QueryError> {
    let filter = match schema {
        Some(schema) => format!("table_schema = {}", literal(schema)?),
        None => format!("table_schema not in {SYSTEM}"),
    };
    Ok(columns(adapter, &filter)?
        .into_iter()
        .map(|c| CatalogColumn {
            table: TableRef::new(c.schema, c.table),
            name: c.name,
            data_type: c.column_type,
            nullable: c.nullable,
            primary_key: c.primary_key,
        })
        .collect())
}

/// The foreign keys of the tables in `schema`.
pub(crate) fn catalog_foreign_keys(
    adapter: &mut Mysql,
    schema: &str,
) -> Result<Vec<kuery_core::catalog::ForeignKey>, QueryError> {
    let keys = foreign_keys(adapter, &format!("k.table_schema = {}", literal(schema)?))?;
    Ok(grouped(&keys)
        .into_iter()
        .map(|group| {
            let first = group[0];
            kuery_core::catalog::ForeignKey {
                name: first.name.clone(),
                from: TableRef::new(schema, first.table.clone()),
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

/// The database queries start in.
pub(crate) fn search_path(adapter: &mut Mysql) -> Result<Vec<String>, QueryError> {
    Ok(fetch(adapter, "select database()")?
        .get(0, 0)
        .map(|database| vec![database.to_string()])
        .unwrap_or_default())
}
