//! vxl — differential, deduplicating backups for Minecraft Java (Anvil) worlds.
//!
//! The library is split so each piece can be tested on its own:
//! - [`anvil`]    : read/write region files (`r.X.Z.mca`), no knowledge of the repo
//! - [`repo`]     : the on-disk repository and its content-addressed object store
//! - [`pack`]     : the pack file format objects are stored in
//! - [`manifest`] : the snapshot format (what a backup *is*)
//! - [`world`]    : walking a world folder and classifying files
//! - [`commands`] : the actual `init/backup/list/restore/prune/verify` logic
//!
//! `main.rs` is just a thin CLI wrapper around this.

pub mod anvil;
pub mod commands;
pub mod manifest;
pub mod pack;
pub mod repo;
pub mod util;
pub mod world;

#[doc(hidden)]
pub mod synth;
