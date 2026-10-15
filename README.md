# dbadmin

Desktop database manager (macOS, Windows and Linux) built with gpui.

Status: early prototype. Several connections per project, a sidebar tree
(connections → schemas → tables, views, functions, triggers, enums), SQL
query tabs that always run read-only with streamed results, and table tabs
with server-side filtering, sorting and paging, the table definition, and
row editing through staged changes (never on production connections).

## Layout

| Crate | What it does |
|---|---|
| `crates/core` | Read-only policy (`sqlparser`), columnar results (Arrow), the catalog model, and escaped SQL for browsing and editing tables. No UI. |
| `crates/pg` | PostgreSQL driver on its own tokio runtime: batched streaming, `BEGIN TRANSACTION READ ONLY`, timeout, cancellation, TLS with rustls, catalog queries, and transactional row changes. |
| `crates/app` | The gpui app (`gpui-kit`, pinned version). Custom fonts and icons in `crates/app/assets`. |

## Development

```bash
docker compose -p dbadmin-dev -f dev/compose.yaml up -d   # Postgres with 1,000,000 orders and an analytics database
cargo run -p dbadmin -- .                                 # the app, on this repo's project
cargo test --workspace                                    # tests without a database
cargo test -p dbadmin-pg -- --ignored                     # tests against the dev Postgres
cargo run --release -p dbadmin-pg --example stream_bench  # streaming without a UI
```

## Projects

A project is a folder. dbadmin keeps everything for it in `.dbadmin/` inside
that folder, so it can live in a code repository:

```text
<folder>/.dbadmin/
├─ project.toml      project name and connections (no passwords)
└─ queries/          saved queries as .sql files, in any folder layout
```

Open a folder from the welcome window, with "Open Project…" (`secondary-o`),
or from the command line with `cargo run -p dbadmin -- <folder>`; a folder
without `.dbadmin/` becomes a new project. Opening a project from a project
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

In the SQL editor `secondary-enter` runs the query (or the selection) and
`secondary-shift-enter` inserts a new line. Command shortcuts are also bound
inside text fields, so a field's own keys never shadow them.

In a query tab the editor and the bottom panel are split by a divider you can
drag (double-click it to reset). The panel has a Results tab with the grid and
an Output tab that logs each run (SQL, rows and time, or the error); it
collapses to its tab strip with `secondary-j`, its button or a double-click on
the strip, and running a query brings it back.

Query tabs save with `secondary-s` into `.dbadmin/queries/`. The first line of
a saved query, `-- connection: <name>`, records its connection and is not
shown in the editor; opening the file from the sidebar's Queries list opens
that connection first if needed.

The repository is itself a project: `cargo run -p dbadmin -- .` opens it with
connections to the dev Postgres (which trusts local connections, so no
password is needed) and a couple of saved queries.

## Commands and shortcuts

Every command lives in one registry (`crates/app/src/commands.rs`) with a
stable id, a title, its default shortcuts and the key context they apply in.
The command palette (`secondary-k` or `secondary-shift-p`) searches them, plus
the tables of the open connections and the recent projects, and runs the same
action a shortcut would.

Shortcuts can be changed in `keymap.toml` in the user config directory
(`~/Library/Application Support/dbadmin` on macOS, `~/.config/dbadmin` on
Linux, `%APPDATA%\dbadmin` on Windows; `DBADMIN_KEYMAP` overrides the path).
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
becomes an `UPDATE`, `INSERT` or `DELETE` generated by `dbadmin_core::edit`,
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

The TLS tests need a Postgres with `ssl = on`; if `DBADMIN_TLS_DATABASE_URL`
is not set, they do nothing:

```bash
docker run -d --rm --name dbadmin-tls -p 54330:5432 \
  -e POSTGRES_USER=dbadmin -e POSTGRES_PASSWORD=dbadmin -e POSTGRES_DB=dbadmin \
  postgres:16 -c ssl=on \
  -c ssl_cert_file=/etc/ssl/certs/ssl-cert-snakeoil.pem \
  -c ssl_key_file=/etc/ssl/private/ssl-cert-snakeoil.key
DBADMIN_TLS_DATABASE_URL=postgres://dbadmin:dbadmin@localhost:54330/dbadmin \
  cargo test -p dbadmin-pg -- --ignored
```

## Fonts and icons

The UI uses IBM Plex Sans, and the editor and table use JetBrains Mono, both
licensed under the SIL Open Font License 1.1 (`crates/app/assets/fonts/*/OFL.txt`).
They are embedded in the binary and registered with gpui's text system at
startup. The icons missing from gpui-kit's default set (lock, database, table,
key, link, funnel, sort, trash, code, pencil, refresh, bottom panel) come from Lucide (ISC, `crates/app/assets/icons/LICENSE-LUCIDE`) and are served
by `AppAssets`, which delegates the rest to `gpui_kit::assets::Assets`.

## CI

`.github/workflows/ci.yml` runs `cargo fmt`, `clippy -D warnings` and
`cargo test --workspace` on macOS, Windows and Linux, plus the ignored
`dbadmin-pg` tests against `postgres:16` (and a second Postgres with TLS) on Linux.
The Rust toolchain is pinned in `rust-toolchain.toml` (gpui uses APIs stabilized after Rust 1.93).
