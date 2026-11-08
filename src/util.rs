//! Small helpers shared by several modules.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A temp-file name that's unique within this process (and across processes via pid).
pub fn tmp_name() -> String {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{n}.tmp", std::process::id())
}

/// Write `data` to `dest` so that readers see either the old file or the
/// complete new one, never a half-written file: write to `tmp_dir`, fsync, rename.
/// `tmp_dir` must be on the same filesystem as `dest`.
pub fn atomic_write(dest: &Path, tmp_dir: &Path, data: &[u8]) -> Result<()> {
    let tmp = tmp_dir.join(tmp_name());
    let mut f = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    f.write_all(data)?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, dest).with_context(|| format!("renaming into {}", dest.display()))?;
    Ok(())
}

/// fsync a directory so a rename inside it survives a power cut. No-op on
/// Windows, which can't open directories as files.
pub fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// `18400000` → `"18.4 MB"` (decimal units, like most file managers).
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1000.0 && i < UNITS.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[i]) }
}

/// `4812` → `"4,812"`.
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(18_400_000), "18.4 MB");
        assert_eq!(human_bytes(1_200_000_000), "1.2 GB");
        assert_eq!(thousands(4812), "4,812");
        assert_eq!(thousands(12), "12");
        assert_eq!(thousands(1_000_000), "1,000,000");
    }
}
