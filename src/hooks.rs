//! Pre/post hooks: shell commands run around a backup so the world is consistent
//! on disk while vxl reads it (`save-off` over RCON, `docker exec`, a ZFS snapshot,
//! stopping the server, …). vxl knows nothing about how the server runs.
//!
//! The rule that matters: once `pre` has been started, `post` **always** runs —
//! when `pre` fails, when the backup fails, and on Ctrl-C / SIGTERM / SIGHUP.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use anyhow::{Context, Result, anyhow, bail};

/// How many interrupt signals have arrived (see [`install_interrupt_handler`]).
static INTERRUPTS: AtomicU32 = AtomicU32::new(0);
/// A hook was killed by SIGINT/SIGTERM/SIGHUP. A terminal's Ctrl-C reaches the hook
/// and vxl at once, and we may see the hook die before our handler thread has run,
/// so this counts as an interrupt too.
static HOOK_INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Catch Ctrl-C, SIGTERM and SIGHUP instead of dying, so the post hook still runs.
/// The first signal asks the running backup to stop ([`check_interrupted`]); the
/// third quits immediately. Call once, before running hooks.
pub fn install_interrupt_handler() -> Result<()> {
    ctrlc::set_handler(|| match INTERRUPTS.fetch_add(1, Ordering::SeqCst) + 1 {
        1 => eprintln!("\ninterrupted: stopping the backup, then running the post hook…"),
        2 => eprintln!("\nstill finishing the post hook; interrupt once more to quit without waiting"),
        _ => {
            eprintln!("\nquitting; the post hook may not have finished (check the server is saving again)");
            std::process::exit(130);
        }
    })
    .context("installing the Ctrl-C handler")
}

pub fn interrupted() -> bool {
    INTERRUPTS.load(Ordering::SeqCst) > 0 || HOOK_INTERRUPTED.load(Ordering::SeqCst)
}

/// Fail if an interrupt arrived. Called by long-running work between steps.
pub fn check_interrupted() -> Result<()> {
    if interrupted() {
        bail!("interrupted");
    }
    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct Hooks {
    pub pre: Option<String>,
    pub post: Option<String>,
    /// Working directory for both commands.
    pub cwd: PathBuf,
    /// Extra environment for both commands.
    pub env: Vec<(String, String)>,
}

/// What [`Hooks::around`] produced.
pub struct Outcome<T> {
    /// The body's result, or the pre hook's error if the body never ran.
    pub result: Result<T>,
    /// Set if the post hook failed. Reported separately: a backup that succeeded
    /// is still a good snapshot even if `post` (e.g. `save-on`) didn't work.
    pub post_error: Option<anyhow::Error>,
}

impl Hooks {
    /// Run `pre`, then `body` (only if `pre` succeeded), then `post` (always, once
    /// `pre` was started). `post` gets `VXL_RESULT` = `ok` / `failed` /
    /// `interrupted` plus whatever `post_env` derives from the body's result.
    pub fn around<T>(
        &self,
        body: impl FnOnce() -> Result<T>,
        post_env: impl FnOnce(&T) -> Vec<(String, String)>,
    ) -> Outcome<T> {
        let result = match &self.pre {
            Some(cmd) => run(cmd, &self.cwd, &self.env_for("pre", &[]))
                .context("pre hook failed, so nothing was backed up"),
            None => Ok(()),
        }
        .and_then(|()| check_interrupted())
        .and_then(|()| body());

        let post_error = self.post.as_ref().and_then(|cmd| {
            let status = match &result {
                Ok(_) => "ok",
                Err(_) if interrupted() => "interrupted",
                Err(_) => "failed",
            };
            let mut extra = vec![("VXL_RESULT".to_string(), status.to_string())];
            if let Ok(t) = &result {
                extra.extend(post_env(t));
            }
            let r = run_detached(cmd, &self.cwd, &self.env_for("post", &extra));
            r.context("post hook failed").err()
        });
        Outcome { result, post_error }
    }

    fn env_for(&self, which: &str, extra: &[(String, String)]) -> Vec<(String, String)> {
        let mut env = vec![("VXL_HOOK".to_string(), which.to_string())];
        env.extend(self.env.iter().cloned());
        env.extend(extra.iter().cloned());
        env
    }
}

fn shell(cmd: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd");
        // Passed through untouched: cmd.exe does its own (non-standard) parsing.
        c.arg("/C").raw_arg(cmd);
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    }
}

/// Run `cmd` through the platform shell, inheriting stdout/stderr. Non-zero exit = error.
pub fn run(cmd: &str, cwd: &Path, env: &[(String, String)]) -> Result<()> {
    status(shell(cmd), cmd, cwd, env)
}

/// Like [`run`], but out of reach of a terminal's Ctrl-C (its own process group),
/// so pressing Ctrl-C during the backup doesn't also kill the post hook.
fn run_detached(cmd: &str, cwd: &Path, env: &[(String, String)]) -> Result<()> {
    let mut c = shell(cmd);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP: the console's Ctrl-C is not delivered to it.
        c.creation_flags(0x0000_0200);
    }
    status(c, cmd, cwd, env)
}

fn status(mut c: Command, cmd: &str, cwd: &Path, env: &[(String, String)]) -> Result<()> {
    c.current_dir(cwd).envs(env.iter().map(|(k, v)| (k, v)));
    let st = c.status().with_context(|| format!("couldn't start `{cmd}`"))?;
    if st.success() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = st.signal() {
            // SIGHUP, SIGINT, SIGTERM (same numbers on Linux and macOS).
            if matches!(sig, 1 | 2 | 15) {
                HOOK_INTERRUPTED.store(true, Ordering::SeqCst);
            }
            return Err(anyhow!("`{cmd}` was killed by signal {sig}"));
        }
    }
    Err(match st.code() {
        Some(code) => anyhow!("`{cmd}` exited with status {code}"),
        None => anyhow!("`{cmd}` was killed by a signal"),
    })
}

// Tests live in tests/hooks.rs: they spawn processes, and a forked child briefly holds
// copies of every open file (including other tests' repo locks), so they must not run
// in the same process as the repo tests.
