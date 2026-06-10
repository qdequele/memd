# memd auto-update — design

**Date:** 2026-06-10
**Status:** Approved
**Author:** Quentin de Quelen (with Claude)

## Goal

memd keeps itself and its managed Meilisearch engine up to date automatically, with
no user action. A daily check against GitHub Releases drives both updates. Updates
are fully automatic by default, with a config opt-out. Memory data is never lost:
engine migrations use Meilisearch's dump → import flow and roll back on any failure.

## Decisions (validated with user)

- **Update policy:** fully automatic — download, swap, restart, no prompt.
  Opt-out via `[update] auto = false` in config.toml.
- **Scope:** both the memd binary and the pinned Meilisearch engine.
- **Engine version source:** track `meilisearch/meilisearch` GitHub releases
  directly (not lockstep with memd releases). Stable releases only — skip
  prereleases and drafts.
- **Architecture:** in-daemon background task (no external scheduler). The
  daemon is always-on, so the schedule is reliable and no extra system config
  (second plist, cron) is needed.

## Components

New module `src/update/` with three parts, plus hooks elsewhere:

### Checker (`src/update/check.rs`)

- Queries `GET https://api.github.com/repos/qdequele/memd/releases/latest` and
  `GET https://api.github.com/repos/meilisearch/meilisearch/releases/latest`.
  Unauthenticated (60 req/h rate limit is ample for a daily check); set a
  `User-Agent` header (GitHub requires one).
- Compares semver: release tag vs `CARGO_PKG_VERSION` (memd) and
  `cfg.meilisearch.version` (engine). Skips prereleases and drafts.
- Returns a plan: `None | MemdUpdate(version) | EngineUpdate(version)`.

### Self-updater (`src/update/self_update.rs`)

1. Pick the release asset matching the compiled target triple
   (`memd-aarch64-apple-darwin`, `memd-x86_64-apple-darwin`,
   `memd-x86_64-unknown-linux-gnu`, `memd-aarch64-unknown-linux-gnu` — names
   match what `.github/workflows/release.yml` stages).
2. Download to a temp file **in the same directory** as the installed binary
   (same filesystem, so the final rename is atomic). `chmod 0755`.
3. Sanity check: run `<tmpfile> --version` and confirm it prints the expected
   new version. Catches corrupt or wrong-arch downloads before touching
   anything.
4. Atomic `rename(tmp, installed_path)`. The running process keeps its old
   inode — safe on Unix.
5. Restart:
   - Under launchd: `exit(0)`; the plist's `KeepAlive=true` relaunches the
     daemon on the new binary.
   - Linux / foreground: re-`exec` self (`std::os::unix::process::CommandExt::exec`).

### Engine-updater (`src/update/engine.rs`)

The careful one. Orchestrated by the daemon, which already supervises the
engine process.

1. `POST /dumps` on the running engine; poll the task to completion (a dump of
   a live instance is the official Meilisearch upgrade path).
2. Download the new engine binary to its versioned path (existing
   `meili::binary_path(version)` layout — the old binary stays on disk, which
   is the rollback).
3. Stop the engine. Move `data.ms` to a timestamped backup
   (same pattern as `fix_db_mismatch` in `cli.rs`).
4. Start the new engine with `--import-dump <latest dump>`; wait for import,
   healthcheck, and verify stats look sane (indexes non-empty if the backup
   had documents).
5. **Success:** persist the new version to `config.toml`
   (`meilisearch.version`), log it, prune old `data.ms` backups (keep the most
   recent).
6. **Any failure:** stop the new engine, restore the `data.ms` backup, restart
   the old engine version, leave `config.toml` untouched, record the failed
   version in `update-state.json`, and never retry that version — wait for the
   next release. Worst case is "still on the old version"; memories are never
   lost.

### Daemon hook (`src/daemon.rs`)

- A tokio background task: first check a few minutes after startup (with
  jitter, to avoid a thundering herd and to let the engine come up), then
  every 24 h.
- Last-check timestamp persisted in `update-state.json` in the data dir, so
  frequent daemon restarts do not re-check more than daily.
- Ordering: when both updates are available, memd self-update goes first; the
  new daemon re-checks shortly after restart and then performs the engine
  migration — so engine migrations always run on the newest orchestration
  code. Each tick applies at most one update.

### CLI (`src/cli.rs`)

- `memd update` — check and apply now.
- `memd update --check` — dry run; report what would happen.
- `memd status` — adds a line with current vs latest versions and the time and
  result of the last check.
- A lock file in the data dir prevents a manual `memd update` racing the
  daemon's automatic run.

## State & config

`update-state.json` (data dir):

```json
{
  "last_check_secs": 1780000000,
  "last_result": "ok | error: <msg>",
  "failed_engine_versions": ["v1.46.0"]
}
```

`config.toml`:

```toml
[update]
auto = true   # set false to disable automatic updates entirely
```

`memd update` (manual) works regardless of `auto`.

## Error handling

- Network/API failures: skip silently, retry at the next tick.
- Download or `--version` sanity-check failure: abort, nothing touched.
- Engine import failure: full rollback as described above; failed version is
  blacklisted in state.
- All update activity goes to the existing log file and is surfaced in
  `memd status`.

## Testing

- **Unit:** semver comparison; asset-name selection per target triple;
  release-JSON parsing against fixtures; `update-state.json` round-trip;
  failed-version blacklist behavior.
- **Integration:** run the engine machinery as dump → import-dump on the
  *same* version (exercises dump, stop, backup, import, verify without
  needing two real engine versions). A failure-path test forces the import to
  fail and asserts the backup is restored and the old engine restarted.
- **Manual:** verify a real self-update by installing an older build locally
  and letting it update to the latest published release.

## Out of scope

- Windows support (memd has no Windows service integration yet).
- Delta/patch downloads, signature verification beyond the `--version` sanity
  check (can be added later — GitHub release assets are served over TLS).
- Update channels (beta/nightly).
