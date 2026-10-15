//! Catalog model: tables, columns, keys, indexes, constraints, RLS policies
//! and triggers. Each engine's driver fills it in.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TableRef {
    pub schema: String,
    pub name: String,
}

impl TableRef {
    pub fn new(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            schema: schema.into(),
            name: name.into(),
        }
    }
}

impl fmt::Display for TableRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.schema, self.name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    Table,
    PartitionedTable,
    View,
    MaterializedView,
    ForeignTable,
}

impl TableKind {
    pub fn label(self) -> &'static str {
        match self {
            TableKind::Table => "Table",
            TableKind::PartitionedTable => "Partitioned table",
            TableKind::View => "View",
            TableKind::MaterializedView => "Materialized view",
            TableKind::ForeignTable => "Foreign table",
        }
    }

    /// Only tables allow editing rows from the explorer.
    pub fn is_editable(self) -> bool {
        matches!(self, TableKind::Table | TableKind::PartitionedTable)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableInfo {
    pub table: TableRef,
    pub kind: TableKind,
    /// Planner estimate; `None` if the table has never been analyzed.
    pub estimated_rows: Option<u64>,
    pub rls_enabled: bool,
    pub policy_count: u32,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyRef {
    pub table: TableRef,
    pub column: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnInfo {
    pub name: String,
    /// Type as PostgreSQL writes it: `numeric(10,2)`, `user_role[]`…
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub primary_key: bool,
    pub foreign_key: Option<ForeignKeyRef>,
    /// `GENERATED … AS IDENTITY` or a generated column: not written by hand.
    pub generated: bool,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexInfo {
    pub name: String,
    pub definition: String,
    pub unique: bool,
    pub primary: bool,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintKind {
    PrimaryKey,
    ForeignKey,
    Unique,
    Check,
    Exclusion,
    Other,
}

impl ConstraintKind {
    pub fn label(self) -> &'static str {
        match self {
            ConstraintKind::PrimaryKey => "PRIMARY KEY",
            ConstraintKind::ForeignKey => "FOREIGN KEY",
            ConstraintKind::Unique => "UNIQUE",
            ConstraintKind::Check => "CHECK",
            ConstraintKind::Exclusion => "EXCLUDE",
            ConstraintKind::Other => "CONSTRAINT",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConstraintInfo {
    pub name: String,
    pub kind: ConstraintKind,
    pub definition: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PolicyInfo {
    pub name: String,
    /// `SELECT`, `INSERT`, `UPDATE`, `DELETE` or `ALL`.
    pub command: String,
    pub roles: Vec<String>,
    pub permissive: bool,
    pub using: Option<String>,
    pub with_check: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TriggerInfo {
    pub name: String,
    pub definition: String,
    pub enabled: bool,
}

/// Everything the explorer shows about a table.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TableDefinition {
    pub columns: Vec<ColumnInfo>,
    pub indexes: Vec<IndexInfo>,
    pub constraints: Vec<ConstraintInfo>,
    pub policies: Vec<PolicyInfo>,
    pub triggers: Vec<TriggerInfo>,
}

impl TableDefinition {
    pub fn primary_key(&self) -> Vec<String> {
        self.columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.clone())
            .collect()
    }

    pub fn column(&self, name: &str) -> Option<&ColumnInfo> {
        self.columns.iter().find(|c| c.name == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutineKind {
    Function,
    Procedure,
    Aggregate,
    Window,
}

impl RoutineKind {
    pub fn label(self) -> &'static str {
        match self {
            RoutineKind::Function => "function",
            RoutineKind::Procedure => "procedure",
            RoutineKind::Aggregate => "aggregate",
            RoutineKind::Window => "window function",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FunctionInfo {
    pub schema: String,
    pub name: String,
    /// Identity arguments, e.g. `p_day date`.
    pub arguments: String,
    pub result: String,
    pub kind: RoutineKind,
    pub language: String,
    /// Catalog id, used to fetch the definition.
    pub oid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumInfo {
    pub schema: String,
    pub name: String,
    pub values: Vec<String>,
}

/// A trigger as listed in the schema tree; its full definition lives in the
/// owning table's [`TableDefinition`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerRef {
    pub table: TableRef,
    pub name: String,
}

/// Every object the explorer tree shows for one connection.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CatalogOverview {
    pub schemas: Vec<String>,
    pub tables: Vec<TableInfo>,
    pub functions: Vec<FunctionInfo>,
    pub enums: Vec<EnumInfo>,
    pub triggers: Vec<TriggerRef>,
}
