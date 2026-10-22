# Slop audit 001: follow-up (2026-10-22)

Applied on branch `chore/slop-cleanup`, commit `81e7534` (12 files, 20 lines added, 73 removed). Not merged.

Approved and applied: 1, 2, 3, 4, 5, 7, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24.
Not touched, as asked: 6, 8, 25.

Adjusted while applying:
- #14: "Not run with the suite; run it on its own:" became "Run it on its own:". The rest is as proposed.

Facts the shorter comments no longer carry (both were in the audit's proposed text, so they went as approved):
- #11 (`Cargo.toml`): that `sql-browser-tokio` is there for named instances, and that targets outside the release set would need cmake to build aws-lc-sys.
- #14 (`bench.rs`): why headless numbers are lower than on screen (no rasterizing, stand-in text system).
Say so if you want either line back.

Checks:
- The diff changes only comment lines, plus the one rewrapped line in `docs/guide.md`.
- `cargo fmt --all -- --check`: clean.
- `cargo build --workspace`: ok.
- `cargo test --workspace`: green (database tests ignored as usual).
- Em dashes added: none.
