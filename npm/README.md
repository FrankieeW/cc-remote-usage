# cc-remote-usage

Pull **Codex / Claude Code agent usage** from your servers into the
[CC Switch](https://github.com/farion1231/cc-switch) Usage dashboard.
ccr discovers SSH hosts, lets you pick which ones to sync, and incrementally
fetches their session logs.

The program is a single Rust binary. This npm package is the installer: on
install it downloads the binary for your platform.

[中文说明](https://github.com/FrankieeW/cc-remote-usage/blob/master/README.md)
· [Full English docs](https://github.com/FrankieeW/cc-remote-usage/blob/master/README.en.md)

## Install

```bash
npm i -g cc-remote-usage
ccr --version
```

macOS (arm64/x64) and Linux (arm64/x64). The installer downloads the matching
binary from [GitHub Releases](https://github.com/FrankieeW/cc-remote-usage/releases).
If the network blocks that, download it yourself, place it at
`vendor/<platform>-<arch>/ccr` inside the package, and `chmod +x`.

> CC Switch must already be installed and configured on this machine.
> `ccr config --set-importer native` uses the built-in importer (database
> schemas 23–27 today). Or keep a compatible CC Switch CLI and pin the
> version you have checked with `ccr config --pin-cc-switch`.

## Usage

```bash
ccr hosts --probe          # discover servers and probe which ones can sync
ccr sync                   # pick hosts → incremental sync → import into CC Switch
ccr sync --all --quiet     # unattended (for a scheduler)
ccr import netcup          # retry import of the local mirror; no SSH
ccr report --days 7        # tokens and cost, by server and model
ccr status                 # last sync, failure count, next retry
ccr daemon install         # install a launchd job (every 15 minutes)
```

Command help and status text are in Chinese. Flag reference:
[English](https://github.com/FrankieeW/cc-remote-usage/blob/master/README.en.md#usage),
[中文](https://github.com/FrankieeW/cc-remote-usage/blob/master/README.md#用法).

When an orchestrator such as Pharos uses its own `CODEX_HOME`, add that log
directory yourself. ccr does not search for it:

```bash
ccr config --add-codex-root 'server=/srv/pharos/runtime/projects'
ccr sync server
```

The first sync of a new directory is a full backfill. Later syncs are
incremental. Only `.jsonl` files under `sessions/` and `archived_sessions/`
are transferred. `ccr sync` and `ccr import` exit non-zero when any host
fails, including with `--quiet`.

## Environment variables

| Variable | Purpose |
|---|---|
| `CCR_BINARY_PATH` | Override the binary this package runs |
| `CCR_LOCAL_BINARY` | Use a local binary at install time, and skip the download (offline or development) |

## License

MIT
