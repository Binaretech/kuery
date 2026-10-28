# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.5](https://github.com/Binaretech/kuery/compare/v0.4.4...v0.4.5) - 2026-10-28

### Changed

- *(app)* open table, diagram, activity and Redis key tabs through focus_or_open
- *(app)* share data file tabs between the project and Data files windows
- quote identifiers only through the SQL dialect
- *(core)* give drivers one place that decides the tunnel address and TLS host name
- *(mysql)* drop the conversion for values the text protocol never returns
- *(pg)* drop the sslrootcert support no connection could use

### Fixed

- check the Redis server certificate against its host name through an SSH tunnel
- *(app)* add the tab right-click menu to the Data files window
- *(app)* show a data file's path when hovering its tab in the Data files window
- *(app)* say when the audit log can't be written instead of failing silently
- *(app)* save recent projects and diagram layouts without risking half-written files
- *(app)* keep the recent projects list when its file cannot be read
- *(app)* copy MySQL and SQL Server function names with their own quotes
- *(app)* keep a hand-written environment when saving a connection
- *(core)* match square brackets literally in SQL Server table filters
- *(core)* send the bytes Redis console hex escapes spell
- *(files)* refuse JSONL files over 256 MiB in File SQL instead of running out of memory
- *(mssql)* check the server certificate against its host name through an SSH tunnel
- *(core)* match brackets literally in SQL Server table filters
- *(mysql)* check the server certificate against its host name through an SSH tunnel
- *(pg)* connect with verify-ca and verify-full chosen in the connection form

## [0.4.4](https://github.com/Binaretech/kuery/compare/v0.4.3...v0.4.4) - 2026-10-26

### Fixed

- *(app)* apply sidebar loads to their connection after a reorder
- *(core)* refuse unreadable MySQL statements where commits are refused

### Other

- *(app)* trim a test helper's comment

## [0.4.3](https://github.com/Binaretech/kuery/compare/v0.4.2...v0.4.3) - 2026-10-26

### Fixed

- *(mysql)* show binary columns as hex and edit rows by them
- *(app)* describe the list item removal instead of showing its script
- *(app)* remove Redis list items only while they are unchanged
- *(app)* keep a query tab open when saving it on close fails
- *(app)* keep tabs still waiting for their connection in the session
- *(core)* end a read's transaction when its setup fails
- *(core)* keep NULL and empty rows when exporting a single column
- *(core)* split and format long single-line SQL in linear time
- *(mysql)* keep binary-charset enums and sets as text
- *(pg)* fail the query instead of the connection on non-UTF-8 text
- *(update)* stop update checks and downloads that stall

### Other

- *(app)* trim the session comments
- license Kuery under GPL-3.0-only
