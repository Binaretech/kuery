//! Crash reports, sent to Sentry only when the user says yes.
//!
//! A panic leaves a report next to `panic.log`; on the next start Kuery asks
//! whether to send it. What leaves the machine is the panic message and
//! backtrace with quoted text, passwords, URL credentials and the home folder
//! taken out, plus the version and platform. Builds made without
//! `KUERY_SENTRY_DSN` never send anything.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui_kit::{App, PromptLevel};
use sentry::protocol::{Envelope, Event, Exception, Level, Mechanism};
use sentry::types::Dsn;
use serde::{Deserialize, Serialize};

/// Where reports go, set when the release is built.
pub const DSN: Option<&str> = option_env!("KUERY_SENTRY_DSN");

const QUESTION: &str = "Kuery closed unexpectedly. Send a report?";
const DETAIL: &str = "The report has the error and where in Kuery's code it happened, \
     with the version and platform. Quoted text such as queries and values, passwords and \
     your home folder are left out.";

/// What a panic leaves for the next start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub version: String,
    pub os: String,
    pub arch: String,
    pub thread: String,
    pub message: String,
    pub location: Option<String>,
    pub backtrace: String,
    /// Seconds since the Unix epoch.
    pub time: u64,
}

impl Report {
    pub fn new(message: &str, location: Option<String>, thread: &str, backtrace: &str) -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            thread: thread.into(),
            message: message.into(),
            location,
            backtrace: backtrace.into(),
            time: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs()),
        }
    }
}

/// `crash-report.json` in the config folder.
pub fn pending_path() -> PathBuf {
    crate::paths::config_dir().join("crash-report.json")
}

/// Keeps `report` for the next start, replacing an older one.
pub fn record(path: &Path, report: &Report) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec(report)?)
}

/// The report a crash left, removed so it is offered once.
pub fn take(path: &Path) -> Option<Report> {
    let bytes = std::fs::read(path).ok()?;
    let _ = std::fs::remove_file(path);
    serde_json::from_slice(&bytes).ok()
}

/// The report to offer and where to send it: none when this build has no
/// DSN, or an empty one. The report is removed either way.
fn to_offer<'a>(path: &Path, dsn: Option<&'a str>) -> Option<(Report, &'a str)> {
    let report = take(path)?;
    Some((report, dsn.filter(|dsn| !dsn.trim().is_empty())?))
}

/// Only the first button, "Send Report", sends.
fn accepts(answer: Option<usize>) -> bool {
    answer == Some(0)
}

/// `text` without what could identify the user or their data: the home
/// folder becomes `~`; quoted text, URL credentials and passwords become
/// `<redacted>`. Text outside quotes is kept, so an unquoted value stays.
pub fn scrub(text: &str, home: Option<&Path>) -> String {
    let text = scrub_home(text, home);
    // Quotes first, so a quoted password goes whole.
    let text = redact_quoted(&text);
    let text = redact_url_credentials(&text);
    redact_passwords(&text)
}

const REDACTED: &str = "<redacted>";

/// The user's home folder: `HOME`, or `USERPROFILE` on Windows, where
/// `HOME` is usually unset.
fn home_in(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(env)
        .find(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The home folder becomes `~`, written with either slash and in any case
/// (Windows and macOS paths ignore it), but not as the start of a longer
/// name: `/Users/someone2` stays.
fn scrub_home(text: &str, home: Option<&Path>) -> String {
    let Some(home) = home.map(|home| home.to_string_lossy()) else {
        return text.to_string();
    };
    let home = home.trim_end_matches(['/', '\\']);
    if home.len() < 2 {
        return text.to_string();
    }
    let mut text = text.to_string();
    for home in [home.replace('\\', "/"), home.replace('/', "\\")] {
        text = replace_folder(&text, &home.to_ascii_lowercase());
    }
    text
}

/// `text` with every `folder` (lowercase) replaced by `~`, ignoring ASCII
/// case. Lowercasing ASCII keeps byte offsets, so they index `text` too.
fn replace_folder(text: &str, folder: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut from = 0;
    while let Some(at) = lower[from..].find(folder) {
        let start = from + at;
        let end = start + folder.len();
        from = end;
        let continues = text[end..]
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'));
        if continues {
            continue;
        }
        out.push_str(&text[copied..start]);
        out.push('~');
        copied = end;
    }
    out.push_str(&text[copied..]);
    out
}

/// `scheme://user:password@host` becomes `scheme://<redacted>@host`.
fn redact_url_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("://") {
        let (before, after) = rest.split_at(start + 3);
        out.push_str(before);
        // Up to the last `@` before a space: a password may hold a `/`.
        let end = after.find(char::is_whitespace).unwrap_or(after.len());
        match after[..end].rfind('@') {
            Some(at) => {
                out.push_str(REDACTED);
                rest = &after[at..];
            }
            None => rest = after,
        }
    }
    out.push_str(rest);
    out
}

/// Keys whose value is a secret, as connection strings and options write
/// them.
const SECRET_KEYS: [&str; 9] = [
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "access_key",
    "access_token",
];

/// The value after one of the [`SECRET_KEYS`] and `=` or `:`, as connection
/// strings and messages write it.
fn redact_passwords(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut from = 0;
    while let Some(found) = SECRET_KEYS
        .iter()
        .filter_map(|key| lower[from..].find(key).map(|at| (from + at, key.len())))
        .min()
    {
        let (start, len) = found;
        from = start + len;
        let starts_word = !text[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_');
        let after = &text[from..];
        let spaced = after.trim_start();
        let Some(value) = spaced.strip_prefix(['=', ':']) else {
            continue;
        };
        if !starts_word {
            continue;
        }
        let value = value.trim_start();
        let value_start = text.len() - value.len();
        let value_end = value_start
            + value
                .find(|c: char| c.is_whitespace() || c == ';')
                .unwrap_or(value.len());
        if value_end == value_start {
            continue;
        }
        out.push_str(&text[copied..value_start]);
        out.push_str(REDACTED);
        copied = value_end;
        from = value_end;
    }
    out.push_str(&text[copied..]);
    out
}

/// Text in double quotes, or in single quotes that open after a non-letter
/// (so `can't` stays), becomes `<redacted>`. Backticks stay: Rust's own
/// messages put code in them.
fn redact_quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut previous: Option<char> = None;
    while let Some(c) = chars.next() {
        let opens = c == '"' || (c == '\'' && !previous.is_some_and(char::is_alphanumeric));
        if !opens {
            out.push(c);
            previous = Some(c);
            continue;
        }
        out.push(c);
        out.push_str(REDACTED);
        let mut closed = false;
        while let Some(inner) = chars.next() {
            if inner == '\\' {
                chars.next();
            } else if inner == c {
                closed = true;
                break;
            }
        }
        if closed {
            out.push(c);
        }
        previous = Some(c);
    }
    out
}

/// The Sentry event for `report`, already scrubbed.
pub fn event(report: &Report, home: Option<&Path>) -> Event<'static> {
    let mut stacktrace =
        sentry::integrations::backtrace::parse_stacktrace(&scrub_home(&report.backtrace, home));
    // Oldest first, so the hook's own frames are the last ones.
    for trace in stacktrace.iter_mut() {
        while trace.frames.last().is_some_and(is_panic_machinery) {
            trace.frames.pop();
        }
    }
    for frame in stacktrace
        .iter_mut()
        .flat_map(|trace| trace.frames.iter_mut())
    {
        frame.in_app = Some(
            frame
                .function
                .as_deref()
                .is_some_and(|function| function.starts_with("kuery")),
        );
    }
    let mut event = Event {
        level: Level::Fatal,
        platform: "native".into(),
        timestamp: UNIX_EPOCH + Duration::from_secs(report.time),
        release: Some(format!("kuery@{}", report.version).into()),
        culprit: report
            .location
            .as_deref()
            .map(|location| scrub_home(location, home)),
        exception: vec![Exception {
            ty: "panic".into(),
            value: Some(scrub(&report.message, home)),
            stacktrace,
            mechanism: Some(Mechanism {
                ty: "panic".into(),
                handled: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        }]
        .into(),
        ..Default::default()
    };
    event.tags.insert("os".into(), report.os.clone());
    event.tags.insert("arch".into(), report.arch.clone());
    event
}

/// A frame of the panic itself or of the hook that captured it, not of the
/// code that panicked.
fn is_panic_machinery(frame: &sentry::protocol::Frame) -> bool {
    const PREFIXES: [&str; 8] = [
        "std::backtrace",
        "std::panicking",
        "std::sys::backtrace",
        "core::panicking",
        "__rustc::",
        "<alloc::boxed::Box",
        "kuery::log_panics",
        // Inlined frames that only have a path in the standard library.
        "/rustc/",
    ];
    frame
        .function
        .as_deref()
        .is_none_or(|function| PREFIXES.iter().any(|prefix| function.starts_with(prefix)))
}

/// Sends `report` to `dsn`. Blocking: call it off the UI thread.
pub fn send(dsn: &str, report: &Report) -> Result<(), String> {
    let dsn: Dsn = dsn
        .parse()
        .map_err(|error| format!("invalid DSN: {error}"))?;
    let home = home_in(|name| std::env::var_os(name));
    let envelope = Envelope::from(event(report, home.as_deref()));
    let mut body = Vec::new();
    envelope
        .to_writer(&mut body)
        .map_err(|error| error.to_string())?;
    let auth = dsn.to_auth(Some(concat!("kuery/", env!("CARGO_PKG_VERSION"))));
    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|error| error.to_string())?
        .post(dsn.envelope_api_url().as_str())
        .header("X-Sentry-Auth", auth.to_string())
        .header("Content-Type", "application/x-sentry-envelope")
        .body(body)
        .send()
        .map_err(|error| error.to_string())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("Sentry answered {}", response.status()))
    }
}

/// Asks, in the active window, whether to send the report the last crash
/// left. Called once the windows of a start are open.
pub fn offer(cx: &mut App) {
    // After those windows are on screen: a prompt asked for before is lost.
    cx.defer(|cx| {
        let Some(window) = cx.active_window().or_else(|| cx.windows().last().copied()) else {
            return;
        };
        // Taken only now, so a start without a window keeps it for the next.
        let Some((report, dsn)) = to_offer(&pending_path(), DSN) else {
            return;
        };
        ask(window, report, dsn, cx);
    });
}

fn ask(window: gpui_kit::AnyWindowHandle, report: Report, dsn: &'static str, cx: &mut App) {
    let _ = window.update(cx, |_, window, cx| {
        let answer = window.prompt(
            PromptLevel::Warning,
            QUESTION,
            Some(DETAIL),
            &["Send Report", "Don't Send"],
            cx,
        );
        cx.spawn(async move |_| {
            if accepts(answer.await.ok()) {
                std::thread::spawn(move || {
                    if let Err(error) = send(dsn, &report) {
                        eprintln!("kuery: could not send the crash report: {error}");
                    }
                });
            }
        })
        .detach();
    });
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("kuery-crash-{name}-{}", crate::project::new_id()))
            .join("crash-report.json")
    }

    fn report(message: &str) -> Report {
        Report::new(
            message,
            Some("crates/app/src/grid.rs:10:5".into()),
            "main",
            "   0: kuery::grid::paint\n             at /Users/someone/kuery/crates/app/src/grid.rs:10:5\n   1: kuery::main\n",
        )
    }

    #[test]
    fn a_report_carries_the_version_and_platform() {
        let report = report("boom");
        assert_eq!(report.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(report.os, std::env::consts::OS);
        assert_eq!(report.arch, std::env::consts::ARCH);
        assert!(report.time > 0);
    }

    #[test]
    fn a_recorded_report_is_taken_once() {
        let file = temp_file("take");
        record(&file, &report("first")).unwrap();
        record(&file, &report("second")).unwrap();
        assert_eq!(take(&file).map(|r| r.message), Some("second".into()));
        assert_eq!(take(&file), None);
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn without_a_dsn_nothing_is_offered_and_the_report_is_dropped() {
        let file = temp_file("nodsn");
        record(&file, &report("boom")).unwrap();
        assert_eq!(to_offer(&file, None), None);
        assert!(!file.exists());
        // CI passes an unset secret as an empty value.
        record(&file, &report("boom")).unwrap();
        assert_eq!(to_offer(&file, Some("")), None);
        assert!(!file.exists());
        record(&file, &report("boom")).unwrap();
        let offered = to_offer(&file, Some("https://key@example.com/1"));
        assert_eq!(
            offered.map(|(r, dsn)| (r.message, dsn)),
            Some(("boom".into(), "https://key@example.com/1"))
        );
        assert_eq!(to_offer(&file, Some("https://key@example.com/1")), None);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn only_send_report_sends() {
        assert!(accepts(Some(0)));
        assert!(!accepts(Some(1)));
        assert!(!accepts(None));
    }

    #[test]
    fn scrub_removes_a_windows_home_folder_in_either_slash_and_case() {
        let home = Path::new(r"C:\Users\Name");
        assert_eq!(
            scrub(r"could not read C:\Users\Name\data\x.csv", Some(home)),
            r"could not read ~\data\x.csv"
        );
        assert_eq!(
            scrub("could not read c:/users/name/data/x.csv", Some(home)),
            "could not read ~/data/x.csv"
        );
    }

    #[test]
    fn scrub_only_replaces_the_whole_home_folder() {
        let home = Path::new("/Users/someone");
        assert_eq!(
            scrub("/Users/someone2/x and /Users/someone", Some(home)),
            "/Users/someone2/x and ~"
        );
    }

    #[test]
    fn other_secret_keys_and_colons_are_redacted() {
        assert_eq!(
            scrub(
                "passwd=a1 secret=b2 token=c3 api_key=d4 apikey=e5 access_key=f6",
                None
            ),
            "passwd=<redacted> secret=<redacted> token=<redacted> api_key=<redacted> \
             apikey=<redacted> access_key=<redacted>"
        );
        assert_eq!(
            scrub("auth failed, password: hunter2", None),
            "auth failed, password: <redacted>"
        );
        // A key that only starts a longer word stays.
        assert_eq!(scrub("tokens: 5", None), "tokens: 5");
    }

    #[test]
    fn a_url_password_with_a_slash_is_redacted() {
        assert_eq!(
            scrub("connect postgres://admin:pa/ss@db:5432/shop failed", None),
            "connect postgres://<redacted>@db:5432/shop failed"
        );
    }

    #[test]
    fn a_quoted_password_is_removed_whole() {
        assert_eq!(
            scrub(r#"Server=db;Password="my pass";User=sa"#, None),
            r#"Server=db;Password=<redacted>;User=sa"#
        );
    }

    #[test]
    fn the_home_folder_comes_from_home_or_userprofile() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| std::ffi::OsString::from(value))
            }
        };
        assert_eq!(
            home_in(env(&[("HOME", "/Users/a"), ("USERPROFILE", r"C:\Users\b")])),
            Some(PathBuf::from("/Users/a"))
        );
        assert_eq!(
            home_in(env(&[("USERPROFILE", r"C:\Users\b")])),
            Some(PathBuf::from(r"C:\Users\b"))
        );
        assert_eq!(home_in(env(&[("HOME", "")])), None);
    }

    #[test]
    fn scrub_removes_the_home_folder_quotes_and_credentials() {
        let home = Path::new("/Users/someone");
        assert_eq!(
            scrub("could not open /Users/someone/data/sales.csv", Some(home)),
            "could not open ~/data/sales.csv"
        );
        assert_eq!(
            scrub(
                r#"Db("syntax error at or near \"selec\"") in 'select 1'"#,
                Some(home)
            ),
            r#"Db("<redacted>") in '<redacted>'"#
        );
        assert_eq!(
            scrub(
                "connect postgres://admin:hunter2@db.example.com:5432/shop failed",
                None
            ),
            "connect postgres://<redacted>@db.example.com:5432/shop failed"
        );
        assert_eq!(
            scrub(
                "host=db user=sa password=hunter2;port=1433 PWD = secret",
                None
            ),
            "host=db user=sa password=<redacted>;port=1433 PWD = <redacted>"
        );
        // Apostrophes in words and Rust's own backticks stay.
        assert_eq!(
            scrub("can't call `Option::unwrap()` on a `None` value", None),
            "can't call `Option::unwrap()` on a `None` value"
        );
    }

    #[test]
    fn the_stack_trace_starts_where_the_panic_happened() {
        // The frames above the panic are the panic hook capturing it.
        let backtrace = "   0: std::backtrace_rs::backtrace::libunwind::trace
             at /rustc/abc/library/std/src/../../backtrace/src/backtrace/libunwind.rs:117:9
   1: std::backtrace::Backtrace::create
             at /rustc/abc/library/std/src/backtrace.rs:331:13
   2: kuery::log_panics::{closure#0}
             at ./crates/app/src/main.rs:218:25
   3: std::panicking::panic_with_hook
             at /rustc/abc/library/std/src/panicking.rs:823:13
   4: __rustc::rust_begin_unwind
             at /rustc/abc/library/std/src/panicking.rs:679:5
   5: core::panicking::panic_fmt
             at /rustc/abc/library/core/src/panicking.rs:80:14
   6: kuery::grid::paint
             at ./crates/app/src/grid.rs:10:5
   7: kuery::main
             at ./crates/app/src/main.rs:108:10
";
        let report = Report::new("boom", None, "main", backtrace);
        let event = event(&report, None);
        let frames = &event.exception.values[0]
            .stacktrace
            .as_ref()
            .unwrap()
            .frames;
        // Oldest first: the innermost frame is the last one.
        let functions: Vec<_> = frames
            .iter()
            .filter_map(|f| f.function.as_deref())
            .collect();
        assert_eq!(functions, ["kuery::main", "kuery::grid::paint"]);
        assert!(frames.iter().all(|f| f.in_app == Some(true)));
    }

    #[test]
    fn the_event_is_a_scrubbed_fatal_panic_of_this_release() {
        let home = Path::new("/Users/someone");
        let event = event(&report(r#"bad value "4111 1111""#), Some(home));
        assert_eq!(event.level, Level::Fatal);
        assert_eq!(
            event.release.as_deref(),
            Some(concat!("kuery@", env!("CARGO_PKG_VERSION")))
        );
        assert_eq!(
            event.tags.get("os").map(String::as_str),
            Some(std::env::consts::OS)
        );
        assert_eq!(
            event.tags.get("arch").map(String::as_str),
            Some(std::env::consts::ARCH)
        );
        assert_eq!(event.server_name, None);
        assert_eq!(event.user, None);
        let exception = &event.exception.values[0];
        assert_eq!(exception.ty, "panic");
        assert_eq!(
            exception.value.as_deref(),
            Some(r#"bad value "<redacted>""#)
        );
        let frames = &exception.stacktrace.as_ref().unwrap().frames;
        assert!(
            frames
                .iter()
                .any(|f| f.function.as_deref() == Some("kuery::grid::paint"))
        );
        let text = serde_json::to_string(&event).unwrap();
        assert!(!text.contains("someone"), "{text}");
        assert!(!text.contains("4111"), "{text}");
    }

    /// Answers one request with `response`, after reading all of it (an
    /// unread body could reset the connection before the client sees the
    /// answer), and returns its header lines and body.
    fn serve_once(
        response: &'static [u8],
    ) -> (u16, std::thread::JoinHandle<(Vec<String>, String)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = Vec::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
                head.push(line);
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            stream.write_all(response).unwrap();
            (head, String::from_utf8(body).unwrap())
        });
        (port, server)
    }

    #[test]
    fn send_posts_the_event_to_the_projects_envelope_endpoint() {
        let (port, server) = serve_once(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}");
        let dsn = format!("http://publickey@127.0.0.1:{port}/42");
        send(&dsn, &report(r#"bad "secret""#)).unwrap();

        let (head, body) = server.join().unwrap();
        assert!(head[0].starts_with("POST /api/42/envelope/ "), "{head:?}");
        assert!(
            head.iter()
                .any(|h| h.to_ascii_lowercase().starts_with("x-sentry-auth:")
                    && h.contains("sentry_key=publickey")),
            "{head:?}"
        );
        assert!(
            body.contains(concat!("kuery@", env!("CARGO_PKG_VERSION"))),
            "{body}"
        );
        assert!(
            body.contains("<redacted>") && !body.contains("secret"),
            "{body}"
        );
    }

    #[test]
    fn send_reports_a_rejected_report() {
        let (port, server) = serve_once(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\n\r\n");
        let result = send(
            &format!("http://publickey@127.0.0.1:{port}/42"),
            &report("boom"),
        );
        server.join().unwrap();
        assert!(result.unwrap_err().contains("403"));
    }

    #[test]
    fn an_invalid_dsn_is_an_error() {
        assert!(send("not a dsn", &report("boom")).is_err());
    }
}
