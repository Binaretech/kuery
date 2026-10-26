# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
