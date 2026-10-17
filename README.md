# Kuery

Desktop database manager (macOS, Windows and Linux) built with gpui.

Status: early prototype. PostgreSQL, MySQL (and MariaDB), SQLite and Redis.
Several connections per project, a sidebar tree
(connections → schemas → tables, views, functions, triggers, enums), SQL
query tabs that run read-only with streamed results (or, in write mode,
inside a transaction you commit), and table tabs
with server-side filtering, sorting and paging, the table definition, and
row editing through staged changes (never on production connections).
Redis connections get a key browser, key tabs with staged edits and a console
(see [Redis](#redis)).

![A query and its results](docs/screenshots/query.png)

## Screenshots

**Profile.** Compare a measure across groups with box plots and η², or see
how a category splits over time. Nothing is computed until you press
Analyze. See [Editor](#editor).

![Order totals by status](docs/screenshots/profile-groups.png)
![Order status per hour](docs/screenshots/profile-breakdown.png)

**Plan.** Explain Analyze as a readable tree, with each step's share of the
time, estimated against actual rows and the details of the selected step, or
as a graph.

![A measured plan as a tree](docs/screenshots/plan-tree.png)
![The same plan as a graph](docs/screenshots/plan-graph.png)

**Write mode.** Before a script that writes runs, a review lists its
statements, flags the risky ones and fills in the rows each would change
from a preview that is undone. See [Write mode](#write-mode).

![Reviewing two changes](docs/screenshots/write-review.png)

**Redis.** Databases and keys grouped by `:` in the sidebar, a key tab per
type (JSON strings indented, streams as a grid) and a console. See
[Redis](#redis).

![A JSON string key](docs/screenshots/redis-string.png)
![A stream key](docs/screenshots/redis-stream.png)
![The Redis console](docs/screenshots/redis-console.png)

**Drivers.** The connection dialog picks the driver from a searchable list
grouped by family. See [Drivers](#drivers).

![Picking a driver](docs/screenshots/connection-drivers.png)

## Layout

| Crate | What it does |
|---|---|
| `crates/core` | Read-only policy and write-mode statement analysis (`sqlparser`), columnar results (Arrow), the catalog model, and escaped SQL for browsing and editing tables. No UI. |
| `crates/pg` | PostgreSQL driver on its own tokio runtime: batched streaming, `BEGIN TRANSACTION READ ONLY`, timeout, cancellation, TLS with rustls, catalog queries, transactional row changes, and write transactions with a savepoint per run. |
| `crates/redis` | Redis driver (`redis-rs`) on its own tokio runtime: a reconnecting connection per database, the keyspace, `SCAN` pages, key details, value pages per type, and console commands. TLS with the same rustls as Postgres. |
| `crates/files` | Local CSV/TSV, JSON, JSONL/NDJSON and Parquet previews with Polars, native schema metadata, and conversion to the same results as SQL. No UI. |
| `crates/app` | The gpui app (`gpui-kit`, pinned version). Custom fonts and icons in `crates/app/assets`. |

## Local data file reader (library)

`kuery-files` reads local CSV/TSV, JSON arrays of objects, JSONL/NDJSON and
Parquet with Polars 0.55.2. This is the file-reader foundation; opening data
files in the desktop UI is a subsequent integration step. It returns the
existing `ResultSet` consumed by the results grid, Profile and charts, with
native schema metadata and an explicit complete/prefix-limited scope.

```rust,ignore
use kuery_files::{FileDataset, FileSource, PreviewRequest};

let dataset = FileDataset::open(FileSource::new("orders.parquet")?)?;
let preview = dataset.preview(PreviewRequest::default())?;
// preview.results: Kuery ResultSet; preview.schema: original/inferred types.
// preview.scope: Complete or PrefixLimited, never an estimated total count.
```

Run synchronous reads on a background worker. Defaults: first 10,000 rows
(maximum 100,000), with a 64 MiB budget for the resulting Arrow text arrays.
Readers request one extra row to distinguish a complete file from a prefix.
The byte budget counts actual Arrow allocations, including buffer capacity;
it does not bound Polars/parser memory, temporary conversions or process RSS.
CSV and Parquet/NDJSON use limited lazy reads; Polars may parse beyond the
requested prefix internally. No full-file count is requested.

CSV options include comma/semicolon/tab delimiters, headers, empty-field NULL
handling and a custom NULL literal. Unquoted empty fields are NULL by default;
quoted empty strings remain empty. CSV types are inferred from up to 1,000
prefix records. A second text-only pass preserves numeric source text, even
when inference promotes a large integer to floating point. Late conflicts
inside the parsed data are errors rather than skipped rows.

JSON must contain an array of objects; JSONL has one object per nonblank line.
Conventional JSON is parsed eagerly with a 32 MiB input limit, then only its
prefix is materialized in Polars. JSONL is scanned lazily; validation passes
visit one prefix record at a time, with a 32 MiB per-record limit. Missing fields
are NULL. Validation rejects silent coercions, dropped fields and integer
precision loss, including nested values. Floating-point values use f64.
Integer literals outside the signed/unsigned 64-bit range are rejected before
JSON parsing; encode those as strings to keep
their exact value. Records outside the preview are not validated
(conventional JSON syntax/container shape and integer ranges are checked in full).

Parquet preserves native schema; nested cells display compact JSON and binary
cells display `0x` hexadecimal. Zoned timestamps use RFC 3339 offsets understood
by Profile, with the original timezone retained in schema metadata; decimal and
integer text is not routed through f64. Invalid calendar values return errors.
Profile's existing f64 calculations and timestamp-hour interpretation still
apply. A file whose size or modification time changes must be reopened; this
check detects changes but does not provide an atomic filesystem snapshot.
Sources are read-only and are never added to SQL history or project settings.

To inspect a reader result without showing cell values:

```bash
CARGO_TARGET_DIR=/private/tmp/kuery-polars-target cargo run -p kuery-files --example preview_bench -- /path/to/orders.parquet 10000
CARGO_TARGET_DIR=/private/tmp/kuery-polars-target cargo test -p kuery-files --locked
```

`preview_bench` reports elapsed time, result bytes, scope and schema. For peak
RSS on macOS, run the built example with `/usr/bin/time -l`; result bytes alone
are not a measure of total memory use. The target directory above keeps build
artifacts on the internal disk when the checkout is on an external disk.

## Interface

The window has a title bar (project switcher, the search that opens the
command palette, sidebar and panel toggles), an activity rail on the left
(Explorer, Saved Queries, History; Audit Log and Keyboard Shortcuts at the
bottom; clicking the active view hides the sidebar), the sidebar, tabs, and a
status bar with the active connection and environment, the tab's mode or
transaction, the last run and the cursor position.

Toolbars put actions behind icon buttons whose tooltips name the shortcut.
There are no colored banners: a query tab's mode is the Read / Write / Full
control, an open transaction or staged row changes are a pill with their
actions, and before a script that writes runs, a review dialog lists its
statements, flags the risky ones, and fills in the rows each one would
change when you click Preview Impact.

## Development

```bash
docker compose -p kuery-dev -f dev/compose.yaml up -d   # Postgres with 1,000,000 orders and an analytics database, MySQL with a shop, a seeded Redis and an SSH bastion
cargo run -p kuery-sqlite --example seed                # dev/shop.sqlite, for the local-sqlite connection
cargo run -p kuery -- .                                 # the app, on this repo's project
cargo test --workspace                                    # tests without a database
cargo test -p kuery-pg -- --ignored                     # tests against the dev Postgres
cargo test -p kuery-redis -- --ignored                  # tests against the dev Redis
cargo test -p kuery-mysql -- --ignored                  # tests against the dev MySQL
cargo test -p kuery -- --ignored                        # SSH tunnels through the dev bastion
cargo run --release -p kuery-pg --example stream_bench  # streaming without a UI
```

## Projects

A project is a folder. Kuery keeps everything for it in `.kuery/` inside
that folder, so it can live in a code repository:

```text
<folder>/.kuery/
├─ project.toml      project name and connections (no passwords)
├─ queries/          saved queries as .sql files, in any folder layout
└─ diagrams/         where diagram cards were moved, per connection and schema
```

Open a folder from the welcome window, with "Open Project…" (`secondary-o`),
or from the command line with `cargo run -p kuery -- <folder>`; a folder
without `.kuery/` becomes a new project. Opening a project from a project
window asks whether it takes over that window (after confirming if tabs have
unsaved changes) or gets a new one; a project that is already open is just
brought to the front.

Recent projects are listed in the welcome window (where each can be removed
from the list or revealed in the file manager, and missing folders are marked),
under File › Open Recent, in the project switcher on the title bar (click the
project name) and in the command palette once you type. "Clear Recent
Projects" empties the list; it lives in `recent.toml` in the user config
directory.

Connections are added and edited from the sidebar (`+` and the pencil next to
each one) in a dialog, with "Test Connection" before saving. Passwords go to the system
keychain (Keychain on macOS, Credential Manager on Windows, Secret Service on
Linux) under the connection's id, never into `project.toml`. A connection's
`environment` (development, staging or production) is detected from the host
if not set; production connections open read-only (the session also sets
`default_transaction_read_only`) and the explorer offers no row editing.

### SSH tunnels

Any connection can go through an SSH host ("Connect through an SSH host" in
the dialog): Host and Port are then the server as the SSH host sees it, and
the SSH host, port, user and identity file go under `[connections.ssh]`:

```toml
[connections.ssh]
host = "bastion.example.com"   # or a ~/.ssh/config alias
port = 22                      # optional
user = "deploy"                # optional
identity_file = "~/.ssh/id_ed25519"  # optional
```

The tunnel is the system's `ssh` (OpenSSH) forwarding a local port, so it
follows `~/.ssh/config` (aliases, `ProxyJump`, keys), the SSH agent and
`known_hosts`; empty fields are left to them. It never asks anything: it
signs in by key or agent, never with an SSH password; a host seen for the
first time is added to `known_hosts`, and one whose key changed is refused.
It opens with the connection and closes with it. Through SSH the
environment is detected from the SSH host (the server's `localhost` is the
SSH host's), and Postgres still checks TLS certificates against the
server's name. Redis over TLS through a tunnel checks them against
`127.0.0.1`, so it needs certificates that allow it.

The dev compose file has an SSH bastion (`localhost:22229`, user `kuery`,
no password) and the repo's project a `local-dev-ssh` connection that reaches
the dev Postgres through it.

## Drivers

The connection dialog picks the driver from a searchable list, grouped by
family (type to find it by name, alias such as `pg`, or family). A
connection stores the driver's id (`driver = "redis"` in `project.toml`;
connections without one are Postgres), and the rest of the form follows the
driver: its default port, what its database and user fields mean and whether
they are required, and how it asks about TLS.

A SQL driver is an adapter: everything that differs between databases,
and nothing else. Everything that must work the same lives once, in core:

```
            app (query tabs, table tabs, explorer, write mode…)
                                  │  one type: Session
┌─────────────────────────────────▼──────────────────────────────────┐
│ kuery_core::session::Session                                        │
│  read-only in two layers · write mode (transaction, a savepoint per │
│  script) · full writes (a whole script or nothing) · previews that   │
│  undo · row edits with their checks · batches · timeouts · cancel · │
│  catalog views (completion, diagrams) · a thread per connection     │
└─────────────────────────────────┬──────────────────────────────────┘
                     kuery_core::adapter::Adapter
        ┌──────────────┬──────────┴──────┬──────────────┐
     kuery-pg      kuery-mysql     kuery-sqlite     kuery-<new>
```

- **The adapter** (`kuery_core::adapter::Adapter`) is one blocking
  connection: it runs one statement exactly as given (rows to a sink, or the
  rows it changed), sets the statement and lock timeouts, says how to stop
  what runs, and reads the catalog in bulk (overview, a table's definition,
  columns, foreign keys, the search path; optionally a routine's source and
  the server's activity). A driver over an async client waits on its own
  runtime; the session gives each connection a thread of its own.
- **The dialect** (`kuery_core::sql::Dialect`) is data, not code: how it
  quotes, casts to text, matches without case, sorts NULLs, compares with
  NULL, opens read-only transactions and savepoints, whether schema changes
  commit by themselves (MySQL: refused wherever an undo is promised), and
  how to EXPLAIN for the Plan tab. `POSTGRES`, `MYSQL` and `SQLITE` are
  there; a new driver can bring its own.
- **The registration** is one `DRIVER` (`kuery_core::driver::SqlDriver`)
  per crate: the descriptor the form follows (id, name and aliases, default
  port, the database and user fields, TLS choices, whether it is a file),
  the dialect, and how to open an adapter from the same `ConnectOptions`
  every driver gets. The app lists them in `drivers::SQL`, and connects,
  tests, tunnels through SSH and reopens (write mode, Server Activity) the
  same way for all.

Adding a SQL database means a crate with an adapter, its dialect and its
`DRIVER`, one line in `drivers::SQL`, and passing the conformance tests
(`kuery_core::conformance`, behind the `conformance` feature): the same
checks every driver runs against its real database, so behaving like the
others is tested, not promised. The session's own rules are tested once,
against a fake adapter. Redis is the key-value family, with its own session,
sidebar and tabs.

### MySQL and MariaDB

A MySQL connection asks for a host, a user and optionally the database to
start in; the sidebar lists every database the user can see as a schema
(MySQL's own are left out), with tables, views, functions, procedures and
triggers. Read-only queries run inside `START TRANSACTION READ ONLY`, and
production connections also set `SESSION TRANSACTION READ ONLY`. Values show
as the server writes them, so row edits compare against exactly that.

MySQL commits `CREATE`, `ALTER`, `DROP`, `TRUNCATE` and a few others by
itself, so nothing can undo them: write mode refuses them inside its
transaction and in previews, and runs them in Full write. Cancelling a query
kills it on the server (`KILL QUERY`) from a side connection.

### SQLite

A SQLite connection is a file: the dialog asks for its path (relative paths
start at the project folder, `~/` works too), and opening a path where there
is no database is an error rather than an empty new file. Its attached
databases (`main`) are the schemas. Read-only queries also run with `PRAGMA
query_only`; production opens the file read-only; read-write connections turn
foreign keys on, so their cascades and checks apply. Write mode uses
savepoints like the other drivers, and a running statement can be cancelled
or stopped by its timeout.

## Redis

For Redis the database is the index the console starts on, the user an
optional ACL user, and TLS is on or off (with the server's certificate
checked against the system's roots). Standalone servers only, for now.

In the sidebar a Redis connection lists its databases with their key
counts (empty ones are left out, except the default). Expanding one reads its
keys with `SCAN`, a thousand at a time ("Load more keys" reads on; `KEYS`,
which blocks the server, is never used), grouped into folders by `:`. The
sidebar's filter becomes the `SCAN` pattern: `user` looks for `*user*`, and
anything with `*`, `?` or `[` is used as typed.

A key opens in a tab with its type, time to live, length, memory and
encoding. Strings show as text, JSON indented (Pretty / Raw), up to the first
MB. Hashes, lists, sets, sorted sets and streams fill the results grid 500
entries at a time; stream fields become columns.

The console (from a database's hover icon or menu, or a connection's menu)
runs commands one per line, as `redis-cli` reads them, with command-name
completion and shell-style highlighting (there is no Redis grammar; Bash colors
the command, strings, numbers and comments): `secondary-enter` runs the line under the cursor or the selected
lines, `secondary-alt-enter` every line. The last reply fills the grid; the
Output tab logs each reply the way `redis-cli` prints it. Commands that would
change or hold the connection never run (`SELECT`, `SUBSCRIBE`, `MONITOR`,
blocking pops, `MULTI`; pick the database in the toolbar instead), and `KEYS`
runs on development connections only.

### Writing to Redis

The console starts in Read-only, where commands that write are refused; its
Read-only / Write control switches. Redis has no transactions, so there is
no transactional mode: each command applies as soon as it runs and cannot be
rolled back. Writes follow the environment, as in Postgres write mode:

- development asks only before risky commands: `FLUSHDB`, `FLUSHALL`, `DEL`
  or `UNLINK` of ten keys or more, `CONFIG SET`, `SHUTDOWN`, `DEBUG`,
  `SWAPDB`, `CLIENT KILL`, ACL and module changes, and a `RENAME` (or `COPY …
  REPLACE`) onto a key that exists;
- staging asks before every write;
- production needs writes allowed for the connection session first (typing
  its name; never saved, gone when it disconnects; "Stop allowing writes" in
  the status bar takes it back), then asks before every write.

The confirmation lists the commands in order, flags the risky ones, and shows
what each key they change holds now (type, size, time to live and the start
of the value, or "new key"). Every write that runs is logged to the audit
log, with its connection, database and outcome.

A key tab stages changes and applies them the same way: a new string value
(edited as text, keeping the time to live), the time to live, a new name,
deleting the key, and adding entries (+) or changing and removing them from a
row's menu (hash fields, list items at the head or tail, set and sorted-set
members and scores, stream entries). The staged list shows each change with
its command; Apply runs them in order, stops at the first that fails and
keeps the rest, then reloads the key. Removed list items are first set to a
marker and removed together, so indexes do not move under the other changes.
On production nothing can be staged until writes are allowed ("Allow
writes…" in the tab).

The dev compose starts a Redis on port 63799 with every key type, keys that
expire, 1,500 products to page through and a second database; the repo's
project has a `local-cache` connection to it, and `prod-cache`, the same
server marked as production, to try the production rules.

## Diagrams

A schema's diagram opens from its menu in the sidebar ("Open Diagram") or
with "Open Schema Diagram" in the command palette (the schema of the table in
view, else `public`). Each table is a card with its columns (the primary
key's in amber, foreign keys' in blue, types in their short form, `?` when
nullable), or only its keys with "Keys only".

Foreign keys are lines in crow's foot notation, from the referencing column
(three prongs: many) to the key it refers to (two bars: one); a dashed line
with a ring is an optional relation (the column may be NULL). The layout
reads one way: tables others refer to sit on the left and the tables that
depend on them to their right, each layer ordered to cross as few lines as
it can (down to the row each line leaves from) and each card level with the
tables it relates to; tables without relations are set apart on the right.
A relation that skips columns passes through the gaps between their cards
instead of over them.

Cards can be dragged; they snap to a 20 pt grid, and a card dropped on
others pushes them out of its way. Where cards were left is saved in
`.kuery/diagrams/<connection>-<schema>.toml` (only positions, fine to commit).
"Untangle" tidies an arrangement up keeping its columns and about where
each card is: it reorders each column to cross fewer lines, lines cards up
with the tables they relate to and pulls apart the ones that overlap.
"Reset layout" forgets the saved places and goes back to the automatic
layout.

Clicking a table highlights it, its relations and its neighbors, and the
footer says what it refers to (with its delete rule when it cascades or sets
NULL) and what refers to it; double-clicking opens it. With a table selected,
"Neighbors" and "2 hops" narrow the diagram to the tables within one or two
relations of it, laid out on their own ("Whole schema" goes back; cards moved
there are not saved). The diagram zooms in steps (click the percentage for
100%) or to fit the view, and exports what is shown as an SVG image, a
Mermaid `erDiagram` (`.mmd`) or DBML (`.dbml`), to a file or the clipboard.

## Server activity

"Server Activity" in a Postgres connection's menu (or "Show Server
Activity" in the command palette) opens a tab with the server's sessions:
state, how long each has been at it (its query, or its open transaction
when idle in one), user and application, the lock it waits for and its
query. It refreshes every two seconds on a connection of its own, so a long
query on the others never holds it up; pause it to read. "Busy" shows
sessions that run something, hold a transaction open or block others; "All
sessions" adds idle ones and background processes.

Sessions that need a look stand out: those blocking others (a banner names
the root of each chain and how many sessions wait on it), those waiting,
queries running for over a minute and transactions idle for over a minute,
which keep their locks. Selecting a session shows its query in full, to
copy or open in a query tab, and offers to cancel its query or end the
session. Both are confirmed first; on production they need writes allowed
for the connection session, like any write, and both go to the audit log.

## Write mode

Each query tab has three modes, chosen in the toolbar's Read / Write / Full
control or from the command palette ("Read-only Mode", "Write Mode (Transactional)", "Full Write
Mode"):

- **Read-only** (the default): nothing can change, as described above.
- **Write**: changes stay in a transaction until you commit them (below).
- **Full write**: each script is applied as soon as it finishes, all of it or
  nothing (PostgreSQL runs a multi-statement script as one implicit
  transaction), with no Roll Back. A single statement may be one that cannot
  run in a transaction block, such as `VACUUM`. Switching to it with a
  transaction open asks to commit or roll back first.

In both write modes:

- The tab opens a connection of its own. In Write, the first script that writes opens a
  transaction that stays open until **Commit** or **Roll Back** in the
  toolbar, next to the statements and rows changed so far. Reads in
  that tab see the uncommitted changes; other tabs and sessions do not.
- In Write, every run gets a savepoint: if a script fails, only its own changes
  are undone and the transaction stays usable.
- Scripts are analysed first. `BEGIN`, `COMMIT` and friends are refused (the
  transaction belongs to the tab), and `UPDATE`/`DELETE` without `WHERE`,
  `TRUNCATE`, `DROP`, dropping columns and `GRANT`/`REVOKE` are flagged.
- What is confirmed depends on the environment: development asks before risky
  statements, staging before every change, production before every change and
  refuses SQL it cannot analyse.
- Production needs writes allowed first, by typing the connection's name, and
  full write needs a second, separate permission (typing the name again). Both
  last for the connection session only (until it disconnects or the app
  quits), are never saved in the project, and "Stop allowing writes" in the
  status bar ends them. A thin amber line under the title bar marks a
  production connection.
- Writes have a 60 s `statement_timeout` and a 5 s `lock_timeout`. A
  transaction with no runs for 10 minutes is rolled back. Closing the tab asks whether to commit; closing the
  window or turning write mode off with a transaction open asks first too,
  and so does quitting (which also asks about unsaved SQL and staged row
  changes).

- **Preview Impact**, in the confirmation or as a command (palette and the
  editor's menu), runs the script inside a transaction (or under a savepoint
  in the open one), lists the rows each statement would change or return,
  and undoes it all; in the review dialog the counts fill its Rows column,
  and Run then runs the script for real. Statements that cannot run in a transaction block (`VACUUM`)
  cannot be previewed.
- Every write run, preview, commit and rollback is appended to an **audit
  log**, `audit.jsonl` in the user config directory (one JSON object per
  line: time, project, connection, environment, mode, action, SQL,
  statements and rows changed, outcome). "Open Audit Log" in the palette
  opens it.

Table tabs keep their own staged row editing, which stays off on production.

## Context menus

Right-click shows what can be done with the thing under the pointer:

- **Sidebar** — a connection: connect or disconnect, new query, refresh the
  catalog, edit, copy its host and database, delete. A schema: new query,
  refresh, copy its name. A table or view: open its data or definition, query
  it in a new tab, copy its name or a `SELECT`. Functions, triggers and enums:
  open them and copy names, signatures or values. Saved queries and folders:
  open, reveal in the file manager, copy the path, delete (asks first).
  History: open in a new tab, copy the SQL, remove it.
- **Tabs** — close, close others, close tabs to the right, close all (tabs with
  unsaved work are left for last and ask one at a time), and reveal or copy
  the file of a saved query or the name of a table.
- **Grids** — copy the value, the row (tab-separated or as JSON) or the column
  name. In table tabs also edit the row, stage its deletion, or filter by the
  value.
- **SQL editor** — run (or run the selection), cancel, cut, copy, paste,
  select all, save, and switch between Read-only, Write and Full write.

## Editor

In the SQL editor `secondary-enter` runs the selection or, without one, the
statement under the cursor (statements are split on semicolons outside
strings, comments and `$$` bodies); `secondary-alt-enter` runs everything, and
`secondary-shift-enter` inserts a new line. `secondary-shift-f` formats the
selection or the whole script (one clause per line, keyword case and function
bodies left as written; undo brings the old text back). Command shortcuts are also bound
inside text fields, so a field's own keys never shadow them.

The editor completes as you type: keywords (in the case you type them), tables
and views after `FROM`, `JOIN`, `UPDATE` and `INTO` (those outside the
`search_path` schema-qualified), and columns of the tables the statement reads,
also through an alias (`o.` in `select o.| from orders o`). `ctrl-space` asks
for suggestions where typing alone does not, like an empty select list. The
tab loads the catalog when it opens and again after DDL runs or a commit.

In a query tab the editor and the bottom panel are split by a divider you can
drag (double-click it to reset). The panel has a Results tab with the grid, a
Profile tab, a Plan tab and an Output tab that logs each run (SQL, rows and
time, or the error); it collapses to its tab strip with `secondary-j`, its
button or a double-click on the strip, and running a query brings it back.

Plan shows how PostgreSQL runs the statement under the cursor (or the
selection), made readable:

- Explain (`secondary-e`, or the plan button) asks for the estimated plan
  without running anything, so it is safe for any statement, writes
  included.
- Explain Analyze (`secondary-alt-e`, or alt-click the plan button) runs the
  query and measures every step, with buffers. Reads run read-only. A
  statement that writes needs Write or Full write and a yes: it runs for real
  and is rolled back (the data stays as it was, but triggers fire and
  sequences advance), and it is logged to the audit log.
- An `EXPLAIN` typed in the editor also lands in Plan: as a tree when it
  asked for `FORMAT JSON`, as text otherwise (with buttons to explain it
  again as a tree).

The tab starts with a summary (measured or estimated, execution and planning
time, rows, cost, and how much data came from the cache or was read), then
insights in plain words: a full scan that throws most rows away (an index
would help), an estimate off by 10× or more (fresh statistics: `ANALYZE`), a
sort or hash that spilled to disk (`work_mem`), an inner step repeated
thousands of times, and the step that takes most of the time. Below is the
plan as a tree: each step says what it does ("Read all of orders", "Join
through a hash table") with an icon for its kind, estimated against actual
rows (flagged when far off), its time (or cost) and a bar with the share it
takes by itself. Clicking a step opens its conditions, keys, loops,
discarded rows and buffers; the hottest step starts open. The JSON can be
copied.

Tree / Graph switches the plan to a graph: a box per step, root at the top,
joined by lines that thicken with the rows flowing through them. Each box
shows the step's title, its rows and its share, coloured like the tree's
bars; clicking one shows its details and insights beside the graph.

The tab keeps the previous plan. Once there is one, "Compare with previous"
puts the two side by side, for the "before and after creating an index"
workflow: total time and cost before → after (how many times faster), the
steps that changed ("Read all of orders" → "Fetch the matching rows of
orders", with the step types), the ones that are new or gone, the time spent
on each table (or estimated cost, when either plan is an estimate), and the
insights that went away, appeared or are still there. Steps are paired by
table, index and step type, from the strictest match to the loosest.
Clicking a changed step shows it in the plan.

Profile computes nothing until asked. The list on the left shows every
column's name and kind ("Profile all" adds nulls, distinct values and a small
chart to each). Above the result, a builder picks a column (click it, or the
first dropdown), optionally a second one to compare (⌘-click, or the "vs"
dropdown), and an analysis; Analyze runs it. Double-clicking a column
analyzes it at once. While a result is on screen, its options (bins, groups,
periods, aggregate…) update it right away; changing the columns or the
analysis waits for Analyze.

One column, "Analyze as":

- Auto chooses from the kind and the values: keys get their range,
  booleans a true/false split, integers with few values and short lists of
  text their categories, other numbers a histogram, dates a timeline.
- Numeric: a histogram (bins, log scale, hiding outliers beyond 1.5 × IQR)
  with mean, standard deviation and percentiles.
- Categorical: the most frequent values with their share (top 5 to 50,
  NULL included on request).
- Temporal: counts by hour, day, week, month or year (too many buckets fall
  back to a coarser one), or by weekday or hour of day to show patterns,
  as a line or as bars. Hours are read as written, offset ignored.
- Text: the distribution of lengths, blank values and the most frequent
  values.

Two columns, offered by what their kinds allow (integers count both as
numbers and as categories):

- By group (number × category): a box plot per group on a shared scale with
  n, mean, median, min and max, and η², the share of the variance the
  groups explain. Smaller groups fold into "Other".
- Cross tab (category × category): counts per pair, shaded, optionally as
  row percentages, with Cramér's V for the strength of the association.
- Over time (date × number): sum, avg, min, max, median or count per
  period, as a line (broken where a period has no values) or as bars.
- Breakdown over time (date × category): stacked bars per period, as counts
  or 100%.
- Correlation (number × number): Pearson and Spearman coefficients, r², and
  a scatter plot with the least-squares line (up to 4,000 points, spread
  evenly over the rows; the coefficients use every pair).

Hidden columns (the eye on a row) and the builder's choices are kept by
column name while the tab is open, so re-running a query keeps them (the
previous result is dropped: its rows are gone). Kinds come from Postgres
when a single read runs (the query is prepared first to learn them; numeric
cells then align right in the grid); otherwise they are inferred from the
first thousand values, and the result says so. Everything runs in the
background over the rows the grid holds.

The download icon in the panel (or `secondary-shift-e`, the palette or the editor's
menu) saves the results to a file: the extension picks CSV (RFC 4180), TSV
(tabs and line breaks escaped) or JSON (an array of objects; values as text,
NULL as `null`). It writes in the background; a million rows take well under
a second.

The sidebar's History lists what ran, newest first, with its connection,
outcome and how long ago. It survives restarts: the last 200 entries of each
project are kept in `history/` in the user config directory, never in the
project folder (the SQL may hold data). "Clear" empties it.

Query tabs save with `secondary-s` into `.kuery/queries/`. The first line of
a saved query, `-- connection: <name>`, records its connection and is not
shown in the editor; opening the file from the sidebar's Queries list opens
that connection first if needed.

The repository is itself a project: `cargo run -p kuery -- .` opens it with
connections to the dev Postgres (which trusts local connections, so no
password is needed) and a couple of saved queries.

## Commands and shortcuts

Every command lives in one registry (`crates/app/src/commands.rs`) with a
stable id, a title, its default shortcuts and the key context they apply in.
The command palette (`secondary-k` or `secondary-shift-p`) searches them, plus
the tables of the open connections and the recent projects, and runs the same
action a shortcut would.

Shortcuts can be changed in `keymap.toml` in the user config directory
(`~/Library/Application Support/kuery` on macOS, `~/.config/kuery` on
Linux, `%APPDATA%\kuery` on Windows; `KUERY_KEYMAP` overrides the path).
"Open Keymap File" creates it with every command listed, and "Reload Keymap"
applies changes without restarting:

```toml
[bindings]
"cmd-w" = "tab.close"     # shortcut = command id
"secondary-t" = ""        # an empty command unbinds a default shortcut
```

`secondary` is `cmd` on macOS and `ctrl` elsewhere. By default
`secondary-w` closes the active tab and `secondary-shift-w` the window.

## Writing data

SQL query tabs never write. Rows are edited only from a table tab: each change
becomes an `UPDATE`, `INSERT` or `DELETE` generated by `kuery_core::edit`,
addressed by primary key and, for updates, guarded by the values the row had
when it was loaded. The SQL is shown before saving, and all staged changes run
in one transaction that rolls back if any statement does not touch exactly one
row. Views and tables without a primary key are read-only.

## TLS

The URL's `sslmode` follows libpq semantics (`disable`, `allow`, `prefer`,
`require`, `verify-ca`, `verify-full`), with `sslrootcert` for a PEM file of
root certificates; without it the system ones are used. Without `sslmode`, a
local database (`localhost`, `127.0.0.1`, `::1` or a Unix socket) connects
without TLS and everything else uses `prefer`. Details in `crates/pg/src/tls.rs`.

The TLS tests need a Postgres with `ssl = on`; if `KUERY_TLS_DATABASE_URL`
is not set, they do nothing:

```bash
docker run -d --rm --name kuery-tls -p 54330:5432 \
  -e POSTGRES_USER=kuery -e POSTGRES_PASSWORD=kuery -e POSTGRES_DB=kuery \
  postgres:16 -c ssl=on \
  -c ssl_cert_file=/etc/ssl/certs/ssl-cert-snakeoil.pem \
  -c ssl_key_file=/etc/ssl/private/ssl-cert-snakeoil.key
KUERY_TLS_DATABASE_URL=postgres://kuery:kuery@localhost:54330/kuery \
  cargo test -p kuery-pg -- --ignored
```

## Fonts and icons

Kuery's application icon is `crates/app/assets/branding/kuery.png` (1024 × 1024,
with transparency). It is embedded in the welcome screen, sets the macOS Dock
icon even during development, and is passed to X11 windows. Windows builds embed
the multi-size `kuery.ico` as resource 1. The matching `kuery.icns` is available
for macOS app bundles. The image was generated with the built-in ImageGen tool;
the complete prompt is recorded in `crates/app/assets/branding/generation.txt`.

The UI uses IBM Plex Sans, and the editor and table use JetBrains Mono, both
licensed under the SIL Open Font License 1.1 (`crates/app/assets/fonts/*/OFL.txt`).
They are embedded in the binary and registered with gpui's text system at
startup. The icons missing from gpui-kit's default set (lock, database, table,
key, link, funnel, sort, trash, code, pencil, refresh, bottom panel) come from Lucide (ISC, `crates/app/assets/icons/LICENSE-LUCIDE`) and are served
by `AppAssets`, which delegates the rest to `gpui_kit::assets::Assets`.

## CI

`.github/workflows/ci.yml` runs `cargo fmt`, `clippy -D warnings` and
`cargo test --workspace` on macOS, Windows and Linux, plus the ignored
`kuery-pg` tests against `postgres:16` (and a second Postgres with TLS) on Linux.
The Rust toolchain is pinned in `rust-toolchain.toml` (gpui uses APIs stabilized after Rust 1.93).

`.github/workflows/release.yml` builds release binaries for Linux (x86_64),
macOS (Apple Silicon and Intel, as an ad-hoc signed `Kuery.app`) and Windows
(x86_64). Pushing a tag publishes them as a GitHub release with a
`SHA256SUMS` file:

```sh
git tag v0.1.0 && git push origin v0.1.0
```

Running the workflow by hand (Actions → Release → Run workflow) only builds the
archives and keeps them as workflow artifacts.
