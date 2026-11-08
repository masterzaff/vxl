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
| `init <path> [--zstd-level N] [--pack-size-mb N]` | create an empty repository (defaults: level 6, 16 MiB packs) |
| `backup <world> [-m msg]` | snapshot a world; only new/changed chunks are stored |
| `list` | snapshots, newest first, with what each one added |
| `restore <id> <folder> [--force \| --force-wipe]` | rebuild a snapshot into a new or empty folder, or over an existing world (see below) |
| `verify [id]` | re-read and re-hash every object a snapshot (default: all) needs |
| `prune [--keep-last N] [--dry-run]` | drop old snapshots, delete/compact packs holding data nothing references |

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
  config.json               format version, zstd level, pack size
  lock                      OS file lock: backup/prune are exclusive, restore/verify shared
  packs/<id>.pack           ~16 MiB files of zstd-compressed objects, each with its own index
  snapshots/<id>.json.zst   one manifest per snapshot
  tmp/                      staging for atomic writes
```

- **Objects** (a chunk's NBT, or a whole file) are named by the BLAKE3 hash of their
  uncompressed content and stored zstd-compressed inside **pack files**. A pack is written
  once and never modified: object data, then an index (object id → offset, length) at the
  end. Its file name is the hash of its contents. On open, vxl reads every pack's index into
  memory, so finding an object is a hash-map lookup plus one read. A big world ends up
  as a few hundred files instead of hundreds of thousands, which is much kinder to HDDs,
  network shares, rsync and cloud sync, and wastes no space to filesystem block slack.

- **Region files** (`r.X.Z.mca` in `region/`, `entities/`, `poi/`, every dimension) are split
  into chunks. Each chunk's NBT is decompressed (gzip, zlib, none or LZ4 — all four
  Minecraft types, including oversized chunks stored in `c.X.Z.mcc` files) and stored as
  an object keyed by the hash of the *decompressed* data. Two chunks with the same
  content share storage, however Minecraft compressed them.
- **Everything else** (`level.dat`, `playerdata/`, `data/`, …) is stored as a whole-file
  object. `session.lock` is skipped.
- **The manifest** lists every file, and for region files every chunk's slot, timestamp,
  original compression and object hash. Its own hash is the snapshot id.
- **Crash safety**: packs are built in memory, written to `tmp/`, fsynced and renamed into
  place; the manifest is written last, after the final pack. A backup that dies halfway
  leaves no snapshot, only packs nobody references, which the next `prune` removes.
- **Restore** rebuilds each region file and recompresses each chunk with its original
  compression type. The result is chunk-for-chunk identical to the original, not
  byte-for-byte (sector layout and zlib output may differ); Minecraft doesn't care.
- **Damaged data**: a chunk that won't decompress is stored as its raw compressed bytes
  (`"verbatim": true` in the manifest) and written back unchanged on restore, so it's exactly
  as broken as before, never worse. A region file that isn't valid at all is stored as a
  plain file. A chunk whose header points outside the file (or at a missing `.mcc`) has no
  bytes to keep and is skipped. All of these print a warning.
- **Prune** deletes the dropped manifests, then works out which objects the remaining
  snapshots still need. Packs with nothing live are deleted. Packs that are ≥25% garbage,
  and small leftover packs from incremental backups, are **repacked**: their live objects
  are verified and copied into new packs, which are made durable *before* the old packs
  are deleted. Garbage in packs that are still mostly in use stays until a later prune
  pushes them over the threshold (prune tells you how much). If any kept snapshot can't
  be read, it refuses to delete anything.

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

### Experiment: transforming chunks before storing them

`examples/nbt_bench.rs` simulates a series of backups with reversible chunk transforms
(`src/nbt.rs`, `src/transform.rs`; not used by the CLI yet):

- **unpack**: expand bit-packed block/biome/heightmap arrays to one value per byte
- **split**: store each chunk as a small skeleton plus one object per 16³ section,
  so unchanged sections dedupe even when the chunk's timestamps change

```bash
cargo run --release --example nbt_bench                       # synthetic
cargo run --release --example nbt_bench -- day1/ day2/ day3/  # real world copies, oldest first
```

Synthetic result (4,096 chunks, 5 nights, 30% of chunks loaded and 3% edited per night):

| method | 1st backup | avg night | total after 5 nights |
|---|---|---|---|
| whole (today) | 25.8 MB | 8.2 MB | 67.0 MB |
| unpack | 105% | 105% | 105% |
| split | 133% | **25%** | **66%** |
| split + unpack | 139% | 23% | 68% |
| **grouped split** | 114% | **25%** | **59%** |

Unpacking didn't help on this data (its sections all fit in 4 bits per block, which is
already byte-aligned). Splitting makes nightly backups ~4× smaller, but compressing each
section on its own loses shared context, so the first backup grows by a third. *Grouped*
split keeps the dedup but compresses a chunk's skeleton and its new sections together as one
object, which brings the first backup down to +14%. Most of what's left is probably the
32-byte section hashes inside each skeleton (24 per chunk, incompressible: ~3.1 MB here).
Splitting runs ~3.3× slower than storing chunks whole. Every transformed chunk was verified
byte-identical; zero fallbacks.

## Known limitations

- Chunks the server has *loaded* get new `LastUpdate`/`InhabitedTime` values in their NBT,
  so they count as changed even when no block moved. Only chunks nobody visited dedupe for free.
- The pack index is held in memory: roughly 100 bytes per object, so ~100 MB for a
  million distinct chunks across all snapshots.
- Repositories made by the first development build (loose object files, format v1)
  can't be opened; create a new one.
- `list` reads every manifest; fine up to hundreds of snapshots.

## Code map (for reading the source)

| file | role |
|---|---|
| `src/main.rs` | CLI (clap) and output formatting only |
| `src/anvil.rs` | region file parsing/writing, chunk (de)compression incl. Minecraft's LZ4 framing |
| `src/repo.rs` | repository, locking, object store (`put`/`get`/`repack`), snapshot storage |
| `src/pack.rs` | the pack file format (pure: bytes in, bytes out) |
| `src/manifest.rs` | the snapshot data structures (serde) |
| `src/world.rs` | walks a world folder and classifies files |
| `src/commands/*.rs` | one file per subcommand, each returns a report struct |
| `src/synth.rs` | synthetic world generator for tests/benchmarks |
| `tests/roundtrip.rs` | end-to-end tests: backup → change → restore → prune |

```bash
cargo test                       # all tests
cargo run --release --example gen_world -- /tmp/world 2   # make a test world
```
