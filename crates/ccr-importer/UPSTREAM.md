# CC Switch import code provenance

The Codex and Claude session import, pricing and deduplication routines in this
crate are adapted from [CC Switch desktop v4.0.21, commit c4db63c60d2a1ddcb4bd9f81152b9c6126413659](https://github.com/farion1231/cc-switch/tree/c4db63c60d2a1ddcb4bd9f81152b9c6126413659/src-tauri/src),
including `services/session_usage.rs`, `services/session_usage_codex.rs`,
`services/usage_stats.rs`, `proxy/usage/calculator.rs`, `proxy/usage/parser.rs`
and `codex_rollout_file.rs`. The upstream MIT license is in `UPSTREAM-LICENSE`.

This crate deliberately excludes the rest of CC Switch and does not create or
migrate its database. It checks schema 23 and required columns before opening
for writes, receives mirror roots explicitly, starts write transactions in
IMMEDIATE mode and propagates row-write errors instead of advancing a cursor
after a failed insert. `ccr` serializes its own sync and import operations;
SQLite serializes concurrent database writes from the desktop application.

When updating these routines, compare against a specific desktop release,
check its database schema and import/dedup/cursor semantics, and run the fixture
and isolated database parity tests before adding support for a new schema.
