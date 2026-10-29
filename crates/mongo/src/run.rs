//! Running MongoDB operations from the session's thread without ever
//! dropping one of the driver's futures mid-operation.
//!
//! Every operation is spawned on the driver's runtime, and the session's
//! thread waits for its task. A cancel or a timeout does not drop it: the
//! operation is stopped on the server (`killOp` on the operation carrying
//! its comment tag) and its task is still awaited, for a grace period,
//! until it ends with `Interrupted`. Only when the grace runs out is the
//! task left to finish on its own, and the error says so.

use std::future::Future;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bson::{Document, doc};
use kuery_core::session::QueryError;
use mongodb::error::{ErrorKind, WriteFailure};
use mongodb::{Client, Cursor};
use tokio::sync::{Notify, mpsc};
use tokio::task::{JoinError, JoinHandle};
use tokio::time::Instant;

use crate::{message, runtime};

/// How long a stopped operation has to end, killing it included; it ends
/// in milliseconds where the server could stop it.
const GRACE: Duration = Duration::from_secs(5);

/// How often a stopped operation is looked for again on the server: the
/// first look may come before it got there (while the client picks a
/// server, waits for a connection or signs in).
const KILL_AGAIN: Duration = Duration::from_millis(250);

/// How long documents read wait for more before they are sent on.
const FLUSH_AFTER: Duration = Duration::from_millis(50);

/// What an operation's error says when the grace ran out.
pub(crate) const DETACHED: &str =
    "Cancelling only stopped waiting: the operation may still run on the server";

/// `kuery:<process>:<adapter>:<command>`, added as `comment` to a command
/// (and to its `getMore`s), so the server's list of running operations
/// finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tag(String);

impl Tag {
    pub(crate) fn new(adapter: u64, command: u64) -> Tag {
        Tag(format!("{}{command}", Tag::prefix(adapter)))
    }

    /// What every tag of `adapter` starts with.
    pub(crate) fn prefix(adapter: u64) -> String {
        format!("kuery:{}:{adapter}:", process())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Tells this process's tags from another's: two Kuery instances signed
/// in as one user number their adapters and commands alike, and a cancel
/// in one must not kill the other's operation.
fn process() -> &'static str {
    static PROCESS: OnceLock<String> = OnceLock::new();
    // Five of an ObjectId's bytes are random for each process.
    PROCESS.get_or_init(|| bson::oid::ObjectId::new().to_hex())
}

/// A cancel request for the operation that runs now: the canceller's side
/// sets it from any thread.
#[derive(Default)]
pub(crate) struct Stop {
    running: AtomicBool,
    requested: AtomicBool,
    notify: Notify,
}

impl Stop {
    fn start(&self) {
        self.requested.store(false, Ordering::SeqCst);
        self.running.store(true, Ordering::SeqCst);
    }

    fn end(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Stops the running operation; nothing when none runs.
    pub(crate) fn request(&self) {
        if self.running.load(Ordering::SeqCst) {
            self.requested.store(true, Ordering::SeqCst);
            self.notify.notify_waiters();
        }
    }

    async fn requested(&self) {
        loop {
            // Made before the check, so a request in between still wakes it.
            let notified = self.notify.notified();
            if self.requested.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

/// Why waiting stopped before the operation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stopped {
    Cancelled,
    TimedOut,
}

impl Stopped {
    fn error(self) -> QueryError {
        match self {
            Stopped::Cancelled => QueryError::Cancelled,
            Stopped::TimedOut => timed_out(),
        }
    }
}

fn timed_out() -> QueryError {
    QueryError::Database {
        code: "50".into(),
        message: "Took longer than the time limit".into(),
    }
}

/// Runs `op` on the runtime and waits for it from the session's thread.
/// A stop or `timeout` kills the operation on the server by its tag and
/// keeps waiting for the task (see [`GRACE`]) before leaving it.
pub(crate) fn wait<T: Send + 'static>(
    client: &Client,
    tag: &Tag,
    stop: &Stop,
    timeout: Option<Duration>,
    op: impl Future<Output = mongodb::error::Result<T>> + Send + 'static,
) -> Result<T, QueryError> {
    stop.start();
    let ended = runtime().block_on(async {
        let mut task = runtime().spawn(op);
        let stopped = tokio::select! {
            joined = &mut task => return joined_result(joined),
            () = stop.requested() => Stopped::Cancelled,
            () = elapse(timeout) => Stopped::TimedOut,
        };
        Err(halt(client, tag, task, stopped).await)
    });
    stop.end();
    ended
}

/// Runs a command that answers with a cursor and sends what it reads to
/// `sink`, as it arrives. The cursor lives in a task on the runtime that
/// hands documents over a channel; when the sink refuses more (its reader
/// went away), the task is told to stop after the read it is in, which
/// closes the cursor on the server. Returns how many documents it read.
pub(crate) fn stream(
    client: &Client,
    tag: &Tag,
    stop: &Stop,
    timeout: Option<Duration>,
    flush: usize,
    open: impl Future<Output = mongodb::error::Result<Cursor<Document>>> + Send + 'static,
    sink: &mut dyn FnMut(Vec<Document>) -> Result<(), QueryError>,
) -> Result<u64, QueryError> {
    stop.start();
    let ended = stream_on(client, tag, stop, timeout, flush, open, sink);
    stop.end();
    ended
}

fn stream_on(
    client: &Client,
    tag: &Tag,
    stop: &Stop,
    timeout: Option<Duration>,
    flush: usize,
    open: impl Future<Output = mongodb::error::Result<Cursor<Document>>> + Send + 'static,
    sink: &mut dyn FnMut(Vec<Document>) -> Result<(), QueryError>,
) -> Result<u64, QueryError> {
    let (tx, mut rx) = mpsc::channel(1);
    let mut task = runtime().spawn(read(open, flush.max(1), tx));
    let deadline = timeout.map(|timeout| tokio::time::Instant::now() + timeout);
    loop {
        let next = runtime().block_on(async {
            tokio::select! {
                batch = rx.recv() => Ok(batch),
                () = stop.requested() => Err(Stopped::Cancelled),
                () = elapse_until(deadline) => Err(Stopped::TimedOut),
            }
        });
        match next {
            Ok(Some(batch)) => {
                if let Err(error) = sink(batch) {
                    // The task ends at its next send.
                    drop(rx);
                    runtime().block_on(halt(client, tag, task, Stopped::Cancelled));
                    return Err(error);
                }
            }
            // The task is done sending: it ended, well or not.
            Ok(None) => return runtime().block_on(async { joined_result((&mut task).await) }),
            Err(stopped) => {
                drop(rx);
                return Err(runtime().block_on(halt(client, tag, task, stopped)));
            }
        }
    }
}

/// The cursor's side: reads documents and sends them in groups of `flush`,
/// or sooner when the next read keeps them waiting. The read in flight is
/// never dropped: a reader that went away is noticed after it.
async fn read(
    open: impl Future<Output = mongodb::error::Result<Cursor<Document>>> + Send,
    flush: usize,
    tx: mpsc::Sender<Vec<Document>>,
) -> mongodb::error::Result<u64> {
    let mut cursor = open.await?;
    let mut pending = Vec::new();
    let mut read = 0;
    let mut gone = false;
    loop {
        let more = {
            let advance = cursor.advance();
            tokio::pin!(advance);
            loop {
                tokio::select! {
                    more = &mut advance => break more?,
                    () = tokio::time::sleep(FLUSH_AFTER), if !pending.is_empty() && !gone => {
                        gone = tx.send(std::mem::take(&mut pending)).await.is_err();
                    }
                }
            }
        };
        if gone {
            return Ok(read);
        }
        if !more {
            break;
        }
        pending.push(cursor.deserialize_current()?);
        read += 1;
        if pending.len() >= flush && tx.send(std::mem::take(&mut pending)).await.is_err() {
            return Ok(read);
        }
    }
    if !pending.is_empty() {
        let _ = tx.send(pending).await;
    }
    Ok(read)
}

/// Stops an operation whose wait ended early: kills it on the server, and
/// again every [`KILL_AGAIN`] until its task ends, for the grace period at
/// most, however long the server takes to answer. What the caller reports.
async fn halt<T: Send + 'static>(
    client: &Client,
    tag: &Tag,
    mut task: JoinHandle<mongodb::error::Result<T>>,
    stopped: Stopped,
) -> QueryError {
    let deadline = Instant::now() + GRACE;
    loop {
        let again = Instant::now() + KILL_AGAIN;
        let killed = kill(client, tag);
        tokio::select! {
            joined = &mut task => return ended(joined, stopped),
            () = async {
                let _ = killed.await;
                tokio::time::sleep_until(again).await;
            } => {}
            // Dropping the handles leaves the tasks running: neither is
            // dropped.
            () = tokio::time::sleep_until(deadline) => {
                return QueryError::Unsupported(DETACHED.into());
            }
        }
    }
}

/// What a stopped operation reports once its task ended: the stop, when
/// the server interrupted it or it finished anyway; its own error
/// otherwise, such as a connection that broke.
fn ended<T>(joined: Result<mongodb::error::Result<T>, JoinError>, stopped: Stopped) -> QueryError {
    match joined_result(joined) {
        Ok(_) | Err(QueryError::Cancelled) => stopped.error(),
        Err(error) => error,
    }
}

/// Kills, on the server, the operations carrying `tag`: the command, or a
/// `getMore` of its cursor. A user may kill its own operations without the
/// `killop` privilege. Failing to is not an error: the wait goes on.
fn kill(client: &Client, tag: &Tag) -> JoinHandle<()> {
    let admin = client.database("admin");
    let tag = tag.as_str().to_string();
    runtime().spawn(async move {
        let killing = async {
            let found = admin
                .run_command(doc! {
                    "aggregate": 1,
                    "pipeline": [
                        { "$currentOp": {} },
                        { "$match": { "$or": [
                            { "command.comment": &tag },
                            { "cursor.originatingCommand.comment": &tag },
                        ] } },
                        { "$project": { "opid": 1 } },
                    ],
                    "cursor": {},
                    "maxTimeMS": 5000,
                })
                .await?;
            let operations = found
                .get_document("cursor")
                .and_then(|cursor| cursor.get_array("firstBatch"))
                .cloned()
                .unwrap_or_default();
            for operation in operations {
                if let Some(opid) = operation.as_document().and_then(|op| op.get("opid")) {
                    admin
                        .run_command(doc! { "killOp": 1, "op": opid.clone() })
                        .await?;
                }
            }
            Ok::<_, mongodb::error::Error>(())
        };
        let _ = killing.await;
    })
}

async fn elapse(timeout: Option<Duration>) {
    match timeout {
        Some(timeout) => tokio::time::sleep(timeout).await,
        None => std::future::pending().await,
    }
}

async fn elapse_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn joined_result<T>(
    joined: Result<mongodb::error::Result<T>, tokio::task::JoinError>,
) -> Result<T, QueryError> {
    match joined {
        Ok(outcome) => outcome.map_err(error),
        Err(join) => Err(QueryError::Connection(format!(
            "The MongoDB operation failed: {join}"
        ))),
    }
}

/// A driver error as the session reports it: an interrupted operation is
/// cancelled, one out of time says so, the server's own text otherwise.
pub(crate) fn error(error: mongodb::error::Error) -> QueryError {
    let code = match &*error.kind {
        ErrorKind::Command(command) => Some(command.code),
        ErrorKind::Write(WriteFailure::WriteError(failure)) => Some(failure.code),
        ErrorKind::Write(WriteFailure::WriteConcernError(failure)) => Some(failure.code),
        _ => None,
    };
    match (code, &*error.kind) {
        // Interrupted: killed with `killOp`.
        (Some(11601), _) => QueryError::Cancelled,
        (Some(50), _) => timed_out(),
        (Some(code), _) => QueryError::Database {
            code: code.to_string(),
            message: message(&error),
        },
        (
            None,
            ErrorKind::Io(_)
            | ErrorKind::ConnectionPoolCleared { .. }
            | ErrorKind::ServerSelection { .. }
            | ErrorKind::Shutdown,
        ) => QueryError::Connection(message(&error)),
        (None, _) => QueryError::Database {
            code: String::new(),
            message: message(&error),
        },
    }
}

/// A command's first key: its name.
pub(crate) fn name(command: &Document) -> &str {
    command.keys().next().map_or("", String::as_str)
}

/// A command's `cursor.firstBatch`, for commands run without a cursor.
#[cfg(test)]
pub(crate) fn first_batch(answer: &Document) -> Vec<Document> {
    answer
        .get_document("cursor")
        .and_then(|cursor| cursor.get_array("firstBatch"))
        .map(|batch| {
            batch
                .iter()
                .filter_map(bson::Bson::as_document)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use mongodb::error::{CommandError, Error};

    use super::*;

    fn server(code: i32, message: &str) -> Error {
        let error: CommandError = bson::deserialize_from_document(doc! {
            "code": code,
            "codeName": "Some",
            "errmsg": message,
        })
        .unwrap();
        Error::from(ErrorKind::Command(error))
    }

    #[test]
    fn reads_server_errors_by_their_code() {
        assert_eq!(error(server(11601, "interrupted")), QueryError::Cancelled);
        assert_eq!(
            error(server(50, "operation exceeded time limit")),
            timed_out()
        );
        assert_eq!(
            error(server(13, "not authorized on shop to execute command")),
            QueryError::Database {
                code: "13".into(),
                message: "not authorized on shop to execute command".into()
            }
        );
    }

    #[test]
    fn tags_name_the_process_the_adapter_and_the_command() {
        let process = process();
        assert_eq!(process.len(), 24, "{process}");
        assert_eq!(Tag::new(3, 14).as_str(), format!("kuery:{process}:3:14"));
        assert!(Tag::new(3, 14).as_str().starts_with(&Tag::prefix(3)));
        assert!(!Tag::new(31, 4).as_str().starts_with(&Tag::prefix(3)));
        assert_eq!(name(&doc! { "find": "orders", "filter": {} }), "find");
        assert_eq!(
            first_batch(&doc! { "cursor": { "firstBatch": [{ "a": 1 }], "id": 0_i64 } }),
            [doc! { "a": 1 }]
        );
    }

    #[test]
    fn a_stopped_operation_reports_the_stop_or_its_own_failure() {
        let interrupted = || Ok(Err::<(), _>(server(11601, "operation was interrupted")));
        assert_eq!(
            ended(interrupted(), Stopped::Cancelled),
            QueryError::Cancelled
        );
        assert_eq!(ended(interrupted(), Stopped::TimedOut), timed_out());
        // It finished just as it was stopped.
        assert_eq!(ended(Ok(Ok(())), Stopped::Cancelled), QueryError::Cancelled);
        // Nothing was killed: the connection broke under it.
        let broken = Error::from(ErrorKind::Io(std::sync::Arc::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "connection reset",
        ))));
        assert_eq!(
            ended(Ok(Err::<(), _>(broken)), Stopped::Cancelled),
            QueryError::Connection("connection reset".into())
        );
    }

    #[test]
    fn a_stop_only_reaches_what_runs() {
        let stop = Stop::default();
        stop.request();
        stop.start();
        assert!(!stop.requested.load(Ordering::SeqCst), "kept from before");
        stop.request();
        assert!(stop.requested.load(Ordering::SeqCst));
        stop.end();
        stop.start();
        assert!(!stop.requested.load(Ordering::SeqCst), "reached the next");
    }
}
