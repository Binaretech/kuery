//! PostgreSQL server activity: every session in `pg_stat_activity`, who
//! blocks it (`pg_blocking_pids`) and the lock it waits for, and signals to
//! cancel a session's query or end the session.

use kuery_core::activity::{Activity, Backend, BackendId, Signal, State, parse_ids};
use kuery_core::adapter::fetch;

use crate::catalog::rows;
use crate::{Postgres, QueryError};

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
pub(crate) fn activity(adapter: &mut Postgres) -> Result<Activity, QueryError> {
    let set = fetch(adapter, ACTIVITY_SQL)?;
    let mut max_connections = None;
    let backends = rows(&set)
        .map(|row| {
            max_connections = max_connections.or_else(|| row.text(14).parse().ok());
            let seconds = |column| row.optional(column).and_then(|s| s.parse::<f64>().ok());
            Backend {
                id: BackendId(row.text(0)),
                user: row.text(1),
                application: row.text(2),
                client: row.text(3),
                database: row.text(4),
                state: State::from_postgres(&row.text(5)),
                wait: row.optional(6),
                query_seconds: seconds(7),
                transaction_seconds: seconds(8),
                query: row.text(9),
                blocked_by: parse_ids(&row.text(10)),
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

/// Sends `signal` to session `id`; `false` when there was no such session
/// to signal, which an id that is not a pid never names.
pub(crate) fn signal(
    adapter: &mut Postgres,
    id: &BackendId,
    signal: Signal,
) -> Result<bool, QueryError> {
    let Ok(pid) = id.0.parse::<i32>() else {
        return Ok(false);
    };
    let sql = match signal {
        Signal::Cancel => format!("select pg_catalog.pg_cancel_backend({pid})"),
        Signal::Terminate => format!("select pg_catalog.pg_terminate_backend({pid})"),
    };
    Ok(fetch(adapter, &sql)?.get(0, 0) == Some("t"))
}
