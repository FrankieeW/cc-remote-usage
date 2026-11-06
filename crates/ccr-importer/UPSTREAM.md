# CC Switch import code provenance

The Codex and Claude session import, pricing and deduplication routines in this
crate are adapted from [CC Switch desktop v4.0.21, commit c4db63c60d2a1ddcb4bd9f81152b9c6126413659](https://github.com/farion1231/cc-switch/tree/c4db63c60d2a1ddcb4bd9f81152b9c6126413659/src-tauri/src),
including `services/session_usage.rs`, `services/session_usage_codex.rs`,
`services/usage_stats.rs`, `proxy/usage/calculator.rs`, `proxy/usage/parser.rs`
and `codex_rollout_file.rs`. The upstream MIT license is in `UPSTREAM-LICENSE`.

This crate deliberately excludes the rest of CC Switch and does not create or
migrate its database. It accepts only the audited schemas listed in
`SUPPORTED_SCHEMAS` (`src/lib.rs`) and checks required columns before opening
for writes, receives mirror roots explicitly, starts write transactions in
IMMEDIATE mode and propagates row-write errors instead of advancing a cursor
after a failed insert. `ccr` serializes its own sync and import operations;
SQLite serializes concurrent database writes from the desktop application.

When updating these routines, compare against a specific desktop release,
check its database schema and import/dedup/cursor semantics, and run the fixture
and isolated database parity tests before adding support for a new schema.

## Schema audits

- **23** — desktop v4.0.21 (commit above), the release the routines come from.
- **24** — desktop [v4.0.22, commit b76426741ef93069e2f374ac01ca7ef5760e1d0e](https://github.com/farion1231/cc-switch/tree/b76426741ef93069e2f374ac01ca7ef5760e1d0e/src-tauri/src)
  through [v4.0.26, commit f621d4216189ef06bcc6acd3588c84937b9794e1](https://github.com/farion1231/cc-switch/tree/f621d4216189ef06bcc6acd3588c84937b9794e1/src-tauri/src),
  audited 2026-11-06. The 23 → 24 migration only adds columns: `enabled_dsh` on
  `mcp_servers` and `skills`, and the nullable DeepSeek Harness parser context
  `session_log_sync.parser_state`. Upstream v4.0.26 still writes Codex and Claude
  cursors with the same statements as v4.0.21, so `parser_state` stays `NULL` on
  rows written here, as it does upstream. A schema 24 database must contain that
  column. Two v4.0.22+ behaviour changes are not ported: verified bounded
  (`history_base`) Codex forks, and matching a proxy `request_model` alias when
  deduplicating Codex sessions against proxy logs. Neither affects the mirrors at
  audit time: none of 1,068 mirrored files uses `history_base`, and remote sessions
  do not pass through the local proxy. Port them if servers start producing such
  forks.
