//! `vxl.toml`: source, repo, worlds, hooks and retention in one place, so a plain
//! `vxl backup && vxl prune` does the right thing (from cron, a systemd timer, …).
//!
//! ```toml
//! source = "/srv/mc"              # a world, or a server folder
//! repo = "/backups/mc-repo"
//! worlds = ["*", "!creative"]     # or "*" (the default)
//! strict = false
//!
//! [hooks]
//! pre = "docker exec mc rcon-cli save-off && docker exec mc rcon-cli save-all flush"
//! post = "docker exec mc rcon-cli save-on"
//!
//! [keep]
//! daily = 7
//! weekly = 4
//! monthly = 6
//! ```
//!
//! Relative paths are relative to the config file's folder. Command-line options win
//! over the file: `--repo`/`VXL_REPO`, a SOURCE argument, `-w`, `--pre`/`--post`, and
//! any `--keep-*` rule (then the file's `[keep]` is ignored entirely, so what you type
//! is exactly what applies).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::retention::Policy;

/// Looked for in the working directory when no `--config` / `VXL_CONFIG` is given.
pub const DEFAULT_NAME: &str = "vxl.toml";

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub source: Option<PathBuf>,
    pub repo: Option<PathBuf>,
    #[serde(default)]
    pub worlds: Worlds,
    #[serde(default)]
    pub strict: bool,
    #[serde(default)]
    pub hooks: HookConfig,
    #[serde(default)]
    pub keep: Policy,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookConfig {
    pub pre: Option<String>,
    pub post: Option<String>,
}

/// `worlds = "*"` or `worlds = ["world", "!creative"]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Worlds {
    #[default]
    All,
    One(String),
    Many(Vec<String>),
}

impl Worlds {
    /// As selectors for [`crate::world::select_worlds`] (empty = all).
    pub fn selectors(&self) -> Vec<String> {
        match self {
            Worlds::All => vec![],
            Worlds::One(s) => vec![s.clone()],
            Worlds::Many(v) => v.clone(),
        }
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Config> {
        Ok(toml::from_str(text)?)
    }

    /// Read a config file, resolving its relative paths against its folder.
    pub fn load(path: &Path) -> Result<Config> {
        let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut c = Config::parse(&text).with_context(|| format!("in {}", path.display()))?;
        let base = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        for p in [&mut c.source, &mut c.repo].into_iter().flatten() {
            if p.is_relative() {
                *p = base.join(&*p);
            }
        }
        Ok(c)
    }

    /// The config to use: `explicit` (from `--config` / `VXL_CONFIG`, must exist), else
    /// `vxl.toml` in the working directory if there is one, else an empty config.
    pub fn find(explicit: Option<&Path>) -> Result<(Config, Option<PathBuf>)> {
        if let Some(p) = explicit {
            if !p.is_file() {
                bail!("config file {} not found", p.display());
            }
            return Ok((Config::load(p)?, Some(p.to_path_buf())));
        }
        let p = Path::new(DEFAULT_NAME);
        if p.is_file() {
            return Ok((Config::load(p)?, Some(p.to_path_buf())));
        }
        Ok((Config::default(), None))
    }

    /// The retention policy: the command line's rules if it has any, else the file's.
    pub fn policy(&self, cli: &Policy) -> Policy {
        if cli.is_empty() { self.keep.clone() } else { cli.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_example() {
        let c = Config::parse(
            r#"
            source = "/srv/mc"
            repo = "/backups/mc-repo"
            worlds = ["*", "!creative"]
            strict = true

            [hooks]
            pre = "save-off"
            post = "save-on"

            [keep]
            daily = 7
            weekly = 4
            "#,
        )
        .unwrap();
        assert_eq!(c.source.as_deref(), Some(Path::new("/srv/mc")));
        assert_eq!(c.worlds.selectors(), ["*", "!creative"]);
        assert!(c.strict);
        assert_eq!(c.hooks.pre.as_deref(), Some("save-off"));
        assert_eq!(c.keep, Policy { daily: Some(7), weekly: Some(4), ..Default::default() });
    }

    #[test]
    fn worlds_forms() {
        assert_eq!(Config::parse("").unwrap().worlds.selectors(), Vec::<String>::new());
        assert_eq!(Config::parse(r#"worlds = "*""#).unwrap().worlds.selectors(), ["*"]);
        assert_eq!(Config::parse(r#"worlds = ["a", "b"]"#).unwrap().worlds.selectors(), ["a", "b"]);
        assert!(Config::parse("worlds = 3").is_err());
    }

    #[test]
    fn typos_are_errors() {
        // A misspelled key silently doing nothing would be a nasty surprise for retention.
        assert!(Config::parse("[keep]\ndialy = 7").is_err());
        assert!(Config::parse("sorce = \"/srv\"").is_err());
        assert!(Config::parse("[hooks]\nbefore = \"x\"").is_err());
    }

    #[test]
    fn relative_paths_are_relative_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vxl.toml");
        // Absolute on every platform (`/abs` isn't on Windows: no drive letter).
        let abs = dir.path().join("elsewhere").join("repo");
        let toml_str = |p: &Path| p.display().to_string().replace('\\', "\\\\");
        fs::write(&p, format!("source = \"server\"\nrepo = \"{}\"", toml_str(&abs))).unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.source.unwrap(), dir.path().join("server"));
        assert_eq!(c.repo.unwrap(), abs);
    }

    #[test]
    fn command_line_rules_replace_the_files() {
        let c = Config::parse("[keep]\ndaily = 7\nweekly = 4").unwrap();
        assert_eq!(c.policy(&Policy::default()), c.keep);
        let cli = Policy { last: Some(1), ..Default::default() };
        assert_eq!(c.policy(&cli), cli);
    }

    #[test]
    fn missing_explicit_file_is_an_error() {
        assert!(Config::find(Some(Path::new("/definitely/not/here.toml"))).is_err());
    }
}
