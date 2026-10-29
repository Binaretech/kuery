//! Document databases, whichever the engine: the values they hold, their
//! catalog, the changes Kuery makes to documents, the contract a driver
//! implements ([`adapter::DocumentAdapter`]) and the
//! [`session::DocumentSession`] that drives any driver by the same rules.
//!
//! What every engine shares lives here, written once: read-only by default
//! (statements are analysed before anything runs, and only reads pass), the
//! refusal of heavy work, scripts that run in one transaction where the
//! engine has them and stop at the first failure where it does not,
//! document changes checked one by one, timeouts, cancelling, streaming,
//! and the pure helpers that turn documents into rows, schemas, diffs and
//! exports. What differs between engines reaches this code only through the
//! data a driver describes itself with: its [`language::Language`] (how to
//! split, classify, parse and print its statements and literals) and the
//! [`capabilities::Capabilities`] it detected when it connected. There are
//! no branches on the engine's name.
//!
//! The contract is not stable yet: it was shaped by MongoDB, and it stays
//! open to change until an engine that does not speak MongoDB's protocol
//! passes [`conformance`]. An engine that needs a collection tab of its own,
//! or more than a couple of capabilities only it uses, is not a document
//! engine for Kuery: it gets a family of its own, as Redis did, and the
//! contract does not bend to fit it.

pub mod adapter;
pub mod capabilities;
pub mod catalog;
pub mod change;
#[cfg(feature = "conformance")]
pub mod conformance;
pub mod diff;
pub mod export;
pub mod flatten;
pub mod language;
pub mod schema;
pub mod session;
pub mod value;
