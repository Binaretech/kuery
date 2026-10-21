# SQL Server driver — design

Date: 2026-10-21 · Status: draft for approval

## Intent

Kuery speaks PostgreSQL, MySQL/MariaDB, SQLite and Redis. The owner needs to
work with a Microsoft SQL Server installed on a Windows machine on their local
network, from Kuery on their Mac (and the other desktop targets), with the same
tools the other SQL engines get. Success is: they add a SQL Server connection,
and everything Kuery does with a PostgreSQL connection works against it.

## Goals

SQL Server is a fourth SQL-family driver, at parity with the others:

1. **Connect**: host/port (default 1433), database, user, password in the OS
   keychain, TLS, SSH tunnel, environment policies (development / staging /
   production), "Test connection" showing e.g. `SQL Server 2022 (16.0.4135)`.
2. **Explore**: schemas, tables, views, procedures, functions, triggers, user
   types; columns, keys, indexes, constraints, definitions; routine source.
3. **Browse tables** with server-side filter, sort and paging.
4. **Query**: editor highlighting, table/column completion, formatting, run
   selection / statement under cursor, scripts, cancel, statement and lock
   timeouts, output log, export.
5. **Controlled writes**: read-only by default, write mode with transaction
   and commit/rollback, previews that undo, staged row edits with their checks,
   full writes.
6. **Schema diagrams** from foreign keys.
7. **Query plans** in the Plan tab (tree/graph, step details, compare).
8. **Activity**: the server's sessions, what they run, who blocks whom, and
   cancel/kill — as PostgreSQL has today.

Goals 1–6 are what MySQL already has; 7 and 8 exist today only for
PostgreSQL and are new work for this driver.

## Non-goals

- Azure SQL Database / Azure AD (Entra) authentication. It may happen to work
  with SQL logins, but is not designed or tested for.
- Windows authentication of any kind (NTLM `DOMAIN\user`, Kerberos,
  integrated "current Windows user" single sign-on).
- SQL Server Agent jobs, SSIS, replication, backups, users/permissions admin.
- SQLCMD mode (`:setvar`, `:connect`) in the editor.

## Anti-goals

- Kuery must never let a write through on a read-only (or production) SQL
  Server connection while telling the user it is read-only. If the server
  cannot be made to refuse a write, the UI and docs must say so honestly.
- No regressions or special-casing leaking into the PostgreSQL/MySQL/SQLite
  paths: dialect differences go through `Dialect`, not `if mssql` branches in
  shared code.

## Existing architecture to build on

- A SQL driver is a crate exporting `pub static DRIVER: SqlDriver`
  (`crates/core/src/driver.rs`): a `DriverInfo`, a `&'static Dialect`
  (`crates/core/src/sql.rs`) and an `open` fn returning a blocking
  `Box<dyn Adapter>` (`crates/core/src/adapter.rs`).
- All behaviour (read-only layers, write transactions with savepoints,
  previews, edits, scripts, timeouts, cancel) lives once in
  `crates/core/src/session.rs` and drives any adapter.
- `crates/core/src/conformance.rs` is the contract: a driver "works like the
  others" when its tests call `conformance::run` against a real server and pass.
- The app registers drivers in `crates/app/src/drivers.rs` (`SQL` array).
- `crates/mysql` is the closest template (async client on its own tokio
  runtime, catalog from `information_schema`, cancel from a side connection).

## Decisions

### Made by the owner

- Target: a SQL Server on a Windows machine on the LAN.
- Scope: every operation the other SQL drivers support (goals above).

### My calls (open to change)

- **New crate `crates/mssql` (`kuery-mssql`)**, driver id `mssql`, name
  "SQL Server", aliases `sqlserver`, `mssql`, `tsql`, `microsoft`.
- **Client library: `tiberius`** (pure Rust TDS, tokio, rustls). It is the
  only maintained Rust TDS client.
- **Authentication**: SQL Server logins (user + password), as the owner
  confirmed.
- **TLS**: `Tls::Modes(&["disable", "require", "trust server certificate"])`,
  default = encrypt when the server offers it. A LAN install almost always has
  a self-signed certificate, so "trust server certificate" must be easy to
  find, and the connection error for an untrusted certificate must point to it.
- **Named instances** (`HOST\SQLEXPRESS`): supported by accepting
  `host\instance` in the host field and resolving the port through the SQL
  Browser service (UDP 1434, tiberius `sql-browser-tokio`). Through an SSH
  tunnel only a fixed port works; the form says so.
- **Values as text**: rows reach `RowSink` as the text SQL Server itself
  gives with `convert(nvarchar(max), x, <style>)`, so what is shown can be
  compared back by `Dialect::as_text`. Dates/times use ISO style 126/121,
  `bit` shows `1`/`0`, `uniqueidentifier` uppercase, binary as `0x…` hex.
  The adapter's rendering of tiberius values and `as_text` must agree; a test
  covers each type.
- **Dialect `MSSQL`** in `crates/core/src/sql.rs`:
  - identifiers quoted with `[`…`]` (needs `Dialect` to support an opening and
    closing quote that differ; `]` doubled inside);
  - literals `N'…'` (`'` doubled), no backslash escapes;
  - `as_text`: `convert(nvarchar(max), expr, 121)`;
  - `like_ignoring_case`: `like` (collation-dependent; acceptable);
  - `nulls_last: false`; `same`: `(a = b or (a is null and b is null))`;
  - `insert_defaults`: `insert into {} default values`;
  - parser `sqlparser::dialect::MsSqlDialect`, formatter `Generic` (or
    `TSql` if `sqlformat` gains one);
  - `implicit_commits: false` (SQL Server DDL is transactional).
- **Paging**: browse uses `limit … offset`, which T-SQL lacks. Add a dialect
  hook for "page of N from offset M"; for SQL Server it is
  `offset M rows fetch next N rows only`, which requires `order by` — when the
  table has no sort, use `order by (select null)`. Every other place that
  generates `limit` in shared code goes through the same hook.
- **Transactions / savepoints**: T-SQL uses `begin transaction`,
  `save transaction name`, `rollback transaction name`, and has **no release
  of a savepoint**. `Transactions::release` becomes optional (`None` = skip);
  savepoint names stay valid T-SQL identifiers. `xact_abort` stays `off` so a
  failed statement does not abort the whole transaction; the adapter checks
  `@@trancount` after an error and reports a lost transaction when SQL Server
  rolled it back itself (e.g. on deadlock).
- **Read-only, the second layer** (the hard part): SQL Server has no
  read-only transaction or session switch. The plan:
  1. First layer unchanged: Kuery's parser refuses writes
     (`crates/core/src/read_only.rs`), extended with T-SQL side-effect
     statements and procedures (`exec`/`execute` of anything except a short
     allow-list of read-only system procedures like `sp_help*`, `xp_cmdshell`,
     `openrowset`, `opendatasource`, `bulk insert`, `waitfor`-free, etc.).
  2. Second layer: reads run in a transaction that is always rolled back
     (DDL included, since it is transactional), **and** before the rollback the
     adapter checks `sys.dm_tran_database_transactions` for log bytes written
     by its own transaction; any write is rolled back and reported as a
     refused write. This turns "undo" into "refuse" for the conformance check.
  3. For production connections the form recommends a login that only has
     `db_datareader`, since that is the only refusal SQL Server enforces
     itself. Docs say this plainly.
  If (2) cannot be made to satisfy `refuses_writes_twice_when_reading`, stop
  and bring it back to the owner rather than weakening the test silently.
- **Cancel**: TDS cancels with an *attention* packet, which tiberius does not
  expose. First choice: send attention through tiberius if a version/fork
  allows it. Fallback: `KILL <spid>` from a side connection, which ends the
  whole session; the adapter then reports the connection lost and the session
  reopens (`Session::reopen`), losing any open write transaction — the UI must
  say so when cancelling inside one. **This is the first spike**: decide which
  before building the rest.
- **Timeouts**: `set lock_timeout <ms>`; SQL Server has no per-session
  statement timeout, so the statement timeout is enforced client-side by the
  session firing the canceller (check `session.rs` already does this for
  drivers that cannot; add it if not).
- **Catalog** from `sys.*` views (not `information_schema`, which lacks
  triggers, routines' source and index details): schemas from `sys.schemas`
  excluding system ones (`sys`, `INFORMATION_SCHEMA`, `db_*`, `guest`), routine
  source from `sys.sql_modules`/`OBJECT_DEFINITION`, user types as the
  "types" node. The "search path" is the user's default schema
  (`schema_name()`), then `dbo`.
- **Database = schema container**: a connection opens one database (the form's
  database field, default the login's default database); the sidebar shows
  its schemas. Switching database is a different connection (as PostgreSQL).
- **Scripts and `GO`**: `GO` on its own line is a batch separator understood
  by the script splitter for this dialect (it is not T-SQL). Statements within
  a batch run as one `execute` so variables and temp tables work.
- **Multiple result sets**: a batch can return several; the first non-empty
  one goes to the grid, the rest are noted in the output log (same as other
  drivers do for scripts — follow whatever `run_script` already does).
- **Query plans**: `Dialect::explain` returns
  `set showplan_xml on; <stmt>; set showplan_xml off` (estimated) or
  `set statistics xml on` (measured). `crates/core/src/plan.rs` gains a
  showplan-XML parser producing the same `Plan` tree: operator
  (`PhysicalOp`/`LogicalOp`), object/index, estimated rows/cost, actual rows
  and time when measured. Descriptions in plain words like the PostgreSQL ones
  ("Read all of dbo.orders", "Look up … through IX_…"). Insights that only
  make sense for PostgreSQL are skipped; SQL Server's own
  `MissingIndexes` and warnings become insights.
- **Activity**: `sys.dm_exec_sessions` + `sys.dm_exec_requests` +
  `sys.dm_exec_sql_text` for what runs and `blocking_session_id` for who
  blocks whom; signal = `KILL <spid>` (SQL Server cannot cancel another
  session's statement without ending it, so "cancel" and "terminate" both end
  the session and the UI says so). Needs `VIEW SERVER STATE`; without it,
  show only the user's own sessions and say why.
- **Column kinds** mapped from TDS types: integers, decimal/money/float →
  Number, date → Date, datetime/datetime2/smalldatetime/datetimeoffset →
  Timestamp, bit → Boolean if `ColumnKind` has one, else Integer; rest Text.
- **Dev environment**: `dev/compose.yaml` gets a
  `mcr.microsoft.com/mssql/server:2022-latest` service (amd64; on Apple
  silicon it runs under Docker's Rosetta emulation) on port `14339`, with a
  `dev/seed-mssql.sql` port of the sample shop, loaded by a one-shot
  `sqlcmd` container. SA password is a development-only constant documented
  next to the others. Conformance tests run `#[ignore]`d like MySQL's, and in
  CI if the existing workflow runs the other databases' integration tests.
- **Docs**: README driver list and `docs/guide.md` get SQL Server, including
  the TLS / named-instance / read-only notes.

### Settled after the builder check (my calls)

- **Read-only check placement**: the adapter knows when it is inside the
  read-only transaction and checks `sys.dm_tran_database_transactions` after
  *every* statement, so `execute` itself returns the refusal (what
  `conformance.rs` requires). Pending the owner's answer on whether
  detect-and-undo is acceptable at all (see open questions).
- **Cancel fallback**: if no attention packet is available, the adapter
  cancels by dropping its own socket (needs no `ALTER ANY CONNECTION`) and
  reconnects itself in place, so the same `Session` keeps working. It then
  reports `Cancelled` plus, if a write transaction was open, that it was lost;
  the UI warns before cancelling inside a write transaction.
- **Statement timeout**: with no server-side timeout, the adapter enforces it
  client-side through the same path as cancel (no session change needed). With the socket-drop fallback the
  default timeout inside a write transaction is disabled (only lock timeout
  applies), so a slow statement never silently drops uncommitted work.
- **Scripts**: for T-SQL, the unit the session splits on is the `GO` batch,
  not the statement; a script with no `GO` is one batch. Conformance builds its
  multi-statement scripts with a `Dialect::join_script` (`; ` for the others,
  `GO` lines for T-SQL), so its assertions stay the same for every driver.
  The read-only analyser accepts `DECLARE`, `SET @var`, and temp tables
  (`#t`, `select … into #t`) for T-SQL, since they only touch the session.
- **TLS stack**: verify tiberius' rustls version matches the workspace's
  0.23 in the spike; if not, use its native-tls feature on the mssql crate
  only.

## Answers from the owner (after the builder check)

1. **Read-only guarantee: detect and undo is accepted.** Kuery's analyser
   refuses writes; anything that slips through runs inside the always-rolled-
   back transaction, is detected after the statement and reported as refused.
   Effects outside the transaction (identity/sequence values, mail, linked
   servers, `xp_cmdshell`) are not undone: the analyser must refuse those
   procedures/functions explicitly, and the docs and the production note on
   the form say so and recommend a `db_datareader` login.
2. **Server version: unknown.** Support SQL Server 2012 and later; read
   `@@version` at connect and show it. Develop and test against 2022 in Docker;
   if the owner's server needs TLS 1.0/1.1, fall back to tiberius' native-tls
   for this crate. Step 8 (the owner's real server) is where version issues
   surface, so it is not optional.
3. **Authentication: SQL Server logins only.** NTLM/`DOMAIN\user` moves to
   non-goals (the earlier call to add it is withdrawn).

4. **Read-only without `VIEW SERVER PERFORMANCE STATE` (asked after the
   spike): undo and warn.** At connect the adapter probes the write-detection
   query. With the permission, writes that slip into read mode are refused
   with an error (as designed). Without it (error 300), reads still run in the
   always-rolled-back transaction, so nothing is kept, but the refusal cannot
   be reported; the connection shows a warning saying read-only is enforced by
   Kuery's checks and automatic undo only, and recommending either granting
   `VIEW SERVER PERFORMANCE STATE` (`VIEW SERVER STATE` before 2022) or a
   `db_datareader`-only login.

## Suggested order

1. Spike: cancel (attention vs KILL) and read-only second layer against the
   Docker server. Report back before continuing if either fails.
2. Dialect changes in core (bracket quoting, paging hook, optional release,
   `GO` splitting) with unit tests; existing drivers' tests stay green.
3. `kuery-mssql` adapter + catalog, passing `conformance::run`.
4. App registration, connection form (TLS modes, `DOMAIN\user`, instance),
   completion/highlighting check with T-SQL.
5. Plans (showplan XML parser + descriptions), with fixtures like
   `crates/core/fixtures/plans/`.
6. Activity.
7. Dev compose, seed, docs.
8. Manual check against the owner's real Windows server.

## Left to the builder

Exact SQL of catalog queries, type-to-text styles per type (within the rule
above), crate/module layout inside `kuery-mssql`, how the showplan XML maps
onto existing `Node` fields, test fixtures.

## Spike results

Run on 2026-10-21 against `mcr.microsoft.com/mssql/server:2022-latest`
(16.0.4305.5, Developer, Linux, amd64 under emulation) on `localhost:14339`
with the code in `crates/mssql/examples/spike.rs`
(`cargo run -p kuery-mssql --example spike -- cancel|cancel2|socket|write|values|astext|tls|parse`).

### Library and TLS

- **`tiberius = 0.13.0`**, `default-features = false`, features
  `tds73`, `rustls`, `sql-browser-tokio`; plus `tokio-util` with `compat`
  (`TcpStream::compat_write()`). 0.12.3 was rejected: its `rustls` feature uses
  rustls 0.21 (a second TLS stack) and it has no cancel call.
- **TLS backend: rustls 0.23.** `cargo tree -p kuery-mssql -d | grep rustls`
  shows no duplicate rustls; the whole workspace has one `rustls 0.23.46`.
  tiberius' `tokio-rustls` keeps its default `aws_lc_rs` feature, so
  `aws-lc-sys` is in the build graph, but it already was (polars via reqwest,
  `kuery-files`); it adds no new native requirement. With the workspace's
  `ring` feature also on, rustls still negotiates without anyone installing a
  default provider (verified, TLS connects). No need for `native-tls`.
- TLS mode mapping: see "TLS modes" below. `@@version` reads fine at connect.
- `sqlparser` 0.63 `MsSqlDialect` **parses `waitfor delay '00:00:20'`** (to
  `Statement::WaitFor`), alone and followed by `; select 1`. So no
  substitute slow statement is needed for tests (a cross join over
  `sys.all_objects` also ran > 5 s and was stopped; see below).

### Cancel: drop the socket and reconnect

- **tiberius 0.13.0 has `Client::cancel_query()` (TDS attention), but it
  cannot be used.** After the statement future is dropped (select on a
  `Notify` at 300 ms), `cancel_query` sends the attention and the server does
  stop the statement (no `WAITFOR` request left), yet `cancel_query` returns
  `Err(Protocol("Never got a DONE token acknowledging the Attention
  signal."))` in 1 to 20 ms, and the connection is left **one response behind**:
  the next query returns zero result sets and every following query returns the
  previous query's rows (`select 100`, `select 200`, `select 300` came back as
  0 sets, `100`, `200`). A second `cancel_query` returns `Ok` but does not fix
  it. Same for all four cases tried (waitfor; `select 1; waitfor`; slow
  aggregate; slow select streaming rows, which took 851 ms to drain first).
  A desynchronised connection that returns wrong rows is worse than a lost
  one, so attention through tiberius is **rejected**.
- **Chosen: drop the client and socket, then connect again.** The adapter
  runs each statement under a `select!` against a cancel signal; on cancel it
  drops the `Client` and opens a new one in place.
  - The server session goes away by itself: 8 to 50 ms after the drop for
    `waitfor`; 30 ms for a slow aggregate `select` that has sent no rows;
    ~30 to 50 ms with an open write transaction. Checked from a second
    connection with `select count(*) from sys.dm_exec_sessions where
    session_id = <old spid>` returning `0`. No `KILL` and no
    `ALTER ANY CONNECTION` needed.
  - **The old connection is unusable; a fresh connect is needed.** Reconnect
    (TCP + TLS + login) took **12 to 34 ms** on localhost (emulated server).
    The new session has a new `@@spid`.
  - An open write transaction is lost: after the drop, the table created
    inside the transaction was not there (`0` in `sys.tables`) and its
    session was gone. Session state (temp tables, `set` options, `use`) is
    lost too; the adapter must replay its `set lock_timeout` after reconnect.

### Write detection (read-only second layer)

Query, run in the same connection and transaction:

```sql
select coalesce(sum(database_transaction_log_record_count), 0)
from sys.dm_tran_database_transactions
where transaction_id = (select transaction_id from sys.dm_tran_current_transaction)
  and database_id <> 2
```

| Step in an open transaction | Result |
| --- | --- |
| `select * into #t from sys.objects` | `0` |
| `create table dbo.spike (i int)` | `20` |
| after `rollback`, `dbo.spike` exists? | no (DDL is transactional) |
| `select count(*) from dbo.spike` | `0` |
| `insert into dbo.spike values (1)` | `2` |

Temp-table work (tempdb, `database_id = 2`) is invisible to it, so `#t`,
`declare` and `select … into #t` stay allowed without special cases; real
writes, DDL included, are visible right after the statement. The value is
`bigint` (`I64`).

**Amended (task 7b): a read can log one record.** Query Store (on by default
for new databases since SQL Server 2022) records each new runtime-stats
interval, and its first-ever setup in a database, in a system transaction
nested in whatever transaction runs the first query of the interval. That
logs the running transaction's `LOP_BEGIN_XACT` (108 bytes) and nothing else
in it, so a plain `select` read `1` once per interval (reproduced with
`interval_length_minutes = 1` and after `set query_store clear`). Every write
logs that begin record plus at least one more (measured: insert, update
including `set q = q`, delete `2`; `alter table add` `10`; `truncate` `11`;
`grant` `5`; `create table` `20`; `select into` `49`), so the check counts
`database_transaction_log_record_count - 1` for each database with records.
Auto-created and auto-updated statistics, sequences, spools, schemabound
views and `set statistics xml on` all read `0` either way.

**Permission finding (important): the above was run as `sa`, and it does not
work for an ordinary login.** Tried on the dev server (SQL Server 2022) with
logins created by `spike -- logins`: `kuery_reader` (user in `shop` with only
`db_datareader`) and `kuery_writer` (`db_datareader` + `db_datawriter`, the
realistic read-only-mode login that can write):

- `sys.dm_tran_database_transactions`, `sys.dm_tran_session_transactions`,
  `sys.dm_tran_current_transaction`, `sys.dm_tran_active_transactions`,
  `sys.dm_exec_connections`, `sys.dm_db_session_space_usage` and
  `sys.dm_tran_locks` all fail for both logins with error 300: `VIEW SERVER
  PERFORMANCE STATE permission was denied on object 'server', database
  'master'.` (`has_perms_by_name(null, null, 'VIEW SERVER STATE')` = 0). On
  SQL Server 2019 and earlier the permission is `VIEW SERVER STATE`.
- What does work without any permission is unreliable: `@@trancount` /
  `xact_state()` only say a transaction is open; `sys.dm_exec_sessions.writes`
  (own row) stayed `0` for `kuery_writer` after an insert, and moved for
  `kuery_reader` after plain selects (page writes by the engine, not log
  records); `sys.dm_exec_requests.writes` was `0`.
- Partial fallbacks that need no permission, verified as `kuery_writer`
  (with `db_ddladmin` added to allow DDL; `db_datawriter` alone gets error 262
  `CREATE TABLE permission denied`): the rows-affected count tiberius returns
  for an `insert` (`[2]`), and `select max(modify_date) from sys.objects`
  changing after `create table`. They miss writes made inside a procedure or
  trigger with `set nocount on`, updates that touch zero rows through
  triggers, and other side effects, so they are **not** a proof.
- **With the permission granted it works for the writer login:**
  `use master; grant VIEW SERVER PERFORMANCE STATE to kuery_writer` (also
  `VIEW SERVER STATE`, which is what 2012 to 2019 need and which 2022 accepts)
  gave, as `kuery_writer` (`spike -- grants`): after select `0`, after insert
  of 2 rows `3`, after `create table` `22`. Note the grant is server-scope and
  must be run in `master`.

Conclusion for the read-only second layer: **feasible, but only for a login
with `VIEW SERVER STATE` (`VIEW SERVER PERFORMANCE STATE` on 2022+).** For a
login without it there is no reliable detector, so the adapter must probe at
connect (run the detection query once in a throwaway transaction; error 300 =
unavailable) and then say so honestly: the connection's read-only protection
is Kuery's analyser plus the rollback, without server-side detection. The
form/docs must tell users that a writer login needs the grant for read-only
mode to be enforced, and that a `db_datareader`-only login is the safe
choice because the server itself then refuses every write. Read-only mode
with a login that is both able to write and lacks the grant cannot satisfy
`refuses_writes_twice_when_reading` and should be shown as "not enforced".

### `as_text` and value text

No single expression gives the right text for every type:

- `convert(nvarchar(max), x, 126)` is right for all types except **xml**
  (error 6358, style not valid) and **binary** (`varbinary`: reinterprets the
  bytes as UTF-16 garbage, e.g. `0x00FF10` becomes `"\u{ff00}\u{10}"`). Style
  121 is worse (float loses digits: `0.123457`; money gets thousands commas).
- `sql_variant_property(cast(x as sql_variant), 'BaseType')` cannot be used to
  pick a style: it errors for `nvarchar(max)`, `varbinary(max)`, `varchar(max)`
  and `xml`.
- `format()`, `concat()` and a plain `cast` fail or give non-ISO dates
  (`Oct 21 2026  1:45PM`) or 6-digit floats.

**Decision: `Dialect::as_text` for MSSQL is**

```sql
coalesce(try_convert(nvarchar(max), {expr}, 126), convert(nvarchar(max), {expr}))
```

`try_convert` turns the xml error into `NULL`, and the fallback then gives the
xml text (`<a x="1"><b/></a>`). `NULL` stays `NULL`. This is correct for every
type except binary. **Binary columns must not be compared through `as_text`**:
the adapter shows them as `0x` plus uppercase hex (what style 1 gives), which
`as_text` cannot produce without knowing the type. The staged-edit check
(`edit.rs`) and the `like` filter (`browse.rs`) are the two users; for binary
columns the edit check should be skipped or key-only (a catalog column type is
available there), and the filter on a binary column is not offered.

Rendering rule per tiberius `ColumnData` variant so that the adapter's text is
byte for byte what style 126 gives. The rules are implemented as `render()` in
`crates/mssql/examples/spike.rs` and compared with the server's `as_text`
expression by `spike -- render` for 117 values (types above, edge values such
as `0001-01-01`, `9999-12-31 23:59:59.9999999`, offsets `+14:00` / `-12:00`
and a midnight crossing, datetime 1/300 s rounding, 38-digit decimals, float
extremes, empty strings, an emoji, `NULL` of each type): 115 equal and 2 known
differences (money beyond about 10^14, below). Binary is compared with
`convert(nvarchar(max), x, 1)`.

| SQL type | tiberius value | Text rule | Example |
| --- | --- | --- | --- |
| `tinyint` | `U8` | decimal | `7` |
| `smallint` / `int` / `bigint` | `I16` / `I32` / `I64` | decimal | `-42` |
| `bit` | `Bit` | `1` / `0` | `1` |
| `decimal` / `numeric` | `Numeric` | `Display`, keeps scale (also `-0.05`, 38 digits); scale 0 uses the integer value only (`Display` writes `5.0`, the server `5`) | `12345.60`, `5` |
| `money` / `smallmoney` | `F64` | `format!("{:.4}")` (4 decimals). **Known difference:** `money` arrives as f64, so values beyond about 10^14 lose the last digits (`922337203685477.5807` renders `…477.6250`); `99999999999.9999` is still exact. Accepted, documented | `1234.5000` |
| `float` | `F64` | `{:.15e}`, exponent as sign plus 3 digits | `1.000000000000000e-001` |
| `real` | `F32` | `{:.7e}` of the f32, exponent as sign plus 3 digits | `1.0000000e-001` |
| `date` | `Date(days)` | days since 0001-01-01 to `YYYY-MM-DD` | `2026-10-21` |
| `time(n)` | `Time{increments, scale}` | `increments` is in 10^-scale s; `HH:MM:SS`, plus `.` and exactly `scale` digits only when the fraction is not zero (whole seconds print no fraction at all) | `13:45:09.1234567`, `13:45:09.120` (scale 3), `13:45:09` (scale 0, or scale 7 with zero fraction) |
| `smalldatetime` | `SmallDateTime{days, minutes}` | days since 1900-01-01; `minutes` since midnight; seconds are always `:00` | `2026-10-21T13:45:00` |
| `datetime` | `DateTime{days, seconds_fragments}` | days since 1900-01-01; fragments are 1/300 s; seconds = fragments / 300, milliseconds = round((fragments % 300) * 10 / 3); 3 ms digits, but none when the fraction is zero | `2026-10-21T13:45:09.123`, `.007`, `1900-01-01T00:00:00` |
| `datetime2(n)` | `DateTime2{date, time}` | `date` + `T` + time rule (scale digits) | `2026-10-21T13:45:09.1234567`, scale 0: `…:09` |
| `datetimeoffset(n)` | `DateTimeOffset{datetime2, offset}` | the stored time is **UTC**; add `offset` minutes to get local, then datetime2 rule, then `+HH:MM` / `-HH:MM` (`+00:00` for zero) | `2026-10-21T13:45:09.1234567+05:30`, `…T03:15:09.5-08:00` |
| `uniqueidentifier` | `Guid` | hyphenated **uppercase** (`uuid` prints lowercase) | `6F9619FF-8B86-D011-B42D-00C04FC964FF` |
| `char` / `varchar` / `nchar` / `nvarchar` / `text` | `String` | as is | `héllo` |
| `binary` / `varbinary` / `image` | `Binary` | `0x` + uppercase hex; not comparable with `as_text` (see above) | `0x00FF10` |
| `xml` | `Xml(XmlData{data, ..})` | `data` as is | `<a x="1"><b/></a>` |

The `float` / `real` rules agree with the server for every value tried
(`0`, `0.1`, `1e15`, `1e16`, `-1.5e-300`, `1.7976931348623157e308`,
`2.2250738585072014e-308`, `123456789.123456789`, `3.4e38`, `16777216`); Task 3
should keep a test with these values (and a few random floats) against the
Docker server.

### TLS modes

`EncryptionLevel` and `trust_cert()` for each mode, run against the dev server
(self-signed certificate) by `spike -- tlsmodes`; `encrypt_option` read from
`sys.dm_exec_connections`:

| Kuery mode | tiberius | Result on the dev server |
| --- | --- | --- |
| `disable` | `NotSupported`, no trust | connects, `encrypt_option` FALSE, no TLS handshake at all |
| default (none chosen: encrypt if the server offers it, check the certificate) | `On`, no trust | **fails** on the self-signed certificate (error below) |
| `require` | `Required`, no trust | **fails** with the same error |
| `trust server certificate` | `Required` + `trust_cert()` | connects, `encrypt_option` TRUE (certificate and host name not checked) |
| (not offered) | `On` + `trust_cert()` | connects, TRUE |
| (not offered) | `Off` + `trust_cert()` | connects, `encrypt_option` FALSE (only the login is encrypted) |

`Off` without trust fails too: SQL Server always runs the TLS handshake for the
login, so only `NotSupported` avoids certificate checking. Do not use `Off`.
Mapping decision: `disable` = `NotSupported`; default = `On`; `require` =
`Required`; `trust server certificate` = `Required` + `trust_cert()`.

Error text for an untrusted self-signed certificate (default and `require`):

> Error forming TLS connection: the server's certificate was rejected during
> the TLS handshake: Other(OtherError(UnsupportedCertVersion)). The SQL Server
> presented a certificate rustls considers too old (an outdated X.509 version),
> which frequently happens with the self-signed certificate SQL Server
> auto-generates. To connect anyway you can: (1) trust a specific CA
> certificate with `Config::trust_cert_ca(path)` … (3) skip certificate
> validation entirely with `Config::trust_cert()` … (4) build tiberius with
> the `native-tls` backend, which is more lenient toward legacy certificates.

Kuery should recognise this error (a TLS handshake error from tiberius) and
replace it with its own message pointing to the "trust server certificate"
option. Note that the cause here is `UnsupportedCertVersion`: rustls rejects
the old X.509 certificate that SQL Server generates, so even a certificate the
user installs as trusted may fail on such servers; that is what the
`native-tls` fallback in "Answers from the owner" (2) is for.
