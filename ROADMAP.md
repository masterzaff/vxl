# vxl roadmap

Ordered by what makes vxl trustworthy for a real (Paper) server first, then nicer to
use, then smaller. Phase 3 is decided by measurements, not guesses.

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

## Phase 1: safe nightly backups of a Paper server (v0.2)

*Goal: one cron line backs up everything, correctly, every night.*

1. **Whole-server snapshots.** `vxl backup ~/server` finds world folders itself
   (`world`, `world_nether`, `world_the_end`, or anything with `level.dat` or `region/`)
   and stores them as **one snapshot**. Skips `logs/`, caches and jars unless asked;
   `--include` / `--exclude` to adjust. Restore gets `--only world_nether`.
   *First because today the Nether and End are easy to miss entirely.*
2. **RCON built in.** `save-off` → `save-all flush` → wait for "Saved the game" → back up →
   `save-on`. `save-on` **always** runs, also on error or Ctrl-C. Password from a file or
   environment variable, never the command line.
3. **Config file (`vxl.toml`).** Source, repo, RCON and retention in one place, so plain
   `vxl backup` does everything. README gets cron and systemd-timer examples.
4. **`vxl delete <id>…`.** Delete specific snapshots and garbage-collect in one step,
   under the repo lock. (Today: remove `snapshots/<id>.json.zst`, then `vxl prune`.)
5. **Retention policies.** `--keep-last 5 --keep-daily 7 --keep-weekly 4 --keep-monthly 6`,
   set once in the config.

**Done when:** a throwaway Paper server with players gets backed up nightly by cron,
all three dimensions in one snapshot, and a restore of it loads cleanly.

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
4. **Frequency:** nightly only, or every few hours at peak? Affects the RCON design and
   how much incremental size matters.
