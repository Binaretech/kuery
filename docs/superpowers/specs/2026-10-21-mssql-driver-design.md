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
