//! Public preview metadata; row counts only describe the loaded prefix.

use std::time::SystemTime;

use kuery_core::result_set::{ColumnKind, ResultSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFingerprint {
    pub bytes: u64,
    pub modified: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSchema {
    pub name: String,
    pub native_type: String,
    pub kind: ColumnKind,
    pub inferred: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitReason {
    Rows,
    Bytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewScope {
    Complete,
    PrefixLimited {
        loaded_rows: usize,
        row_limit: usize,
        reason: LimitReason,
    },
}

#[derive(Debug, Clone)]
pub struct PreviewResult {
    pub results: ResultSet,
    pub scope: PreviewScope,
    pub schema: Vec<ColumnSchema>,
    pub warnings: Vec<String>,
    pub fingerprint: FileFingerprint,
}
