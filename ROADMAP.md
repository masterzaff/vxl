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

1. **Whole-server snapshots.** `vxl backup ~/server` backs up several worlds as **one
   snapshot**. Which worlds is set by `worlds` in the config (`--world` on the CLI):
   - `"*"` (the default) or `["*"]`: every top-level folder with a `level.dat`;
   - `["world", "world_nether", "lobby"]`: exactly these;
   - `["*", "!creative"]`: all but some.

   A world is a folder with a `level.dat`; everything inside it belongs to it, so the
   nested dimensions of vanilla/Fabric/Forge (`world/DIM-1`, `world/DIM1`,
   `world/dimensions/<mod>/<name>/`) come along with `world`, while Paper's separate
   `world_nether` / `world_the_end` and Multiverse worlds are matched as their own entries.
   A world nested inside another matched world isn't counted twice.

   **Storage:** manifest paths become relative to the server folder
   (`world_nether/DIM-1/region/r.0.0.mca`) and the header lists the worlds. Objects stay
   repo-wide and content-addressed, so nothing changes in the packs and identical content
   across worlds (a copied lobby, the same `level.dat` pieces) is stored once. Restore
   gets `--only world_nether`, which is just a path-prefix filter.
   *First because today the Nether and End are easy to miss entirely.*
2. **Pre/post hooks.** vxl doesn't know how the server runs; it just needs the world
   consistent on disk while it reads. `pre` and `post` are shell commands (`--pre` /
   `--post`, or in the config); `post` **always** runs, also on error or Ctrl-C, and a
   failing `pre` aborts the backup (after still running `post`). Run via `sh -c` on Unix
   and `cmd /C` on Windows, with the server folder as working directory. The same mechanism covers
   RCON, `docker exec`, panel APIs, tmux, stopping the server, or ZFS/btrfs snapshots.
3. **Torn-read detection.** vxl parses every chunk anyway, so it can catch a region file
   that changed mid-read (bad chunk length, failed decompression, size or mtime changed
   between start and end of the read). Re-read that region a few times; warn, or fail with
   `--strict`, if it never settles. Makes hook-less backups safe in practice and double-checks
   hooked ones.
4. **Hook recipes in the README**, ready to paste: RCON (`mcrcon`), itzg Docker
   (`docker exec mc rcon-cli`), tmux `send-keys`, ZFS snapshot, stop/start. A built-in RCON
   client is sugar for later, only if people keep asking. The README also states the scope:
   which files get chunk-level dedup (Anvil `.mca`/`.mcc`) and which are stored whole.
5. **Config file (`vxl.toml`).** Source, repo, hooks and retention in one place, so plain
   `vxl backup` does everything. README gets cron and systemd-timer examples.
6. **`vxl delete <id>…`.** Delete specific snapshots and garbage-collect in one step,
   under the repo lock. (Today: remove `snapshots/<id>.json.zst`, then `vxl prune`.)
7. **Retention policies.** `--keep-last 5 --keep-daily 7 --keep-weekly 4 --keep-monthly 6`,
   set once in the config.

**Done when:** each of these gets backed up nightly by cron, all dimensions in one
snapshot, and a restore of it loads cleanly:

- a Paper server with players (separate `world_nether` / `world_the_end`);
- a vanilla or Fabric server (dimensions nested inside `world/`);
- a modded server with custom dimensions (`world/dimensions/<mod>/…`);
- a server in Docker (itzg image), quiesced through a hook.

## Phase 2: restores that fit how servers break (v0.3)

*Goal: roll back a griefed spawn without rolling back everyone's progress.*

1. **Partial restore.** `--dimension`, `--region`, or `--area x1,z1..x2,z2` (block
   coordinates) into a stopped live world. Must bring the matching `entities/` and `poi/`
   chunks along: since 1.17 entities live in separate files, and restoring blocks without
   them leaves mobs and item frames wrong.
2. **`vxl diff <a> <b>`** between snapshots, without restoring: which dimensions, regions
   and chunks changed. (`vxl compare` already does this for two folders.)
3. **`vxl show <id>`** for snapshot details; **`restore --at "2026-11-07 03:00"`** picks
   the nearest snapshot by time.

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

- **Offsite copies.** Packs never change once written, so `rsync`/`rclone` of a repo is
  cheap and incremental. Document first; built-in S3 target and encryption only if needed.
- **Fuzzing** the parsers that read untrusted bytes (anvil, NBT, pack) with `cargo-fuzz`.
- **Big-repo speed.** Cache pack indexes and snapshot headers so `open`/`list` stay fast
  after hundreds of snapshots.
- **Close the untested paths:** an LZ4 world, real use on Windows.
- **Releases.** Prebuilt binaries via GitHub Releases, so servers don't need Rust.

## Open questions

1. **Retention:** how far back should restores go? A week, a month, a season?
2. **Offsite:** repo on the server's disk only, or copied to another machine / cloud?
3. **Partial restore:** is griefing or broken builds a real problem? Decides whether
   Phase 2 comes before Phase 3.
4. **Frequency:** nightly only, or every few hours at peak? Affects how disruptive the
   hooks may be and how much incremental size matters.
