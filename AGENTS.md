# Working on Wardex

Rules for anyone changing this repository, human or agent. Read
[`docs/SCOPE.md`](docs/SCOPE.md) before starting; it is the source of truth for
what Wardex does and in which milestone.

## Workflow

- Every change goes through a pull request against `main`. CI (fmt, clippy,
  tests, secret scan) must be green before merge. Squash merge.
- One topic per PR. Keep PRs small enough to review.
- Before pushing, run:

  ```sh
  cargo fmt --all
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace
  ```

- Add an entry under `## [Unreleased]` in `CHANGELOG.md` for user-visible
  changes.
- Commit messages: imperative, short subject (`Add codex driver handshake`).

## Never

- **Never commit secrets or deployment-specific values**: tokens, keys,
  webhook URLs, hostnames, user names, home paths, bot or agent names. Those
  belong in the user's own config file or environment. The repo only contains
  `deploy/wardex.example.toml` with placeholders.
- **Never stop, restart or replace the Wardex daemon that is running on the
  host**, and never touch its installed binary, config or database. You may be
  running inside it. To test manually, start a separate instance with its own
  temporary config, socket, port and database.
- Never add a policy, approval or command-filter layer. It is out of scope by
  design.

## Code

- Rust, edition 2024. `unsafe` is forbidden.
- Code, comments and docs are in English.
- Match the surrounding style; no unnecessary abstractions.
- Errors: `thiserror` in library crates, `anyhow` only in the `wardex` binary.
- Logging via `tracing`. Never log secrets, tokens or full webhook URLs.
- Migrations in `crates/wardex-core/migrations/` are append-only. Never edit a
  merged migration. Queries use the runtime `sqlx::query` API (no compile-time
  macros, no `DATABASE_URL`).
- Driver tests use mock processes and recorded fixtures of real CLI output
  (`crates/wardex-drivers/tests/fixtures/`). Tests must not call real
  `claude` / `codex` / `grok` binaries or the network.
- Note the tested CLI versions in `docs/drivers.md` when a driver changes.

## Layout

```text
crates/wardex-core     event model, config, store (SQLite), session manager,
                       driver interface, process launcher, git helpers
crates/wardex-drivers  claude, codex, grok
crates/wardex-server   Unix socket API, HTTP (/healthz, MCP), webhooks
crates/wardex          the binary (clap CLI)
deploy/                systemd unit, example config, tunnel guide
docs/                  scope and design docs
```
