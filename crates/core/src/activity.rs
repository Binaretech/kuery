//! A database server's activity: its sessions, what each runs and for how
//! long, and who blocks whom. Plain data read from the server (for
//! PostgreSQL, `pg_stat_activity` and `pg_blocking_pids`), plus what the
//! monitor works out from it: which sessions need attention, the chains of
//! blocked sessions and their roots, and short durations.

/// What a session is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Active,
    Idle,
    /// Inside a transaction, not running anything: it keeps its locks.
    IdleInTransaction,
    /// Inside a transaction that failed and was not rolled back yet.
    IdleInFailedTransaction,
    /// Background processes (autovacuum, WAL writer…) report none.
    Background,
    Other(String),
}

impl State {
    /// From `pg_stat_activity.state`.
    pub fn from_postgres(state: &str) -> Self {
        match state {
            "active" => State::Active,
            "idle" => State::Idle,
            "idle in transaction" => State::IdleInTransaction,
            "idle in transaction (aborted)" => State::IdleInFailedTransaction,
            "" => State::Background,
            other => State::Other(other.to_string()),
        }
    }

    pub fn label(&self) -> &str {
        match self {
            State::Active => "active",
            State::Idle => "idle",
            State::IdleInTransaction => "idle in transaction",
            State::IdleInFailedTransaction => "idle in failed transaction",
            State::Background => "background",
            State::Other(other) => other,
        }
    }
}

/// One session (a server process) and what it is doing.
#[derive(Debug, Clone, PartialEq)]
pub struct Backend {
    pub pid: i32,
    pub user: String,
    pub application: String,
    /// The client's address, empty for a local socket or a background
    /// process.
    pub client: String,
    pub database: String,
    pub state: State,
    /// What it waits for, such as `Lock: transactionid`.
    pub wait: Option<String>,
    /// Seconds since its current (or, when idle, last) query started.
    pub query_seconds: Option<f64>,
    /// Seconds since its transaction started.
    pub transaction_seconds: Option<f64>,
    pub query: String,
    /// The sessions that hold the locks it waits for.
    pub blocked_by: Vec<i32>,
    /// `client backend`, `autovacuum worker`…
    pub kind: String,
    /// The monitor's own session.
    pub own: bool,
    /// The table it waits to lock, and in which mode.
    pub waiting_for: Option<String>,
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
    /// For the audit log: `cancel 4521`, `terminate 4521`.
    pub fn describe(self, pid: i32) -> String {
        match self {
            Signal::Cancel => format!("cancel {pid}"),
            Signal::Terminate => format!("terminate {pid}"),
        }
    }
}

/// A query running this long is worth a look, and an idle transaction
/// open this long holds its locks for nothing.
pub const LONG_SECONDS: f64 = 60.;

impl Backend {
    pub fn is_client(&self) -> bool {
        self.kind == "client backend"
    }

    /// Doing something now, or holding a transaction open.
    pub fn is_busy(&self) -> bool {
        matches!(
            self.state,
            State::Active | State::IdleInTransaction | State::IdleInFailedTransaction
        ) || !self.blocked_by.is_empty()
    }

    /// What deserves attention: blocked, running long, or idle in a
    /// transaction for long.
    pub fn concern(&self) -> Option<Concern> {
        if !self.blocked_by.is_empty() {
            return Some(Concern::Blocked);
        }
        match self.state {
            State::Active if self.query_seconds.unwrap_or(0.) >= LONG_SECONDS => {
                Some(Concern::LongQuery)
            }
            State::IdleInTransaction | State::IdleInFailedTransaction
                if self.transaction_seconds.unwrap_or(0.) >= LONG_SECONDS =>
            {
                Some(Concern::IdleTransaction)
            }
            _ => None,
        }
    }

    /// How long it has been at what it is doing: the query when active,
    /// the transaction when idle in one, the last query when idle.
    pub fn seconds(&self) -> Option<f64> {
        match self.state {
            State::IdleInTransaction | State::IdleInFailedTransaction => self.transaction_seconds,
            _ => self.query_seconds,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Concern {
    Blocked,
    LongQuery,
    IdleTransaction,
}

/// What the server reported, at one moment.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Activity {
    pub backends: Vec<Backend>,
    /// `max_connections`.
    pub max_connections: Option<u32>,
}

/// A session that blocks others without being blocked itself, and every
/// session waiting on it, directly or down a chain.
#[derive(Debug, Clone, PartialEq)]
pub struct Blocker {
    pub pid: i32,
    pub waiting: Vec<i32>,
}

impl Activity {
    pub fn backend(&self, pid: i32) -> Option<&Backend> {
        self.backends.iter().find(|b| b.pid == pid)
    }

    /// Client sessions, and the active, blocked and blocking ones of any
    /// kind; busy ones first, the longest at it first.
    pub fn shown(&self, all: bool) -> Vec<&Backend> {
        let blocking = self.blocking_pids();
        let mut shown: Vec<&Backend> = self
            .backends
            .iter()
            .filter(|b| {
                all || (b.is_busy() && (b.is_client() || !b.blocked_by.is_empty()))
                    || blocking.contains(&b.pid)
            })
            .collect();
        let rank =
            |b: &Backend| match (&b.state, b.blocked_by.is_empty(), blocking.contains(&b.pid)) {
                (_, _, true) => 0,
                (_, false, _) => 1,
                (State::Active, ..) => 2,
                (State::IdleInTransaction | State::IdleInFailedTransaction, ..) => 3,
                (State::Idle, ..) => 4,
                _ => 5,
            };
        shown.sort_by(|a, b| {
            rank(a)
                .cmp(&rank(b))
                .then_with(|| {
                    b.seconds()
                        .unwrap_or(0.)
                        .total_cmp(&a.seconds().unwrap_or(0.))
                })
                .then_with(|| a.pid.cmp(&b.pid))
        });
        shown
    }

    /// Sessions others wait for.
    pub fn blocking_pids(&self) -> Vec<i32> {
        let mut pids: Vec<i32> = self
            .backends
            .iter()
            .flat_map(|b| b.blocked_by.iter().copied())
            .collect();
        pids.sort_unstable();
        pids.dedup();
        pids
    }

    /// Pids waiting on `pid`, directly or through others.
    pub fn waiting_on(&self, pid: i32) -> Vec<i32> {
        let mut found: Vec<i32> = Vec::new();
        let mut frontier = vec![pid];
        while let Some(current) = frontier.pop() {
            for backend in &self.backends {
                if backend.blocked_by.contains(&current)
                    && backend.pid != pid
                    && !found.contains(&backend.pid)
                {
                    found.push(backend.pid);
                    frontier.push(backend.pid);
                }
            }
        }
        found.sort_unstable();
        found
    }

    /// The roots of blocking chains: sessions that block others and wait on
    /// nobody (or only on each other, in a deadlock about to be broken),
    /// those holding up the most first.
    pub fn blockers(&self) -> Vec<Blocker> {
        let blocking = self.blocking_pids();
        let mut roots: Vec<Blocker> = blocking
            .iter()
            .filter(|pid| {
                self.backend(**pid)
                    .is_none_or(|b| b.blocked_by.iter().all(|other| !blocking.contains(other)))
            })
            .map(|pid| Blocker {
                pid: *pid,
                waiting: self.waiting_on(*pid),
            })
            .collect();
        if roots.is_empty() {
            // Everyone blocking is also waiting: a cycle. Show each.
            roots = blocking
                .iter()
                .map(|pid| Blocker {
                    pid: *pid,
                    waiting: self.waiting_on(*pid),
                })
                .collect();
        }
        roots.sort_by(|a, b| {
            b.waiting
                .len()
                .cmp(&a.waiting.len())
                .then(a.pid.cmp(&b.pid))
        });
        roots
    }

    /// Counts for a summary: (active, idle in transaction, waiting).
    pub fn counts(&self) -> (usize, usize, usize) {
        let clients = self.backends.iter().filter(|b| b.is_client());
        let mut counts = (0, 0, 0);
        for backend in clients {
            match backend.state {
                State::Active => counts.0 += 1,
                State::IdleInTransaction | State::IdleInFailedTransaction => counts.1 += 1,
                _ => {}
            }
            if !backend.blocked_by.is_empty() {
                counts.2 += 1;
            }
        }
        counts
    }

    pub fn connections(&self) -> usize {
        self.backends.iter().filter(|b| b.is_client()).count()
    }
}

/// `0.4 s`, `12 s`, `3 m 12 s`, `2 h 5 m`, `3 d 4 h`.
pub fn duration(seconds: f64) -> String {
    let seconds = seconds.max(0.);
    if seconds < 10. {
        return format!("{seconds:.1} s");
    }
    let whole = seconds as u64;
    match whole {
        0..60 => format!("{whole} s"),
        60..3_600 => format!("{} m {} s", whole / 60, whole % 60),
        3_600..86_400 => format!("{} h {} m", whole / 3_600, whole % 3_600 / 60),
        _ => format!("{} d {} h", whole / 86_400, whole % 86_400 / 3_600),
    }
}

/// `pg_blocking_pids` joined with commas → pids.
pub fn parse_pids(text: &str) -> Vec<i32> {
    text.split(',')
        .filter_map(|pid| pid.trim().parse().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(pid: i32, state: &str, seconds: f64, blocked_by: &[i32]) -> Backend {
        Backend {
            pid,
            user: "app".into(),
            application: "web".into(),
            client: "10.0.0.5".into(),
            database: "shop".into(),
            state: State::from_postgres(state),
            wait: None,
            query_seconds: Some(seconds),
            transaction_seconds: Some(seconds),
            query: format!("query of {pid}"),
            blocked_by: blocked_by.to_vec(),
            kind: "client backend".into(),
            own: false,
            waiting_for: None,
        }
    }

    /// 10 idle in a transaction for 5 minutes blocks 20, which blocks 30;
    /// 40 runs long; 50 is idle; 60 is the autovacuum.
    fn busy() -> Activity {
        let mut vacuum = backend(60, "", 3., &[]);
        vacuum.kind = "autovacuum worker".into();
        Activity {
            backends: vec![
                backend(50, "idle", 900., &[]),
                backend(30, "active", 20., &[20]),
                backend(10, "idle in transaction", 300., &[]),
                backend(40, "active", 95., &[]),
                backend(20, "active", 40., &[10]),
                vacuum,
            ],
            max_connections: Some(100),
        }
    }

    #[test]
    fn finds_the_root_of_a_blocking_chain() {
        let activity = busy();
        assert_eq!(activity.blocking_pids(), [10, 20]);
        assert_eq!(activity.waiting_on(10), [20, 30]);
        assert_eq!(
            activity.blockers(),
            [Blocker {
                pid: 10,
                waiting: vec![20, 30]
            }]
        );
        assert_eq!(activity.counts(), (3, 1, 2));
        assert_eq!(activity.connections(), 5);
    }

    #[test]
    fn a_deadlock_still_names_its_sessions() {
        let activity = Activity {
            backends: vec![
                backend(1, "active", 2., &[2]),
                backend(2, "active", 2., &[1]),
            ],
            max_connections: None,
        };
        let roots: Vec<i32> = activity.blockers().iter().map(|b| b.pid).collect();
        assert_eq!(roots, [1, 2]);
    }

    #[test]
    fn shows_blockers_first_then_the_blocked_then_the_longest_running() {
        let activity = busy();
        let pids: Vec<i32> = activity.shown(false).iter().map(|b| b.pid).collect();
        // Idle 50 and the autovacuum are left out unless everything is shown.
        assert_eq!(pids, [10, 20, 30, 40]);
        let all: Vec<i32> = activity.shown(true).iter().map(|b| b.pid).collect();
        assert_eq!(all, [10, 20, 30, 40, 50, 60]);
    }

    #[test]
    fn says_what_needs_attention() {
        let activity = busy();
        let concern = |pid| activity.backend(pid).unwrap().concern();
        assert_eq!(concern(10), Some(Concern::IdleTransaction));
        assert_eq!(concern(20), Some(Concern::Blocked));
        assert_eq!(concern(40), Some(Concern::LongQuery));
        assert_eq!(concern(50), None);
        assert_eq!(activity.backend(10).unwrap().seconds(), Some(300.));
    }

    #[test]
    fn writes_short_durations() {
        assert_eq!(duration(0.42), "0.4 s");
        assert_eq!(duration(12.7), "12 s");
        assert_eq!(duration(192.), "3 m 12 s");
        assert_eq!(duration(7_500.), "2 h 5 m");
        assert_eq!(duration(273_600.), "3 d 4 h");
        assert_eq!(parse_pids("12, 7,"), [12, 7]);
        assert!(parse_pids("").is_empty());
        assert_eq!(
            State::from_postgres("idle in transaction (aborted)").label(),
            "idle in failed transaction"
        );
    }
}
