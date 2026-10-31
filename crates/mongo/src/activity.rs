//! MongoDB server activity: the operations `$currentOp` lists, of every
//! user where the user may see them (the `inprog` privilege), else its own
//! with a notice, and `killOp` to stop one. MongoDB stops operations, not
//! sessions, so there is no session to end; and it does not say who holds
//! the lock an operation waits for, only that it waits.

use bson::{Bson, Document as BsonDocument, doc};
use kuery_core::activity::{Activity, Backend, BackendId, Signal, State};
use kuery_core::session::QueryError;

use crate::Mongo;
use crate::run::{self, Tag};

/// What the list says when it holds only the user's own operations.
const OWN_ONLY: &str =
    "Only this user's operations: seeing every user's needs the inprog privilege";

/// The server's code for a command the user may not run.
const UNAUTHORIZED: &str = "13";

/// Command fields every operation carries, which say nothing about what it
/// does.
const NOISE: &[&str] = &[
    "lsid",
    "$clusterTime",
    "$db",
    "$readPreference",
    "$audit",
    "$client",
    "txnNumber",
    "autocommit",
    "startTransaction",
];

impl Mongo {
    /// What the server runs now (see the module).
    pub(crate) fn activity_now(&mut self) -> Result<Activity, QueryError> {
        let own = Tag::prefix(self.id);
        match self.current_ops(true, None) {
            Ok(ops) => Ok(activity(&ops, &own, None)),
            Err(error) if unauthorized(&error) => match self.current_ops(false, None) {
                Ok(ops) => Ok(activity(&ops, &own, Some(OWN_ONLY))),
                Err(error) if unauthorized(&error) => Err(QueryError::Unsupported(
                    "This user may not list the server's running operations".into(),
                )),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        }
    }

    /// Stops operation `id` with `killOp`; `false` when it was gone. Only
    /// [`Signal::Cancel`]: an operation has no session of its own to end.
    pub(crate) fn kill_operation(
        &mut self,
        id: &BackendId,
        signal: Signal,
    ) -> Result<bool, QueryError> {
        if signal == Signal::Terminate {
            return Err(QueryError::Unsupported(
                "MongoDB stops operations, not sessions: stop the operation instead".into(),
            ));
        }
        let opid = opid(id);
        // `killOp` answers the same whether or not the operation exists.
        let found = match self.current_ops(true, Some(&opid)) {
            Err(error) if unauthorized(&error) => self.current_ops(false, Some(&opid))?,
            found => found?,
        };
        if found.is_empty() {
            return Ok(false);
        }
        match self.kill_op(opid) {
            Err(error) if unauthorized(&error) => Err(QueryError::Unsupported(
                "This user may not stop other users' operations: that needs the killop privilege"
                    .into(),
            )),
            killed => killed.map(|_| true),
        }
    }

    /// The operations `$currentOp` lists: every user's or the user's own,
    /// all of them or the one numbered `opid`. Idle connections and idle
    /// sessions are left out: there is nothing in them to stop.
    fn current_ops(
        &mut self,
        all_users: bool,
        opid: Option<&Bson>,
    ) -> Result<Vec<BsonDocument>, QueryError> {
        let mut pipeline = vec![Bson::Document(doc! {
            "$currentOp": {
                "allUsers": all_users,
                "idleConnections": false,
                "idleSessions": false,
            },
        })];
        if let Some(opid) = opid {
            pipeline.push(Bson::Document(doc! { "$match": { "opid": opid.clone() } }));
        }
        let mut ops = Vec::new();
        self.cursor(
            "admin",
            doc! { "aggregate": 1, "pipeline": pipeline, "cursor": {} },
            &mut |batch| {
                ops.extend(batch);
                Ok(())
            },
        )?;
        Ok(ops)
    }

    /// Sends `killOp`, which [`crate::wire::guard`] does not let a
    /// read-only connection send: it stops an operation and changes no
    /// data, and only the activity monitor sends it, never a statement.
    fn kill_op(&mut self, opid: Bson) -> Result<BsonDocument, QueryError> {
        self.commands += 1;
        let tag = Tag::new(self.id, self.commands);
        let admin = self.pool.client.database("admin");
        let command = doc! { "killOp": 1, "op": opid, "comment": tag.as_str() };
        run::wait(
            &self.pool.client,
            &tag,
            &self.stop,
            self.patience(),
            async move { admin.run_command(command).await },
        )
    }
}

fn unauthorized(error: &QueryError) -> bool {
    matches!(error, QueryError::Database { code, .. } if code == UNAUTHORIZED)
}

/// An operation's id as the server takes it back: a number from a
/// `mongod`, text (`shard01:4521`) from a `mongos`.
fn opid(id: &BackendId) -> Bson {
    match id.number() {
        Some(number) => match i32::try_from(number) {
            Ok(small) => Bson::Int32(small),
            Err(_) => Bson::Int64(number),
        },
        None => Bson::String(id.0.clone()),
    }
}

/// The listed operations as sessions of an [`Activity`]; `own` is what the
/// tags of the monitor's own commands start with.
fn activity(ops: &[BsonDocument], own: &str, notice: Option<&str>) -> Activity {
    Activity {
        backends: ops.iter().filter_map(|op| backend(op, own)).collect(),
        max_connections: None,
        notice: notice.map(str::to_string),
    }
}

/// One operation; `None` without an id, which `killOp` could not stop.
fn backend(op: &BsonDocument, own: &str) -> Option<Backend> {
    let id = match op.get("opid")? {
        Bson::Int32(n) => n.to_string(),
        Bson::Int64(n) => n.to_string(),
        Bson::String(text) => text.clone(),
        _ => return None,
    };
    let ns = op.get_str("ns").unwrap_or_default();
    let command = what_it_runs(op);
    let comment = command
        .and_then(|command| command.get_str("comment").ok())
        .unwrap_or_default();
    let client = op
        .get_str("client")
        .or_else(|_| op.get_str("client_s"))
        .unwrap_or_default();
    let locked = op.get_bool("waitingForLock").unwrap_or(false);
    let state = match (
        op.get_bool("active").unwrap_or(false),
        op.get_bool("killPending"),
    ) {
        (_, Ok(true)) => State::Other("stopping".into()),
        (true, _) => State::Active,
        (false, _) => State::Idle,
    };
    let wait = match (locked, op.get_bool("waitingForFlowControl")) {
        (true, _) => Some("Lock".to_string()),
        (false, Ok(true)) => Some("Flow control".to_string()),
        _ => None,
    };
    let users: Vec<&str> = op
        .get_array("effectiveUsers")
        .map(|users| {
            users
                .iter()
                .filter_map(|user| user.as_document()?.get_str("user").ok())
                .collect()
        })
        .unwrap_or_default();
    Some(Backend {
        id: BackendId(id),
        user: users.join(", "),
        application: op.get_str("appName").unwrap_or_default().to_string(),
        client: client.to_string(),
        database: match ns.split_once('.') {
            Some((database, _)) => database.to_string(),
            None => ns.to_string(),
        },
        state,
        wait,
        query_seconds: micros(op, "microsecs_running").or_else(|| number(op.get("secs_running")?)),
        transaction_seconds: op
            .get_document("transaction")
            .ok()
            .and_then(|transaction| micros(transaction, "timeOpenMicros")),
        query: command.map(shown).unwrap_or_default(),
        blocked_by: Vec::new(),
        kind: match client.is_empty() {
            false => "client backend".to_string(),
            true => op.get_str("desc").unwrap_or("background").to_string(),
        },
        own: comment.starts_with(own),
        waiting_for: (locked && !ns.is_empty()).then(|| format!("{ns} (lock)")),
    })
}

/// The command an operation runs: for a `getMore`, the one that opened its
/// cursor, which says what it reads.
fn what_it_runs(op: &BsonDocument) -> Option<&BsonDocument> {
    let originating = op
        .get_document("cursor")
        .ok()
        .and_then(|cursor| cursor.get_document("originatingCommand").ok());
    match op.get_str("op") {
        Ok("getmore") => originating.or_else(|| op.get_document("command").ok()),
        _ => op.get_document("command").ok(),
    }
}

/// A command as relaxed Extended JSON, without the fields every command
/// carries, and without Kuery's own tag. One too long for the server to
/// list comes as its truncated text.
fn shown(command: &BsonDocument) -> String {
    if let Ok(truncated) = command.get_str("$truncated") {
        return truncated.to_string();
    }
    let mut command = command.clone();
    for field in NOISE {
        command.remove(field);
    }
    if command
        .get_str("comment")
        .is_ok_and(|comment| comment.starts_with("kuery:"))
    {
        command.remove("comment");
    }
    Bson::Document(command).into_relaxed_extjson().to_string()
}

fn micros(document: &BsonDocument, field: &str) -> Option<f64> {
    number(document.get(field)?).map(|micros| micros / 1e6)
}

fn number(value: &Bson) -> Option<f64> {
    match value {
        Bson::Int32(n) => Some(f64::from(*n)),
        Bson::Int64(n) => Some(*n as f64),
        Bson::Double(n) => Some(*n),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN: &str = "kuery:5f3a:3:";

    /// A `find` of another user waiting for a lock, recorded from
    /// `$currentOp` on a replica set.
    fn waiting_find() -> BsonDocument {
        doc! {
            "type": "op",
            "host": "localhost:27019",
            "desc": "conn42",
            "connectionId": 42,
            "client": "172.18.0.1:53814",
            "appName": "orders-api",
            "clientMetadata": { "driver": { "name": "nodejs", "version": "6.9.0" } },
            "active": true,
            "currentOpTime": "2026-10-31T10:12:03.441+00:00",
            "effectiveUsers": [{ "user": "orders", "db": "admin" }],
            "threaded": true,
            "opid": 18_204,
            "lsid": { "id": { "$binary": { "base64": "AAAA", "subType": "04" } } },
            "secs_running": 12_i64,
            "microsecs_running": 12_480_211_i64,
            "op": "query",
            "ns": "shop.orders",
            "command": {
                "find": "orders",
                "filter": { "status": "paid" },
                "lsid": { "id": 1 },
                "$clusterTime": { "clusterTime": 1 },
                "$db": "shop",
            },
            "numYields": 0,
            "waitingForLock": true,
            "waitingForFlowControl": false,
            "transaction": { "parameters": { "txnNumber": 3_i64 }, "timeOpenMicros": 30_000_000_i64 },
        }
    }

    #[test]
    fn reads_an_operation_from_current_op() {
        let read = activity(&[waiting_find()], OWN, None);
        let [find] = read.backends.as_slice() else {
            panic!("{read:?}");
        };
        assert_eq!(find.id, BackendId("18204".into()));
        assert_eq!(find.user, "orders");
        assert_eq!(find.application, "orders-api");
        assert_eq!(find.client, "172.18.0.1:53814");
        assert_eq!(find.database, "shop");
        assert_eq!(find.state, State::Active);
        assert_eq!(find.wait.as_deref(), Some("Lock"));
        assert_eq!(find.waiting_for.as_deref(), Some("shop.orders (lock)"));
        assert_eq!(find.query_seconds, Some(12.480211));
        assert_eq!(find.transaction_seconds, Some(30.));
        assert_eq!(
            find.query,
            r#"{"find":"orders","filter":{"status":"paid"}}"#
        );
        assert!(find.is_client() && !find.own);
        assert!(find.blocked_by.is_empty());
        assert_eq!(read.notice, None);
    }

    #[test]
    fn shows_what_a_get_more_reads_and_knows_its_own() {
        let get_more = doc! {
            "type": "op",
            "client": "127.0.0.1:61000",
            "active": true,
            "opid": 77,
            "op": "getmore",
            "ns": "shop.orders",
            "command": { "getMore": 9_i64, "collection": "orders", "comment": "kuery:5f3a:3:12" },
            "cursor": {
                "cursorId": 9_i64,
                "originatingCommand": {
                    "aggregate": "orders",
                    "pipeline": [{ "$match": { "total": { "$gt": 100 } } }],
                    "comment": "kuery:5f3a:3:12",
                },
            },
            "secs_running": 2,
        };
        let other_kuery = doc! {
            "client": "127.0.0.1:61001",
            "active": true,
            "opid": 78,
            "op": "command",
            "command": { "aggregate": 1, "comment": "kuery:5f3a:4:1" },
        };
        let read = activity(&[get_more, other_kuery], OWN, Some(OWN_ONLY));
        let mine = &read.backends[0];
        assert_eq!(
            mine.query,
            r#"{"aggregate":"orders","pipeline":[{"$match":{"total":{"$gt":100}}}]}"#
        );
        assert!(mine.own);
        assert_eq!(mine.query_seconds, Some(2.));
        // Another Kuery session's operation is not the monitor's own.
        assert!(!read.backends[1].own);
        assert_eq!(read.notice.as_deref(), Some(OWN_ONLY));
    }

    #[test]
    fn tells_server_threads_and_stopping_operations_apart() {
        let monitor = doc! {
            "type": "op",
            "desc": "TTLMonitor",
            "active": false,
            "opid": 3,
            "op": "none",
            "ns": "",
            "command": {},
        };
        let stopping = doc! {
            "client": "10.0.0.8:4000",
            "active": true,
            "killPending": true,
            "opid": "shard01:4521",
            "op": "update",
            "ns": "shop",
            "command": { "update": "orders", "$truncated": "{ update: \"orders\", updates: [ { q: { ..." },
        };
        let no_id = doc! { "type": "op", "active": true, "desc": "ReplWriterWorker" };
        let read = activity(&[monitor, stopping, no_id], OWN, None);
        assert_eq!(read.backends.len(), 2);
        let (monitor, stopping) = (&read.backends[0], &read.backends[1]);
        assert_eq!(monitor.kind, "TTLMonitor");
        assert!(!monitor.is_client());
        assert_eq!(monitor.state, State::Idle);
        assert_eq!(stopping.id, BackendId("shard01:4521".into()));
        assert_eq!(stopping.state, State::Other("stopping".into()));
        assert_eq!(stopping.database, "shop");
        assert_eq!(
            stopping.query,
            "{ update: \"orders\", updates: [ { q: { ..."
        );
    }

    #[test]
    fn gives_back_ids_as_the_server_numbers_them() {
        assert_eq!(opid(&BackendId("18204".into())), Bson::Int32(18_204));
        assert_eq!(
            opid(&BackendId("8589934592".into())),
            Bson::Int64(8_589_934_592)
        );
        assert_eq!(
            opid(&BackendId("shard01:4521".into())),
            Bson::String("shard01:4521".into())
        );
    }
}
