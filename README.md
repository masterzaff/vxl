# vxl

Differential, deduplicating backups for Minecraft Java Edition worlds (Anvil format).

Your live world stays a normal world folder. vxl keeps its own content-addressed
store next to it: every chunk is decompressed, hashed with BLAKE3 and stored
**once** with zstd, so a chunk that hasn't changed since the last backup costs nothing.

```console
$ vxl init /backups/survival-repo
$ vxl backup ~/server/world --repo /backups/survival-repo -m "before nether update"
snapshot 3f2a9c | 4,812 chunks | 217 new | +18.4 MB stored (0.4s)

$ vxl list --repo /backups/survival-repo
3f2a9c  2026-11-08 14:30  before nether update  +18.4 MB
a91e07  2026-11-07 03:00  nightly               +6.1 MB
5c0d22  2026-11-01 12:00  initial               +1.2 GB

$ vxl restore a91e07 ~/restored-world --repo /backups/survival-repo
$ vxl verify --repo /backups/survival-repo
$ vxl prune --keep-last 10 --repo /backups/survival-repo
```

Set `VXL_REPO=/backups/survival-repo` to skip `--repo` every time.

## Install

```bash
cargo install --path .
```

## Commands

| command | what it does |
|---|---|
| `init <path> [--zstd-level N]` | create an empty repository (default level 6, see below) |
| `backup <world> [-m msg]` | snapshot a world; only new/changed chunks are stored |
| `list` | snapshots, newest first, with what each one added |
| `restore <id> <folder> [--force \| --force-wipe]` | rebuild a snapshot into a new or empty folder, or over an existing world (see below) |
| `verify [id]` | re-read and re-hash every object a snapshot (default: all) needs |
| `prune [--keep-last N] [--dry-run]` | drop old snapshots and garbage-collect data nothing references |

Snapshot ids can be shortened to any unique prefix, like git.

## Restoring over an existing world

By default `restore` only writes into a new or empty folder. To roll back a world in place
(**stop the server first**):

- `--force-wipe`: delete everything in the folder, then restore. The folder ends up exactly
  like the snapshot. This is what you normally want for a rollback.
- `--force`: overwrite only the files the snapshot has and leave every other file alone.
  Careful: region files created after the snapshot stay at their newer version, so areas
  explored later are *not* rolled back.

Both rebuild the full snapshot in a `.vxl-staging-*` folder inside the target first and only
move files into place once that has worked, so a failed restore (e.g. a damaged object)
leaves the world unchanged. They refuse to touch a folder without `level.dat`, so a typo
can't wipe your home directory.

## Backing up a running server

Minecraft writes chunks in the background, so a world copied mid-save can be torn.
Pause saving around the backup:

```
save-off
save-all flush
# run vxl backup
save-on
```

(Over RCON, e.g. with `mcrcon`.) If a file changes while vxl reads it, vxl warns you.

## How it works

```
repo/
  config.json               format version + zstd level
  lock                      OS file lock: backup/prune are exclusive, restore/verify shared
  objects/ab/cdef…          zstd(data), named by BLAKE3(data)
  snapshots/<id>.json.zst   one manifest per snapshot
  tmp/                      staging for atomic writes
```

- **Region files** (`r.X.Z.mca` in `region/`, `entities/`, `poi/`, every dimension) are split
  into chunks. Each chunk's NBT is decompressed (gzip, zlib, none or LZ4 — all four
  Minecraft types, including oversized chunks stored in `c.X.Z.mcc` files) and stored as
  an object keyed by the hash of the *decompressed* data. Two chunks with the same
  content share storage, however Minecraft compressed them.
- **Everything else** (`level.dat`, `playerdata/`, `data/`, …) is stored as a whole-file
  object. `session.lock` is skipped.
- **The manifest** lists every file, and for region files every chunk's slot, timestamp,
  original compression and object hash. Its own hash is the snapshot id.
- **Crash safety**: objects are written to `tmp/`, fsynced and renamed into place, and the
  manifest is written last. A backup that dies halfway leaves no snapshot, only unreferenced
  objects, which the next `prune` removes.
- **Restore** rebuilds each region file and recompresses each chunk with its original
  compression type. The result is chunk-for-chunk identical to the original, not
  byte-for-byte (sector layout and zlib output may differ); Minecraft doesn't care.
- **Damaged data**: a chunk that won't decompress is stored as its raw compressed bytes
  (`"verbatim": true` in the manifest) and written back unchanged on restore, so it's exactly
  as broken as before, never worse. A region file that isn't valid at all is stored as a
  plain file. A chunk whose header points outside the file (or at a missing `.mcc`) has no
  bytes to keep and is skipped. All of these print a warning.
- **Prune** deletes manifests first, then objects. If any kept snapshot can't be read,
  it refuses to delete objects at all.

### Choosing the zstd level

`examples/zstd_levels.rs` compresses chunks one at a time (as the store does) at every level:

```bash
cargo run --release --example zstd_levels                 # synthetic chunks
cargo run --release --example zstd_levels -- ~/server/world  # your own world
```

On synthetic terrain (1,600 chunks, single thread):

| level | size vs Minecraft's zlib | compress MB/s |
|---|---|---|
| 1 | 110.8% | 492 |
| 3 | 107.5% | 414 |
| **6** | **98.5%** | **146** |
| 9 | 96.6% | 88 |
| 12 | 95.1% | 30 |
| 19 | 88.5% | 6 |

Level 6 is the knee. Going up to 19 saves another ~10% but is ~25× slower, and decompression
speed is about the same at every level. Chunks deduplicate across snapshots, so
the level matters less over time than it might seem. Run it on your own world to check;
`vxl init --zstd-level N` overrides the default (it's fixed per repository).

## Known limitations

- Chunks the server has *loaded* get new `LastUpdate`/`InhabitedTime` values in their NBT,
  so they count as changed even when no block moved. Only chunks nobody visited dedupe for free.
- One file per object: on most filesystems each object also uses up to one block (4 KiB) of
  slack, so disk usage is a bit above the "stored" figure. Packing objects into larger
  files would fix that.
- `list` reads every manifest; fine up to hundreds of snapshots.

## Code map (for reading the source)

| file | role |
|---|---|
| `src/main.rs` | CLI (clap) and output formatting only |
| `src/anvil.rs` | region file parsing/writing, chunk (de)compression incl. Minecraft's LZ4 framing |
| `src/repo.rs` | repository, locking, object store (`put`/`get`), snapshot storage |
| `src/manifest.rs` | the snapshot data structures (serde) |
| `src/world.rs` | walks a world folder and classifies files |
| `src/commands/*.rs` | one file per subcommand, each returns a report struct |
| `src/synth.rs` | synthetic world generator for tests/benchmarks |
| `tests/roundtrip.rs` | end-to-end tests: backup → change → restore → prune |

```bash
cargo test                       # all tests
cargo run --release --example gen_world -- /tmp/world 2   # make a test world
```
