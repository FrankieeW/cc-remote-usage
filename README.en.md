# ccr — server agent usage on one CC Switch dashboard

[中文](README.md)

[![release](https://github.com/FrankieeW/cc-remote-usage/actions/workflows/release.yml/badge.svg)](https://github.com/FrankieeW/cc-remote-usage/actions/workflows/release.yml)

The CC Switch Usage dashboard only scans session logs on **this machine**
(`~/.codex/sessions`, `~/.claude/projects`, scoped by `CODEX_HOME` /
`CLAUDE_CONFIG_DIR`). Codex and Claude Code agents running on a VPS or
another server never appear there.

`ccr` incrementally pulls those session logs into a local mirror, then
imports the usage with the built-in importer or
`cc-switch sessions sync-usage`. Server usage joins the same stats, so
"today" on the dashboard includes the agents on your servers.

Command help and status text are in Chinese.

## Install

**npm (recommended; downloads the binary for your platform)**

```bash
npm i -g cc-remote-usage
ccr --version
```

**Download a binary.** From [Releases](https://github.com/FrankieeW/cc-remote-usage/releases),
take `ccr-<target>` (macOS / Linux, arm64 / x64), run `chmod +x`, and put it on `PATH`.

**Build locally**

```bash
cargo build --release --offline   # offline; drop --offline if crates.io is reachable
./target/release/ccr --help
```

An npm install makes `/opt/homebrew/bin/ccr` a symlink to the Node shim.
Do not `cp` a binary onto that path.

## Usage

```bash
ccr hosts                 # discover candidate hosts (~/.ssh/config + CC Switch Fleet + config)
ccr hosts --probe         # probe connectivity, and log count and size for the last 3 days (skip hosts in cooldown)
ccr hosts --probe --force # ignore cooldown and probe every candidate
ccr sync                  # pick servers interactively → incremental sync → import into CC Switch
ccr sync netcup           # one host (a host named on the command line ignores cooldown)
ccr sync --all --quiet    # unattended (for the scheduler: only reachable hosts that have logs)
ccr sync --retry-now      # retry hosts in cooldown now, still incremental
ccr sync --all --force    # ignore cooldown and the checkpoint; pull everything again
ccr import netcup         # retry import of an existing mirror and prune it; no SSH, checkpoint stays put
ccr report --days 7       # token totals and estimated cost, by server and model
ccr report --days 7 --daily   # add a per-day breakdown
ccr report --json         # machine-readable
ccr status                # last sync, consecutive failures, next attempt, mirror size
ccr licenses              # licenses for this project and the built-in importer
ccr daemon install        # install a launchd job (every 15 minutes by default)
ccr daemon status
ccr daemon uninstall
ccr config --set-hosts netcup,frankie-vps   # fixed default hosts (no interactive pick)
ccr config --set-days 3                     # how many days the first sync looks back
ccr config --set-backoff 15,360             # backoff: 15-minute base, 6-hour cap
ccr config --set-retain-days 7              # mirror retention in days (0 = never prune)
ccr config --set-importer native            # built-in importer (database schemas 23 and 24)
ccr config --set-importer cc-switch         # switch back to the CC Switch CLI
ccr config --pin-cc-switch                  # pin the CLI version that is installed now
ccr config --unpin-cc-switch                # clear the version pin
```

## Extra Codex log directories (opt-in)

By default ccr only scans the SSH user's `~/.codex/sessions` and
`~/.claude/projects`. Orchestrators such as Pharos may give main, worker,
verifier, and subagent each their own `CODEX_HOME`. You name the remote
absolute paths of those sessions. ccr does not search a project tree or the
whole server.

```bash
ccr config --add-codex-root 'server=/srv/pharos/runtime/projects'
ccr sync server
ccr config --clear-codex-roots server
```

`--add-codex-root` can be repeated. Inside each configured root, ccr pulls
only `.jsonl` files under `sessions/` and `archived_sessions/`. It does not
copy `auth.json` or `config.toml`. It skips nested `materials`, `.git`,
`.venv`, and `node_modules`, so a historical copy sitting in materials is
not treated as the current actor's log. To sync one of those trees, configure
that session directory itself.

After you add or change the list, the next sync backfills those directories
in full, then returns to incremental sync. A failed import does not mark the
backfill done. Files that share a name across sources are stored apart and
placed at a directory depth CC Switch can scan. A host with extra directories
is included in `sync --all` even when the default directories have no recent
logs. `hosts --probe` still counts files only in the default directories and
labels the extra directories on their own.

## Importer version and retrying a failed import

The built-in importer follows the session import from CC Switch desktop
v4.0.21. It supports Codex and Claude, writes usage and cursors into the
existing CC Switch database in a SQLite transaction, and does not create or
migrate the database. It accepts only schemas 23 and 24, which were audited
against upstream (desktop v4.0.21 through v4.0.26). An unknown database
version stops the run before any SSH probe. The database is
`~/.cc-switch/cc-switch.db` by default. When `CC_SWITCH_CONFIG_DIR` is set,
ccr uses the database in that directory.

After the desktop app migrates to a newer schema, every scheduled run stops
at that preflight check. The error is only in the log that
`ccr daemon status` prints. The pull checkpoint does not move, so the next
sync after you upgrade `ccr` fills in the logs from the gap.

Source and the MIT copyright notice: [built-in importer](crates/ccr-importer/UPSTREAM-LICENSE).

Existing installs keep the CLI backend. Switch with
`ccr config --set-importer native`.

On the CLI backend, confirm that the desktop app and the CLI are compatible,
then run `ccr config --pin-cc-switch`. The pin is stored as
`cc_switch_version`. Each sync checks the CLI version, and fixes the binary
path for that run, before it probes SSH or pulls logs. A version mismatch
stops the run. After you confirm a newer CLI is compatible, pin it again.
The pin stops an accidental CLI upgrade. It does not decide whether the
desktop app and the database match. The pin applies only to the CLI backend,
and it is still in effect when you switch back to the CLI.

When the logs are already mirrored but the import failed, run
`ccr import <host...>`. It retries the local mirror only. It shares the sync
lock and clears failure state for hosts that succeed, and it leaves
`last_epoch` unchanged. The next normal sync still picks up new remote logs.
Named hosts whose import succeeded with no deferred files and no warnings are
pruned by the retention window. Failed hosts, and hosts you did not name, are
left as they are.

`ccr sync` and `ccr import` exit non-zero when any host fails. `--quiet` does
not change that. If `config.json` fails to parse, the run stops, so a broken
config cannot skip the version pin.

The first sync looks back two days by default. Later incremental syncs do not
fetch older files that were never modified again. For the full history, run
`ccr sync <host> --force`. CC Switch deduplicates historical usage. After a
successful import, local files still follow mirror retention.

## Backoff

An unreachable machine is not retried every round. Consecutive failures back
off exponentially:

| Consecutive failures | 1 | 2 | 3 | 4 | 5 | 6+ |
|---|---|---|---|---|---|---|
| Next attempt | 15 min | 30 min | 1 hour | 2 hours | 4 hours | 6 hours (cap) |

- One successful sync clears the failure count and the cooldown.
- When **every** host probed this round is unreachable, `ccr sync` exits
  non-zero. That is usually the local network or `ssh-agent`. The reason
  shows up in the scheduler log and in `ccr daemon status`.
- Hosts in cooldown are skipped by `ccr sync --all` and `ccr hosts --probe`
  with no SSH connection, so a round does not wait out a timeout per host.
- A host **named on the command line** (`ccr sync yeats-vps`) ignores
  cooldown. The host list from `ccr daemon install --hosts` belongs to the
  scheduler and still respects cooldown.
- Retry every host now, still incrementally: `ccr sync --all --retry-now`.
  `--force` also drops the incremental checkpoint and pulls everything again.
- Change the window with `ccr config --set-backoff <base minutes>,<cap minutes>`.
  The values are `backoff_base_min` and `backoff_max_min` in `config.json`.
- `ccr status` lists, for each host, the consecutive failure count, the next
  attempt time, and the latest error.

## How it works

```
server                                  this machine
~/.codex/sessions/**.jsonl   ──ssh tar──▶  ~/.cc-switch/remote-usage/<host>/codex/sessions/
~/.claude/projects/**.jsonl  ──ssh tar──▶  ~/.cc-switch/remote-usage/<host>/claude/projects/
                                              │
                                              ▼
                       built-in importer / cc-switch sessions sync-usage --app codex|claude
                                              │
                                              ▼
                       ~/.cc-switch/cc-switch.db: proxy_request_logs(data_source=codex_session/claude...)
                                              │
                                              ▼
                                   CC Switch Usage dashboard / ccr report
```

- **Incremental.** Files are filtered by mtime
  (`find -newermt @<last sync minus 5 minutes>`; the extra 5 minutes covers
  clock skew). State is `~/.cc-switch/remote-usage/state.json`, written as soon as
  each host finishes. `--force` pulls everything again.
- **No rsync required.** Servers often do not have it. ccr streams a gzip tar
  over SSH.
- **Database compatibility.** The built-in importer writes only schemas it has
  been audited for, and stops on an unknown version. The CLI backend imports
  through the upstream command. Neither path copies OAuth credentials.
- **Codex and Claude only.** Each app is imported on its own. `--all` also
  scans local Gemini, Pi, and other directories the environment variables do
  not control, and counts that local usage in the server import totals.
- **Dedup.** CC Switch deduplicates on the `session_log_sync` byte offset.
  Importing the same bytes again has no effect.
- **Alias dedup.** When several aliases point at one machine (for example
  `macmini`, `macmini-lan`, and `macmini-tailscale`), the probe fingerprints
  session files and syncs that machine once. An alias that points at this
  machine is skipped.
- **One run at a time.** `ccr sync` and `ccr import` share `sync.lock`. A
  scheduled run skips itself when a manual run holds the lock. The interactive
  multi-select does not hold the lock while you are choosing. A stuck remote
  cannot hold the lock forever: 2 minutes per host probe, 60 minutes per
  directory transfer, 15 minutes per cc-switch import. Hitting a limit aborts
  that step. The next round retries it.
- **A stalled transfer ends itself.** If the remote command is killed, the SSH
  channel often stays open and the local tar waits forever for EOF.
  ServerAlive only checks that the connection is up, so it cannot see this.
  ccr counts bytes between ssh's stdout and tar's stdin. No new data for 10
  minutes during a transfer, or no first byte for 15 minutes, aborts that
  host, records a failure, and starts backoff. Later hosts in the same round,
  and later scheduled runs, are not stuck behind it. Override the thresholds
  with `CCR_TRANSFER_IDLE_SECS` and `CCR_TRANSFER_STARTUP_SECS` (seconds).

## Where data lives

| Path | Contents |
|---|---|
| `~/.cc-switch/remote-usage/<host>/` | Session-log mirror (files untouched for 7 days are pruned; `retain_days` changes that) |
| `~/.cc-switch/remote-usage/state.json` | Incremental checkpoint and last result, per host |
| `~/.cc-switch/remote-usage/config.json` | Default hosts, extra Codex directories, lookback days, backoff, retention, CLI path, version pin |
| `~/.cc-switch/remote-usage/sync.lock` | Run lock (two syncs cannot run together) |
| `~/Library/LaunchAgents/com.fwmbam4.ccr-sync.plist` | launchd job (`ccr daemon install` writes it) |
| `~/.cc-switch/remote-usage/launchd.err.log` | Scheduler log |

## What the server needs

- Linux or macOS. GNU or BSD `find` and `tar` both work. A `find` that rejects
  `-newermt @epoch` falls back to `-mmin`.
- Key-based SSH that succeeds with `BatchMode=yes`.
- `RemoteCommand` and `RequestTTY` in ssh config, set for interactive login,
  do not apply. ccr overrides them with `-T -o RemoteCommand=none`.
- Any login shell. The remote script is fed to `sh -s` on stdin, so fish and
  csh are fine. Session directories may be symlinks.
- Agent logs in the default locations, or extra Codex log directories you configured.

## Notes

- **Pricing.** Cost is the token cost from CC Switch's `model-pricing.json`.
  It is not the bill from a relay or an API provider. A model missing from
  the price table is shown as `n/a` (the CC Switch dashboard shows 0).
- **Attribution.** CC Switch assigns parsed session usage to pseudo-providers
  such as `_codex_session` and `_session`. It does not record which server a
  session came from. Use `ccr report` for a per-server split.
- **Request counts.** Claude Code writes one row per content block of a
  response (thinking, text, tool_use). `ccr report` deduplicates Claude by
  `message.id` and Codex by `response_id`, which matches the dashboard.
- **Disk.** After a successful import with no deferred files and no warnings,
  that host's mirror drops files that have not been modified in 7 days
  (`ccr config --set-retain-days`). Parent sessions of in-progress Codex
  subagent sessions are kept, because CC Switch needs them to import the
  child. A `config.json` that fails to parse skips cleanup for the whole
  round. A `report --days` window wider than retention can miss older rows,
  and the report says so. A first full pull can be large. One measured case
  was 41 Codex rollouts over 3 days, about 228MB after compression.
- When the server clock and the local timezone disagree, "today" follows the
  timestamp stored in the session, converted to local time.
