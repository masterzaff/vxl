//! vxl — differential, deduplicating backups for Minecraft Java (Anvil) worlds.
//!
//! The library is split so each piece can be tested on its own:
//! - [`anvil`]    : read/write region files (`r.X.Z.mca`), no knowledge of the repo
//! - [`repo`]     : the on-disk repository and its content-addressed object store
//! - [`pack`]     : the pack file format objects are stored in
//! - [`manifest`] : the snapshot format (what a backup *is*)
//! - [`world`]    : finding worlds in a server folder, walking a world and classifying files
//! - [`retention`]: which snapshots `prune` keeps (`--keep-daily` etc.)
//! - [`config`]   : `vxl.toml`
//! - [`hooks`]    : pre/post shell commands around a backup, Ctrl-C handling
//! - [`nbt`], [`transform`]: byte-exact NBT + experimental reversible chunk transforms
//! - [`commands`] : the actual `init/backup/list/restore/prune/verify` logic
//!
//! `main.rs` is just a thin CLI wrapper around this.

pub mod anvil;
pub mod commands;
pub mod config;
pub mod hooks;
pub mod manifest;
pub mod nbt;
pub mod pack;
pub mod repo;
pub mod retention;
pub mod transform;
pub mod util;
pub mod world;

#[doc(hidden)]
pub mod synth;
