//! Kuery core, with no UI dependencies: read-only policy, statement
//! analysis for write mode, columnar results, the catalog model, and
//! generated SQL for browsing and editing tables.

pub mod analysis;
pub mod browse;
pub mod catalog;
pub mod edit;
pub mod fuzzy;
pub mod read_only;
pub mod result_set;
pub mod sql;
