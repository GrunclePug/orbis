//! Finding Steam, its KSP install, and the Proton build Steam uses for KSP.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::paths;
use crate::vdf::Vdf;

/// KSP's Steam app id.
pub const KSP_APP_ID: &str = "220200";
/// The Steam Linux Runtime 3.0 (sniper) app id that Proton 8+ runs inside.
pub const SNIPER_APP_ID: &str = "1628350";

/// What orbis found of the user's Steam setup.
#[derive(Clone, Debug, Default)]
pub struct SteamInfo {
    /// Steam's root (`~/.local/share/Steam`).
    pub root: Option<PathBuf>,
    /// Every Steam library folder.
    pub libraries: Vec<PathBuf>,
    /// The KSP install directory.
    pub ksp: Option<PathBuf>,
    /// The library that holds KSP (its `steamapps/compatdata` has the prefix).
    pub ksp_library: Option<PathBuf>,
    /// The compatibility tool Steam maps to KSP, by internal name.
    pub proton_name: Option<String>,
    /// That tool's directory (containing the `proton` script).
    pub proton: Option<PathBuf>,
    /// The Steam Linux Runtime (sniper) directory, if installed.
    pub sniper: Option<PathBuf>,
}

impl SteamInfo {
    /// Steam's own Proton prefix for KSP.
    pub fn ksp_prefix(&self) -> Option<PathBuf> {
        self.ksp_library
            .as_ref()
            .map(|l| l.join("steamapps/compatdata").join(KSP_APP_ID))
    }

    /// One line per finding, for `orbis steam` and the GUI settings page.
    pub fn report(&self) -> Vec<(String, String)> {
        let show = |p: &Option<PathBuf>| {
            p.as_ref()
                .map_or("not found".into(), |p| p.display().to_string())
        };
        vec![
            ("Steam".into(), show(&self.root)),
            ("KSP".into(), show(&self.ksp)),
            (
                "Proton tool".into(),
                self.proton_name.clone().unwrap_or_else(|| "not set".into()),
            ),
            ("Proton".into(), show(&self.proton)),
            ("Steam Runtime".into(), show(&self.sniper)),
            ("KSP prefix".into(), show(&self.ksp_prefix())),
        ]
    }
}

/// Looks for Steam and KSP. `root` overrides the Steam root, and `proton`
/// overrides the Proton directory. Missing pieces are left as `None`.
pub fn detect(root: Option<&Path>, proton: Option<&Path>) -> SteamInfo {
    let mut info = SteamInfo {
        root: root.map(Path::to_path_buf).or_else(find_root),
        ..SteamInfo::default()
    };
    let Some(root) = info.root.clone() else {
        return info;
    };
    info.libraries = library_folders(&root);
    for lib in &info.libraries {
        let dir = lib.join("steamapps/common/Kerbal Space Program");
        if crate::game::is_game_dir(&dir) {
            info.ksp = Some(dir);
            info.ksp_library = Some(lib.clone());
            break;
        }
    }
    info.sniper = info
        .libraries
        .iter()
        .map(|l| l.join("steamapps/common/SteamLinuxRuntime_sniper"))
        .find(|p| p.join("_v2-entry-point").is_file());
    info.proton_name = compat_tool_name(&root, KSP_APP_ID);
    info.proton = match proton {
        Some(p) => Some(p.to_path_buf()),
        None => find_proton(&root, &info.libraries, info.proton_name.as_deref()),
    };
    info
}

fn find_root() -> Option<PathBuf> {
    let home = paths::home().ok()?;
    [
        ".local/share/Steam",
        ".steam/steam",
        ".steam/root",
        ".var/app/com.valvesoftware.Steam/.local/share/Steam",
    ]
    .iter()
    .map(|p| home.join(p))
    .find(|p| p.join("steamapps").is_dir())
    .map(|p| fs::canonicalize(&p).unwrap_or(p))
}

fn read_vdf(path: &Path) -> Result<Vdf> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Vdf::parse(&String::from_utf8_lossy(&bytes))
        .with_context(|| format!("parse {}", path.display()))
}

/// The library folders listed in `libraryfolders.vdf`, the root first.
pub fn library_folders(root: &Path) -> Vec<PathBuf> {
    let mut libs = vec![root.to_path_buf()];
    if let Ok(v) = read_vdf(&root.join("steamapps/libraryfolders.vdf"))
        && let Some(folders) = v.get("libraryfolders")
    {
        for (_, lib) in folders.pairs() {
            if let Some(p) = lib.get("path").and_then(Vdf::as_str) {
                let p = PathBuf::from(p);
                let canon = fs::canonicalize(&p).unwrap_or(p);
                if !libs.contains(&canon) {
                    libs.push(canon);
                }
            }
        }
    }
    libs
}

/// The compatibility tool Steam uses for `app_id`, falling back to the
/// global default (app id "0").
pub fn compat_tool_name(root: &Path, app_id: &str) -> Option<String> {
    let v = read_vdf(&root.join("config/config.vdf")).ok()?;
    let map = v.path(&[
        "InstallConfigStore",
        "Software",
        "Valve",
        "Steam",
        "CompatToolMapping",
    ])?;
    [app_id, "0"].iter().find_map(|id| {
        map.get(id)?
            .get("name")?
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    })
}

/// Finds a Proton directory: the named custom tool in
/// `compatibilitytools.d`, a Valve Proton in a library, or the newest
/// GE-Proton as a last resort.
pub fn find_proton(root: &Path, libraries: &[PathBuf], name: Option<&str>) -> Option<PathBuf> {
    let custom = custom_tools(root);
    if let Some(name) = name {
        if let Some((_, dir)) = custom.iter().find(|(n, _)| n == name) {
            return Some(dir.clone());
        }
        if let Some(dir) = valve_proton(libraries, name) {
            return Some(dir);
        }
    }
    let mut ge: Vec<&(String, PathBuf)> = custom
        .iter()
        .filter(|(n, _)| n.to_ascii_lowercase().contains("proton"))
        .collect();
    ge.sort_by(|a, b| {
        crate::version::ModVersion::new(&a.0).cmp(&crate::version::ModVersion::new(&b.0))
    });
    ge.last().map(|(_, d)| d.clone())
}

/// Custom compatibility tools: (internal name, directory).
fn custom_tools(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let mut dirs = vec![root.join("compatibilitytools.d")];
    dirs.push(PathBuf::from("/usr/share/steam/compatibilitytools.d"));
    for base in dirs {
        let Ok(entries) = fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            let Ok(v) = read_vdf(&dir.join("compatibilitytool.vdf")) else {
                continue;
            };
            let Some(tools) = v.path(&["compatibilitytools", "compat_tools"]) else {
                continue;
            };
            for (name, tool) in tools.pairs() {
                let install = tool
                    .get("install_path")
                    .and_then(Vdf::as_str)
                    .unwrap_or(".");
                let tool_dir = if install == "." {
                    dir.clone()
                } else {
                    dir.join(install)
                };
                if tool_dir.join("proton").is_file() {
                    out.push((name.clone(), tool_dir));
                }
            }
        }
    }
    out
}

/// Maps Valve's internal tool names (`proton_9`, `proton_experimental`) to
/// their install directories.
fn valve_proton(libraries: &[PathBuf], name: &str) -> Option<PathBuf> {
    let wanted = match name {
        "proton_experimental" => "Proton - Experimental".to_owned(),
        "proton_hotfix" => "Proton Hotfix".to_owned(),
        n => {
            let ver = n.strip_prefix("proton_")?;
            let ver = if ver.contains('.') {
                ver.to_owned()
            } else {
                format!("{ver}.0")
            };
            format!("Proton {ver}")
        }
    };
    libraries
        .iter()
        .map(|l| l.join("steamapps/common").join(&wanted))
        .find(|d| d.join("proton").is_file())
}

/// Whether a Proton build asks to run inside the Steam Linux Runtime.
pub fn proton_needs_sniper(proton: &Path) -> bool {
    read_vdf(&proton.join("toolmanifest.vdf"))
        .ok()
        .and_then(|v| {
            v.path(&["manifest", "require_tool_appid"])?
                .as_str()
                .map(str::to_owned)
        })
        .is_some_and(|id| id == SNIPER_APP_ID)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_fake_steam() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let ksp = root.join("steamapps/common/Kerbal Space Program");
        fs::create_dir_all(ksp.join("GameData")).expect("mkdir");
        fs::write(ksp.join("KSP_x64.exe"), b"").expect("write");
        fs::create_dir_all(root.join("config")).expect("mkdir");
        fs::write(
            root.join("config/config.vdf"),
            r#""InstallConfigStore" { "Software" { "Valve" { "Steam" { "CompatToolMapping" {
                "220200" { "name" "GE-Proton10-1" "config" "" }
            } } } } }"#,
        )
        .expect("write");
        let tool = root.join("compatibilitytools.d/GE-Proton10-1");
        fs::create_dir_all(&tool).expect("mkdir");
        fs::write(tool.join("proton"), b"").expect("write");
        fs::write(
            tool.join("compatibilitytool.vdf"),
            r#""compatibilitytools" { "compat_tools" { "GE-Proton10-1" { "install_path" "." } } }"#,
        )
        .expect("write");
        fs::write(
            tool.join("toolmanifest.vdf"),
            r#""manifest" { "require_tool_appid" "1628350" }"#,
        )
        .expect("write");

        let info = detect(Some(root), None);
        assert_eq!(info.ksp.as_deref(), Some(ksp.as_path()));
        assert_eq!(info.proton_name.as_deref(), Some("GE-Proton10-1"));
        assert_eq!(info.proton.as_deref(), Some(tool.as_path()));
        assert!(proton_needs_sniper(&tool));
    }
}
