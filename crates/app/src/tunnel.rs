//! SSH tunnels: a connection can reach its server through an SSH host (a
//! bastion), the server's host and port as that SSH host sees them.
//!
//! The tunnel is the system's own `ssh` forwarding a local port, so it
//! follows `~/.ssh/config` (host aliases, `ProxyJump`, keys), the SSH agent
//! and `known_hosts`. It never asks anything: authentication is by key or
//! agent (`BatchMode`), a host seen for the first time is remembered and a
//! host whose key changed is refused (`StrictHostKeyChecking=accept-new`).
//! The tunnel lives as long as the connection that opened it.

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::project::{ConnectionSpec, SshSpec};

/// How long `ssh` gets to connect, authenticate and start forwarding.
const START: Duration = Duration::from_secs(20);

/// A running `ssh` that forwards `127.0.0.1:<local_port>` to the server.
pub struct Tunnel {
    child: Child,
    local_port: u16,
}

impl Tunnel {
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// Starts `ssh` and waits until it forwards, or says why it could not.
    /// Blocks: call it off the UI thread (see [`open_for`]).
    pub fn open(ssh: &SshSpec, host: &str, port: u16) -> Result<Tunnel, String> {
        let local_port = free_port()?;
        let mut command = Command::new("ssh");
        command
            .args(arguments(ssh, local_port, host, port))
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
                return Ok(Tunnel { child, local_port });
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

/// Opens the tunnel a connection asks for, if any, off the calling thread.
pub async fn open_for(spec: &ConnectionSpec) -> Result<Option<Arc<Tunnel>>, String> {
    let Some(ssh) = spec.ssh.clone() else {
        return Ok(None);
    };
    let (host, port) = (spec.host.clone(), spec.port);
    let (send, receive) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = send.send(Tunnel::open(&ssh, &host, port));
    });
    match receive.await {
        Ok(result) => result.map(|tunnel| Some(Arc::new(tunnel))),
        Err(_) => Err("The SSH tunnel stopped before it started".into()),
    }
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

/// `ssh`'s arguments: forward the local port to `host:port`, no command, no
/// questions.
fn arguments(ssh: &SshSpec, local_port: u16, host: &str, port: u16) -> Vec<String> {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
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
        "-L",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    arguments.push(format!("127.0.0.1:{local_port}:{host}:{port}"));
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

    #[test]
    fn forwards_a_local_port_without_asking_anything() {
        let arguments = arguments(&ssh("bastion"), 40001, "db.internal", 5432);
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
        let line = arguments(&spec, 40001, "::1", 5432).join(" ");
        assert!(line.contains("-L 127.0.0.1:40001:[::1]:5432"));
        assert!(line.contains("-p 2222"));
        assert!(line.contains("-l deploy"));
        assert!(line.contains("-i /keys/id -o IdentitiesOnly=yes"));
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
        let tunnel = Tunnel::open(&spec, "postgres", 5432).expect("opens the tunnel");
        let port = tunnel.local_port();
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());
        drop(tunnel);
        assert!(!listening(port));

        let wrong = SshSpec {
            user: "nobody".into(),
            ..spec
        };
        let error = Tunnel::open(&wrong, "postgres", 5432).err().unwrap();
        assert!(error.contains("Permission denied"), "{error}");
    }
}
