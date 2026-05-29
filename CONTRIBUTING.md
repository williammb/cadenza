# Contributing to Cadenza

Thanks for taking the time to improve Cadenza.

Cadenza is still pre-release. Small, focused pull requests are easiest to review and merge.

## Before you start

- Read `README.md` for the project overview.
- Read `AGENTS.md` or `CLAUDE.md` for repository-specific implementation guidance.
- Do not change canonical Portuguese state values, CLI exit codes, IPC shape, i18n toolchain, or license policy without prior discussion.
- Keep UI changes framework-free: no package-manager workflow, bundlers, React, Vue, Svelte, or build step.

## Development setup

Install Rust stable and the Tauri 2 prerequisites for your operating system, then:

```bash
cargo install tauri-cli --version "^2" --locked
cargo tauri dev
```

Run the CLI directly with:

```bash
cargo run -p cadenza-cli -- current --json
```

## Checks

Before opening a pull request, run:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

On Linux, also run:

```bash
cargo deny check advisories bans licenses sources
rg '\.innerHTML\s*=' ui/ --type js -g '!**/vendor/**' -g '!**/markdown.js'
```

The `rg` command should produce no matches.

## Pull requests

- Keep changes surgical and explain the user-visible reason.
- Add or update tests when behavior changes.
- Update docs when a command, workflow, storage behavior, release process, or security assumption changes.
- Avoid unrelated formatting churn.
- Note any checks you could not run.

Portuguese is welcome for architecture discussions. English is fine for code, comments, commits, issues, and pull requests.
