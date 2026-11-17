//! Where orbis keeps its files, following the XDG base directories.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The layout of everything orbis keeps under its config and data
/// directories. Bump it when a change would confuse an older orbis (a file
/// renamed, a field whose meaning changed), and migrate older data up to it
/// in `Paths::check_format`.
pub const DATA_FORMAT: u32 = 1;

/// `<data>/format.toml`: which layout the data is in, and which orbis
/// wrote it last.
#[derive(Debug, Serialize, Deserialize)]
struct FormatFile {
    format: u32,
    #[serde(default)]
    written_by: String,
}

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

    fn format_file(&self) -> PathBuf {
        self.data.join("format.toml")
    }

    /// Makes sure this orbis understands the data on disk. Data written by
    /// a newer orbis, in a layout this one doesn't know, is refused before
    /// anything reads or rewrites it. Data from before the format was
    /// recorded is format 1.
    pub fn check_format(&self) -> Result<()> {
        let path = self.format_file();
        let found = match std::fs::read_to_string(&path) {
            Ok(text) => Some(
                toml::from_str::<FormatFile>(&text)
                    .with_context(|| format!("parse {}", path.display()))?,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        if let Some(f) = &found
            && f.format > DATA_FORMAT
        {
            bail!(
                "orbis {} saved its data in a newer format ({} > {DATA_FORMAT}); \
                 update orbis to {} or later to use it",
                f.written_by,
                f.format,
                f.written_by
            );
        }
        // Migrations from older formats go here, oldest first.
        let current = FormatFile {
            format: DATA_FORMAT,
            written_by: env!("CARGO_PKG_VERSION").to_owned(),
        };
        if found.is_none_or(|f| f.format != current.format || f.written_by != current.written_by) {
            let text = toml::to_string(&current).context("serialize format")?;
            write_atomic(&path, text.as_bytes())?;
        }
        Ok(())
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

/// A Unix time as "YYYY-MM-DD HH:MM UTC".
pub fn when(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
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
    fn refuses_data_from_a_newer_format() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = Paths::new(Some(tmp.path())).expect("paths");
        paths.check_format().expect("first use records the format");
        let text = std::fs::read_to_string(paths.format_file()).expect("written");
        assert!(text.contains(&format!("format = {DATA_FORMAT}")), "{text}");
        paths.check_format().expect("same format");

        let newer = format!("format = {}\nwritten_by = \"9.0.0\"\n", DATA_FORMAT + 1);
        std::fs::write(paths.format_file(), &newer).expect("write");
        let err = paths.check_format().expect_err("newer format");
        assert!(format!("{err:#}").contains("9.0.0"), "{err:#}");
        assert_eq!(
            std::fs::read_to_string(paths.format_file()).expect("read"),
            newer,
            "left alone"
        );
    }

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
