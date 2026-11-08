# vxl roadmap

Ordered by what makes vxl trustworthy for a real server first, then nicer to use, then
smaller. Phase 3 is decided by measurements, not guesses.

**Scope: Minecraft Java Edition only** (vanilla, Paper and forks, Fabric, Forge/NeoForge,
any host). Bedrock (LevelDB worlds) is out of scope. Non-Anvil files a Java server may have
(old `.mcr` regions, the `.linear` format of some Paper forks) are still backed up, but
stored whole, without chunk-level dedup.

## Where things stand (v0.1)

Validated on a real Paper survival world, 3 days of copies (119k → 131k chunks):

| check | result |
|---|---|
| backup | 3.6 s for the first snapshot (119k chunks), ~1–2 s after |
| storage | 3 world copies (2.4 GiB) → repo of 661 MiB in 46 files |
| `verify` | 160,212 objects, all intact |
| `restore` + `compare` | 130,789 / 130,789 chunks identical, readable region layout |
| in game | restored world loads on a throwaway Paper server |
| zstd level | level 6 confirmed as the knee on real chunks |
| NBT parser | byte-exact on ~370k real chunks, 0 fallbacks |

Untested: LZ4-compressed worlds (`region-file-compression=lz4`), real use on Windows
(CI passes there).

## Phase 1: safe nightly backups of any Java server (v0.2)

*Goal: one cron line backs up everything, correctly, every night.*

1. ✅ **Whole-server snapshots.** `vxl backup ~/server` backs up several worlds as **one
   snapshot**, and only worlds: plugins, configs, logs and jars are left to a normal file
   backup. Which worlds is set by `worlds` in the config (`--world` on the CLI):
   - `"*"` (the default) or `["*"]`: every world found;
   - `["world", "world_nether", "lobby"]`: exactly these;
   - `["*", "!creative"]`: all but some.

   **What counts as a world:** a folder with a `level.dat`, found up to two levels below the
   server folder (so `world/` and `worlds/world1/`, e.g. with Paper's `--world-container`).
   Search stops descending once it finds one, and skips hidden folders, `plugins/`, `mods/`,
   `config/`, `backups/`, `logs/`, `cache/`, `libraries/`, `versions/` and `crash-reports/`
   so a plugin's own world copies aren't picked up. Every backup
   prints the worlds it found.

   **A world is named by its own folder:** `worlds/world1` is stored as `world1`. Two
   matched worlds with the same name (`a/world1`, `b/world1`) are an error naming both
   paths; pick one by listing its path (`"a/world1"`) instead of `"*"`. Everything inside a world belongs to it, so
   the nested dimensions of vanilla/Fabric/Forge (`world/DIM-1`, `world/DIM1`,
   `world/dimensions/<mod>/<name>/`) come along with `world`, while Paper's separate
   `world_nether` / `world_the_end` and Multiverse worlds are worlds of their own. A matched
   world that is a symlink is followed (inside a world, symlinks are still skipped). Selecting
   a world and another world inside it is an error.

   **Storage:** a snapshot becomes a list of worlds, each with its name, where it was found,
   and its own file list with paths relative to that world, exactly like today's
   single-world manifest. Objects stay repo-wide and content-addressed, so the packs don't
   change and identical content across worlds is stored once. The manifest version goes up
   by one; older snapshots read as a single world named after their source folder.

   **Restore:** `vxl restore <id> <dest>` puts every world where it was in the server
   (`<dest>/world_nether`, `<dest>/worlds/lobby`) and writes nothing else, so restoring into
   the server folder rolls the worlds back in place; if any world fails to rebuild, none is
   touched. A snapshot of a single world folder still restores as that folder. `--world abc`
   writes just that one into `<dest>` as a standalone world folder, ready to load or hand
   to someone. A world's blocks restore on their own, but player data (inventories,
   positions, stats, advancements) lives only in the main world's `playerdata/`,
   `stats/` and `advancements/`, so restoring just `world_nether` doesn't roll back anyone's
   inventory. The docs must say so.
   *First because today the Nether and End are easy to miss entirely.*
2. ✅ **Pre/post hooks.** vxl doesn't know how the server runs; it just needs the world
   consistent on disk while it reads. `pre` and `post` are shell commands (`--pre` /
   `--post`, or in the config); `post` **always** runs, also on error or Ctrl-C, and a
   failing `pre` aborts the backup (after still running `post`). Run via `sh -c` on Unix
   and `cmd /C` on Windows, in vxl's working directory (the source may not exist until
   `pre` ran, e.g. a ZFS snapshot), with `VXL_SOURCE`, `VXL_REPO`, and for `post`
   `VXL_RESULT` and `VXL_SNAPSHOT`. `post` runs in its own process group so a Ctrl-C
   doesn't kill it too. The same mechanism covers
   RCON, `docker exec`, panel APIs, tmux, stopping the server, or ZFS/btrfs snapshots.
3. ✅ **Torn-read detection.** vxl parses every chunk anyway, so it can catch a region file
   that changed mid-read (bad chunk length, failed decompression, size or mtime changed
   between start and end of the read). Re-read it up to 5 times; a damaged file that reads
   identically twice is damaged on disk and kept as is; warn, or fail with `--strict`, if it
   never settles. Blobs (`level.dat`, `playerdata/`) are re-read on size/mtime changes too. Makes hook-less backups safe in practice and double-checks
   hooked ones.
4. ✅ **Hook recipes in the README**, ready to paste: RCON (`mcrcon`), itzg Docker
   (`docker exec mc rcon-cli`), tmux `send-keys`, ZFS snapshot, stop/start. A built-in RCON
   client is sugar for later, only if people keep asking. The README also states the scope:
   which files get chunk-level dedup (Anvil `.mca`/`.mcc`) and which are stored whole.
5. ✅ **Config file (`vxl.toml`).** Source, repo, worlds, hooks, strict and retention in one
   place (`./vxl.toml`, or `--config` / `VXL_CONFIG`), so plain `vxl backup && vxl prune`
   does everything. Command-line options win; unknown keys are errors. README has cron and
   systemd-timer examples.
6. ✅ **`vxl delete <id>…`.** Delete specific snapshots and garbage-collect in one step,
   under the repo lock. (Today: remove `snapshots/<id>.json.zst`, then `vxl prune`.)
7. ✅ **Retention policies.** `--keep-last 5 --keep-daily 7 --keep-weekly 4 --keep-monthly 6`
   (also hourly, yearly), set once in the config. Like restic: newest snapshot per period,
   periods without snapshots don't count, local time, applied per source (folder + worlds).

**Done when:** each of these gets backed up nightly by cron, all dimensions in one
snapshot, and a restore of it loads cleanly:

- a Paper server with players (separate `world_nether` / `world_the_end`);
- a vanilla or Fabric server (dimensions nested inside `world/`);
- a modded server with custom dimensions (`world/dimensions/<mod>/…`);
- a server in Docker (itzg image), quiesced through a hook.

## Phase 2: restores that fit how servers break (v0.3)

*Goal: roll back a griefed spawn without rolling back everyone's progress.*

1. ✅ **Partial restore.** `--dimension` and/or `--area x1,z1..x2,z2` (block coordinates)
   into a stopped live world with `--force`. Brings the matching `entities/` and `poi/`
   chunks along: since 1.17 entities live in separate files, and restoring blocks without
   them leaves mobs and item frames wrong. Inside the selection the world becomes exactly
   the snapshot (newer chunks are removed and regenerate); `--area` needs `--dimension` when
   a world has several. (`--region` was dropped: an area of 512×512 blocks says the same.)
2. ✅ **`vxl diff <a> [b]`** between snapshots, without restoring: per world and dimension,
   chunks changed/added/removed by content, other files changed; `--files` lists every file.
   (`vxl compare` does this for two folders.)
3. ✅ **`vxl show <id>`** for snapshot details. Every command that takes a snapshot also
   takes `latest` or a time (`vxl restore "2026-11-07 03:00" …`): the newest snapshot *at or
   before* it (not the nearest: a rollback should never pick a snapshot from after the
   moment asked for), refusing to guess across several sources.

## Phase 3: storage format v3 (decided by data)

*Goal: smaller repos, only with proven gains.*

Real-world results so far (`examples/nbt_bench.rs`, `examples/zstd_levels.rs`, 3 days):

| idea | result on the real world | cost |
|---|---|---|
| trained zstd dictionary | −10% on everything | repo-wide dependency that must never be lost |
| unpack bit-packed arrays | −5% on everything | ~3× slower encoding |
| grouped split (sub-chunk dedup) | nightly −37…−70%, first backup +14% | complexity in prune/restore |
| compact section references | expected to remove most of that +14% | not measured yet |

Steps:

1. Add compact section references to `nbt_bench` (1-byte "#n in this object" for
   sections stored alongside, short numeric ids for older ones instead of 32-byte hashes;
   the 2.57M hashes in the first real backup are ~82 MB on their own).
2. Rerun on **~7 days** of copies (3 days is too few to judge incremental gains).
3. **Decision rule:** adopt grouped split only if the 7-day total is clearly below today's
   **and** the first backup is within ~+5%. Otherwise ship unpack + dictionary alone.
4. **`vxl migrate`** v2 → v3 (rewrite packs). Real repos exist now, so unlike v1 this
   needs a proper migration.
5. Optional: recompress at a higher zstd level during prune (~−10%, costs only prune time).

## Phase 4: robustness and operations

- ✅ **Offsite copies, documented.** Packs never change once written, so `rsync`/`rclone` of a
  repo is cheap and incremental; README shows a two-step copy (packs first) under a shared
  `flock`. Built-in S3 target and encryption only if needed.
- **Fuzzing** the parsers that read untrusted bytes (anvil, NBT, pack). ✅ Stable-Rust
  mutation tests (`tests/robustness.rs`: flips, extreme lengths, truncation, splices, hostile
  nesting) run with every `cargo test`; they found an overflow in the pack-index bounds check
  (fixed). Still open: coverage-guided `cargo-fuzz` targets (needs nightly).
- **Big-repo speed.** ✅ Snapshot headers are cached (`cache/headers/`), so `list` and
  `prune` don't decompress whole manifests. Still open: a combined pack-index cache, so
  `open` reads one file instead of every pack's index once there are thousands of packs.
- **Close the untested paths:** an LZ4 world, real use on Windows.
- **Releases.** Prebuilt binaries via GitHub Releases, so servers don't need Rust.

## Open questions

1. **Retention:** how far back should restores go? A week, a month, a season?
2. **Offsite:** repo on the server's disk only, or copied to another machine / cloud?
3. **Partial restore:** is griefing or broken builds a real problem? Decides whether
   Phase 2 comes before Phase 3.
4. **Frequency:** nightly only, or every few hours at peak? Affects how disruptive the
   hooks may be and how much incremental size matters.
