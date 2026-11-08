use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::Result;
use chrono::Local;
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};

use vxl::commands::{backup, compare, list, prune, restore, verify};
use vxl::repo::{self, LockMode, Repo};
use vxl::util::{human_bytes, thousands};

/// Differential, deduplicating backups for Minecraft Java worlds.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args)]
struct RepoArg {
    /// Path to the vxl repository.
    #[arg(long, short = 'r', env = "VXL_REPO")]
    repo: PathBuf,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a new, empty repository.
    Init {
        path: PathBuf,
        /// zstd level for stored chunks (1–22). Higher = smaller but slower backups.
        #[arg(long, default_value_t = repo::DEFAULT_ZSTD_LEVEL)]
        zstd_level: i32,
        /// Target size of pack files, in MiB.
        #[arg(long, default_value_t = repo::DEFAULT_PACK_SIZE >> 20)]
        pack_size_mb: u64,
    },
    /// Snapshot a world. Only new or changed chunks are stored.
    Backup {
        world: PathBuf,
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
    /// Rebuild a snapshot into a new folder (never touches the live world).
    Restore {
        /// Snapshot id (any unique prefix).
        snapshot: String,
        /// Folder to restore into; must not exist or be empty unless --force/--force-wipe.
        target: PathBuf,
        /// Target is an existing world: overwrite the files the snapshot has, keep any others.
        #[arg(long, conflicts_with = "force_wipe")]
        force: bool,
        /// Target is an existing world: delete everything in it first, so it ends up exactly
        /// like the snapshot. This is what you want to roll a world back.
        #[arg(long)]
        force_wipe: bool,
        #[command(flatten)]
        repo: RepoArg,
    },
    /// Delete old snapshots and garbage-collect unreferenced data.
    Prune {
        /// Keep this many most recent snapshots. Without it, only garbage is collected.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        keep_last: Option<u64>,
        /// Show what would be deleted without deleting anything.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        repo: RepoArg,
    },
    /// Check that every object a snapshot needs is present and intact.
    Verify {
        /// Snapshot id to check (default: all snapshots).
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

fn open(r: &RepoArg, lock: LockMode) -> Result<Repo> {
    let repo = Repo::open(&r.repo, lock)?;
    print_warnings(&repo.warnings);
    Ok(repo)
}

fn run(cli: Cli) -> Result<ExitCode> {
    match cli.cmd {
        Cmd::Init { path, zstd_level, pack_size_mb } => {
            Repo::init(&path, zstd_level, pack_size_mb << 20)?;
            println!("initialized empty vxl repository in {}", path.display());
        }

        Cmd::Backup { world, repo, message } => {
            let repo = open(&repo, LockMode::Exclusive)?;
            let start = Instant::now();
            let pb = bar("backing up", true);
            let r = backup::backup(&repo, &world, message, &pb)?;
            pb.finish_and_clear();
            print_warnings(&r.warnings);
            let s = &r.stats;
            println!(
                "snapshot {} | {} chunks | {} new | +{} stored ({:.1}s)",
                short(&r.id, 6),
                thousands(s.chunks),
                thousands(s.new_chunks),
                human_bytes(s.stored_bytes),
                start.elapsed().as_secs_f64()
            );
        }

        Cmd::List { repo } => {
            let repo = open(&repo, LockMode::None)?;
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
                    "{}  {}  {:width$}  +{}",
                    short(id, len),
                    h.created.with_timezone(&Local).format("%Y-%m-%d %H:%M"),
                    h.message.as_deref().unwrap_or(""),
                    human_bytes(h.stats.stored_bytes),
                );
            }
        }

        Cmd::Restore { snapshot, target, force, force_wipe, repo } => {
            let repo = open(&repo, LockMode::Shared)?;
            let id = repo.resolve(&snapshot)?;
            let mode = match (force, force_wipe) {
                (_, true) => restore::Mode::Wipe,
                (true, _) => restore::Mode::Replace,
                _ => restore::Mode::Fresh,
            };
            if mode != restore::Mode::Fresh {
                let source = repo.load_header(&id)?.source;
                if target.canonicalize().is_ok_and(|t| t.display().to_string() == source) {
                    eprintln!(
                        "warning: restoring over the world this snapshot came from; make sure the server is stopped"
                    );
                }
            }
            let pb = bar("restoring", false);
            let r = restore::restore(&repo, &id, &target, mode, &pb)?;
            pb.finish_and_clear();
            let extra = match mode {
                restore::Mode::Fresh => String::new(),
                restore::Mode::Replace => format!(" | {} replaced", thousands(r.replaced)),
                restore::Mode::Wipe => format!(" | wiped {} old entries", thousands(r.wiped)),
            };
            println!(
                "restored {} into {} | {} files | {} chunks{extra}",
                short(&r.id, 6),
                target.display(),
                thousands(r.files),
                thousands(r.chunks)
            );
        }

        Cmd::Prune { keep_last, dry_run, repo } => {
            let repo = open(&repo, LockMode::Exclusive)?;
            let r = prune::prune(&repo, keep_last.map(|n| n as usize), dry_run)?;
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
                println!(
                    "({} unused objects stay in packs that are mostly still in use)",
                    thousands(r.garbage_left)
                );
            }
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
            let repo = open(&repo, LockMode::Shared)?;
            let ids = match snapshot {
                Some(s) => vec![repo.resolve(&s)?],
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
