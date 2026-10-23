//! One Kuery per user: the first one listens on a local socket (a named pipe
//! on Windows), and a later `kuery [PATH...]` hands its paths to it instead
//! of starting a second app.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
};
use tokio::runtime::Runtime;

use crate::cli::Target;

/// How long either side waits for the other.
const PATIENCE: Duration = Duration::from_secs(5);
/// Far longer than any command line.
const MAX_MESSAGE: u64 = 1 << 20;

#[derive(Debug, PartialEq, Eq)]
pub enum HandOff {
    /// The running Kuery took the paths.
    Delivered,
    /// No Kuery is listening.
    NotRunning,
}

/// Where this user's Kuery listens. Development builds use their own, so
/// `cargo run` never hands off to an installed Kuery.
pub fn address() -> PathBuf {
    let name = if cfg!(debug_assertions) {
        "kuery-dev"
    } else {
        "kuery"
    };
    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME").unwrap_or_default();
        PathBuf::from(format!(r"\\.\pipe\{name}-{user}"))
    }
    #[cfg(not(windows))]
    {
        crate::paths::config_dir().join(format!("{name}.sock"))
    }
}

/// Sends `targets` to the Kuery listening on `address`, if there is one.
pub fn hand_off(address: &Path, targets: &[Target]) -> io::Result<HandOff> {
    let message = serde_json::to_string(targets).map_err(io::Error::other)? + "\n";
    runtime()?.block_on(async {
        let Some(stream) = connect(address).await? else {
            return Ok(HandOff::NotRunning);
        };
        tokio::time::timeout(PATIENCE, exchange(stream, &message))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Kuery did not answer"))??;
        Ok(HandOff::Delivered)
    })
}

async fn exchange(stream: impl AsyncRead + AsyncWrite + Unpin, message: &str) -> io::Result<()> {
    let mut stream = BufReader::new(stream);
    stream.get_mut().write_all(message.as_bytes()).await?;
    let mut answer = String::new();
    stream.read_line(&mut answer).await?;
    if answer.trim_end() == "ok" {
        Ok(())
    } else {
        Err(io::Error::other("Kuery did not take the paths"))
    }
}

#[cfg(unix)]
async fn connect(address: &Path) -> io::Result<Option<tokio::net::UnixStream>> {
    match tokio::net::UnixStream::connect(address).await {
        Ok(stream) => Ok(Some(stream)),
        // No socket, or one a crashed Kuery left behind.
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
async fn connect(
    address: &Path,
) -> io::Result<Option<tokio::net::windows::named_pipe::NamedPipeClient>> {
    const ERROR_PIPE_BUSY: i32 = 231;
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        match tokio::net::windows::named_pipe::ClientOptions::new().open(address) {
            Ok(pipe) => return Ok(Some(pipe)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            // Every instance is taken; the server makes another right away.
            Err(error)
                if error.raw_os_error() == Some(ERROR_PIPE_BUSY)
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn runtime() -> io::Result<Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

/// Listens on `address`; fails when another Kuery already does.
pub struct Listener {
    runtime: Runtime,
    #[cfg(unix)]
    socket: tokio::net::UnixListener,
    #[cfg(windows)]
    pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    #[cfg(windows)]
    address: PathBuf,
}

impl Listener {
    #[cfg(unix)]
    pub fn bind(address: &Path) -> io::Result<Self> {
        use std::os::unix::fs::PermissionsExt as _;

        let runtime = runtime()?;
        let _context = runtime.enter();
        if let Some(parent) = address.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let socket = match tokio::net::UnixListener::bind(address) {
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                if std::os::unix::net::UnixStream::connect(address).is_ok() {
                    return Err(error);
                }
                // Left behind by a Kuery that crashed.
                std::fs::remove_file(address)?;
                tokio::net::UnixListener::bind(address)?
            }
            bound => bound?,
        };
        std::fs::set_permissions(address, std::fs::Permissions::from_mode(0o600))?;
        drop(_context);
        Ok(Self { runtime, socket })
    }

    #[cfg(windows)]
    pub fn bind(address: &Path) -> io::Result<Self> {
        let runtime = runtime()?;
        let _context = runtime.enter();
        let pipe = tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .create(address)?;
        drop(_context);
        Ok(Self {
            runtime,
            pipe,
            address: address.to_path_buf(),
        })
    }

    /// Answers hand-offs on a background thread, passing each one's targets
    /// to `open`.
    pub fn serve(self, open: impl Fn(Vec<Target>) + Send + Sync + 'static) {
        let open: Open = Arc::new(open);
        let runtime = self.runtime;
        #[cfg(unix)]
        let accepting = accept(self.socket, open);
        #[cfg(windows)]
        let accepting = accept(self.pipe, self.address, open);
        let spawned = std::thread::Builder::new()
            .name("kuery-instance".into())
            .spawn(move || runtime.block_on(accepting));
        if let Err(error) = spawned {
            eprintln!("kuery: cannot listen for other kuery commands: {error}");
        }
    }
}

type Open = Arc<dyn Fn(Vec<Target>) + Send + Sync>;

#[cfg(unix)]
async fn accept(socket: tokio::net::UnixListener, open: Open) {
    loop {
        match socket.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(answer(stream, open.clone()));
            }
            Err(error) => {
                eprintln!("kuery: a kuery command could not connect: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

#[cfg(windows)]
async fn accept(
    mut pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    address: PathBuf,
    open: Open,
) {
    use tokio::net::windows::named_pipe::ServerOptions;

    loop {
        let connected = pipe.connect().await;
        // An instance serves one command; the next one waits on a new one.
        // A failed instance is dropped too: connecting again on it fails
        // the same way (a command that went away before the connect).
        let next = match ServerOptions::new().create(&address) {
            Ok(next) => next,
            Err(error) => {
                eprintln!("kuery: cannot listen for other kuery commands: {error}");
                return;
            }
        };
        let served = std::mem::replace(&mut pipe, next);
        match connected {
            Ok(()) => {
                tokio::spawn(answer(served, open.clone()));
            }
            Err(error) => eprintln!("kuery: a kuery command could not connect: {error}"),
        }
    }
}

/// Reads one hand-off and says "ok" once its targets are passed on.
async fn answer(stream: impl AsyncRead + AsyncWrite + Unpin, open: Open) {
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    let read = tokio::time::timeout(
        PATIENCE,
        (&mut stream).take(MAX_MESSAGE).read_line(&mut line),
    )
    .await;
    if !matches!(read, Ok(Ok(_))) {
        return;
    }
    match serde_json::from_str::<Vec<Target>>(line.trim_end()) {
        Ok(targets) => {
            open(targets);
            let _ = stream.get_mut().write_all(b"ok\n").await;
        }
        Err(error) => eprintln!("kuery: ignored a garbled kuery command: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    /// Removes the test's socket file when it ends (a pipe goes by itself).
    struct Cleanup(PathBuf);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn test_address() -> PathBuf {
        let id = crate::project::new_id();
        if cfg!(windows) {
            PathBuf::from(format!(r"\\.\pipe\kuery-test-{id}"))
        } else {
            std::env::temp_dir().join(format!("kuery-test-{id}.sock"))
        }
    }

    fn targets() -> Vec<Target> {
        vec![
            Target::Project(PathBuf::from("/work/shop")),
            Target::DataFile(PathBuf::from("/work/orders.csv")),
        ]
    }

    /// A listener on a new address whose hand-offs arrive on the receiver.
    fn serve() -> (Cleanup, mpsc::Receiver<Vec<Target>>) {
        let address = Cleanup(test_address());
        let (sent, received) = mpsc::channel();
        Listener::bind(&address.0)
            .unwrap()
            .serve(move |targets| sent.send(targets).unwrap());
        (address, received)
    }

    #[test]
    fn the_running_kuery_receives_the_paths() {
        let (address, received) = serve();
        assert_eq!(
            hand_off(&address.0, &targets()).unwrap(),
            HandOff::Delivered
        );
        assert_eq!(
            received.recv_timeout(Duration::from_secs(5)).unwrap(),
            targets()
        );
        // And the next command's, with nothing to open: it comes to the front.
        assert_eq!(hand_off(&address.0, &[]).unwrap(), HandOff::Delivered);
        assert_eq!(
            received.recv_timeout(Duration::from_secs(5)).unwrap(),
            vec![]
        );
    }

    #[test]
    fn without_a_running_kuery_nothing_is_handed_off() {
        assert_eq!(
            hand_off(&test_address(), &targets()).unwrap(),
            HandOff::NotRunning
        );
    }

    #[test]
    fn a_second_kuery_cannot_listen() {
        let (address, _received) = serve();
        assert!(Listener::bind(&address.0).is_err());
    }

    #[test]
    fn a_garbled_message_is_ignored() {
        let (address, received) = serve();
        send_raw(&address.0, b"not json\n");
        assert_eq!(
            hand_off(&address.0, &targets()).unwrap(),
            HandOff::Delivered
        );
        assert_eq!(
            received.recv_timeout(Duration::from_secs(5)).unwrap(),
            targets()
        );
    }

    #[cfg(unix)]
    fn send_raw(address: &std::path::Path, bytes: &[u8]) {
        use std::io::Write as _;
        let mut stream = std::os::unix::net::UnixStream::connect(address).unwrap();
        stream.write_all(bytes).unwrap();
    }

    #[cfg(windows)]
    fn send_raw(address: &std::path::Path, bytes: &[u8]) {
        use std::io::Write as _;
        let mut pipe = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(address)
            .unwrap();
        pipe.write_all(bytes).unwrap();
    }

    /// A crashed Kuery leaves its socket file behind.
    #[cfg(unix)]
    #[test]
    fn a_stale_socket_is_replaced() {
        let address = test_address();
        let _cleanup = Cleanup(address.clone());
        drop(std::os::unix::net::UnixListener::bind(&address).unwrap());
        assert!(address.exists());
        assert_eq!(hand_off(&address, &targets()).unwrap(), HandOff::NotRunning);
        let (sent, received) = mpsc::channel();
        Listener::bind(&address)
            .unwrap()
            .serve(move |targets| sent.send(targets).unwrap());
        assert_eq!(hand_off(&address, &targets()).unwrap(), HandOff::Delivered);
        assert_eq!(
            received.recv_timeout(Duration::from_secs(5)).unwrap(),
            targets()
        );
    }

    #[test]
    fn development_builds_listen_apart() {
        let name = address()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(name.starts_with("kuery"), "{name}");
        assert_eq!(name.contains("-dev"), cfg!(debug_assertions), "{name}");
    }
}
