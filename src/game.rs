//! Facts about a KSP game directory: its version and installed DLC.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::version::{GameVersion, ModVersion};

/// A DLC found in a game directory. CKAN models DLC as modules named
/// `<Name>-DLC` so mods can depend on them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dlc {
    pub identifier: String,
    /// `None` when the readme has no version line (the Steam builds don't).
    pub version: Option<ModVersion>,
}

/// The Windows build's executable (run through Proton or Wine).
pub const WINDOWS_EXE: &str = "KSP_x64.exe";
/// The native Linux build's executable.
pub const LINUX_EXE: &str = "KSP.x86_64";

/// Which build of KSP a directory holds. Steam, GOG and the KSP store all
/// ship both; Steam installs the Windows one when Proton is forced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Build {
    Windows,
    Linux,
}

impl Build {
    pub fn exe(self) -> &'static str {
        match self {
            Build::Windows => WINDOWS_EXE,
            Build::Linux => LINUX_EXE,
        }
    }
}

/// The builds present in `dir`, Windows first.
pub fn builds(dir: &Path) -> Vec<Build> {
    [Build::Windows, Build::Linux]
        .into_iter()
        .filter(|b| dir.join(b.exe()).is_file())
        .collect()
}

/// Whether `dir` looks like a KSP install (either build).
pub fn is_game_dir(dir: &Path) -> bool {
    dir.join("GameData").is_dir() && !builds(dir).is_empty()
}

/// KSP installs outside Steam in their usual places: GOG's Linux
/// installer and Heroic (GOG's Windows build).
pub fn other_installs() -> Vec<std::path::PathBuf> {
    let Ok(home) = crate::paths::home() else {
        return vec![];
    };
    [
        "GOG Games/Kerbal Space Program/game",
        "Games/Heroic/Kerbal Space Program",
        "Games/Heroic/KerbalSpaceProgram",
    ]
    .iter()
    .map(|p| home.join(p))
    .filter(|p| is_game_dir(p))
    .collect()
}

/// Reads the game version from `readme.txt` (`Version 1.12.5`) and the
/// build number from `buildID64.txt`, giving e.g. `1.12.5.3190`.
pub fn detect_version(dir: &Path) -> Result<GameVersion> {
    if !is_game_dir(dir) {
        bail!(
            "{} is not a KSP install (no GameData beside {WINDOWS_EXE} or {LINUX_EXE})",
            dir.display()
        );
    }
    let readme = read_lossy(&dir.join("readme.txt")).context("read readme.txt")?;
    let base = readme
        .lines()
        .find_map(|l| l.trim().strip_prefix("Version "))
        .and_then(|v| GameVersion::parse(v.trim()))
        .context("no version line in readme.txt")?;
    let build = read_lossy(&dir.join("buildID64.txt"))
        .ok()
        .and_then(|t| {
            t.lines().find_map(|l| {
                l.trim()
                    .strip_prefix("build id = ")
                    .map(|b| b.trim().to_owned())
            })
        })
        .and_then(|b| b.trim_start_matches('0').parse::<u32>().ok());
    let full = match build {
        Some(b) => GameVersion::parse(&format!("{base}.{b}")).unwrap_or(base),
        None => base,
    };
    Ok(full)
}

/// The DLC installed in `dir`. Steam leaves empty folders behind for
/// disabled DLC, so a DLC counts only when its readme exists.
pub fn detect_dlc(dir: &Path) -> Vec<Dlc> {
    let known = [
        ("Serenity", "BreakingGround-DLC"),
        ("MakingHistory", "MakingHistory-DLC"),
    ];
    let mut out = Vec::new();
    for (folder, identifier) in known {
        let readme = dir
            .join("GameData/SquadExpansion")
            .join(folder)
            .join("readme.txt");
        let Ok(text) = read_lossy(&readme) else {
            continue;
        };
        let version = text.lines().find_map(|l| {
            let rest = l.trim().strip_prefix("Version")?;
            let v = rest.split_whitespace().next()?;
            Some(ModVersion::new(if v == "1.0" { "1.0.0" } else { v }))
        });
        out.push(Dlc {
            identifier: identifier.to_owned(),
            version,
        });
    }
    out
}

/// CKAN's default compatible versions for an install: every minor release
/// back to the last Unity upgrade (1.12.5 -> 1.8 through 1.12).
pub fn default_compat(version: GameVersion) -> Vec<GameVersion> {
    let (Some(major), Some(minor)) = (version.major(), version.minor()) else {
        return vec![];
    };
    if major != 1 {
        return vec![version.major_minor()];
    }
    let floor = [8, 4, 2, 1]
        .into_iter()
        .find(|&b| b <= minor)
        .unwrap_or(minor);
    (floor..=minor)
        .rev()
        .map(|m| GameVersion::new(1, m, None))
        .collect()
}

fn read_lossy(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_version_and_dlc() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        fs::create_dir_all(dir.join("GameData/SquadExpansion/Serenity")).expect("mkdir");
        fs::create_dir_all(dir.join("GameData/SquadExpansion/MakingHistory")).expect("mkdir");
        fs::write(dir.join("KSP_x64.exe"), b"").expect("write");
        fs::write(dir.join("readme.txt"), b"KSP\r\nVersion 1.12.5\r\n").expect("write");
        fs::write(dir.join("buildID64.txt"), b"[config]\nbuild id = 03190\n").expect("write");
        fs::write(
            dir.join("GameData/SquadExpansion/Serenity/readme.txt"),
            b"\xe9 no version",
        )
        .expect("write");

        assert_eq!(
            detect_version(dir).expect("version").to_string(),
            "1.12.5.3190"
        );
        let dlc = detect_dlc(dir);
        assert_eq!(dlc.len(), 1, "MakingHistory has no readme");
        assert_eq!(dlc[0].identifier, "BreakingGround-DLC");
        assert_eq!(dlc[0].version, None);
    }

    #[test]
    fn tells_the_builds_apart() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        assert!(!is_game_dir(dir));
        fs::create_dir_all(dir.join("GameData")).expect("mkdir");
        assert!(!is_game_dir(dir), "GameData alone isn't an install");
        fs::write(dir.join(LINUX_EXE), b"").expect("write");
        assert!(is_game_dir(dir));
        assert_eq!(builds(dir), [Build::Linux]);
        fs::write(dir.join(WINDOWS_EXE), b"").expect("write");
        assert_eq!(builds(dir), [Build::Windows, Build::Linux]);
    }

    #[test]
    fn default_compat_matches_ckan() {
        let v = |s| GameVersion::parse(s).expect("valid");
        let got: Vec<String> = default_compat(v("1.12.5"))
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(got, ["1.12", "1.11", "1.10", "1.9", "1.8"]);
        let got: Vec<String> = default_compat(v("1.3.1"))
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(got, ["1.3", "1.2"]);
    }
}
