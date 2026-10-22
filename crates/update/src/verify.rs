use std::fs::File;
use std::io::Read;
use std::path::Path;

use minisign_verify::{PublicKey, Signature};
use sha2::{Digest, Sha256};

use crate::{Error, Update};

/// Checks a downloaded archive against `update`: its SHA-256, then its
/// minisign signature by `public_key` (a minisign `.pub` file's contents),
/// whose trusted comment must be `kuery <version> <file name>` so a valid
/// signature for another release or file is refused.
pub fn verify_file(path: &Path, update: &Update, public_key: &str) -> Result<(), Error> {
    let key = PublicKey::decode(public_key).map_err(|e| Error::Signature(e.to_string()))?;
    let signature =
        Signature::decode(&update.asset.signature).map_err(|e| Error::Signature(e.to_string()))?;
    // The signature covers its trusted comment, so this ties it to the
    // version and file the manifest offers.
    let expected = format!("kuery {} {}", update.version, update.file_name);
    if signature.trusted_comment() != expected {
        return Err(Error::SignedFor {
            expected,
            found: signature.trusted_comment().to_string(),
        });
    }

    let mut verifier = key
        .verify_stream(&signature)
        .map_err(|e| Error::Signature(e.to_string()))?;
    let mut sha256 = Sha256::new();
    let mut file = File::open(path)?;
    let mut buf = vec![0; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        sha256.update(&buf[..n]);
        verifier.update(&buf[..n]);
    }

    let sha256: String = sha256
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if !sha256.eq_ignore_ascii_case(&update.asset.sha256) {
        return Err(Error::Sha256Mismatch);
    }
    verifier
        .finalize()
        .map_err(|e| Error::Signature(e.to_string()))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Cursor;

    use semver::Version;

    use super::*;
    use crate::Asset;

    /// A throwaway key pair; `sign` makes `.minisig` files with it.
    pub(crate) struct TestKey {
        pair: minisign::KeyPair,
    }

    impl TestKey {
        pub(crate) fn new() -> Self {
            Self {
                pair: minisign::KeyPair::generate_unencrypted_keypair().unwrap(),
            }
        }

        pub(crate) fn public(&self) -> String {
            self.pair.pk.to_box().unwrap().into_string()
        }

        pub(crate) fn sign(&self, data: &[u8], trusted_comment: &str) -> String {
            minisign::sign(
                None,
                &self.pair.sk,
                Cursor::new(data),
                Some(trusted_comment),
                None,
            )
            .unwrap()
            .into_string()
        }
    }

    pub(crate) fn sha256_hex(data: &[u8]) -> String {
        use sha2::Digest;
        sha2::Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// An update for `data` named `file_name`, signed for `version` and
    /// `file_name` by `key`.
    pub(crate) fn signed_update(
        key: &TestKey,
        data: &[u8],
        version: &str,
        file_name: &str,
    ) -> Update {
        Update {
            version: Version::parse(version).unwrap(),
            notes_url: format!("https://example.com/v{version}"),
            asset: Asset {
                url: format!("https://example.com/{file_name}"),
                sha256: sha256_hex(data),
                signature: key.sign(data, &format!("kuery {version} {file_name}")),
            },
            file_name: file_name.to_string(),
        }
    }

    fn write(data: &[u8]) -> (tempdir::Dir, std::path::PathBuf) {
        let dir = tempdir::Dir::new();
        let path = dir.path().join("archive.zip");
        std::fs::write(&path, data).unwrap();
        (dir, path)
    }

    const DATA: &[u8] = b"the Kuery archive";

    #[test]
    fn accepts_a_signed_archive() {
        let key = TestKey::new();
        let update = signed_update(&key, DATA, "0.3.0", "kuery.zip");
        let (_dir, path) = write(DATA);
        verify_file(&path, &update, &key.public()).unwrap();
    }

    #[test]
    fn rejects_a_tampered_archive() {
        let key = TestKey::new();
        let update = signed_update(&key, DATA, "0.3.0", "kuery.zip");
        let (_dir, path) = write(b"the Kuery archive, changed");
        assert!(matches!(
            verify_file(&path, &update, &key.public()),
            Err(Error::Sha256Mismatch)
        ));
    }

    #[test]
    fn rejects_a_tampered_archive_with_a_matching_sha256() {
        // Whoever can change the release can change the manifest's SHA-256
        // too; only the signature stops them.
        let key = TestKey::new();
        let mut update = signed_update(&key, DATA, "0.3.0", "kuery.zip");
        let tampered = b"malware";
        update.asset.sha256 = sha256_hex(tampered);
        let (_dir, path) = write(tampered);
        assert!(matches!(
            verify_file(&path, &update, &key.public()),
            Err(Error::Signature(_))
        ));
    }

    #[test]
    fn rejects_another_key() {
        let key = TestKey::new();
        let update = signed_update(&key, DATA, "0.3.0", "kuery.zip");
        let (_dir, path) = write(DATA);
        let other = TestKey::new();
        assert!(matches!(
            verify_file(&path, &update, &other.public()),
            Err(Error::Signature(_))
        ));
    }

    #[test]
    fn rejects_a_signature_for_another_version() {
        // An old, validly signed archive offered as a newer version.
        let key = TestKey::new();
        let mut update = signed_update(&key, DATA, "0.2.1", "kuery.zip");
        update.version = Version::parse("9.0.0").unwrap();
        let (_dir, path) = write(DATA);
        let err = verify_file(&path, &update, &key.public()).unwrap_err();
        assert!(
            matches!(&err, Error::SignedFor { expected, found }
                if expected == "kuery 9.0.0 kuery.zip" && found == "kuery 0.2.1 kuery.zip"),
            "{err:?}"
        );
    }

    #[test]
    fn rejects_a_signature_for_another_file() {
        // The Linux archive offered to a Mac.
        let key = TestKey::new();
        let mut update = signed_update(&key, DATA, "0.3.0", "kuery-linux.tar.gz");
        update.file_name = "kuery-mac.zip".into();
        let (_dir, path) = write(DATA);
        assert!(matches!(
            verify_file(&path, &update, &key.public()),
            Err(Error::SignedFor { .. })
        ));
    }

    #[test]
    fn rejects_a_malformed_signature_or_key() {
        let key = TestKey::new();
        let mut update = signed_update(&key, DATA, "0.3.0", "kuery.zip");
        let (_dir, path) = write(DATA);
        assert!(matches!(
            verify_file(&path, &update, "not a key"),
            Err(Error::Signature(_))
        ));
        update.asset.signature = "not a signature".into();
        assert!(matches!(
            verify_file(&path, &update, &key.public()),
            Err(Error::Signature(_))
        ));
    }

    /// A temporary directory removed on drop, without another dependency.
    pub(crate) mod tempdir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicUsize, Ordering};

        pub(crate) struct Dir(PathBuf);

        impl Dir {
            pub(crate) fn new() -> Self {
                static NEXT: AtomicUsize = AtomicUsize::new(0);
                let path = std::env::temp_dir().join(format!(
                    "kuery-update-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }

            pub(crate) fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
