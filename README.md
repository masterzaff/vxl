# vxl

Differential, deduplicating backups for Minecraft Java Edition worlds (Anvil format).

Your live world stays a normal world folder. vxl keeps its own content-addressed
store next to it: every chunk is decompressed, hashed with BLAKE3 and stored
**once** with zstd, so a chunk that hasn't changed since the last backup costs nothing.

Java Edition only (vanilla, Paper and forks, Fabric, Forge/NeoForge). Region files (`.mca`,
`.mcc`) are deduplicated chunk by chunk; anything else in a world, including old `.mcr`
regions or the `.linear` format of some Paper forks, is backed up whole. Bedrock worlds
aren't supported.

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
$ vxl prune --keep-daily 7 --keep-weekly 4 --keep-monthly 6 --repo /backups/survival-repo
```

Set `VXL_REPO=/backups/survival-repo` to skip `--repo` every time, or put everything in a
[config file](#config-file-and-scheduling).

## Install

```bash
cargo install --path .
```

## Commands

| command | what it does |
|---|---|
| `init [path] [--zstd-level N] [--pack-size-mb N]` | create an empty repository (defaults: level 6, 16 MiB packs) |
| `backup [folder] [-w world]… [--pre cmd] [--post cmd] [--strict] [-m msg]` | snapshot a world, or every world in a server folder, as one snapshot; only new/changed chunks are stored (see below) |
| `list` | snapshots, newest first, with what each one added and which worlds it holds |
| `show <snapshot>` | one snapshot in detail: its worlds, files, chunks and sizes |
| `diff <a> [b] [--files]` | what changed from snapshot `a` to `b` (default: latest), per world and dimension, without restoring; chunks compare by content |
| `restore <id> <folder> [-w world] [--force \| --force-wipe] [--dimension d] [--area x1,z1..x2,z2]` | rebuild a snapshot (or one world, dimension or area of it) into a new or empty folder, or over an existing world (see below) |
| `verify [id]` | re-read and re-hash every object a snapshot (default: all) needs |
| `compare <a> <b>` | compare two world folders chunk by chunk (e.g. original vs restored) and check `b`'s region files are laid out so Minecraft can read them |
| `prune [--keep-last/hourly/daily/weekly/monthly/yearly N]… [--dry-run]` | drop snapshots no rule keeps, delete/compact packs holding data nothing references (see below) |
| `delete <id>… [--dry-run]` | drop exactly these snapshots and collect the data only they used |

Wherever a snapshot is expected you can give its id, shortened to any unique prefix like
git; `latest`; or a local time, for the newest snapshot at or before it:
`vxl restore "2026-11-07 03:00" ~/rollback` (a bare date means the end of that day). If the
repo holds snapshots of several sources that would match, vxl asks for an id instead of
guessing. Every command takes
`-c/--config <file>` (see below).

## Config file and scheduling

Instead of repeating options, put them in `vxl.toml`. vxl reads `./vxl.toml` if it exists,
or the file given with `--config` / `VXL_CONFIG`:

```toml
source = "/srv/mc"              # a world, or a server folder
repo = "/backups/mc-repo"
worlds = ["*", "!creative"]     # optional; "*" (all) is the default
strict = false                  # true = fail rather than save a possibly torn snapshot

[hooks]
pre = "docker exec mc rcon-cli save-off && docker exec mc rcon-cli save-all flush"
post = "docker exec mc rcon-cli save-on"

[keep]                          # retention for `vxl prune`
daily = 7
weekly = 4
monthly = 6
```

Relative paths are relative to the file. Command-line options win: `--repo`/`VXL_REPO`, a
folder argument, `-w`, `--pre`/`--post`. If `prune` gets any `--keep-*` rule, the file's
`[keep]` is ignored entirely, so what you type is exactly what applies. Unknown keys are an
error, so a typo like `dialy = 7` can't silently keep nothing.

With that in place a nightly backup is one line. **cron** (`crontab -e`):

```cron
30 4 * * *  vxl -c /srv/mc/vxl.toml backup -m nightly && vxl -c /srv/mc/vxl.toml prune
```

**systemd timer** (`/etc/systemd/system/vxl.service` and `vxl.timer`):

```ini
# vxl.service
[Unit]
Description=vxl backup of the Minecraft server

[Service]
Type=oneshot
Environment=VXL_CONFIG=/srv/mc/vxl.toml
ExecStart=/usr/local/bin/vxl backup -m nightly
ExecStartPost=/usr/local/bin/vxl prune
```

```ini
# vxl.timer
[Unit]
Description=Nightly vxl backup

[Timer]
OnCalendar=*-*-* 04:30
Persistent=true

[Install]
WantedBy=timers.target
```

Enable with `systemctl enable --now vxl.timer`. If systemd stops the backup (SIGTERM),
the post hook still runs.

## Retention

`prune` keeps a snapshot if any rule keeps it:

| rule | keeps |
|---|---|
| `--keep-last N` | the newest N snapshots |
| `--keep-hourly N` | the newest snapshot of each of the last N hours that have one |
| `--keep-daily N` | … of each of the last N days |
| `--keep-weekly N` | … of each of the last N weeks (ISO weeks, Monday to Sunday) |
| `--keep-monthly N` | … of each of the last N months |
| `--keep-yearly N` | … of each of the last N years |

Days, weeks etc. are in local time, and periods without a snapshot don't count, so a server
that was off for a week still keeps 7 *backed-up* days. Rules apply to each source on its own
(the folder backed up, plus which worlds), so one repo can hold several servers without one
crowding out another. Without any rule, `prune` deletes no snapshots and only collects garbage.
`--dry-run` shows what would go and which rules keep the rest:

```console
$ vxl prune --keep-daily 7 --keep-weekly 4 --dry-run
would keep   3f2a9c (daily, weekly)
would keep   a91e07 (daily)
…
would remove snapshot 5c0d22
```

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

## Rolling back part of a world

Griefed spawn, but everyone else's progress should stay? Restore just that area (with the
server stopped):

```bash
vxl restore latest ~/server --force --dimension overworld --area -200,-200..200,200
```

`--area` takes block coordinates (corners in any order) and restores every chunk the
rectangle touches. `--dimension` alone restores a whole dimension (`overworld`, `nether`,
`end`, or a modded `namespace:name`). In a server snapshot the selection applies to every
world, so `--dimension nether` finds `world_nether/DIM-1` on Paper and `world/DIM-1` on
vanilla alike.

- Blocks, entities (mobs, item frames, armor stands) and points of interest (beds,
  workstations, portals) are restored together; they live in `region/`, `entities/` and
  `poi/` at the same coordinates, and restoring only blocks leaves mobs and villagers wrong.
- Inside the selection the world becomes exactly the snapshot: chunks generated after it
  are removed and the game generates them again. Outside it, nothing changes.
- `level.dat` and player data are never touched, so inventories and positions stay as they
  are now.
- All affected region files are rebuilt in a staging folder first; if anything fails, the
  world is unchanged. Without `--force` it only writes into a new or empty folder, which
  gives you just those region files.

## Backing up a running server

Minecraft writes chunks in the background, so a world read mid-save can be torn. vxl
doesn't know how your server runs; you tell it with two shell commands:

- `--pre <cmd>` runs first and should leave the world consistent on disk (usually: pause
  saving and flush). If it fails, nothing is backed up.
- `--post <cmd>` runs afterwards (usually: resume saving). It **always** runs once `--pre`
  has run: also when the backup fails, and on Ctrl-C, SIGTERM or SIGHUP (vxl stops the
  backup, runs `--post`, then exits with 130). It runs in its own process group, so the
  Ctrl-C doesn't hit it too. If `--post` fails, vxl exits non-zero, but a snapshot that was
  written is kept.

They run through `sh -c` (`cmd /C` on Windows) in vxl's working directory, with
`VXL_SOURCE` and `VXL_REPO` set; `--post` also gets `VXL_RESULT` (`ok`, `failed` or
`interrupted`) and, on success, `VXL_SNAPSHOT` (the snapshot id).

Hooks or not, vxl checks every read: a file whose size or modification time changed while it
was being read, or a region file with damaged chunks, is read again (up to 5 times, waiting a
little longer each time). A damaged file that reads the same twice is damaged on disk, not
torn, and is kept exactly as it is. A file that never settles is stored as last read with a
warning; with `--strict` the backup fails instead, so no possibly torn snapshot is saved.

**Docker** ([itzg/minecraft-server](https://github.com/itzg/docker-minecraft-server) has
`rcon-cli` built in):

```bash
vxl backup ~/mc/data -m nightly \
  --pre  'docker exec mc rcon-cli save-off && docker exec mc rcon-cli save-all flush' \
  --post 'docker exec mc rcon-cli save-on'
```

**RCON** with [mcrcon](https://github.com/Tiiffi/mcrcon) (`enable-rcon=true` in
`server.properties`; keep the RCON port off the internet, it's plaintext):

```bash
export MCRCON_PASS="$(cat ~/.rcon-password)"   # not on the command line
vxl backup ~/server -m nightly \
  --pre  'mcrcon save-off "save-all flush"' \
  --post 'mcrcon save-on'
```

**tmux / screen console** (no reply to wait for, so give the save a moment):

```bash
vxl backup ~/server \
  --pre  'tmux send-keys -t mc "save-off" Enter "save-all flush" Enter && sleep 15' \
  --post 'tmux send-keys -t mc "save-on" Enter'
```

**Stop the server** (simplest and always consistent, at the cost of downtime):

```bash
vxl backup ~/server --pre 'systemctl stop minecraft' --post 'systemctl start minecraft'
```

**ZFS snapshot** (the server never pauses; vxl reads the frozen copy, which `--pre` creates):

```bash
vxl backup /tank/mc/.zfs/snapshot/vxl \
  --pre  'zfs destroy tank/mc@vxl 2>/dev/null; zfs snapshot tank/mc@vxl' \
  --post 'zfs destroy tank/mc@vxl'
```

The snapshot is only crash-consistent (like pulling the plug), so pair it with
`save-all flush` in `--pre` if you can. btrfs and LVM snapshots work the same way.

## Whole servers

Point `backup` at the server folder and every world in it goes into **one snapshot**:

```console
$ vxl backup ~/server -m nightly
worlds: world, world_nether, world_the_end
snapshot 8c41d0 | 131,052 chunks | 2,311 new | +41.7 MB stored (1.9s)
```

A world is a folder with a `level.dat`, found up to two levels down (`world/`, or
`worlds/lobby/` with Paper's `--world-container`). Everything inside a world belongs to it,
so vanilla/Fabric/Forge dimensions (`world/DIM-1`, `world/dimensions/<mod>/…`) come along
with `world`, while Paper's separate `world_nether` / `world_the_end` and Multiverse worlds
are worlds of their own. `plugins/`, `mods/`, `config/`, `backups/`, `logs/` and similar
folders are never searched, so a backup plugin's world copies aren't picked up. Symlinked
worlds are followed.

**Only worlds are backed up.** Plugin data (claims, permissions, economy), configs and jars
are not. Back those up with an ordinary file backup.

Pick worlds with `-w` / `--world` (repeatable):

| `-w` | backs up |
|---|---|
| *(none)* or `'*'` | every world found |
| `world -w world_nether` | exactly these (by folder name) |
| `worlds/lobby` | the world at that path under the server folder |
| `'*' -w '!creative'` | all but `creative` |

Two selected worlds with the same folder name (`a/world1`, `b/world1`) are an error; select
one by its path.

Restoring a server snapshot puts each world back where it was (`<folder>/world_nether`,
`<folder>/worlds/lobby`, …) and writes nothing else, so `vxl restore <id> ~/server --force-wipe`
rolls every world back in place and leaves plugins and configs alone. If any world fails to
rebuild, none of them is touched. `-w <name>` restores a single world into `<folder>` itself,
as a standalone world folder you can load or hand to someone:

```bash
vxl restore 8c41d0 ~/nether-copy -w world_nether
```

Player data (inventories, positions, stats, advancements) lives only in the main world
(`world/playerdata/` etc.), so restoring just `world_nether` rolls back the Nether's blocks
but not anyone's inventory.

Backing up a single world folder (`vxl backup ~/server/world`) still works and restores as
that world folder, like before.

To test a restore, start a throwaway copy of the server (same Paper version and plugins,
different `server-port`) on the restored folders. Opening a Paper world in vanilla
singleplayer can fail on data packs even when the world itself is fine.

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
- **The manifest** lists the snapshot's worlds and, for each, every file, and for region
  files every chunk's slot, timestamp, original compression and object hash. Its own hash
  is the snapshot id. All worlds share one object store, so content that is identical across
  worlds (a copied arena or lobby) is stored once.
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
speed is about the same at every level. A real survival world (2,986 sampled chunks, avg
29 KB raw) showed the same shape: level 6 at 97.4% of zlib's size, levels 6–12 within 1.5%
of each other, level 19 at 88.1% but 40× slower. Chunks deduplicate across snapshots, so
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
