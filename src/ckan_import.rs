//! Importing a KSP install that CKAN manages: a new instance with the
//! install's mods, saves and settings, where every mod CKAN installed
//! becomes a tracked mod (same version, same files, same dependency flag).
//!
//! CKAN keeps its state in `<install>/CKAN/registry.json`:
//!
//! ```text
//! { "installed_modules": { "<id>": {
//!       "source_module": { ...the .ckan metadata... },
//!       "auto_installed": false,
//!       "installed_files": { "GameData/Mod/a.dll": {}, ... } } } }
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::history;
use crate::install::{self, Planned, Source};
use crate::instance::{
    self, CopyMode, CopyProgress, CreateOptions, InstalledMod, Instance, ModSource,
};
use crate::manual;
use crate::metadata::{Kind, Module};
use crate::paths::{self, Paths};
use crate::version::GameVersion;

/// Where CKAN keeps an install's registry.
pub fn registry_path(install: &Path) -> PathBuf {
    install.join("CKAN/registry.json")
}

/// Whether a directory is a KSP install that CKAN manages.
pub fn is_ckan_install(dir: &Path) -> bool {
    registry_path(dir).is_file()
}

/// One mod CKAN installed.
#[derive(Clone, Debug)]
pub struct CkanMod {
    pub module: Module,
    pub auto_installed: bool,
    /// Paths relative to the game directory, as CKAN recorded them.
    pub files: Vec<String>,
}

/// Reads CKAN's registry. DLC entries are skipped (orbis detects DLC).
pub fn read_registry(install: &Path) -> Result<(Vec<CkanMod>, Vec<String>)> {
    let path = registry_path(install);
    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let root: Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let Some(installed) = root.get("installed_modules").and_then(Value::as_object) else {
        bail!("{} has no installed_modules", path.display());
    };
    let mut mods = Vec::new();
    let mut warnings = Vec::new();
    for (id, entry) in installed {
        let Some(source) = entry.get("source_module") else {
            warnings.push(format!("{id}: no metadata in CKAN's registry; skipped"));
            continue;
        };
        let module: Module = match serde_json::from_value(source.clone()) {
            Ok(m) => m,
            Err(e) => {
                // Keep the essentials when the full metadata doesn't parse.
                let version = source.get("version").and_then(Value::as_str);
                match version {
                    Some(v) => serde_json::from_value(serde_json::json!({
                        "identifier": id, "version": v,
                        "name": source.get("name").and_then(Value::as_str).unwrap_or(id),
                    }))
                    .with_context(|| format!("{id}: minimal metadata"))?,
                    None => {
                        warnings.push(format!("{id}: unreadable metadata ({e}); skipped"));
                        continue;
                    }
                }
            }
        };
        if module.kind == Kind::Dlc {
            continue;
        }
        let files = entry
            .get("installed_files")
            .and_then(Value::as_object)
            .map(|m| m.keys().map(|k| k.replace('\\', "/")).collect())
            .unwrap_or_default();
        mods.push(CkanMod {
            module,
            auto_installed: entry
                .get("auto_installed")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            files,
        });
    }
    Ok((mods, warnings))
}

/// The compatible game versions CKAN was set to, if any.
fn ckan_compat(install: &Path) -> Vec<GameVersion> {
    let path = install.join("CKAN/compatible_ksp_versions.json");
    let Ok(text) = fs::read_to_string(path) else {
        return vec![];
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return vec![];
    };
    v.get("Versions")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .filter_map(GameVersion::parse)
                .collect()
        })
        .unwrap_or_default()
}

/// What an import did.
#[derive(Debug)]
pub struct Report {
    pub instance: Instance,
    pub imported: usize,
    /// GameData folders CKAN didn't track; they can be adopted.
    pub unmanaged: Vec<String>,
    pub warnings: Vec<String>,
}

/// Creates an instance from a CKAN-managed install. `mode` is `Link` (stock
/// files linked from the install, mods and saves copied) or `Copy` (an
/// independent copy of everything).
pub fn import(
    paths: &Paths,
    install: &Path,
    name: &str,
    mode: CopyMode,
    progress: CopyProgress,
) -> Result<Report> {
    let (mods, mut warnings) = read_registry(install)?;
    let mut inst = instance::create(
        paths,
        &CreateOptions {
            name: name.to_owned(),
            source: install.to_path_buf(),
            stock_only: false,
            with_saves: true,
            mode,
        },
        progress,
    )?;
    let game = inst.game_dir();
    if mode == CopyMode::Link {
        // Linked instances only link stock GameData; copy the mods in.
        let gamedata = install.join("GameData");
        for e in fs::read_dir(&gamedata)
            .with_context(|| format!("read {}", gamedata.display()))?
            .flatten()
        {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.eq_ignore_ascii_case("Squad") || name.eq_ignore_ascii_case("SquadExpansion") {
                continue;
            }
            copy_entry(&e.path(), &game.join("GameData").join(&name))?;
        }
    }

    let mut reg = inst.registry()?;
    let before = history::snapshot(&reg);
    for m in &mods {
        let mut files = Vec::new();
        let mut planned = Vec::new();
        for f in &m.files {
            if install::through_link(&game, f).is_some() {
                warnings.push(format!(
                    "{}: {f} is outside the instance's own files; not tracked",
                    m.module.identifier
                ));
                continue;
            }
            let path = game.join(f);
            if path.is_file() {
                files.push(f.clone());
                planned.push(Planned {
                    source: f.clone(),
                    dest: f.clone(),
                    is_dir: false,
                });
            } else if path.is_dir() {
                planned.push(Planned {
                    source: f.clone(),
                    dest: f.clone(),
                    is_dir: true,
                });
            }
        }
        if files.is_empty() && m.module.kind == Kind::Package {
            warnings.push(format!(
                "{}: none of its files are present; skipped",
                m.module.identifier
            ));
            continue;
        }
        files.sort();
        reg.mods.insert(
            m.module.identifier.clone(),
            InstalledMod {
                source: ModSource::Ckan,
                auto_installed: m.auto_installed,
                installed_at: paths::now_secs(),
                files,
                dirs: install::dirs_of(&planned),
                module: m.module.clone(),
            },
        );
    }
    let imported = reg.mods.len();
    inst.save_registry(&reg)?;
    history::record(
        &inst,
        before,
        history::snapshot(&reg),
        vec![format!(
            "import {imported} mods from CKAN install {}",
            install.display()
        )],
    )?;
    let compat = ckan_compat(install);
    if !compat.is_empty() {
        inst.config.compat = compat;
    }
    inst.config.source = format!("CKAN install {}", install.display());
    inst.save()?;
    let (unmanaged, _) = manual::unmanaged(&game, &reg.owners())?;
    Ok(Report {
        instance: inst,
        imported,
        unmanaged: unmanaged.into_iter().map(|u| u.folder).collect(),
        warnings,
    })
}

/// Copies a file or folder (reflinked where the filesystem allows).
fn copy_entry(from: &Path, to: &Path) -> Result<()> {
    if from.is_file() {
        fs::copy(from, to).with_context(|| format!("copy {}", from.display()))?;
        return Ok(());
    }
    fs::create_dir_all(to).with_context(|| format!("create {}", to.display()))?;
    for e in Source::Dir(from.to_path_buf()).entries()? {
        let dest = to.join(&e.name);
        if e.is_dir {
            fs::create_dir_all(&dest).with_context(|| format!("create {}", dest.display()))?;
        } else {
            fs::copy(from.join(&e.name), &dest).with_context(|| format!("copy {}", e.name))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_ckan_install(dir: &Path) {
        for d in [
            "GameData/Squad",
            "GameData/ModA/Plugins",
            "GameData/Lib",
            "GameData/HandMod",
            "saves/Career",
            "CKAN",
        ] {
            fs::create_dir_all(dir.join(d)).expect("mkdir");
        }
        for (f, body) in [
            ("KSP_x64.exe", ""),
            ("readme.txt", "Version 1.12.5\n"),
            ("GameData/ModA/Plugins/a.dll", "a"),
            ("GameData/Lib/lib.dll", "l"),
            ("GameData/HandMod/h.cfg", "h"),
            ("saves/Career/persistent.sfs", "save"),
        ] {
            fs::write(dir.join(f), body).expect("write");
        }
        let registry = serde_json::json!({
            "registry_version": 3,
            "installed_modules": {
                "ModA": {
                    "install_time": "2024-01-01T00:00:00",
                    "auto_installed": false,
                    "source_module": {
                        "spec_version": "v1.4", "identifier": "ModA", "name": "Mod A",
                        "version": "1.2", "ksp_version": "1.12",
                        "depends": [{ "name": "Lib" }],
                        "download": "https://example.invalid/a.zip"
                    },
                    "installed_files": {
                        "GameData/ModA": {}, "GameData/ModA/Plugins": {},
                        "GameData/ModA/Plugins/a.dll": {}
                    }
                },
                "Lib": {
                    "auto_installed": true,
                    "source_module": { "identifier": "Lib", "version": "3.0", "weird_field": [1, 2] },
                    "installed_files": { "GameData/Lib/lib.dll": {} }
                },
                "BreakingGround-DLC": {
                    "source_module": { "identifier": "BreakingGround-DLC", "version": "1.7.1", "kind": "dlc" },
                    "installed_files": {}
                }
            }
        });
        fs::write(dir.join("CKAN/registry.json"), registry.to_string()).expect("write");
        fs::write(
            dir.join("CKAN/compatible_ksp_versions.json"),
            r#"{"GameVersionWhenWritten":"1.12.5","Versions":["1.12","1.11"]}"#,
        )
        .expect("write");
    }

    #[test]
    fn imports_a_ckan_install() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ksp = tmp.path().join("ksp");
        fake_ckan_install(&ksp);
        assert!(is_ckan_install(&ksp));
        let paths = Paths::new(Some(&tmp.path().join("orbis"))).expect("paths");
        let report =
            import(&paths, &ksp, "From CKAN", CopyMode::Link, &mut |_, _| {}).expect("import");
        assert_eq!(report.imported, 2, "{:?}", report.warnings);
        assert_eq!(report.unmanaged, ["HandMod"]);
        let inst = report.instance;
        let g = inst.game_dir();
        assert!(g.join("GameData/ModA/Plugins/a.dll").is_file());
        assert!(g.join("saves/Career/persistent.sfs").is_file());
        let reg = inst.registry().expect("registry");
        assert_eq!(reg.mods["ModA"].files, ["GameData/ModA/Plugins/a.dll"]);
        assert!(
            reg.mods["ModA"]
                .dirs
                .contains(&"GameData/ModA/Plugins".to_owned())
        );
        assert!(reg.mods["Lib"].auto_installed);
        assert_eq!(reg.mods["Lib"].module.version.as_str(), "3.0");
        assert_eq!(reg.required_by("Lib"), ["ModA"]);
        let compat: Vec<String> = inst.config.compat.iter().map(ToString::to_string).collect();
        assert_eq!(compat, ["1.12", "1.11"]);
        assert_eq!(history::list(&inst).expect("history").len(), 1);
        // The source install is untouched.
        assert!(ksp.join("GameData/ModA/Plugins/a.dll").is_file());
    }
}
