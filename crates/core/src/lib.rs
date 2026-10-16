//! Kuery core, with no UI dependencies: read-only policy, statement
//! analysis for write mode, columnar results and their export, the catalog
//! model, generated SQL for browsing and editing tables, statistics over
//! results, readable query plans, schema diagrams, Redis command and key-name
//! helpers, and the drivers' descriptors.

pub mod activity;
pub mod analysis;
pub mod browse;
pub mod catalog;
pub mod compare;
pub mod completion;
pub mod diagram;
pub mod driver;
pub mod edit;
pub mod export;
pub mod format;
pub mod fuzzy;
pub mod plan;
pub mod read_only;
pub mod redis;
pub mod result_set;
pub mod script;
pub mod sql;
pub mod stats;
