//! SQL Server activity: every session in `sys.dm_exec_sessions` with the
//! request it runs, who blocks it and the lock it waits for, and `kill` to
//! end a session. Without `VIEW SERVER STATE` the server shows a login only
//! its own sessions and refuses the transaction, lock, connection and query
//! text views, so the monitor then reads without those columns.

use kuery_core::activity::{Activity, Backend, Signal, State};
use kuery_core::adapter::{Adapter as _, Discard, fetch};

use crate::{Mssql, QueryError, lacks_permission};

/// The parts of the query that need `VIEW SERVER STATE`: how long the
/// session's transaction has been open, the object it waits to lock, its
/// client address and the text of its query.
struct Views {
    transaction: &'static str,
    waiting: &'static str,
    client: &'static str,
    query: &'static str,
    joins: &'static str,
}

const ALL_VIEWS: Views = Views {
    transaction: "(select datediff(second, min(at.transaction_begin_time), getdate()) \
          from sys.dm_tran_session_transactions st \
          join sys.dm_tran_active_transactions at on at.transaction_id = st.transaction_id \
          where st.session_id = s.session_id)",
    waiting: "(select top 1 coalesce( \
            case when l.resource_type = 'OBJECT' \
                 then object_name(cast(l.resource_associated_entity_id as int), l.resource_database_id) end, \
            (select object_name(p.object_id) from sys.partitions p \
              where p.hobt_id = l.resource_associated_entity_id), \
            l.resource_type) + ' (' + l.request_mode + ')' \
          from sys.dm_tran_locks l \
          where l.request_session_id = s.session_id and l.request_status = 'WAIT')",
    client: "c.client_net_address",
    query: "t.text",
    joins: "left join sys.dm_exec_connections c on c.session_id = s.session_id \
         outer apply sys.dm_exec_sql_text(coalesce(r.sql_handle, c.most_recent_sql_handle)) t",
};

/// The views a login without `VIEW SERVER STATE` can read.
const SESSION_VIEWS: Views = Views {
    transaction: "cast(null as int)",
    waiting: "cast(null as nvarchar(200))",
    client: "null",
    query: "null",
    joins: "",
};

/// One row per session. Times are measured from `getdate()`.
fn activity_sql(views: &Views) -> String {
    let Views {
        transaction,
        waiting,
        client,
        query,
        joins,
    } = views;
    format!(
        "select s.session_id, \
                coalesce(s.login_name, ''), \
                coalesce(s.program_name, ''), \
                coalesce({client}, s.host_name, ''), \
                coalesce(db_name(s.database_id), ''), \
                s.status, \
                r.wait_type, \
                case when coalesce(r.start_time, s.last_request_start_time) > dateadd(year, -50, getdate()) \
                     then datediff(second, coalesce(r.start_time, s.last_request_start_time), getdate()) end, \
                {transaction}, \
                coalesce({query}, ''), \
                coalesce(r.blocking_session_id, 0), \
                s.is_user_process, \
                case when s.session_id = @@spid then 1 else 0 end, \
                {waiting}, \
                s.open_transaction_count, \
                @@max_connections \
         from sys.dm_exec_sessions s \
         left join sys.dm_exec_requests r on r.session_id = s.session_id \
         {joins} \
         order by s.session_id"
    )
}

/// What the server is doing now.
pub(crate) fn activity(adapter: &mut Mssql) -> Result<Activity, QueryError> {
    let set = match fetch(adapter, &activity_sql(&ALL_VIEWS)) {
        Err(QueryError::Database { code, .. })
            if code.parse().is_ok_and(|code: u32| lacks_permission(code)) =>
        {
            fetch(adapter, &activity_sql(&SESSION_VIEWS))?
        }
        other => other?,
    };
    let mut max_connections = None;
    let backends = (0..set.rows.len())
        .map(|row| {
            max_connections = max_connections.or_else(|| set.text(row, 15).parse().ok());
            let number = |column| set.get(row, column).and_then(|s| s.parse::<f64>().ok());
            let blocker = number(10).map_or(0, |pid| pid as i32);
            Backend {
                pid: number(0).map_or(0, |pid| pid as i32),
                user: set.text(row, 1),
                application: set.text(row, 2),
                client: set.text(row, 3),
                database: set.text(row, 4),
                state: State::from_mssql(&set.text(row, 5), number(14).map_or(0, |n| n as i32)),
                wait: set.get(row, 6).map(str::to_string),
                query_seconds: number(7),
                transaction_seconds: number(8),
                query: set.text(row, 9),
                blocked_by: if blocker > 0 { vec![blocker] } else { vec![] },
                kind: match set.get(row, 11) {
                    Some("1") => "client backend".into(),
                    _ => "background".into(),
                },
                own: set.get(row, 12) == Some("1"),
                waiting_for: set.get(row, 13).map(str::to_string),
            }
        })
        .collect();
    Ok(Activity {
        backends,
        max_connections,
    })
}

/// Ends session `pid` (SQL Server cannot stop just its query), rolling back
/// its transaction; `false` when there was no such session (error 6106).
pub(crate) fn signal(adapter: &mut Mssql, pid: i32, _signal: Signal) -> Result<bool, QueryError> {
    match adapter.execute(&format!("kill {pid}"), &mut Discard) {
        Ok(_) => Ok(true),
        Err(QueryError::Database { code, .. }) if code == "6106" => Ok(false),
        Err(error) => Err(error),
    }
}
