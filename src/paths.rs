//! Where orbis keeps its files, following the XDG base directories.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The directories orbis reads and writes.
#[derive(Clone, Debug)]
pub struct Paths {
    /// `config.toml` lives here.
    pub config: PathBuf,
    /// Instances and the manual mod library (user data, not re-downloadable).
    pub data: PathBuf,
    /// The CKAN index and downloaded mod archives (safe to delete).
    pub cache: PathBuf,
}

impl Paths {
    /// The standard XDG locations, or everything under `root` when given.
    pub fn new(root: Option<&Path>) -> Result<Self> {
        if let Some(root) = root {
            let root =
                std::path::absolute(root).with_context(|| format!("resolve {}", root.display()))?;
            return Ok(Self {
                config: root.join("config"),
                data: root.join("data"),
                cache: root.join("cache"),
            });
        }
        let home = home()?;
        let xdg = |var: &str, fallback: &str| -> PathBuf {
            env::var_os(var)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.join(fallback))
                .join("orbis")
        };
        Ok(Self {
            config: xdg("XDG_CONFIG_HOME", ".config"),
            data: xdg("XDG_DATA_HOME", ".local/share"),
            cache: xdg("XDG_CACHE_HOME", ".cache"),
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.toml")
    }

    pub fn instances(&self) -> PathBuf {
        self.data.join("instances")
    }

    pub fn manual(&self) -> PathBuf {
        self.data.join("manual")
    }

    pub fn downloads(&self) -> PathBuf {
        self.cache.join("downloads")
    }

    pub fn repo(&self) -> PathBuf {
        self.cache.join("repo")
    }
}

/// The user's home directory from `$HOME`.
pub fn home() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .context("HOME is not set to an absolute path")
}

/// Writes `data` to `path` through a temporary file and a rename, so a crash
/// never leaves a half-written file.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let name = path.file_name().context("path has no file name")?;
    let tmp = dir.join(format!(".{}.tmp", name.to_string_lossy()));
    std::fs::write(&tmp, data).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Turns free text into a safe directory name: lowercase letters, digits,
/// `-`, `_` and `.`, never empty and never starting with a dot.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.trim().chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches(|c| c == '-' || c == '.').to_owned();
    if out.is_empty() {
        "unnamed".into()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        let cases = [
            ("Volumetric Clouds v5", "volumetric-clouds-v5"),
            ("  RSS / RO  ", "rss-ro"),
            ("..hidden", "hidden"),
            ("!!!", "unnamed"),
        ];
        for (input, want) in cases {
            assert_eq!(slug(input), want, "{input}");
        }
    }
}
