//! One module per subcommand. Each returns a report struct; `main.rs` prints it.
//! Keeping printing out of here makes the logic easy to test.

pub mod backup;
pub mod compare;
pub mod diff;
pub mod list;
pub mod prune;
pub mod restore;
pub mod verify;
