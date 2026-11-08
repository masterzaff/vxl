//! Pre/post hooks (src/hooks.rs).

use std::fs;
use std::path::Path;

use anyhow::{Result, bail};
use vxl::hooks::Hooks;

/// Each test gets its own folder; commands append a line to `log` there.
fn hooks(dir: &Path, pre: Option<&str>, post: Option<&str>) -> Hooks {
    Hooks {
        pre: pre.map(str::to_string),
        post: post.map(str::to_string),
        cwd: dir.to_path_buf(),
        env: vec![],
    }
}

fn log(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join("log")).unwrap_or_default().lines().map(|l| l.trim().to_string()).collect()
}

// `echo x>> log` and `exit N` mean the same thing in sh and cmd.exe.

#[test]
fn pre_body_post_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let h = hooks(dir.path(), Some("echo pre>> log"), Some("echo post>> log"));
    let out = h.around(
        || {
            fs::write(dir.path().join("body"), b"").unwrap();
            Ok(7)
        },
        |_| vec![],
    );
    assert_eq!(out.result.unwrap(), 7);
    assert!(out.post_error.is_none());
    assert_eq!(log(dir.path()), ["pre", "post"]);
    assert!(dir.path().join("body").exists());
}

#[test]
fn failing_pre_skips_the_body_but_runs_post() {
    let dir = tempfile::tempdir().unwrap();
    let h = hooks(dir.path(), Some("echo pre>> log && exit 3"), Some("echo post>> log"));
    let out = h.around(|| -> Result<()> { panic!("body must not run") }, |_| vec![]);
    let err = format!("{:#}", out.result.unwrap_err());
    assert!(err.contains("pre hook failed") && err.contains("status 3"), "{err}");
    assert_eq!(log(dir.path()), ["pre", "post"]);
}

#[test]
fn failing_body_still_runs_post() {
    let dir = tempfile::tempdir().unwrap();
    let h = hooks(dir.path(), None, Some("echo post>> log"));
    let out = h.around(|| -> Result<()> { bail!("disk full") }, |_| vec![]);
    assert!(out.result.unwrap_err().to_string().contains("disk full"));
    assert_eq!(log(dir.path()), ["post"]);
}

#[test]
fn failing_post_is_reported_separately() {
    let dir = tempfile::tempdir().unwrap();
    let h = hooks(dir.path(), None, Some("exit 1"));
    let out = h.around(|| Ok("snap"), |_| vec![]);
    assert_eq!(out.result.unwrap(), "snap");
    assert!(format!("{:#}", out.post_error.unwrap()).contains("post hook failed"));
}

#[test]
fn missing_shell_command_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let h = hooks(dir.path(), Some("vxl-no-such-command-xyz"), None);
    assert!(h.around(|| Ok(()), |_| vec![]).result.is_err());
}

#[test]
#[cfg(unix)]
fn post_sees_the_result() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = hooks(
        dir.path(),
        Some(r#"echo "$VXL_HOOK $VXL_SOURCE" >> log"#),
        Some(r#"echo "$VXL_HOOK $VXL_RESULT $VXL_SNAPSHOT" >> log"#),
    );
    h.env.push(("VXL_SOURCE".into(), "/srv/mc".into()));
    let out = h.around(|| Ok("abc123"), |id| vec![("VXL_SNAPSHOT".into(), id.to_string())]);
    assert!(out.result.is_ok());
    let out = h.around(|| -> Result<&str> { bail!("nope") }, |_| unreachable!());
    assert!(out.result.is_err());
    assert_eq!(log(dir.path()), ["pre /srv/mc", "post ok abc123", "pre /srv/mc", "post failed"]);
}
