# Contributing to Kuery

Thank you for helping improve Kuery. Bug reports, ideas and pull requests are
all welcome.

## Before you start

- **Bugs and ideas:** open an issue first. Include your platform, the Kuery
  version or commit, steps to reproduce, and a small sample query or dataset
  if you can. Remove passwords and private data.
- **Larger changes:** open an issue to discuss the approach before writing a lot
  of code, so your work fits the project.
- **Small fixes** (typos, docs, clear bugs): a pull request is fine.

## Making a change

Start with the [user and development guide](docs/guide.md). Before opening a
pull request, run:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

- Add a test that fails without your change and passes with it.
- Update the README and `docs/guide.md` when you change a feature, command or
  message.
- Keep pull requests focused on one change, with a clear title
  ([Conventional Commits](https://www.conventionalcommits.org) style, for
  example `fix(app): keep the tab open when saving fails`).

## Contributor License Agreement

Before we can merge your first pull request, you need to sign the
[Kuery Contributor License Agreement](CLA.md). It takes a minute: a bot
comments on your pull request with a link, you sign in with GitHub and click
"I agree". You only sign once.

**Why we ask for it.** Kuery is open source under the
[GNU General Public License, version 3](LICENSE). We may also offer Kuery under
a commercial license in the future, to fund its development. The agreement lets
us do that with code you contributed. You keep the copyright in your work.

**Our promise.** Kuery stays open source. Everything you contribute will always
remain available under the GPL or another open source license, even if we also
offer a commercial edition. This promise is part of the agreement itself
(section 3), not just this page.

If you contribute as part of your job, check with your employer first. Your
employer may need to approve your contribution, or sign the agreement as an
organization.

## Code of conduct

Be respectful and constructive. We may remove comments, issues or pull requests
that are abusive, and block repeat offenders.

## License

By contributing, you agree that your contributions are licensed under the
[GNU General Public License, version 3](LICENSE) and under the terms of the
Contributor License Agreement.
