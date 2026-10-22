use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use semver::Version;

use crate::{Error, Manifest, Update, verify_file};

/// The manifest is a few kilobytes; anything far bigger is not one.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
/// The universal macOS zip is about 100 MB.
const MAX_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;

/// Download progress, for the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Download {
    pub received: u64,
    /// From `Content-Length`, when the server sends it.
    pub total: Option<u64>,
}

/// Checks for and downloads updates for the running app.
pub struct Updater {
    manifest_url: String,
    public_key: String,
    current: Version,
    platform: String,
    max_archive_bytes: u64,
    http: reqwest::blocking::Client,
}

impl Updater {
    /// `public_key` is the minisign `.pub` file the release workflow signs
    /// for; `current_version` is the running app's (`CARGO_PKG_VERSION`).
    pub fn new(
        manifest_url: &str,
        public_key: &str,
        current_version: &str,
        platform: &str,
    ) -> Result<Self, Error> {
        check_secure(manifest_url)?;
        let current = Version::parse(current_version)
            .map_err(|_| Error::InvalidVersion(current_version.to_string()))?;
        let http = reqwest::blocking::Client::builder()
            .user_agent(concat!("Kuery/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            // Only the archive takes long, and it is read in chunks.
            .timeout(None)
            .build()?;
        Ok(Self {
            manifest_url: manifest_url.to_string(),
            public_key: public_key.to_string(),
            current,
            platform: platform.to_string(),
            max_archive_bytes: MAX_ARCHIVE_BYTES,
            http,
        })
    }

    /// The newer release for this platform, if any.
    pub fn check(&self) -> Result<Option<Update>, Error> {
        let response = self.get(&self.manifest_url)?;
        let mut json = String::new();
        response
            .take(MAX_MANIFEST_BYTES + 1)
            .read_to_string(&mut json)?;
        if json.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(Error::TooLarge(MAX_MANIFEST_BYTES));
        }
        Manifest::parse(&json)?.update_for(&self.current, &self.platform)
    }

    /// Downloads `update`'s archive into `dir` and returns its path once its
    /// SHA-256 and signature are verified. A failed or unverified download
    /// leaves nothing behind.
    pub fn download(
        &self,
        update: &Update,
        dir: &Path,
        progress: impl FnMut(Download),
    ) -> Result<PathBuf, Error> {
        let part = dir.join(format!("{}.part", update.file_name));
        let result = self
            .fetch(&update.asset.url, &part, progress)
            .and_then(|()| verify_file(&part, update, &self.public_key));
        if let Err(err) = result {
            let _ = std::fs::remove_file(&part);
            return Err(err);
        }
        let path = dir.join(&update.file_name);
        std::fs::rename(&part, &path)?;
        Ok(path)
    }

    /// Streams `url` into `path`, refusing more than `max_archive_bytes`.
    fn fetch(
        &self,
        url: &str,
        path: &Path,
        mut progress: impl FnMut(Download),
    ) -> Result<(), Error> {
        let mut response = self.get(url)?;
        let total = response.content_length();
        if total.is_some_and(|total| total > self.max_archive_bytes) {
            return Err(Error::TooLarge(self.max_archive_bytes));
        }
        let mut file = File::create(path)?;
        let mut buf = vec![0; 64 * 1024];
        let mut received = 0;
        loop {
            let n = response.read(&mut buf)?;
            if n == 0 {
                break;
            }
            received += n as u64;
            if received > self.max_archive_bytes {
                return Err(Error::TooLarge(self.max_archive_bytes));
            }
            file.write_all(&buf[..n])?;
            progress(Download { received, total });
        }
        file.sync_all()?;
        Ok(())
    }

    fn get(&self, url: &str) -> Result<reqwest::blocking::Response, Error> {
        check_secure(url)?;
        let response = self.http.get(url).send()?;
        if !response.status().is_success() {
            return Err(Error::Status {
                status: response.status().as_u16(),
                url: url.to_string(),
            });
        }
        Ok(response)
    }
}

/// Updates only come over https; plain http is allowed to this machine, for
/// tests.
fn check_secure(url: &str) -> Result<(), Error> {
    let insecure = || Error::InsecureUrl(url.to_string());
    let parsed = reqwest::Url::parse(url).map_err(|_| insecure())?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")) => Ok(()),
        _ => Err(insecure()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::verify::tests::{TestKey, sha256_hex, tempdir};

    /// Path → status and body.
    type Routes = Arc<Mutex<HashMap<String, (u16, Vec<u8>)>>>;

    /// A tiny HTTP server on 127.0.0.1 answering `routes`; unknown paths get
    /// 404.
    struct Server {
        base: String,
        routes: Routes,
    }

    impl Server {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let routes = Routes::default();
            let served = routes.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request_line = String::new();
                    reader.read_line(&mut request_line).unwrap();
                    // Skip the headers.
                    let mut line = String::new();
                    while reader.read_line(&mut line).unwrap() > 2 {
                        line.clear();
                    }
                    let path = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_string();
                    let (status, body) = served
                        .lock()
                        .unwrap()
                        .get(&path)
                        .cloned()
                        .unwrap_or((404, Vec::new()));
                    let head = format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body);
                }
            });
            Self { base, routes }
        }

        fn serve(&self, path: &str, status: u16, body: impl Into<Vec<u8>>) {
            self.routes
                .lock()
                .unwrap()
                .insert(path.into(), (status, body.into()));
        }

        fn url(&self, path: &str) -> String {
            format!("{}{path}", self.base)
        }
    }

    const ARCHIVE: &[u8] = b"a new Kuery";

    /// A server publishing version 0.3.0 for `linux-x86_64`, with `served`
    /// as the archive's bytes (signed for `ARCHIVE`).
    fn release(key: &TestKey, served: &[u8]) -> Server {
        let server = Server::start();
        let file = "kuery-v0.3.0-x86_64-unknown-linux-gnu.tar.gz";
        let manifest = serde_json::json!({
            "version": "0.3.0",
            "notes_url": "https://example.com/v0.3.0",
            "pub_date": "2026-10-22T17:20:00Z",
            "platforms": {
                "linux-x86_64": {
                    "url": server.url(&format!("/download/{file}")),
                    "sha256": sha256_hex(ARCHIVE),
                    "signature": key.sign(ARCHIVE, &format!("kuery 0.3.0 {file}")),
                }
            }
        });
        server.serve("/latest.json", 200, manifest.to_string());
        server.serve(&format!("/download/{file}"), 200, served);
        server
    }

    fn updater(server: &Server, key: &TestKey, current: &str) -> Updater {
        Updater::new(
            &server.url("/latest.json"),
            &key.public(),
            current,
            "linux-x86_64",
        )
        .unwrap()
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn finds_a_newer_release() {
        let key = TestKey::new();
        let server = release(&key, ARCHIVE);
        let update = updater(&server, &key, "0.2.1").check().unwrap().unwrap();
        assert_eq!(update.version, Version::new(0, 3, 0));
        assert_eq!(
            update.file_name,
            "kuery-v0.3.0-x86_64-unknown-linux-gnu.tar.gz"
        );
    }

    #[test]
    fn nothing_new_when_up_to_date() {
        let key = TestKey::new();
        let server = release(&key, ARCHIVE);
        assert_eq!(updater(&server, &key, "0.3.0").check().unwrap(), None);
    }

    #[test]
    fn reports_server_errors() {
        let key = TestKey::new();
        let server = Server::start();
        let err = updater(&server, &key, "0.2.1").check().unwrap_err();
        assert!(matches!(err, Error::Status { status: 404, .. }), "{err:?}");
    }

    #[test]
    fn downloads_and_keeps_a_verified_archive() {
        let key = TestKey::new();
        let server = release(&key, ARCHIVE);
        let updater = updater(&server, &key, "0.2.1");
        let update = updater.check().unwrap().unwrap();
        let dir = tempdir::Dir::new();
        let mut last = None;
        let path = updater
            .download(&update, dir.path(), |p| last = Some(p))
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), ARCHIVE);
        assert_eq!(files_in(dir.path()), [update.file_name.as_str()]);
        let len = ARCHIVE.len() as u64;
        assert_eq!(
            last,
            Some(Download {
                received: len,
                total: Some(len)
            })
        );
    }

    #[test]
    fn discards_a_tampered_archive() {
        let key = TestKey::new();
        let server = release(&key, b"malware");
        let updater = updater(&server, &key, "0.2.1");
        let update = updater.check().unwrap().unwrap();
        let dir = tempdir::Dir::new();
        let err = updater.download(&update, dir.path(), |_| {}).unwrap_err();
        assert!(matches!(err, Error::Sha256Mismatch), "{err:?}");
        assert!(files_in(dir.path()).is_empty());
    }

    #[test]
    fn discards_an_archive_signed_by_another_key() {
        let key = TestKey::new();
        let server = release(&key, ARCHIVE);
        let other = TestKey::new();
        let updater = updater(&server, &other, "0.2.1");
        let update = updater.check().unwrap().unwrap();
        let dir = tempdir::Dir::new();
        let err = updater.download(&update, dir.path(), |_| {}).unwrap_err();
        assert!(matches!(err, Error::Signature(_)), "{err:?}");
        assert!(files_in(dir.path()).is_empty());
    }

    #[test]
    fn refuses_an_archive_over_the_limit() {
        let key = TestKey::new();
        let server = release(&key, ARCHIVE);
        let mut updater = updater(&server, &key, "0.2.1");
        updater.max_archive_bytes = 4;
        let update = updater.check().unwrap().unwrap();
        let dir = tempdir::Dir::new();
        let err = updater.download(&update, dir.path(), |_| {}).unwrap_err();
        assert!(matches!(err, Error::TooLarge(4)), "{err:?}");
        assert!(files_in(dir.path()).is_empty());
    }

    #[test]
    fn refuses_plain_http_except_on_this_machine() {
        let err = Updater::new(
            "http://example.com/latest.json",
            "",
            "0.2.1",
            "linux-x86_64",
        );
        assert!(matches!(err, Err(Error::InsecureUrl(_))));
        assert!(
            Updater::new(
                "https://example.com/latest.json",
                "",
                "0.2.1",
                "linux-x86_64"
            )
            .is_ok()
        );
        assert!(
            Updater::new(
                "http://127.0.0.1:1/latest.json",
                "",
                "0.2.1",
                "linux-x86_64"
            )
            .is_ok()
        );
    }

    #[test]
    fn refuses_an_archive_over_plain_http() {
        let key = TestKey::new();
        let server = release(&key, ARCHIVE);
        let updater = updater(&server, &key, "0.2.1");
        let mut update = updater.check().unwrap().unwrap();
        update.asset.url = "http://example.com/kuery.tar.gz".into();
        let dir = tempdir::Dir::new();
        let err = updater.download(&update, dir.path(), |_| {}).unwrap_err();
        assert!(matches!(err, Error::InsecureUrl(_)), "{err:?}");
    }

    #[test]
    fn rejects_an_invalid_current_version() {
        let err = Updater::new("https://example.com/latest.json", "", "dev", "linux-x86_64");
        assert!(matches!(err, Err(Error::InvalidVersion(v)) if v == "dev"));
    }
}
