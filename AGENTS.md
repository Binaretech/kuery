# Agent instructions

## Commit messages are the changesets

Kuery's version and changelog come from the commit messages, through
[release-plz](https://release-plz.dev) (configured in `release-plz.toml`).
There is no changeset file to add: every commit you make is the changeset.
When a maintainer runs the Release PR workflow, release-plz reads the commits
since the last `vX.Y.Z` tag, picks the next version from their types, and adds
their titles to `CHANGELOG.md` in a release pull request.

So every commit that changes behavior must be a
[Conventional Commit](https://www.conventionalcommits.org):

```
<type>(<scope>): <title>
```

### Type: decides the version

| Type | Use it for | Version |
| --- | --- | --- |
| `feat` | Something new a user can do or see | minor (0.4.2 → 0.5.0) |
| `fix` | A bug fixed: wrong result, crash, lost data | patch (0.4.2 → 0.4.3) |
| `perf` | The same behavior, faster or lighter | patch |
| `refactor` | Code restructured, no behavior change | patch |
| `docs` | Documentation only | patch |
| `chore` | Maintenance: dependencies, configuration | patch |
| `ci`, `test`, `style` | CI, tests or formatting only | patch, left out of the changelog |

A change that breaks something users rely on (a project file format, a CLI
option, a setting) adds `!` after the scope, `feat(app)!: …`, and explains
what breaks in a `BREAKING CHANGE:` footer. Until 1.0 it raises the minor
version.

### Scope: where it changed

The crate's directory name: `app`, `core`, `files`, `mssql`, `mysql`, `pg`,
`redis`, `sqlite` or `update`. Leave the scope out when a change spans several
crates.

Only commits that touch a crate (`crates/…`, including its `Cargo.toml`)
count toward a release and appear in the changelog. Changes to the website
(`site/`), the README, `docs/` or CI alone ship nothing; use `fix(site):`,
`docs:` or `ci:` for them anyway.

### Title: becomes the changelog line

The title is copied into `CHANGELOG.md` as written, so write it for the people
who use Kuery:

- Say what changed for them, in the imperative, lowercase, no final period:
  `fix(mysql): show binary columns as hex and edit rows by them`, not
  `fix: update value conversion`.
- One change per commit. Two unrelated fixes are two commits, so each gets its
  own line and the right type.
- Put the why and the details in the body; only the title reaches the
  changelog.

### Never by hand

- Do not change `[workspace.package] version` in `Cargo.toml` or edit
  `CHANGELOG.md`: the release pull request does both.
- Do not create `v*` tags: merging the release pull request makes the Release
  workflow tag and publish the version.
