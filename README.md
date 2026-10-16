# Kuery

Desktop database manager (macOS, Windows and Linux) built with gpui.

Status: early prototype. Several connections per project, a sidebar tree
(connections → schemas → tables, views, functions, triggers, enums), SQL
query tabs that run read-only with streamed results (or, in write mode,
inside a transaction you commit), and table tabs
with server-side filtering, sorting and paging, the table definition, and
row editing through staged changes (never on production connections).

## Layout

| Crate | What it does |
|---|---|
| `crates/core` | Read-only policy and write-mode statement analysis (`sqlparser`), columnar results (Arrow), the catalog model, and escaped SQL for browsing and editing tables. No UI. |
| `crates/pg` | PostgreSQL driver on its own tokio runtime: batched streaming, `BEGIN TRANSACTION READ ONLY`, timeout, cancellation, TLS with rustls, catalog queries, transactional row changes, and write transactions with a savepoint per run. |
| `crates/app` | The gpui app (`gpui-kit`, pinned version). Custom fonts and icons in `crates/app/assets`. |

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
docker compose -p kuery-dev -f dev/compose.yaml up -d   # Postgres with 1,000,000 orders and an analytics database
cargo run -p kuery -- .                                 # the app, on this repo's project
cargo test --workspace                                    # tests without a database
cargo test -p kuery-pg -- --ignored                     # tests against the dev Postgres
cargo run --release -p kuery-pg --example stream_bench  # streaming without a UI
```

## Projects

A project is a folder. Kuery keeps everything for it in `.kuery/` inside
that folder, so it can live in a code repository:

```text
<folder>/.kuery/
├─ project.toml      project name and connections (no passwords)
└─ queries/          saved queries as .sql files, in any folder layout
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
Profile tab and an Output tab that logs each run (SQL, rows and time, or the
error); it collapses to its tab strip with `secondary-j`, its button or a
double-click on the strip, and running a query brings it back.

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
