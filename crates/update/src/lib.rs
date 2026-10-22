//! Kuery's updater, with no UI dependencies: reads the release's
//! `latest.json`, decides whether it is newer than the running app, and
//! downloads the archive for this platform, accepting it only when its
//! SHA-256 and its minisign signature (made by the release workflow, see
//! `.github/scripts/update-manifest.sh`) both match.
//!
//! Everything here blocks; the app calls it from a background thread.

mod client;
mod error;
mod manifest;
mod verify;

pub use client::{Download, Updater};
pub use error::Error;
pub use manifest::{Asset, Manifest, Update, platform};
pub use verify::verify_file;

/// The manifest the app reads: always the newest release's.
pub const MANIFEST_URL: &str =
    "https://github.com/angel-afonso/kuery/releases/latest/download/latest.json";
