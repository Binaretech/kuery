use std::collections::BTreeMap;

use semver::Version;
use serde::Deserialize;

use crate::Error;

/// `latest.json`, as written by the release workflow.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub notes_url: String,
    pub pub_date: String,
    pub platforms: BTreeMap<String, Asset>,
}

/// One platform's archive: where to get it and how to check it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Asset {
    pub url: String,
    pub sha256: String,
    /// The full `.minisig` file.
    pub signature: String,
}

/// A release newer than the running app, for this platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub version: Version,
    pub notes_url: String,
    pub asset: Asset,
    /// The archive's name, the last segment of its URL. The signature's
    /// trusted comment names it.
    pub file_name: String,
}

/// This build's key in the manifest's `platforms`, if releases have one.
pub fn platform() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("darwin-aarch64")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("darwin-x86_64")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("linux-x86_64")
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("windows-x86_64")
    } else {
        None
    }
}

impl Manifest {
    pub fn parse(json: &str) -> Result<Self, Error> {
        serde_json::from_str(json).map_err(|e| Error::Manifest(e.to_string()))
    }

    /// The update for `platform` when the manifest's version is newer than
    /// `current`; `None` when the app is up to date.
    pub fn update_for(&self, current: &Version, platform: &str) -> Result<Option<Update>, Error> {
        let version = Version::parse(&self.version)
            .map_err(|e| Error::Manifest(format!("version \"{}\": {e}", self.version)))?;
        if version <= *current {
            return Ok(None);
        }
        let asset = self
            .platforms
            .get(platform)
            .ok_or_else(|| Error::UnsupportedPlatform(platform.to_string()))?;
        let file_name = asset.url.rsplit('/').next().unwrap_or_default();
        // The archive is saved under this name, so it must be a plain one.
        let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_');
        if file_name.is_empty()
            || !file_name.chars().all(plain)
            || file_name.chars().all(|c| c == '.')
        {
            return Err(Error::Manifest(format!(
                "no file name in \"{}\"",
                asset.url
            )));
        }
        Ok(Some(Update {
            version,
            notes_url: self.notes_url.clone(),
            asset: asset.clone(),
            file_name: file_name.to_string(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"{
      "version": "0.3.0",
      "notes_url": "https://github.com/Binaretech/kuery/releases/tag/v0.3.0",
      "pub_date": "2026-10-22T17:20:00Z",
      "platforms": {
        "darwin-aarch64": {
          "url": "https://github.com/Binaretech/kuery/releases/download/v0.3.0/kuery-v0.3.0-universal-apple-darwin.zip",
          "sha256": "33d1629235442b8f7b7110b3e6aea67f0b4d2a8f80e46d89fac58b566e6bf3d8",
          "signature": "untrusted comment: signature from minisign secret key\nRUT...\ntrusted comment: kuery 0.3.0 kuery-v0.3.0-universal-apple-darwin.zip\n..."
        },
        "linux-x86_64": {
          "url": "https://github.com/Binaretech/kuery/releases/download/v0.3.0/kuery-v0.3.0-x86_64-unknown-linux-gnu.tar.gz",
          "sha256": "b5a08988be052363d035f26d1314a1ca69f83ca9c491277b14a6c4459507b4d5",
          "signature": "..."
        }
      }
    }"#;

    fn version(v: &str) -> Version {
        Version::parse(v).unwrap()
    }

    #[test]
    fn parses_the_release_workflow_manifest() {
        let manifest = Manifest::parse(MANIFEST).unwrap();
        assert_eq!(manifest.version, "0.3.0");
        assert_eq!(manifest.platforms.len(), 2);
        assert!(
            manifest.platforms["darwin-aarch64"]
                .url
                .ends_with("universal-apple-darwin.zip")
        );
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(matches!(
            Manifest::parse("{\"version\": 3"),
            Err(Error::Manifest(_))
        ));
        assert!(matches!(Manifest::parse("{}"), Err(Error::Manifest(_))));
    }

    #[test]
    fn offers_a_newer_version() {
        let manifest = Manifest::parse(MANIFEST).unwrap();
        let update = manifest
            .update_for(&version("0.2.1"), "darwin-aarch64")
            .unwrap()
            .unwrap();
        assert_eq!(update.version, version("0.3.0"));
        assert_eq!(update.file_name, "kuery-v0.3.0-universal-apple-darwin.zip");
        assert_eq!(update.asset, manifest.platforms["darwin-aarch64"]);
        assert_eq!(update.notes_url, manifest.notes_url);
    }

    #[test]
    fn nothing_when_up_to_date_or_newer() {
        let manifest = Manifest::parse(MANIFEST).unwrap();
        assert_eq!(
            manifest
                .update_for(&version("0.3.0"), "darwin-aarch64")
                .unwrap(),
            None
        );
        assert_eq!(
            manifest
                .update_for(&version("0.10.0"), "darwin-aarch64")
                .unwrap(),
            None
        );
    }

    #[test]
    fn compares_versions_numerically() {
        let manifest = Manifest::parse(&MANIFEST.replace("\"0.3.0\"", "\"0.10.0\"")).unwrap();
        assert!(
            manifest
                .update_for(&version("0.9.0"), "linux-x86_64")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn up_to_date_even_without_this_platform() {
        let manifest = Manifest::parse(MANIFEST).unwrap();
        assert_eq!(
            manifest
                .update_for(&version("0.3.0"), "windows-x86_64")
                .unwrap(),
            None
        );
    }

    #[test]
    fn newer_version_without_this_platform_is_an_error() {
        let manifest = Manifest::parse(MANIFEST).unwrap();
        let err = manifest
            .update_for(&version("0.2.1"), "windows-x86_64")
            .unwrap_err();
        assert!(matches!(err, Error::UnsupportedPlatform(p) if p == "windows-x86_64"));
    }

    #[test]
    fn rejects_an_invalid_version() {
        let manifest = Manifest::parse(&MANIFEST.replace("\"0.3.0\"", "\"v0.3\"")).unwrap();
        assert!(matches!(
            manifest.update_for(&version("0.2.1"), "linux-x86_64"),
            Err(Error::Manifest(_))
        ));
    }

    #[test]
    fn rejects_an_asset_url_without_a_file_name() {
        let manifest = Manifest::parse(
            &MANIFEST.replace("/kuery-v0.3.0-x86_64-unknown-linux-gnu.tar.gz", "/"),
        )
        .unwrap();
        assert!(matches!(
            manifest.update_for(&version("0.2.1"), "linux-x86_64"),
            Err(Error::Manifest(_))
        ));
    }

    #[test]
    fn rejects_a_file_name_that_is_not_a_plain_name() {
        // As written in the JSON, so the backslash is escaped.
        for name in ["..", "kuery%2F..%2Fx.zip", "kuery\\\\x.zip", "kuery x.zip"] {
            let manifest = Manifest::parse(&MANIFEST.replace(
                "kuery-v0.3.0-x86_64-unknown-linux-gnu.tar.gz\"",
                &format!("{name}\""),
            ))
            .unwrap();
            assert!(
                matches!(
                    manifest.update_for(&version("0.2.1"), "linux-x86_64"),
                    Err(Error::Manifest(_))
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn this_build_has_a_platform_key() {
        // CI builds all four; each must map to a manifest key.
        let platform = platform().expect("a released platform");
        assert!(
            [
                "darwin-aarch64",
                "darwin-x86_64",
                "linux-x86_64",
                "windows-x86_64"
            ]
            .contains(&platform)
        );
    }
}
