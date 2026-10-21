# SQL Server Driver Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Microsoft SQL Server as a fourth SQL-family driver with parity to PostgreSQL (connect, explore, browse, query, controlled writes, diagrams, plans, activity).

**Architecture:** A new crate `crates/mssql` (`kuery-mssql`) exports `pub static DRIVER: SqlDriver` with a blocking `Adapter` over `tiberius` on its own tokio runtime, like `crates/mysql`. Everything dialect-specific reaches shared code through `Dialect` (`crates/core/src/sql.rs`): bracket quoting, `OFFSET … FETCH` paging, savepoints without release, and `GO` batches as the script unit. Showplan XML gets its own parser in `crates/core/src/plan/` producing the existing `Plan`.

**Tech Stack:** Rust (workspace toolchain), tiberius (TDS, rustls), tokio, sqlparser 0.63 `MsSqlDialect`, roxmltree 0.21 (already in `Cargo.lock`), Docker `mcr.microsoft.com/mssql/server:2022-latest`.

**Spec:** `docs/superpowers/specs/2026-10-21-mssql-driver-design.md`

## Global Constraints

- Driver id `mssql`, name `SQL Server`, aliases `sqlserver`, `mssql`, `tsql`, `microsoft`; default port 1433.
- SQL Server 2012 or later; shown version read from `@@version` / `serverproperty('ProductVersion')`.
- Authentication: SQL Server logins only (user + password). No NTLM/Kerberos.
- TLS modes: `Tls::Modes(&["disable", "require", "trust server certificate"])`; default (none chosen) = encrypt if the server offers it.
- Read-only guarantee = analyser refusal + detect-and-undo; never claim the server refuses.
- No `if mssql` branches in shared code: differences go through `Dialect` fields.
- Existing PostgreSQL / MySQL / SQLite tests (`cargo test --workspace`, and their `--ignored` suites where you can run them) stay green after every task.
- Dev server: port `14339`, user `sa`, password `Kuery_dev_2026!` (development only, documented in `dev/compose.yaml` and `.env.example`), database `shop`.
- Comment style and naming follow the surrounding crates (doc comments in plain sentences, short functions, no `utils` modules).

## Review Focus

1. **Self-signed certificate on a LAN server** — "require" fails with an untrusted-cert error; the error must say to pick "trust server certificate". Test in Task 5.
2. **Write slipping through read-only mode** (e.g. `exec dbo.some_proc` that updates, or `select … into dbo.t`) — refused by the analyser, and if run anyway, undone and reported as refused, with the data unchanged. Tests in Tasks 3 and 5.
3. **Temp tables and variables in read mode** (`declare @n int = 3; select top (@n) * from orders`; `select * into #t from orders; select * from #t`) — allowed and not mistaken for writes (tempdb is excluded from the write detection). Tests in Tasks 3 and 5.
4. **Cancel inside an open write transaction** — the query stops, the session keeps working, and the error says the uncommitted changes were lost. Test in Task 5.
5. **Unicode and odd identifiers** (`N'ñandú'`, a table named `Order Items`, a column named `a]b`) — browse, edit and completion quote them correctly. Tests in Tasks 2 and 6.

---

### Task 1: Spike — cancel, write detection, value text, TLS

Throwaway code under `crates/mssql/examples/spike.rs`; the deliverable is a short "Spike results" section appended to the spec. Decisions later tasks depend on are made here.

**Files:**
- Create: `crates/mssql/Cargo.toml`, `crates/mssql/src/lib.rs` (empty `//!` doc only), `crates/mssql/examples/spike.rs`
- Modify: `Cargo.toml` (workspace member + `tiberius` and `kuery-mssql` workspace deps), `dev/compose.yaml` (service `mssql`), `.env.example`
- Modify: `docs/superpowers/specs/2026-10-21-mssql-driver-design.md` (append "Spike results")

- [ ] **Step 1: Add the dev server.** `dev/compose.yaml` service `mssql`: image `mcr.microsoft.com/mssql/server:2022-latest`, `platform: linux/amd64`, env `ACCEPT_EULA=Y`, `MSSQL_SA_PASSWORD=Kuery_dev_2026!`, port `14339:1433`, healthcheck with `/opt/mssql-tools18/bin/sqlcmd -C -S localhost -U sa -P … -Q "select 1"`. Comment in the style of the other services.
- [ ] **Step 2: Start it.** Run `docker compose -f dev/compose.yaml up -d mssql`, then the healthcheck command. Expected: `1`.
- [ ] **Step 3: Pick the tiberius version and TLS feature.** Add `tiberius` with `default-features = false`, features `tds73`, `rustls`, `sql-browser-tokio`, plus tokio-util `compat`. Run `cargo tree -p kuery-mssql -d | grep rustls`. Expected: a single rustls 0.23. If tiberius brings a second rustls, switch it to `native-tls` and note that in the results.
- [ ] **Step 4: Cancel.** In the spike, run `waitfor delay '00:00:20'`, then within 300 ms try (a) tiberius attention/cancel if the version exposes one, else (b) drop the client future and socket from a `tokio::select!` on a `Notify`. Record which works, whether the server session goes away (`select count(*) from sys.dm_exec_sessions where session_id = <old spid>` from a second connection), and the time to reconnect.
- [ ] **Step 5: Write detection.** In one connection: `begin transaction`, `select * into #t from sys.objects`, then `select coalesce(sum(database_transaction_log_record_count), 0) from sys.dm_tran_database_transactions where transaction_id = (select transaction_id from sys.dm_tran_current_transaction) and database_id <> 2`. Expected: `0`. Then `create table dbo.spike (i int)` and the same query. Expected: `> 0`. Record both.
- [ ] **Step 6: Value text.** For one value of each of `int, bigint, bit, decimal(10,2), money, float, real, date, time, datetime, datetime2, datetimeoffset, uniqueidentifier, nvarchar, varbinary, xml`: print tiberius' value and `convert(nvarchar(max), x, 126)`. Record the one `as_text` expression valid for every type (try style 126; if it errors for any type, use `case` on `sql_variant_property(x, 'BaseType')`), and the rendering rule per tiberius `ColumnData` variant that matches it byte for byte.
- [ ] **Step 7: Write the results** into the spec's new "Spike results" section: cancel method, TLS backend, write-detection query, `as_text` expression, value rendering table.
- [ ] **Step 8: Commit.**

```bash
git add Cargo.toml Cargo.lock crates/mssql dev/compose.yaml .env.example docs/superpowers/specs/2026-10-21-mssql-driver-design.md
git commit -m "spike(mssql): dev server and decisions on cancel, TLS and value text"
```

If Step 4 or Step 5 fails (no way to stop a statement and keep the session usable, or writes are not detectable), stop and report to the owner before Task 2.

---

### Task 2: Dialect shape — bracket quotes, paging, optional release, the `MSSQL` dialect

**Files:**
- Modify: `crates/core/src/sql.rs` (Dialect fields, methods, `MSSQL`, tests)
- Modify: `crates/core/src/browse.rs:173-178` and its tests, `crates/core/src/completion.rs:67` (+ wherever `quote` is used there), `crates/core/src/session.rs` (callers of `release`: ~598, 656, 659, 730), `crates/core/src/conformance.rs:93`, `crates/app/src/explorer.rs:1578-1584`

**Interfaces:**
- Produces:
  - `Dialect.ident_quotes: (char, char)` replacing `ident_quote: char` (`('"','"')`, `` ('`','`') ``, `('[',']')`); `quote_ident` doubles the closing char.
  - `pub enum Paging { LimitOffset, OffsetFetch }`, field `Dialect.paging: Paging`.
  - `Dialect::page(&self, ordered: bool, limit: u64, offset: u64) -> String` → `" limit 100 offset 0"` or `" order by (select null) offset 0 rows fetch next 100 rows only"` when `!ordered`, `" offset 0 rows fetch next 100 rows only"` when ordered.
  - `Dialect::first_rows(&self, qualified: &str, n: u64) -> String` → `"select *\nfrom {q}\nlimit 100;\n"` or `"select top (100) *\nfrom {q};\n"`.
  - `Transactions.release: Option<&'static str>`; `Dialect::release(&self, name) -> Option<String>`; session skips `None`.
  - `Dialect.batch_separator: Option<&'static str>` (`Some("GO")` for MSSQL, `None` elsewhere) and `Dialect::join_script(&self, statements: &[&str]) -> String` (`"a; b"` or `"a\nGO\nb"`).
  - `pub static MSSQL: Dialect` with: name `"SQL Server"`, quotes `('[',']')`, literal `N'…'` (`'` doubled, NUL refused), `as_text` from the Task 1 results, `like_ignoring_case: "like"`, `nulls_last: false`, `same: "({a} = {b} or ({a} is null and {b} is null))"`, `insert_defaults: "insert into {} default values"`, parser `MsSqlDialect`, formatter `Generic`, transactions `begin_read_only: &["begin transaction kuery_read"]`, `end_read_only: &["if @@trancount > 0 rollback transaction"]`, `begin: "begin transaction"`, `savepoint: "save transaction"`, `rollback_to: "rollback transaction"`, `release: None`, `implicit_commits: false`, `paging: OffsetFetch`, `batch_separator: Some("GO")`, `explain: None` (Task 7 sets it).

- [ ] **Step 1: Write failing tests** in `sql.rs` `speaks_each_dialect` (or a new `speaks_t_sql`):

```rust
let ms = &MSSQL;
assert_eq!(ms.quote_qualified("dbo", "a]b"), "[dbo].[a]]b]");
assert_eq!(ms.quote_ident("Order Items"), "[Order Items]");
assert_eq!(ms.quote_literal("ñandú's").unwrap(), "N'ñandú''s'");
assert_eq!(ms.quote_literal(r"C:\tmp").unwrap(), r"N'C:\tmp'");
assert_eq!(ms.page(false, 100, 0), " order by (select null) offset 0 rows fetch next 100 rows only");
assert_eq!(ms.page(true, 50, 150), " offset 150 rows fetch next 50 rows only");
assert_eq!(POSTGRES.page(true, 50, 150), " limit 50 offset 150");
assert_eq!(ms.first_rows("[dbo].[t]", 100), "select top (100) *\nfrom [dbo].[t];\n");
assert_eq!(ms.release("s"), None);
assert_eq!(POSTGRES.release("s").as_deref(), Some("release savepoint s"));
assert_eq!(ms.join_script(&["a", "b"]), "a\nGO\nb");
assert_eq!(MYSQL.join_script(&["a", "b"]), "a; b");
assert_eq!(ms.order_nulls_last("[n]", true), "[n] is null, [n] desc");
```

And in `browse.rs` a test that an MSSQL browse with no sort ends in `order by (select null) offset 0 rows fetch next 100 rows only`, and with a sort on `total` ends in `order by [total] is null, [total] desc offset 0 rows fetch next 100 rows only`.

- [ ] **Step 2: Run** `cargo test -p kuery-core sql:: browse::`. Expected: compile errors / FAIL.
- [ ] **Step 3: Implement** the fields, methods and `MSSQL`; switch `browse.rs`, `completion.rs`, `explorer.rs::select_sql`, `conformance.rs:93` (trim `['"', '`', '[', ']']`) and the session's `release` callers. Completion's quoting must use the open/close pair when it inserts quoted names.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: PASS, existing browse/sql tests unchanged.
- [ ] **Step 5: Commit** — `git commit -m "core: dialects with bracket quotes, OFFSET/FETCH paging, optional savepoint release"`.

---

### Task 3: `GO` batches as the script unit, and T-SQL read-only rules

**Files:**
- Modify: `crates/core/src/script.rs`, `crates/core/src/read_only.rs`, `crates/core/src/conformance.rs` (scripts built with `join_script`)
- Test: in the same files' `#[cfg(test)]` modules

**Interfaces:**
- Consumes: `Dialect.batch_separator`, `Dialect::join_script` (Task 2).
- Produces: `script::statements_in(&MSSQL, sql)` returns one range per `GO` batch (the separator line excluded); `read_only::check_in(&MSSQL, sql)` accepts one batch of several statements when every one is allowed.

- [ ] **Step 1: Failing tests in `script.rs`:**

```rust
let sql = "declare @n int = 1;\nselect @n;\nGO\nselect 2\n  go  \n-- GO in a comment\nselect 'GO'";
let batches: Vec<&str> = statements_in(&MSSQL, sql).unwrap().into_iter().map(|r| &sql[r]).collect();
assert_eq!(batches, ["declare @n int = 1;\nselect @n;", "select 2", "-- GO in a comment\nselect 'GO'"]);
assert_eq!(statements_in(&MSSQL, "select 1; select 2").unwrap().len(), 1);
assert_eq!(statements_in(&POSTGRES, "select 1; select 2").unwrap().len(), 2);
```

`GO` counts only as the sole word on its line (case-insensitive, optional trailing count is out of scope and left as a statement); `GO` inside strings, comments or identifiers never splits. `statement_at_in` returns the batch under the cursor.

- [ ] **Step 2: Failing tests in `read_only.rs`:**

```rust
let ok = |sql| assert_eq!(check_in(&MSSQL, sql), Ok(()), "{sql}");
let no = |sql| assert!(check_in(&MSSQL, sql).is_err(), "{sql}");
ok("declare @n int = 3; select top (@n) * from dbo.orders");
ok("set @n = 4");
ok("select * into #t from dbo.orders; select * from #t");
ok("exec sp_help 'dbo.orders'");
ok("set statistics io on");
no("select * into dbo.copy from dbo.orders");
no("exec dbo.recalculate");
no("exec xp_cmdshell 'dir'");
no("select * from openrowset('SQLNCLI', 'x', 'select 1')");
no("update dbo.orders set total = 0");
no("waitfor delay '00:00:01'; delete from dbo.orders");
assert!(matches!(check_in(&MSSQL, "select 1\nGO\nselect 2"), Err(Rejection::MultipleStatements(2))));
```

- [ ] **Step 3: Run** `cargo test -p kuery-core script:: read_only::`. Expected: FAIL.
- [ ] **Step 4: Implement.** `statements_in`: when `batch_separator` is set, split on separator lines first, then return one range per non-empty batch (trimmed, comment-before rule kept). `check_in`: when `batch_separator` is set, split into batches; more than one → `MultipleStatements(n)`; within the batch, each parsed statement must pass `check_statement` or be a T-SQL session-local statement: `DECLARE`, `SET @var`, `SET` of session options in an allow-list (`statistics io|time|xml`, `showplan_xml`, `nocount`, `ansi_*`, `arithabort`, `quoted_identifier`, `language`, `dateformat`, `transaction isolation level`), `SELECT … INTO #t`/`##t`, `INSERT/UPDATE/DELETE/CREATE/DROP` whose target is `#…` or `@…`, and `EXEC` of the read-only allow-list `sp_help`, `sp_helptext`, `sp_helpindex`, `sp_columns`, `sp_tables`, `sp_who`, `sp_who2`, `sp_spaceused`, `sp_databases`, `sp_server_info`. Add `xp_cmdshell`, `openrowset`, `opendatasource`, `openquery`, `sp_send_dbmail`, `next value for` to the side-effect list. Other dialects keep one-statement behaviour exactly.
- [ ] **Step 5: Conformance scripts.** Build every multi-statement script in `conformance.rs` with `dialect.join_script(&[…])` so the existing counts (`statements == 2`, two preview impacts) hold for T-SQL too. Assertions do not change.
- [ ] **Step 6: Run** `cargo test --workspace`. Expected: PASS.
- [ ] **Step 7: Commit** — `git commit -m "core: GO batches and T-SQL session-local statements in read-only mode"`.

---

### Task 4: Showplan XML → `Plan`

Independent of Tasks 2–3 (pure parsing); can run in parallel.

**Files:**
- Create: `crates/core/src/plan/showplan.rs` (move `plan.rs` to `plan/mod.rs` if needed to host it), fixtures `crates/core/fixtures/plans/mssql_orders_estimated.xml`, `crates/core/fixtures/plans/mssql_orders_actual.xml`
- Modify: `crates/core/src/plan.rs` (`parse` dispatches: input starting with `<` → showplan), `crates/core/Cargo.toml` (`roxmltree`)

**Interfaces:**
- Produces: `plan::parse(&str) -> Result<Plan, String>` accepts showplan XML (both `SHOWPLAN_XML` and `STATISTICS XML`). Node mapping: `node_type` = `PhysicalOp`; `relation` = `Object@Schema.Object@Table` without brackets (`dbo.orders`); `index` = `Object@Index`; `estimated_rows` = `EstimateRows`; `total_cost` = `EstimatedTotalSubtreeCost`; `self_cost` = subtree cost minus children's; `actual` from `RunTimeInformation/RunTimeCountersPerThread` summed over threads (`ActualRows`, `ActualExecutions` as loops, `ActualElapsedms`); `details` include `Predicate`/`SeekPredicates` text, `LogicalOp`, `EstimatedExecutionMode`, warnings. `title`/`category` in plain words: `Table Scan`/`Clustered Index Scan` → "Read all of dbo.orders" (Scan); `Index Seek`/`Clustered Index Seek` → "Look up dbo.orders through IX_…" (Scan); `Key Lookup` → "Fetch the matching rows of dbo.orders" (Scan); `Nested Loops`/`Hash Match` (join)/`Merge Join` → join titles as for PostgreSQL (Join); `Hash Match`/`Stream Aggregate` (aggregate) → Aggregate; `Sort`/`Top N Sort` → Sort; `Clustered Index Insert|Update|Delete`/`Table Insert…` → Modify; rest Other. `MissingIndexes` → one insight per missing index ("An index on dbo.orders (customer_id) could help"); `Warnings` → one insight each.

- [ ] **Step 1: Capture fixtures** from the dev server (`set showplan_xml on` / `set statistics xml on` around `select o.*, c.name from dbo.orders o join dbo.customers c on c.id = o.customer_id where o.total > 100`) once Task 5's seed exists; until then hand-write a minimal valid showplan with a Nested Loops over an Index Seek and a Key Lookup plus a `MissingIndexes` block.
- [ ] **Step 2: Failing tests:**

```rust
let plan = plan::parse(include_str!("../../fixtures/plans/mssql_orders_estimated.xml")).unwrap();
assert_eq!(plan.root.category, Category::Join);
assert!(!plan.analyzed);
assert!(plan.walk().iter().any(|n| n.relation.as_deref() == Some("dbo.orders") && n.index.is_some()));
assert!(plan.walk().iter().all(|n| n.actual.is_none()));
let measured = plan::parse(include_str!("../../fixtures/plans/mssql_orders_actual.xml")).unwrap();
assert!(measured.walk().iter().all(|n| n.actual.is_some()));
assert!(measured.total_ms().is_some());
assert!(plan::parse("<ShowPlanXML/>").is_err());
assert!(plan::parse(include_str!("../../fixtures/plans/orders_before_index.json")).is_ok()); // PG unchanged
```

- [ ] **Step 3: Run** `cargo test -p kuery-core plan::`. Expected: FAIL.
- [ ] **Step 4: Implement** `showplan::parse(xml: &str) -> Result<Plan, String>` with roxmltree, taking the first `StmtSimple/QueryPlan/RelOp` and recursing through child `RelOp`s; skip PostgreSQL-only insights (buffers, hash batches) by leaving those fields at their defaults.
- [ ] **Step 5: Run** `cargo test -p kuery-core`. Expected: PASS.
- [ ] **Step 6: Commit** — `git commit -m "core: read SQL Server showplan XML into plans"`.

---

### Task 5: `kuery-mssql` adapter — connect, run, cancel, read-only, limits

**Files:**
- Modify: `crates/mssql/Cargo.toml`, `crates/mssql/src/lib.rs`
- Create: `crates/mssql/src/value.rs` (TDS value → text, `ColumnKind`), `crates/mssql/src/tests.rs`, `dev/seed-mssql.sql`
- Modify: `dev/compose.yaml` (one-shot `mssql-seed` service running `sqlcmd -C -i /seed/seed-mssql.sql`), `crates/mssql/examples/spike.rs` (delete)

**Interfaces:**
- Consumes: `MSSQL` dialect (Task 2), `statements_in` batches (Task 3), Task 1 results (cancel method, write-detection query, value rendering).
- Produces:
  - `pub struct Target { host, port, instance: Option<String>, user, password: Option<String>, database, tls: TlsMode }`, `pub enum TlsMode { Default, Disable, Require, TrustServerCertificate }`, `impl From<&ConnectOptions> for Target` (host `HOST\INSTANCE` splits into host + instance; tunnel → `127.0.0.1` + tunnel port and no instance lookup).
  - `pub fn open(target: &Target, access: Access) -> Result<Box<dyn Adapter>, String>`
  - `pub struct Mssql` implementing `Adapter` (`overview` etc. delegate to `catalog::…` from Task 6; until then return `QueryError::Unsupported`).
  - `pub static DRIVER: SqlDriver` with the Global Constraints' info; database field `Database`, placeholder `shop`, not required, help "The database queries start in; empty for the login's default database."; user field required, placeholder `sa`; host help mentions `HOST\INSTANCE`; production note: "Production connections open read-only. SQL Server cannot refuse writes by itself: Kuery blocks them and undoes any that slip through; for a guarantee, use a login with only db_datareader."

Behaviour the tests pin:
- `server()` → `SQL Server 2022 (16.0.x)` style label from `serverproperty('ProductVersion')` and the major-version name.
- `execute(sql)` runs the text as **one batch** (`simple_query`), streams every result set to the sink in order (`columns` then `row`s each time), returns the summed rows affected; text per value from `value.rs`.
- `limit(options)` → `set lock_timeout <ms>`; stores `statement_timeout`; `execute` enforces it client-side by the same path as cancel, **except** while `@@trancount > 0` outside the read-only transaction (write transactions never time out by dropping the connection).
- Cancel (`canceller()`): the Task 1 method. With drop-and-reconnect, `execute` returns `QueryError::Cancelled` when no write transaction was open, and `QueryError::Connection("Cancelled; the open transaction and its uncommitted changes were lost")` when one was; the adapter has reconnected before returning either way.
- Read-only detection: `execute("begin transaction kuery_read")` sets `reading = true`; any `rollback` clears it. While `reading`, after every batch run the Task 1 write-detection query; on `> 0`: `rollback transaction`, `begin transaction kuery_read`, return `QueryError::WritesDisabled`.
- Errors: server errors → `QueryError::Database { code: <number as text>, message }`; 1222 (lock timeout) message "Waited too long for a lock"; untrusted certificate → `Err` text containing `trust server certificate`.
- Read-only access (`Access::ReadOnly`) needs nothing at open (the session's transactions do the work); `set xact_abort off; set nocount off` at open for both.

- [ ] **Step 1: Seed.** `dev/seed-mssql.sql`: create database `shop`, login/user not needed (tests use `sa`), port `dev/seed-mysql.sql`'s schema and rows to T-SQL in schema `dbo`, plus schema `sales` with one view, one procedure, one scalar function, one trigger and one user type so the catalog has every kind. Start with `docker compose -f dev/compose.yaml up -d mssql mssql-seed`. Expected: `sqlcmd … -d shop -Q "select count(*) from dbo.orders"` prints the row count.
- [ ] **Step 2: Failing tests** in `tests.rs` (all `#[ignore]`, header comment like `crates/mysql/src/tests.rs`, env `KUERY_MSSQL_HOST`/`KUERY_MSSQL_PORT`/`KUERY_MSSQL_PASSWORD` with the dev defaults):
  - `connects_and_names_the_server`: `server()` starts with `"SQL Server 20"`.
  - `streams_every_result_set`: `select 1 as a; select N'ñandú' as b` → sink sees columns `["a"]`, row `["1"]`, then `["b"]`, `["ñandú"]`.
  - `renders_values_like_as_text`: for each type in the Task 1 table, the streamed text equals `select <MSSQL.as_text(col)>` on the same row.
  - `refuses_writes_that_slip_into_read_mode`: on a raw adapter, `begin transaction kuery_read`, `update dbo.<scratch> set qty = 0` → `Err(WritesDisabled)`; after `end_read_only`, the sum is unchanged; `select * into #t from dbo.orders` inside `kuery_read` → `Ok`.
  - `cancels_and_keeps_working`: run `waitfor delay '00:00:20'`, cancel after 300 ms → `Cancelled` in < 5 s; then `select 1` → `1`.
  - `cancel_inside_a_write_transaction_says_what_was_lost`: `begin transaction`, `insert …`, `waitfor delay '00:00:20'`, cancel → `Err(Connection(msg))` with `msg` containing `uncommitted`; afterwards the row is absent.
  - `untrusted_certificate_points_to_the_option`: `TlsMode::Require` against the dev server (self-signed) → `Err(text)` containing `trust server certificate`; `TlsMode::TrustServerCertificate` → `Ok`.
  - `lock_timeout_is_reported`: second connection holds a row lock; `limit` with 1 s lock timeout; update the same row → `Database` error with "lock" in the message.
- [ ] **Step 3: Run** `cargo test -p kuery-mssql -- --ignored`. Expected: FAIL.
- [ ] **Step 4: Implement** `lib.rs` and `value.rs` to the behaviour above (runtime like `crates/mysql/src/lib.rs::runtime`, thread name `kuery-mssql`; 15 s connect timeout with message `"{host}:{port} did not answer in 15 seconds"`).
- [ ] **Step 5: Run** `cargo test -p kuery-mssql -- --ignored` and `cargo test --workspace`. Expected: PASS.
- [ ] **Step 6: Commit** — `git commit -m "mssql: adapter with streaming, cancel, timeouts and read-only detection"`.

---

### Task 6: Catalog from `sys.*`, and conformance

**Files:**
- Create: `crates/mssql/src/catalog.rs`
- Modify: `crates/mssql/src/lib.rs` (delegate catalog methods), `crates/mssql/src/tests.rs`, `.github/workflows/ci.yml` (job `mssql`)

**Interfaces:**
- Consumes: `kuery_core::catalog` types (as `crates/mysql/src/catalog.rs` uses them).
- Produces: `overview`, `definition`, `catalog_columns`, `catalog_foreign_keys`, `search_path`, `function_definition` with the same signatures as the MySQL catalog module.

Rules: schemas from `sys.schemas` excluding `sys`, `INFORMATION_SCHEMA`, `guest` and `db_*` and schemas with no objects unless they are the user's default schema; tables `sys.tables`, views `sys.views`, routines `sys.procedures` + `sys.objects` types `FN, IF, TF`; triggers `sys.triggers` (DML, parented to their table); "enums"/types node = `sys.types where is_user_defined = 1`; columns `sys.columns` with type text like `nvarchar(40)`, `decimal(10,2)`, `nvarchar(max)`; keys/indexes `sys.indexes` + `sys.index_columns`; FKs `sys.foreign_keys` + `sys.foreign_key_columns` with `delete_referential_action_desc` → `OnDelete`; routine source `object_definition(object_id(N'[schema].[name]'))`; `search_path` = `[schema_name(), "dbo"]` deduplicated.

- [ ] **Step 1: Failing tests** (ignored) mirroring `crates/mysql/src/tests.rs`' catalog tests against `shop`: `dbo.orders` has primary key `["id"]`; an FK `orders.customer_id → customers.id`; `sales` has a view, a procedure, a function (source contains `create function`), a trigger on its table and a user type; a column named `a]b` in a scratch table round-trips through `definition` and `edit::build` (Review Focus 5); and the conformance call:

```rust
#[test]
#[ignore]
fn works_like_the_other_drivers() {
    kuery_core::conformance::run(&kuery_core::conformance::Fixture {
        opener: Arc::new(|access| open(&target(), access)),
        schema: "dbo".into(),
        slow_query: "waitfor delay '00:00:20'; select 1".into(),
    });
}
```

- [ ] **Step 2: Run** `cargo test -p kuery-mssql -- --ignored`. Expected: catalog tests and conformance FAIL.
- [ ] **Step 3: Implement** `catalog.rs`.
- [ ] **Step 4: Run** `cargo test -p kuery-mssql -- --ignored`. Expected: PASS, including `works_like_the_other_drivers`. If a conformance check fails for a reason the spec did not foresee, stop and report; do not edit `conformance.rs` assertions.
- [ ] **Step 5: CI.** Add job `mssql` modelled on `postgres`/`redis`: service `mcr.microsoft.com/mssql/server:2022-latest` on `14339:1433` with health check, a seed step running `sqlcmd` in a container on the host network, then `cargo test -p kuery-mssql --locked -- --ignored`.
- [ ] **Step 6: Commit** — `git commit -m "mssql: catalog from sys views; passes the conformance suite"`.

---

### Task 7: Plans and activity for SQL Server

**Files:**
- Modify: `crates/core/src/sql.rs` (`MSSQL.explain`), `crates/core/src/activity.rs` (`State::from_mssql`), `crates/mssql/src/lib.rs`, create `crates/mssql/src/activity.rs`, tests in `crates/mssql/src/tests.rs`, `crates/core/src/activity.rs` tests

**Interfaces:**
- Consumes: `plan::parse` with showplan (Task 4); `Activity`, `Backend`, `Signal` (`crates/core/src/activity.rs`).
- Produces:
  - `MSSQL.explain = Some(|statement, analyze| …)`: one statement only (same refusal text as `plan::explain_sql`), returns `"set showplan_xml on\nGO\n{statement}\nGO\nset showplan_xml off"` or with `statistics xml` when `analyze`. The adapter's `execute` runs `GO`-separated text batch by batch (one `execute` call may hold several batches only for this use), so the session's `first_value` sees the plan (the last result set for `statistics xml`; `Rows` keeps the last).
  - `State::from_mssql(status: &str, open_transactions: i32) -> State`: `running`/`runnable`/`suspended` → active, `sleeping` with `open_transactions > 0` → idle in transaction, `sleeping` → idle, `background` → background.
  - `Mssql::has_activity() -> true`; `activity()` from `sys.dm_exec_sessions` left join `sys.dm_exec_requests`, `sys.dm_exec_sql_text`, `sys.dm_tran_locks` (for `waiting_for`); `kind` = `"client backend"` when `is_user_process = 1` else `"background"`; `own` when `session_id = @@spid`; `blocked_by` = `blocking_session_id` when non-zero. Without `VIEW SERVER STATE` only the login's own sessions come back; `activity()` then still succeeds.
  - `signal(pid, Cancel | Terminate)` → `kill {pid}` for both; returns `false` on error 6106 (no such session).
- UI copy: where the activity view offers Cancel for this driver, its confirmation text says "SQL Server can only end the whole session" — find the confirmation in `crates/app/src/activity_tab.rs` and make it come from a `Dialect`/adapter-provided string rather than an `if`.

- [ ] **Step 1: Failing tests:** core unit test for `State::from_mssql` mapping above; ignored `tests.rs`: `explains_estimated_and_measured` (session `explain(…, false)` → `plan::parse` ok with no actuals; `true` → actuals present), `sees_a_blocker` (connection A `begin tran; update … where id = 1`, B updates the same row in background, `activity()` shows B `blocked_by == [A's spid]`), `kills_a_session` (`signal(B, Terminate)` → `true`; again → `false`).
- [ ] **Step 2: Run** `cargo test -p kuery-core activity:: && cargo test -p kuery-mssql -- --ignored`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** both commands. Expected: PASS. Then `cargo test --workspace`.
- [ ] **Step 5: Commit** — `git commit -m "mssql: query plans and server activity"`.

---

### Task 8: App registration, connection form, docs

**Files:**
- Modify: `Cargo.toml` (workspace dep), `crates/app/Cargo.toml`, `crates/app/src/drivers.rs` (`SQL` array of 4; test ids `["postgres", "mysql", "mssql", "sqlite", "redis"]` — SQL Server listed after MySQL), `crates/app/src/connection_editor.rs` only if the TLS select or host field needs the new help text wired, `README.md`, `docs/guide.md`, `.env.example`

- [ ] **Step 1: Failing test:** update `every_driver_is_found_by_its_id_once` in `drivers.rs` to the list above and add `assert!(find("mssql").unwrap().info().matches("sql server"))`.
- [ ] **Step 2: Run** `cargo test -p kuery drivers::`. Expected: FAIL.
- [ ] **Step 3: Register** `&kuery_mssql::DRIVER`. Extend `connects_through_the_dev_bastion` with an `mssql` case (`host: "mssql"`, port 1433, expecting `"Connected through localhost · SQL Server 2022"`), which needs the bastion on the same compose network (it already is).
- [ ] **Step 4: Run** `cargo test --workspace`, then `cargo run -p kuery` and, in the app, add a SQL Server connection to `localhost:14339` (`sa`, TLS "trust server certificate"), test it, open `dbo.orders`, sort, filter, page, run a script with `GO`, open a plan, open Server Activity. Expected: each works; note anything that does not.
- [ ] **Step 5: Docs.** README: add SQL Server to the engine list and Features table where engines are named; `docs/guide.md`: a SQL Server section (TLS and self-signed certs, `HOST\INSTANCE` and SQL Browser / tunnels needing a fixed port, read-only guarantee and the `db_datareader` advice, `GO` batches, cancel ending the session inside write transactions, needing `VIEW SERVER STATE` for full activity), and the dev server in the development section.
- [ ] **Step 6: Commit** — `git commit -m "app: SQL Server in the driver list, docs"`.

---

### Task 9: Check against the owner's real server

Not code. Done with the owner.

- [ ] **Step 1:** With the owner, connect to their Windows SQL Server (find host/port or `HOST\INSTANCE`; SQL login). Record `select @@version`.
- [ ] **Step 2:** If it fails on TLS (pre-TLS 1.2 server), apply the Task 1 fallback (native-tls) and re-run Task 5's tests.
- [ ] **Step 3:** Walk the same checklist as Task 8 Step 4 on one of their databases in read-only mode, then write mode on a scratch table they choose. Note version-specific failures (catalog columns or showplan attributes missing on older versions) and fix them with a test each.
- [ ] **Step 4: Commit** any fixes — `git commit -m "mssql: fixes for SQL Server <version>"`.
