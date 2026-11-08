use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use chrono::Local;
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};

use vxl::commands::{backup, compare, diff, list, migrate, partial, prune, restore, verify};
use vxl::config::Config;
use vxl::hooks::{self, Hooks};
use vxl::repo::{self, LockMode, Repo};
use vxl::retention::Policy;
use vxl::util::{human_bytes, thousands};

/// Differential, deduplicating backups for Minecraft Java worlds.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Config file with source, repo, worlds, hooks and retention (default: ./vxl.toml if
    /// it exists). Command-line options override it.
    #[arg(long, short = 'c', global = true, env = "VXL_CONFIG", value_name = "FILE")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args)]
struct RepoArg {
    /// Path to the vxl repository (default: `repo` in the config file).
    #[arg(long, short = 'r', env = "VXL_REPO")]
    repo: Option<PathBuf>,
}

impl RepoArg {
    fn path(&self, cfg: &Config) -> Result<PathBuf> {
        match self.repo.as_ref().or(cfg.repo.as_ref()) {
            Some(p) => Ok(p.clone()),
            None => bail!("no repository given: pass --repo, set VXL_REPO, or set `repo` in vxl.toml"),
        }
    }
}

#[derive(clap::Args)]
struct KeepArgs {
    /// Keep the newest N snapshots.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    keep_last: Option<u32>,
    /// Keep the newest snapshot of each of the last N hours that have one.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    keep_hourly: Option<u32>,
    /// Keep the newest snapshot of each of the last N days that have one.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    keep_daily: Option<u32>,
    /// Keep the newest snapshot of each of the last N (ISO) weeks that have one.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    keep_weekly: Option<u32>,
    /// Keep the newest snapshot of each of the last N months that have one.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    keep_monthly: Option<u32>,
    /// Keep the newest snapshot of each of the last N years that have one.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    keep_yearly: Option<u32>,
}

impl KeepArgs {
    fn policy(&self) -> Policy {
        Policy {
            last: self.keep_last,
            hourly: self.keep_hourly,
            daily: self.keep_daily,
            weekly: self.keep_weekly,
            monthly: self.keep_monthly,
            yearly: self.keep_yearly,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a new, empty repository.
    Init {
        /// Where to create it (wins over --repo, VXL_REPO and `repo` in the config file).
        path: Option<PathBuf>,
        #[command(flatten)]
        repo: RepoArg,
        /// zstd level for stored chunks (1–22). Higher = smaller but slower backups.
        #[arg(long, default_value_t = repo::DEFAULT_ZSTD_LEVEL)]
        zstd_level: i32,
        /// Target size of pack files, in MiB.
        #[arg(long, default_value_t = repo::DEFAULT_PACK_SIZE >> 20)]
        pack_size_mb: u64,
        /// Storage format. 3 stores each chunk's unchanged sections only once, and each
        /// region's chunk list only when it changed, which makes nightly backups several
        /// times smaller. 2 stores every changed chunk whole; `vxl migrate --to 2` converts back.
        #[arg(long, default_value_t = repo::DEFAULT_REPO_VERSION, value_parser = clap::value_parser!(u32).range(2..=3))]
        format: u32,
    },
    /// Snapshot a world, or every world in a server folder, as one snapshot.
    /// Only new or changed chunks are stored.
    Backup {
        /// A world folder, or a server folder: then every folder with a level.dat up to two
        /// levels down is a world (except in plugins/, backups/, logs/ and similar).
        /// Default: `source` in the config file.
        source: Option<PathBuf>,
        /// Which worlds to back up: a folder name (`world_nether`), a path under SOURCE
        /// (`worlds/lobby`), `*` for all, `!name` to leave one out. Repeatable; default `*`.
        #[arg(long = "world", short = 'w', value_name = "WORLD")]
        worlds: Vec<String>,
        /// Shell command to run first, to make the world consistent on disk (e.g. pause saving:
        /// `docker exec mc rcon-cli save-off && docker exec mc rcon-cli 'save-all flush'`).
        /// If it fails, nothing is backed up. Runs before SOURCE is read, so it may create it
        /// (e.g. a ZFS snapshot). Gets VXL_SOURCE and VXL_REPO.
        #[arg(long, value_name = "CMD")]
        pre: Option<String>,
        /// Shell command to run afterwards (e.g. `save-on`). Always runs once --pre has run:
        /// also when the backup fails or is interrupted. Gets VXL_RESULT=ok|failed|interrupted
        /// and, on success, VXL_SNAPSHOT=<id>.
        #[arg(long, value_name = "CMD")]
        post: Option<String>,
        /// Fail instead of warning if a file keeps changing while vxl reads it (a sign the
        /// server is still saving), so no possibly torn snapshot is saved.
        #[arg(long)]
        strict: bool,
        #[command(flatten)]
        repo: RepoArg,
        /// Note to attach to the snapshot.
        #[arg(long, short)]
        message: Option<String>,
    },
    /// List snapshots, newest first.
    List {
        #[command(flatten)]
        repo: RepoArg,
    },
    /// What changed between two snapshots, per world and dimension, without restoring.
    /// Chunks compare by content, so a chunk that was only re-saved doesn't count.
    Diff {
        /// The older snapshot: an id, `latest` or a time (see `restore`).
        a: String,
        /// The newer snapshot (default: latest).
        #[arg(default_value = "latest")]
        b: String,
        /// Also list every changed file.
        #[arg(long)]
        files: bool,
        #[command(flatten)]
        repo: RepoArg,
    },
    /// Show one snapshot in detail: its worlds and what each holds.
    Show {
        /// An id (any unique prefix), `latest`, or a time (see `restore`).
        snapshot: String,
        #[command(flatten)]
        repo: RepoArg,
    },
    /// Rebuild a snapshot into a new folder (never touches the live world).
    ///
    /// A snapshot of one world folder restores as that world folder. A snapshot of a server
    /// folder restores each world where it was in the server (TARGET/world_nether,
    /// TARGET/worlds/lobby, …) and nothing else. With --world, just that world is restored,
    /// into TARGET itself.
    Restore {
        /// Snapshot: an id (any unique prefix), `latest`, or a time (`2026-11-07`,
        /// "2026-11-07 03:00") for the newest snapshot at or before it.
        snapshot: String,
        /// Folder to restore into. Each world folder in it must not exist or be empty unless
        /// --force/--force-wipe.
        target: PathBuf,
        /// Restore only this world (by name, see `vxl list`), as a standalone world folder.
        #[arg(long, short = 'w')]
        world: Option<String>,
        /// Target is an existing world: overwrite the files the snapshot has, keep any others.
        #[arg(long, conflicts_with = "force_wipe")]
        force: bool,
        /// Target is an existing world: delete everything in it first, so it ends up exactly
        /// like the snapshot. This is what you want to roll a world back.
        #[arg(long)]
        force_wipe: bool,
        /// Restore only this dimension (`overworld`, `nether`, `end`, or `mod:name`) into the
        /// existing world(s) with --force, leaving the rest alone.
        #[arg(long, conflicts_with = "force_wipe")]
        dimension: Option<String>,
        /// Restore only the chunks touching this rectangle, in block coordinates
        /// (`x1,z1..x2,z2`), with their entities and POIs. Needs --dimension if the world has
        /// several. Inside it the world becomes exactly the snapshot; everything else stays.
        #[arg(long, value_name = "X1,Z1..X2,Z2", conflicts_with = "force_wipe", allow_hyphen_values = true)]
        area: Option<String>,
        #[command(flatten)]
        repo: RepoArg,
    },
    /// Delete snapshots a retention policy doesn't keep, and garbage-collect unreferenced data.
    ///
    /// A snapshot is kept if any --keep-* rule keeps it. Rules apply separately to each
    /// source (folder + worlds) in the repo, and periods are in local time. Without any
    /// rule, nothing is deleted and only garbage is collected.
    Prune {
        #[command(flatten)]
        keep: KeepArgs,
        /// Show what would be deleted (and why the rest is kept) without deleting anything.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        repo: RepoArg,
    },
    /// Delete specific snapshots and garbage-collect what only they used.
    Delete {
        /// Snapshots: ids (any unique prefix), `latest`, or times (see `restore`).
        #[arg(required = true)]
        snapshots: Vec<String>,
        /// Show what would be deleted without deleting anything.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        repo: RepoArg,
    },
    /// Convert a repository to another storage format (see `vxl init --format`). Old copies
    /// of the data are removed only after the new ones check out. To 3, snapshot ids stay
    /// the same; back to 2, snapshots made in format 3 get new ids (printed).
    Migrate {
        /// The format to convert to.
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(2..=3))]
        to: u32,
        #[command(flatten)]
        repo: RepoArg,
    },
    /// Check that every object a snapshot needs is present and intact.
    Verify {
        /// Snapshot to check: an id, `latest` or a time (default: all snapshots).
        snapshot: Option<String>,
        #[command(flatten)]
        repo: RepoArg,
    },
    /// Compare two world folders chunk by chunk (e.g. original vs restored).
    /// Also checks that region files in the second one are readable by Minecraft.
    Compare {
        /// The reference world (e.g. the original).
        a: PathBuf,
        /// The world to check (e.g. the restored copy).
        b: PathBuf,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn bar(msg: &'static str, bytes: bool) -> ProgressBar {
    let tmpl = if bytes {
        "{msg} [{bar:30}] {bytes}/{total_bytes} ({eta})"
    } else {
        "{msg} [{bar:30}] {pos}/{len} ({eta})"
    };
    // Draws to stderr and hides itself automatically when that's not a terminal.
    let pb = ProgressBar::new(0).with_message(msg);
    pb.set_style(ProgressStyle::with_template(tmpl).unwrap().progress_chars("=> "));
    pb
}

fn short(id: &repo::ObjectId, len: usize) -> String {
    id.hex()[..len].to_string()
}

fn print_warnings(warnings: &[String]) {
    const MAX: usize = 20;
    for w in warnings.iter().take(MAX) {
        eprintln!("warning: {w}");
    }
    if warnings.len() > MAX {
        eprintln!("warning: … and {} more", warnings.len() - MAX);
    }
}

fn open(r: &RepoArg, cfg: &Config, lock: LockMode) -> Result<Repo> {
    let repo = Repo::open(&r.path(cfg)?, lock)?;
    print_warnings(&repo.warnings);
    Ok(repo)
}

fn print_removal(r: &prune::PruneReport, dry_run: bool) {
    let verb = if dry_run { "would remove" } else { "removed" };
    for id in &r.removed_snapshots {
        println!("{verb} snapshot {}", short(id, 6));
    }
    println!(
        "{verb} {} snapshots, kept {} | {} {} objects | {} {} packs, {} {} | {} {}",
        r.removed_snapshots.len(),
        r.kept_snapshots,
        if dry_run { "would delete" } else { "deleted" },
        thousands(r.deleted_objects),
        if dry_run { "would drop" } else { "dropped" },
        r.packs_deleted,
        if dry_run { "would repack" } else { "repacked" },
        r.packs_repacked,
        if dry_run { "would free" } else { "freed" },
        human_bytes(r.freed_bytes),
    );
    if r.garbage_left > 0 {
        println!("({} unused objects stay in packs that are mostly still in use)", thousands(r.garbage_left));
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    // `compare` works on plain folders and never needs a config.
    let cfg = match cli.cmd {
        Cmd::Compare { .. } => Config::default(),
        _ => Config::find(cli.config.as_deref())?.0,
    };
    match cli.cmd {
        Cmd::Init { path, repo, zstd_level, pack_size_mb, format } => {
            // A path given right here wins over --repo / VXL_REPO / the config file.
            let path = RepoArg { repo: path.or(repo.repo) }.path(&cfg)?;
            Repo::init_format(&path, zstd_level, pack_size_mb << 20, format)?;
            println!("initialized empty vxl repository (format {format}) in {}", path.display());
        }

        Cmd::Migrate { repo, to } => {
            let mut repo = open(&repo, &cfg, LockMode::Exclusive)?;
            let was = repo.config.version;
            let pb = bar("converting", false);
            let r = migrate::migrate(&mut repo, to, &pb)?;
            pb.finish_and_clear();
            let n = r.renamed.iter().map(|(a, b)| repo::short_len(&[*a, *b])).max().unwrap_or(6);
            for (old, new) in &r.renamed {
                println!("snapshot {} is now {}", &old.hex()[..n], &new.hex()[..n]);
            }
            println!(
                "format {was} → {to}: {} snapshots, {} chunks ({} converted, {} unchanged) | {} → {}",
                r.snapshots,
                thousands(r.chunks),
                thousands(r.converted),
                thousands(r.unchanged),
                human_bytes(r.bytes_before),
                human_bytes(r.bytes_after),
            );
        }

        Cmd::Backup { source, worlds, repo: repo_arg, message, pre, post, strict } => {
            // Lock first: if another backup is running, fail before pausing the server.
            let repo = open(&repo_arg, &cfg, LockMode::Exclusive)?;
            let Some(source) = source.or(cfg.source.clone()) else {
                bail!("nothing to back up: pass a world or server folder, or set `source` in vxl.toml");
            };
            let worlds = if worlds.is_empty() { cfg.worlds.selectors() } else { worlds };
            let strict = strict || cfg.strict;
            // Hooks run in our own working directory: SOURCE may not exist until `pre` ran.
            let abs = |p: &std::path::Path| std::path::absolute(p).map(|p| p.display().to_string());
            let hooks = Hooks {
                pre: pre.or(cfg.hooks.pre.clone()),
                post: post.or(cfg.hooks.post.clone()),
                env: vec![
                    ("VXL_SOURCE".into(), abs(&source).context("resolving the source path")?),
                    ("VXL_REPO".into(), abs(repo.root()).context("resolving the repo path")?),
                ],
                cwd: std::env::current_dir().context("reading the working directory")?,
            };
            if hooks.pre.is_some() || hooks.post.is_some() {
                hooks::install_interrupt_handler()?;
            }
            let start = Instant::now();
            let pb = bar("backing up", true);
            let out = hooks.around(
                || {
                    let opts = backup::Options { worlds, message, strict };
                    let r = backup::backup(&repo, &source, &opts, &pb);
                    pb.finish_and_clear();
                    r
                },
                |r| vec![("VXL_SNAPSHOT".into(), r.id.hex())],
            );
            let r = match out.result {
                Ok(r) => r,
                Err(e) => {
                    pb.finish_and_clear();
                    eprintln!("error: {e:#}");
                    if let Some(pe) = out.post_error {
                        eprintln!("error: {pe:#}");
                    }
                    return Ok(if hooks::interrupted() { ExitCode::from(130) } else { ExitCode::FAILURE });
                }
            };
            print_warnings(&r.warnings);
            if r.worlds.len() > 1 || r.worlds[0].rel != "." {
                let names: Vec<String> = r
                    .worlds
                    .iter()
                    .map(|w| if w.rel == w.name { w.name.clone() } else { format!("{} ({})", w.name, w.rel) })
                    .collect();
                println!("worlds: {}", names.join(", "));
            }
            let s = &r.stats;
            println!(
                "snapshot {} | {} chunks | {} new | +{} stored ({:.1}s)",
                short(&r.id, 6),
                thousands(s.chunks),
                thousands(s.new_chunks),
                human_bytes(s.stored_bytes),
                start.elapsed().as_secs_f64()
            );
            if let Some(pe) = out.post_error {
                eprintln!("error: {pe:#} (the snapshot itself was saved)");
                return Ok(ExitCode::FAILURE);
            }
        }

        Cmd::List { repo } => {
            let repo = open(&repo, &cfg, LockMode::None)?;
            let snaps = list::list(&repo)?;
            if snaps.is_empty() {
                println!("no snapshots yet");
            }
            let ids: Vec<_> = snaps.iter().map(|(id, _)| *id).collect();
            let len = repo::short_len(&ids);
            let width = snaps
                .iter()
                .map(|(_, h)| h.message.as_deref().unwrap_or("").chars().count())
                .max()
                .unwrap_or(0);
            for (id, h) in &snaps {
                println!(
                    "{}  {}  {:width$}  +{:<9}  {}",
                    short(id, len),
                    h.created.with_timezone(&Local).format("%Y-%m-%d %H:%M"),
                    h.message.as_deref().unwrap_or(""),
                    human_bytes(h.stats.stored_bytes),
                    h.world_names.join(", "),
                );
            }
        }

        Cmd::Diff { a, b, files, repo } => {
            let repo = open(&repo, &cfg, LockMode::None)?;
            let (ia, ib) = (list::resolve(&repo, &a)?, list::resolve(&repo, &b)?);
            let (sa, sb) = (repo.load_snapshot(&ia)?, repo.load_snapshot(&ib)?);
            println!("{} → {}", short(&ia, 6), short(&ib, 6));
            let counts = |c: &diff::Counts| {
                let mut parts = vec![];
                for (n, what) in [(c.changed, "changed"), (c.added, "added"), (c.removed, "removed")] {
                    if n > 0 {
                        parts.push(format!("{} {what}", thousands(n)));
                    }
                }
                if parts.is_empty() { "no chunk changes".to_string() } else { parts.join(", ") }
            };
            for w in diff::diff(&sa, &sb) {
                match w.only_in {
                    Some(diff::Side::A) => println!("{}: only in {}", w.name, short(&ia, 6)),
                    Some(diff::Side::B) => println!("{}: only in {}", w.name, short(&ib, 6)),
                    None if w.is_same() => println!("{}: unchanged", w.name),
                    None => {
                        let other = match w.other_files() {
                            0 => String::new(),
                            n => format!(" | {n} other file{} changed", if n == 1 { "" } else { "s" }),
                        };
                        println!("{}: chunks {}{other}", w.name, counts(&w.chunks()));
                        for (dim, c) in &w.dimensions {
                            println!("  {dim}: {}", counts(c));
                        }
                    }
                }
                if files && w.only_in.is_none() {
                    for f in &w.files {
                        let mark = match f.change {
                            diff::Change::Added => 'A',
                            diff::Change::Removed => 'D',
                            diff::Change::Changed => 'M',
                        };
                        let detail =
                            f.chunks.as_ref().map(|c| format!(" ({})", counts(c))).unwrap_or_default();
                        println!("    {mark} {}{detail}", f.path);
                    }
                }
            }
        }

        Cmd::Show { snapshot, repo } => {
            let repo = open(&repo, &cfg, LockMode::None)?;
            let id = list::resolve(&repo, &snapshot)?;
            let snap = repo.load_snapshot(&id)?;
            let h = &snap.header;
            println!("snapshot  {}", id.hex());
            println!("created   {}", h.created.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S %Z"));
            if let Some(m) = &h.message {
                println!("message   {m}");
            }
            println!("source    {}", h.source);
            println!("format    {}", h.version);
            let rows: Vec<[String; 6]> = snap
                .worlds
                .iter()
                .map(|w| {
                    [
                        w.name.clone(),
                        w.path.clone(),
                        thousands(w.stats.files),
                        thousands(w.stats.chunks),
                        human_bytes(w.stats.world_bytes),
                        format!("+{}", human_bytes(w.stats.stored_bytes)),
                    ]
                })
                .chain(std::iter::once([
                    "total".into(),
                    String::new(),
                    thousands(h.stats.files),
                    thousands(h.stats.chunks),
                    human_bytes(h.stats.world_bytes),
                    format!("+{}", human_bytes(h.stats.stored_bytes)),
                ]))
                .collect();
            let head = ["world", "path", "files", "chunks", "on disk", "added"];
            let width =
                |i: usize| rows.iter().map(|r| r[i].chars().count()).chain([head[i].len()]).max().unwrap();
            let w: Vec<usize> = (0..6).map(width).collect();
            println!();
            println!(
                "  {:<a$}  {:<b$}  {:>c$}  {:>d$}  {:>e$}  {:>f$}",
                head[0],
                head[1],
                head[2],
                head[3],
                head[4],
                head[5],
                a = w[0],
                b = w[1],
                c = w[2],
                d = w[3],
                e = w[4],
                f = w[5]
            );
            for r in &rows {
                println!(
                    "  {:<a$}  {:<b$}  {:>c$}  {:>d$}  {:>e$}  {:>f$}",
                    r[0],
                    r[1],
                    r[2],
                    r[3],
                    r[4],
                    r[5],
                    a = w[0],
                    b = w[1],
                    c = w[2],
                    d = w[3],
                    e = w[4],
                    f = w[5]
                );
            }
        }

        Cmd::Restore { snapshot, target, world, force, force_wipe, dimension, area, repo } => {
            let repo = open(&repo, &cfg, LockMode::Shared)?;
            let id = list::resolve(&repo, &snapshot)?;
            if dimension.is_some() || area.is_some() {
                let sel = partial::Selection {
                    dimension: dimension.as_deref().map(vxl::world::normalize_dimension),
                    area: area.as_deref().map(partial::Area::parse).transpose()?,
                };
                let source = PathBuf::from(repo.load_header(&id)?.source);
                if force && target.canonicalize().is_ok_and(|t| t.starts_with(&source)) {
                    eprintln!(
                        "warning: restoring into the world this snapshot came from; make sure the server is stopped"
                    );
                }
                let pb = bar("restoring", false);
                let r = partial::restore_partial(&repo, &id, &target, world.as_deref(), &sel, force, &pb)?;
                pb.finish_and_clear();
                print_warnings(&r.warnings);
                let what = match (&sel.dimension, &sel.area) {
                    (Some(d), Some(a)) => format!("{} chunks of {d}", thousands(a.chunks())),
                    (None, Some(a)) => format!("{} chunks", thousands(a.chunks())),
                    (Some(d), None) => d.clone(),
                    (None, None) => unreachable!(),
                };
                println!(
                    "restored {what} from {} into {} | {} chunks put back, {} removed | {} region files written, {} deleted",
                    short(&id, 6),
                    target.display(),
                    thousands(r.chunks_restored),
                    thousands(r.chunks_removed),
                    thousands(r.files_written),
                    thousands(r.files_deleted),
                );
                return Ok(ExitCode::SUCCESS);
            }
            let mode = match (force, force_wipe) {
                (_, true) => restore::Mode::Wipe,
                (true, _) => restore::Mode::Replace,
                _ => restore::Mode::Fresh,
            };
            if mode != restore::Mode::Fresh {
                let source = PathBuf::from(repo.load_header(&id)?.source);
                if target.canonicalize().is_ok_and(|t| t.starts_with(&source)) {
                    eprintln!(
                        "warning: restoring over the world this snapshot came from; make sure the server is stopped"
                    );
                }
            }
            let pb = bar("restoring", false);
            let r = restore::restore(&repo, &id, &target, world.as_deref(), mode, &pb)?;
            pb.finish_and_clear();
            let extra = match mode {
                restore::Mode::Fresh => String::new(),
                restore::Mode::Replace => format!(" | {} replaced", thousands(r.replaced)),
                restore::Mode::Wipe => format!(" | wiped {} old entries", thousands(r.wiped)),
            };
            let into = match r.worlds.as_slice() {
                [(_, dest)] if *dest == target => target.display().to_string(),
                ws => format!(
                    "{} ({})",
                    target.display(),
                    ws.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(", ")
                ),
            };
            println!(
                "restored {} into {into} | {} files | {} chunks{extra}",
                short(&r.id, 6),
                thousands(r.files),
                thousands(r.chunks)
            );
        }

        Cmd::Prune { keep, dry_run, repo } => {
            let repo = open(&repo, &cfg, LockMode::Exclusive)?;
            let r = prune::prune(&repo, &cfg.policy(&keep.policy()), dry_run)?;
            if dry_run {
                for (id, why) in &r.kept_by {
                    let why: Vec<&str> = why.iter().map(|p| p.name()).collect();
                    println!("would keep   {} ({})", short(id, 6), why.join(", "));
                }
            }
            print_removal(&r, dry_run);
        }

        Cmd::Delete { snapshots, dry_run, repo } => {
            let repo = open(&repo, &cfg, LockMode::Exclusive)?;
            let ids = snapshots.iter().map(|s| list::resolve(&repo, s)).collect::<Result<Vec<_>>>()?;
            let r = prune::delete(&repo, &ids, dry_run)?;
            print_removal(&r, dry_run);
        }

        Cmd::Compare { a, b } => {
            let r = compare::compare(&a, &b)?;
            for d in r.differences.iter().take(30) {
                println!("differs: {d}");
            }
            if r.differences.len() > 30 {
                println!("differs: … and {} more", r.differences.len() - 30);
            }
            for d in r.layout_problems.iter().take(30) {
                println!("layout:  {d}");
            }
            if r.layout_problems.len() > 30 {
                println!("layout:  … and {} more", r.layout_problems.len() - 30);
            }
            println!(
                "compared {} files ({} region files) | {} of {} chunks identical | {}",
                thousands(r.files),
                thousands(r.regions),
                thousands(r.same_chunks),
                thousands(r.chunks),
                if r.identical() {
                    "same world, readable layout".to_string()
                } else {
                    format!(
                        "{} differences, {} layout problems",
                        r.differences.len(),
                        r.layout_problems.len()
                    )
                }
            );
            if !r.identical() {
                return Ok(ExitCode::FAILURE);
            }
        }

        Cmd::Verify { snapshot, repo } => {
            let repo = open(&repo, &cfg, LockMode::Shared)?;
            let ids = match snapshot {
                Some(s) => vec![list::resolve(&repo, &s)?],
                None => repo.snapshot_ids()?,
            };
            let pb = bar("verifying", false);
            let r = verify::verify(&repo, &ids, &pb)?;
            pb.finish_and_clear();
            for (id, why) in &r.problems {
                eprintln!("bad object {}: {why}", short(id, 12));
            }
            println!(
                "checked {} snapshots, {} objects: {}",
                r.snapshots,
                thousands(r.objects as u64),
                if r.problems.is_empty() {
                    "all OK".to_string()
                } else {
                    format!("{} PROBLEMS", r.problems.len())
                }
            );
            if !r.problems.is_empty() {
                return Ok(ExitCode::FAILURE);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}
