# Changelog

## 0.2.0 (2026-11-09)

First release. Differential, deduplicating backups for Minecraft Java Edition worlds:
every chunk is stored once, so a nightly backup stores only what changed.

**Backing up**
- Back up one world, or a whole server folder as one snapshot (`-w`/`worlds` to pick:
  names, paths, `"*"`, `!name`). Worlds are stored under their folder names.
- `--pre`/`--post` hooks around a backup, for pausing saves however the server runs
  (RCON, Docker, screen, ZFS snapshots…). The post hook always runs, also on Ctrl-C/SIGTERM.
- Files that change while being read are re-read; `--strict` refuses a possibly torn
  snapshot.
- `vxl.toml` for source, repo, worlds, hooks, strict mode and retention.

**Restoring and inspecting**
- `restore` a whole snapshot, one world (`-w`), one dimension (`--dimension`) or an area
  (`--area x1,z1..x2,z2`, entities and POIs included), into a new folder or over an
  existing world (`--force`, `--force-wipe`). Nothing is changed unless everything stages.
- `list`, `show`, `diff` (chunk and file changes per world and dimension), `compare` (two
  world folders, chunk by chunk). Snapshots can be named by id prefix, `latest` or a time.

**Keeping the repository healthy**
- `prune` with `--keep-last/hourly/daily/weekly/monthly/yearly`, `delete <id>…`, `verify`.
- Storage format 3 (the default): a chunk stores only the sections that changed, and a
  region's chunk list only when it changed. On a real Paper world: changed nights 18–54% of
  storing chunks whole, an idle night ~20 KB, 7 copies 75% in total.
- `migrate [--to 2|3]` converts a repository in place either way; old copies are removed
  only after the new ones check out.
- Crash-safe writes, a repo lock, header and pack-index caches for big repos.

**Builds**: static Linux (x86_64, arm64), macOS (arm64) and Windows (x86_64) binaries with
`SHA256SUMS`.

Not yet seen in real use: worlds saved with `region-file-compression=lz4`, and running on
Windows (both pass the test suite).
