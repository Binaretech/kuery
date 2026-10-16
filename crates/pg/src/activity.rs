//! PostgreSQL server activity: every session in `pg_stat_activity`, who
//! blocks it (`pg_blocking_pids`) and the lock it waits for, and signals to
//! cancel a session's query or end the session.

use kuery_core::activity::{Activity, Backend, State, parse_pids};

use crate::catalog::rows;
use crate::{QueryError, Session};

/// One row per session. Times are measured from the statement's start, so
/// every row shares one "now"; `pg_blocking_pids` comes joined with commas.
const ACTIVITY_SQL: &str = r"
select a.pid,
       coalesce(a.usename, ''),
       coalesce(a.application_name, ''),
       coalesce(host(a.client_addr), ''),
       coalesce(a.datname, ''),
       coalesce(a.state, ''),
       case when a.wait_event_type is null then null
            else a.wait_event_type || ': ' || a.wait_event end,
       extract(epoch from statement_timestamp() - a.query_start)::float8,
       extract(epoch from statement_timestamp() - a.xact_start)::float8,
       coalesce(a.query, ''),
       array_to_string(pg_catalog.pg_blocking_pids(a.pid), ','),
       coalesce(a.backend_type, ''),
       a.pid = pg_catalog.pg_backend_pid(),
       (select coalesce(l.relation::regclass::text, l.locktype) || ' (' || l.mode || ')'
        from pg_catalog.pg_locks l
        where l.pid = a.pid and not l.granted
        limit 1),
       current_setting('max_connections')
from pg_catalog.pg_stat_activity a
order by a.pid";

/// What the server is doing now.
pub async fn activity(session: Session) -> Result<Activity, QueryError> {
    let set = session.fetch_trusted(ACTIVITY_SQL.to_string()).await?;
    let mut max_connections = None;
    let backends = rows(&set)
        .map(|row| {
            max_connections = max_connections.or_else(|| row.text(14).parse().ok());
            let seconds = |column| row.optional(column).and_then(|s| s.parse::<f64>().ok());
            Backend {
                pid: row.text(0).parse().unwrap_or_default(),
                user: row.text(1),
                application: row.text(2),
                client: row.text(3),
                database: row.text(4),
                state: State::from_postgres(&row.text(5)),
                wait: row.optional(6),
                query_seconds: seconds(7),
                transaction_seconds: seconds(8),
                query: row.text(9),
                blocked_by: parse_pids(&row.text(10)),
                kind: row.text(11),
                own: row.flag(12),
                waiting_for: row.optional(13),
            }
        })
        .collect();
    Ok(Activity {
        backends,
        max_connections,
    })
}

/// What to do to a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Stop its current query; the session stays.
    Cancel,
    /// End the session, rolling back its open transaction.
    Terminate,
}

impl Signal {
    pub fn sql(self, pid: i32) -> String {
        match self {
            Signal::Cancel => format!("select pg_catalog.pg_cancel_backend({pid})"),
            Signal::Terminate => format!("select pg_catalog.pg_terminate_backend({pid})"),
        }
    }
}

/// Sends `signal` to session `pid`; `false` when there was no such session
/// to signal.
pub async fn signal(session: Session, pid: i32, signal: Signal) -> Result<bool, QueryError> {
    let set = session.fetch_trusted(signal.sql(pid)).await?;
    Ok(set.cell(0, 0).flatten() == Some("t"))
}
