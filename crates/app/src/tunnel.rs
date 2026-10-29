//! SSH tunnels: a connection can reach its server through an SSH host (a
//! bastion), the server's host and port as that SSH host sees them.
//!
//! The tunnel is the system's own `ssh` forwarding a local port, so it
//! follows `~/.ssh/config` (host aliases, `ProxyJump`, keys), the SSH agent
//! and `known_hosts`. The port leads to the server, or, for a driver that
//! must dial the server by its name to check its TLS certificate, is a SOCKS
//! proxy (`ssh -D`) the driver dials it through (see
//! [`kuery_core::driver::DriverInfo::ssh_route`]). It never asks anything: authentication is by key or
//! agent (`BatchMode`), a host seen for the first time is remembered and a
//! host whose key changed is refused (`StrictHostKeyChecking=accept-new`).
//! The tunnel lives as long as the connection that opened it.

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kuery_core::driver::SshRoute;

use crate::project::{ConnectionSpec, SshSpec};

/// How long `ssh` gets to connect, authenticate and start forwarding.
const START: Duration = Duration::from_secs(20);

/// What a tunnel's local port leads to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Forward {
    /// The server, at its host and port as the SSH host sees them.
    Port { host: String, port: u16 },
    /// A SOCKS proxy: whatever the driver dials through it, by name, as the
    /// SSH host resolves it.
    Socks,
}

/// A running `ssh` that forwards `127.0.0.1:<local_port>`.
pub struct Tunnel {
    child: Child,
    local_port: u16,
    socks: bool,
}

impl Tunnel {
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// Whether the local port is a SOCKS proxy rather than the server.
    pub fn is_proxy(&self) -> bool {
        self.socks
    }

    /// Starts `ssh` and waits until it forwards, or says why it could not.
    /// Blocks: call it off the UI thread (see [`open_for`]).
    pub fn open(ssh: &SshSpec, forward: &Forward) -> Result<Tunnel, String> {
        let local_port = free_port()?;
        let mut command = Command::new("ssh");
        command
            .args(arguments(ssh, local_port, forward))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            // CREATE_NO_WINDOW: no console flashing up.
            command.creation_flags(0x0800_0000);
        }
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "SSH tunnels need the ssh command (OpenSSH), which was not found".to_string()
            } else {
                format!("Could not start ssh: {error}")
            }
        })?;

        // What ssh says, for the error if it gives up.
        let said = Arc::new(Mutex::new(Vec::<String>::new()));
        let reader = child.stderr.take().map(|stderr| {
            let said = said.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if let Ok(mut said) = said.lock() {
                        said.push(line);
                    }
                }
            })
        });

        let started = Instant::now();
        loop {
            if let Ok(Some(_)) = child.try_wait() {
                if let Some(reader) = reader {
                    let _ = reader.join();
                }
                let said = said.lock().map(|said| said.clone()).unwrap_or_default();
                return Err(explain(&said));
            }
            if listening(local_port) {
                let socks = *forward == Forward::Socks;
                return Ok(Tunnel {
                    child,
                    local_port,
                    socks,
                });
            }
            if started.elapsed() > START {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "The SSH host {} did not answer in {} seconds",
                    ssh.host,
                    START.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Opens the tunnel a connection asks for, if any, off the calling thread:
/// where the driver says it leads (see [`forward_for`]).
pub async fn open_for(spec: &ConnectionSpec) -> Result<Option<Arc<Tunnel>>, String> {
    let Some(ssh) = spec.ssh.clone() else {
        return Ok(None);
    };
    let forward = forward_for(spec)?;
    let (send, receive) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = send.send(Tunnel::open(&ssh, &forward));
    });
    match receive.await {
        Ok(result) => result.map(|tunnel| Some(Arc::new(tunnel))),
        Err(_) => Err("The SSH tunnel stopped before it started".into()),
    }
}

/// Where a connection's tunnel leads, as its driver says, or why it cannot
/// go through SSH: a SOCKS proxy for a driver that dials the server by its
/// name over TLS, else a port forwarded to the server. Without a word from
/// the driver, the host and port as given.
fn forward_for(spec: &ConnectionSpec) -> Result<Forward, String> {
    let route = spec.driver_info().and_then(|info| info.ssh_route);
    let Some(route) = route else {
        return Ok(Forward::Port {
            host: spec.host.clone(),
            port: spec.port,
        });
    };
    Ok(
        match route(&crate::drivers::connect_options(spec, None, None))? {
            SshRoute::Forward { host, port } => Forward::Port { host, port },
            SshRoute::Proxy => Forward::Socks,
        },
    )
}

/// A local port nobody listens on (the system picks it).
fn free_port() -> Result<u16, String> {
    TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .map_err(|error| format!("Could not find a free local port for the tunnel: {error}"))
}

/// Whether something (ssh, by now) listens on the local port. Checked by
/// trying to take the port, so nothing connects through the tunnel.
fn listening(port: u16) -> bool {
    matches!(
        TcpListener::bind(("127.0.0.1", port)),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse
    )
}

/// `ssh`'s arguments: forward the local port, no command, no questions.
fn arguments(ssh: &SshSpec, local_port: u16, forward: &Forward) -> Vec<String> {
    let mut arguments: Vec<String> = [
        "-N",
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        "ConnectTimeout=15",
        "-o",
        "ServerAliveInterval=30",
        "-o",
        "ServerAliveCountMax=3",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    match forward {
        Forward::Port { host, port } => {
            let host = if host.contains(':') && !host.starts_with('[') {
                format!("[{host}]")
            } else {
                host.to_string()
            };
            arguments.extend(["-L".into(), format!("127.0.0.1:{local_port}:{host}:{port}")]);
        }
        Forward::Socks => arguments.extend(["-D".into(), format!("127.0.0.1:{local_port}")]),
    }
    if let Some(port) = ssh.port {
        arguments.extend(["-p".into(), port.to_string()]);
    }
    if !ssh.user.trim().is_empty() {
        arguments.extend(["-l".into(), ssh.user.trim().to_string()]);
    }
    if !ssh.identity_file.trim().is_empty() {
        arguments.extend([
            "-i".into(),
            expand_home(ssh.identity_file.trim()),
            "-o".into(),
            "IdentitiesOnly=yes".into(),
        ]);
    }
    // After `--`, a host can never be taken for an option.
    arguments.extend(["--".into(), ssh.host.trim().to_string()]);
    arguments
}

/// `~/.ssh/id_ed25519` → `/Users/me/.ssh/id_ed25519`.
fn expand_home(path: &str) -> String {
    match path.strip_prefix("~/").or(path.strip_prefix("~\\")) {
        Some(rest) => std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(|home| std::path::Path::new(&home).join(rest).display().to_string())
            .unwrap_or_else(|| path.to_string()),
        None => path.to_string(),
    }
}

/// Why ssh gave up, from what it said.
fn explain(said: &[String]) -> String {
    let text = said
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty() && !line.starts_with("Warning: Permanently added"))
        .collect::<Vec<_>>()
        .join(" ");
    if text.contains("Permission denied") {
        return format!(
            "SSH: {text} Kuery signs in with your SSH agent, ~/.ssh/config or the identity \
             file, and cannot type an SSH password"
        );
    }
    if text.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        return "SSH: the host's key changed since it was last seen (see ~/.ssh/known_hosts); \
                Kuery does not connect"
            .into();
    }
    if text.is_empty() {
        "The SSH tunnel closed without saying why".into()
    } else {
        format!("SSH: {text}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh(host: &str) -> SshSpec {
        SshSpec {
            host: host.into(),
            port: None,
            user: String::new(),
            identity_file: String::new(),
        }
    }

    fn to(host: &str, port: u16) -> Forward {
        Forward::Port {
            host: host.into(),
            port,
        }
    }

    #[test]
    fn forwards_a_local_port_without_asking_anything() {
        let arguments = arguments(&ssh("bastion"), 40001, &to("db.internal", 5432));
        let line = arguments.join(" ");
        assert!(line.contains("-L 127.0.0.1:40001:db.internal:5432"));
        assert!(line.contains("BatchMode=yes"));
        assert!(line.contains("StrictHostKeyChecking=accept-new"));
        assert!(line.ends_with("-- bastion"));
        assert!(!line.contains("-p ") && !line.contains("-l ") && !line.contains("-i "));
    }

    #[test]
    fn passes_port_user_and_identity_when_set() {
        let spec = SshSpec {
            port: Some(2222),
            user: "deploy".into(),
            identity_file: "/keys/id".into(),
            ..ssh("bastion")
        };
        let line = arguments(&spec, 40001, &to("::1", 5432)).join(" ");
        assert!(line.contains("-L 127.0.0.1:40001:[::1]:5432"));
        assert!(line.contains("-p 2222"));
        assert!(line.contains("-l deploy"));
        assert!(line.contains("-i /keys/id -o IdentitiesOnly=yes"));
    }

    #[test]
    fn opens_a_socks_proxy_for_drivers_that_dial_by_name() {
        let line = arguments(&ssh("bastion"), 40001, &Forward::Socks).join(" ");
        assert!(line.contains("-D 127.0.0.1:40001"), "{line}");
        assert!(!line.contains("-L"), "{line}");
        assert!(line.contains("ExitOnForwardFailure=yes"));
        assert!(line.ends_with("-- bastion"));
    }

    #[test]
    fn forwards_to_the_member_the_driver_names() {
        let spec = |driver: &str, host: &str, tls: Option<&str>| ConnectionSpec {
            id: "c".into(),
            name: "c".into(),
            driver: driver.into(),
            host: host.into(),
            port: 27017,
            database: String::new(),
            user: String::new(),
            sslmode: tls.map(str::to_string),
            environment: None,
            ssh: Some(ssh("bastion")),
            options: Default::default(),
        };
        // A member's own port is where the tunnel goes.
        assert_eq!(
            forward_for(&spec("mongodb", "db.internal:27018", None)),
            Ok(to("db.internal", 27018))
        );
        let line = arguments(&ssh("bastion"), 40001, &to("db.internal", 27018)).join(" ");
        assert!(
            line.contains("-L 127.0.0.1:40001:db.internal:27018"),
            "{line}"
        );
        assert_eq!(
            forward_for(&spec("mongodb", "mongo-tls", Some("require"))),
            Ok(Forward::Socks)
        );
        // A host list is refused before ssh starts.
        assert!(
            forward_for(&spec("mongodb", "a:27017,b:27017", None))
                .unwrap_err()
                .starts_with("Through SSH, Kuery reaches a single member")
        );
        // Drivers that say nothing get the host and port as given.
        assert_eq!(
            forward_for(&ConnectionSpec {
                port: 5432,
                ..spec("postgres", "db.internal", None)
            }),
            Ok(to("db.internal", 5432))
        );
    }

    #[test]
    fn explains_why_ssh_gave_up() {
        let said =
            |lines: &[&str]| explain(&lines.iter().map(|l| l.to_string()).collect::<Vec<_>>());
        assert!(said(&["deploy@bastion: Permission denied (publickey)."]).contains("SSH password"));
        assert_eq!(
            said(&["ssh: Could not resolve hostname nowhere: nodename nor servname provided"]),
            "SSH: ssh: Could not resolve hostname nowhere: nodename nor servname provided"
        );
        assert_eq!(said(&[]), "The SSH tunnel closed without saying why");
    }

    #[test]
    fn sees_when_a_port_is_taken() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(listening(port));
        drop(listener);
        assert!(!listening(port));
    }

    /// Needs the dev bastion (`docker compose -f dev/compose.yaml up -d`):
    /// opens a tunnel to its Postgres, reaches it, and closes it on drop.
    #[test]
    #[ignore]
    fn tunnels_to_the_dev_postgres() {
        let spec = SshSpec {
            port: Some(22229),
            user: "kuery".into(),
            ..ssh("localhost")
        };
        let tunnel = Tunnel::open(&spec, &to("postgres", 5432)).expect("opens the tunnel");
        let port = tunnel.local_port();
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());
        drop(tunnel);
        assert!(!listening(port));

        let wrong = SshSpec {
            user: "nobody".into(),
            ..spec
        };
        let error = Tunnel::open(&wrong, &to("postgres", 5432)).err().unwrap();
        assert!(error.contains("Permission denied"), "{error}");
    }
}
