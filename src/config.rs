//! The global settings file, `config.toml`.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths::{self, Paths};

/// The CKAN-meta archive CKAN itself uses for KSP 1.
pub const DEFAULT_REPO: &str = "https://github.com/KSP-CKAN/CKAN-meta/archive/master.tar.gz";

/// Settings shared by every instance.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Steam's root directory; detected when unset.
    pub steam_root: Option<PathBuf>,
    /// Proton directory (holding the `proton` script); when unset, orbis
    /// uses the tool Steam has mapped to KSP.
    pub proton: Option<PathBuf>,
    /// The Wine binary for instances that run through Wine; `wine` from
    /// PATH when unset.
    pub wine: Option<PathBuf>,
    /// Run Proton inside the Steam Linux Runtime when Proton asks for it,
    /// as Steam does.
    pub use_steam_runtime: bool,
    /// CKAN metadata archives (`.tar.gz`), merged in order.
    pub repositories: Vec<Repository>,
    /// Parallel mod downloads.
    pub download_workers: usize,
    /// Save backups kept per instance (taken before mod changes and
    /// launches); 0 turns backups off.
    pub backups_keep: usize,
    /// Refresh the mod index on GUI start when it is older than this many
    /// hours; 0 turns it off.
    pub auto_update_hours: u64,
    /// Look for a newer orbis release when the GUI starts.
    pub check_updates: bool,
}

/// One CKAN metadata repository.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    pub name: String,
    pub url: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            steam_root: None,
            proton: None,
            wine: None,
            use_steam_runtime: true,
            repositories: vec![Repository {
                name: "KSP-default".into(),
                url: DEFAULT_REPO.into(),
            }],
            download_workers: 4,
            backups_keep: 10,
            auto_update_hours: 24,
            check_updates: true,
        }
    }
}

impl Config {
    /// Loads `config.toml`, or the defaults when it doesn't exist.
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.config_file();
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        cfg.validate()
            .with_context(|| format!("invalid {}", path.display()))?;
        Ok(cfg)
    }

    /// Writes `config.toml`.
    pub fn save(&self, paths: &Paths) -> Result<()> {
        self.validate()?;
        let text = toml::to_string_pretty(self).context("serialize config")?;
        paths::write_atomic(&paths.config_file(), text.as_bytes())
    }

    /// Checks values that would break later in a confusing way.
    pub fn validate(&self) -> Result<()> {
        if self.repositories.is_empty() {
            bail!("repositories must list at least one CKAN metadata archive");
        }
        for r in &self.repositories {
            if !(r.url.starts_with("https://")
                || r.url.starts_with("http://")
                || r.url.starts_with("file://"))
            {
                bail!("repository {:?} has an unsupported URL {:?}", r.name, r.url);
            }
        }
        if !(1..=16).contains(&self.download_workers) {
            bail!("download_workers must be between 1 and 16");
        }
        if self.backups_keep > 1000 {
            bail!("backups_keep must be at most 1000");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_validates() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = Paths::new(Some(tmp.path())).expect("paths");
        assert_eq!(Config::load(&paths).expect("defaults"), Config::default());
        let mut cfg = Config {
            proton: Some("/opt/proton".into()),
            ..Config::default()
        };
        cfg.save(&paths).expect("save");
        assert_eq!(Config::load(&paths).expect("load"), cfg);
        cfg.download_workers = 0;
        assert!(cfg.save(&paths).is_err());
    }
}
