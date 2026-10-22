/// Why an update check or download failed. None of these install anything:
/// a download is only kept once it is fully verified.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("update URLs must use https: {0}")]
    InsecureUrl(String),
    #[error("couldn't reach the update server: {0}")]
    Http(#[from] reqwest::Error),
    #[error("the update server answered {status} for {url}")]
    Status { status: u16, url: String },
    #[error("the update manifest is invalid: {0}")]
    Manifest(String),
    #[error("\"{0}\" is not a version")]
    InvalidVersion(String),
    #[error("no update is published for {0}")]
    UnsupportedPlatform(String),
    #[error("the download is larger than {0} bytes")]
    TooLarge(u64),
    #[error("the downloaded file's SHA-256 doesn't match the manifest")]
    Sha256Mismatch,
    #[error("the update's signature is invalid: {0}")]
    Signature(String),
    #[error("the signature is for \"{found}\", not \"{expected}\"")]
    SignedFor { expected: String, found: String },
    #[error("the update could not be installed: {0}")]
    Install(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
