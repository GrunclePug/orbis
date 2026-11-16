//! The manual mod library: mods CKAN doesn't carry (Patreon builds, beta
//! zips, personal patches), stored once and installable into any instance.
//!
//! Each manual mod lives in `<data>/manual/<id>/` with a `manual.toml`
//! and its archive (or an unpacked `files/` folder). Installed, it shows
//! up in an instance as `manual:<id>` and can `provide` CKAN identifiers,
//! so CKAN mods that depend on what it replaces are satisfied.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::install::{self, Entry, Source};
use crate::metadata::{Direct, InstallDirective, Kind, Module, Relationship};
use crate::paths::{self, Paths, slug};
use crate::version::ModVersion;

/// Prefix of manual mod identifiers inside an instance.
pub const PREFIX: &str = "manual:";

/// A manual mod's settings, stored in `manual.toml`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ManualMod {
    pub id: String,
    pub name: String,
    pub version: String,
    /// The stored archive's file name, or `files` for an unpacked folder.
    pub file: String,
    pub is_dir: bool,
    /// CKAN identifiers this mod stands in for.
    #[serde(default)]
    pub provides: Vec<String>,
    /// CKAN identifiers this mod needs.
    #[serde(default)]
    pub depends: Vec<String>,
    /// CKAN identifiers this mod can't be installed with.
    #[serde(default)]
    pub conflicts: Vec<String>,
    /// Where the files go, in CKAN's install stanza format.
    pub install: Vec<InstallDirective>,
    #[serde(default)]
    pub source_url: String,
    #[serde(default)]
    pub notes: String,
    pub added: u64,
}

impl ManualMod {
    /// The identifier used in an instance registry.
    pub fn identifier(&self) -> String {
        format!("{PREFIX}{}", self.id)
    }

    /// The stored archive or folder.
    pub fn source(&self, paths: &Paths) -> Source {
        let p = paths.manual().join(&self.id).join(&self.file);
        if self.is_dir {
            Source::Dir(p)
        } else {
            Source::Zip(p)
        }
    }

    /// Metadata in CKAN's shape, so the resolver treats it like any mod.
    pub fn to_module(&self) -> Module {
        let rels = |names: &[String]| -> Vec<Relationship> {
            names
                .iter()
                .map(|n| Relationship::Direct(Direct::named(n.clone())))
                .collect()
        };
        let mut m: Module = serde_json::from_value(serde_json::json!({
            "identifier": self.identifier(),
            "version": self.version,
        }))
        .expect("a two-field module always parses");
        m.name = self.name.clone();
        m.summary = if self.notes.is_empty() {
            "Manually added mod".into()
        } else {
            self.notes.clone()
        };
        m.kind = Kind::Package;
        m.provides = self.provides.clone();
        m.depends = rels(&self.depends);
        m.conflicts = rels(&self.conflicts);
        m.install = Some(self.install.clone());
        if !self.source_url.is_empty() {
            m.resources.insert(
                "homepage".into(),
                serde_json::Value::String(self.source_url.clone()),
            );
        }
        m
    }

    /// Where each file would go.
    pub fn plan(&self, paths: &Paths) -> Result<Vec<install::Planned>> {
        install::plan(&self.install, &self.source(paths).entries()?)
    }
}

/// Every manual mod, sorted by name.
pub fn list(paths: &Paths) -> Result<Vec<ManualMod>> {
    let dir = paths.manual();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
    };
    let mut out = Vec::new();
    for entry in entries {
        let path = entry
            .context("read manual directory")?
            .path()
            .join("manual.toml");
        if !path.is_file() {
            continue;
        }
        match load(&path) {
            Ok(m) => out.push(m),
            Err(e) => log::warn!("skip {}: {e:#}", path.display()),
        }
    }
    out.sort_by_key(|m| m.name.to_lowercase());
    Ok(out)
}

fn load(path: &Path) -> Result<ManualMod> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

/// Finds a manual mod by id, `manual:<id>` or name.
pub fn find(paths: &Paths, key: &str) -> Result<ManualMod> {
    let key = key.strip_prefix(PREFIX).unwrap_or(key);
    let all = list(paths)?;
    all.iter()
        .find(|m| m.id == key)
        .or_else(|| all.iter().find(|m| m.name.eq_ignore_ascii_case(key)))
        .cloned()
        .with_context(|| format!("no manual mod {key:?}"))
}

/// Saves a manual mod's settings.
pub fn save(paths: &Paths, m: &ManualMod) -> Result<()> {
    if m.name.trim().is_empty() {
        bail!("name must not be empty");
    }
    let text = toml::to_string_pretty(m).context("serialize manual mod")?;
    paths::write_atomic(
        &paths.manual().join(&m.id).join("manual.toml"),
        text.as_bytes(),
    )
}

/// Options for adding a manual mod.
#[derive(Clone, Debug, Default)]
pub struct AddOptions {
    /// A `.zip` file or a folder.
    pub path: PathBuf,
    pub name: String,
    pub version: String,
    pub provides: Vec<String>,
    pub depends: Vec<String>,
    pub conflicts: Vec<String>,
    /// Explicit stanzas; detected from the archive when empty.
    pub install: Vec<InstallDirective>,
    pub source_url: String,
    pub notes: String,
}

/// Copies an archive or folder into the library.
pub fn add(paths: &Paths, opts: &AddOptions) -> Result<ManualMod> {
    let name = opts.name.trim();
    if name.is_empty() {
        bail!("name must not be empty");
    }
    let is_dir = opts.path.is_dir();
    if !is_dir && !opts.path.is_file() {
        bail!("{} does not exist", opts.path.display());
    }
    let ext = opts
        .path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let packed = !is_dir && install::is_packed(&opts.path);
    if !is_dir && ext != "zip" && !packed {
        bail!("supported: .zip, .7z and .rar archives, and folders (got .{ext})");
    }
    let probe = if is_dir {
        Source::Dir(opts.path.clone())
    } else if packed {
        Source::Packed(opts.path.clone())
    } else {
        Source::Zip(opts.path.clone())
    };
    let entries = probe.entries()?;
    let directives = if opts.install.is_empty() {
        detect_directives(&entries, name)
    } else {
        opts.install.clone()
    };
    let planned = install::plan(&directives, &entries)?;
    if planned.iter().all(|p| p.is_dir) {
        bail!("the install stanzas select no files");
    }

    let root = paths.manual();
    fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
    let base = slug(name);
    let mut id = base.clone();
    let mut n = 2;
    while root.join(&id).exists() {
        id = format!("{base}-{n}");
        n += 1;
    }
    let dir = root.join(&id);
    // .7z and .rar are unpacked into the library once and kept as a folder.
    let stored_as_dir = is_dir || packed;
    let file = if stored_as_dir {
        "files".to_owned()
    } else {
        opts.path
            .file_name()
            .context("archive has no file name")?
            .to_string_lossy()
            .into_owned()
    };
    let result = (|| -> Result<ManualMod> {
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        if is_dir {
            copy_dir(&opts.path, &dir.join("files"))?;
        } else if packed {
            install::unpack_with_bsdtar(&opts.path, &dir.join("files"))?;
        } else {
            fs::copy(&opts.path, dir.join(&file))
                .with_context(|| format!("copy {}", opts.path.display()))?;
        }
        let m = ManualMod {
            id: id.clone(),
            name: name.to_owned(),
            version: if opts.version.trim().is_empty() {
                "1.0".into()
            } else {
                opts.version.trim().to_owned()
            },
            file,
            is_dir: stored_as_dir,
            provides: clean_list(&opts.provides),
            depends: clean_list(&opts.depends),
            conflicts: clean_list(&opts.conflicts),
            install: directives,
            source_url: opts.source_url.trim().to_owned(),
            notes: opts.notes.trim().to_owned(),
            added: paths::now_secs(),
        };
        save(paths, &m)?;
        Ok(m)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&dir);
    }
    result
}

/// Deletes a manual mod from the library. Instances that have it installed
/// keep their files and can still remove it.
pub fn delete(paths: &Paths, m: &ManualMod) -> Result<()> {
    let dir = paths.manual().join(&m.id);
    if !dir.join("manual.toml").is_file() {
        bail!("{} is not a manual mod directory", dir.display());
    }
    fs::remove_dir_all(&dir).with_context(|| format!("delete {}", dir.display()))
}

fn clean_list(v: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    v.iter()
        .flat_map(|s| s.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty() && seen.insert(s.to_string()))
        .map(str::to_owned)
        .collect()
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("create {}", to.display()))?;
    for entry in fs::read_dir(from).with_context(|| format!("read {}", from.display()))? {
        let entry = entry.with_context(|| format!("read {}", from.display()))?;
        let src = entry.path();
        let dest = to.join(entry.file_name());
        let ft = entry
            .file_type()
            .with_context(|| format!("stat {}", src.display()))?;
        if ft.is_dir() {
            copy_dir(&src, &dest)?;
        } else if ft.is_file() {
            fs::copy(&src, &dest).with_context(|| format!("copy {}", src.display()))?;
        }
    }
    Ok(())
}

/// A folder in an instance's GameData that no installed mod owns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unmanaged {
    pub folder: String,
    pub files: usize,
}

/// ModuleManager's caches and similar files the game writes into GameData.
fn generated(name: &str) -> bool {
    let n = name.to_lowercase();
    n.starts_with("modulemanager.") && !n.ends_with(".dll") || n == "physics.cfg"
}

/// Folders (and loose files) in GameData that no installed mod owns: mods
/// added by hand. Stock folders and generated caches are left out.
pub fn unmanaged(
    game: &Path,
    owners: &std::collections::HashMap<String, String>,
) -> Result<(Vec<Unmanaged>, Vec<String>)> {
    let gamedata = game.join("GameData");
    let mut folders = Vec::new();
    let mut loose = Vec::new();
    let entries = match fs::read_dir(&gamedata) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((folders, loose)),
        Err(e) => return Err(e).with_context(|| format!("read {}", gamedata.display())),
    };
    let owned_prefixes: BTreeSet<String> = owners
        .keys()
        .filter_map(|k| k.strip_prefix("gamedata/"))
        .filter_map(|k| k.split_once('/').map(|(top, _)| top.to_owned()))
        .collect();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink()
            || name.eq_ignore_ascii_case("Squad")
            || name.eq_ignore_ascii_case("SquadExpansion")
            || name.eq_ignore_ascii_case(crate::flags::FOLDER)
        {
            continue;
        }
        if ft.is_dir() {
            if owned_prefixes.contains(&name.to_lowercase()) {
                continue;
            }
            let files = Source::Dir(e.path())
                .entries()?
                .iter()
                .filter(|x| !x.is_dir)
                .count();
            folders.push(Unmanaged {
                folder: name,
                files,
            });
        } else if !generated(&name)
            && !owners.contains_key(&install::fold(&format!("GameData/{name}")))
        {
            loose.push(name);
        }
    }
    folders.sort_by_key(|u| u.folder.to_lowercase());
    loose.sort();
    Ok((folders, loose))
}

/// Copies a hand-added GameData folder into the library as a manual mod.
/// The instance's copy stays where it is; see `ops::adopt`.
pub fn add_folder(
    paths: &Paths,
    gamedata: &Path,
    folder: &str,
    opts: &AddOptions,
) -> Result<ManualMod> {
    let src = gamedata.join(folder);
    if !src.is_dir() {
        bail!("{} is not a folder", src.display());
    }
    let name = if opts.name.trim().is_empty() {
        folder
    } else {
        opts.name.trim()
    };
    let root = paths.manual();
    fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
    let base = slug(name);
    let mut id = base.clone();
    let mut n = 2;
    while root.join(&id).exists() {
        id = format!("{base}-{n}");
        n += 1;
    }
    let dir = root.join(&id);
    let result = (|| -> Result<ManualMod> {
        copy_dir(&src, &dir.join("files").join(folder))?;
        let m = ManualMod {
            id: id.clone(),
            name: name.to_owned(),
            version: if opts.version.trim().is_empty() {
                "1.0".into()
            } else {
                opts.version.trim().to_owned()
            },
            file: "files".into(),
            is_dir: true,
            provides: clean_list(&opts.provides),
            depends: clean_list(&opts.depends),
            conflicts: clean_list(&opts.conflicts),
            install: vec![InstallDirective {
                file: Some(folder.to_owned()),
                install_to: "GameData".into(),
                ..InstallDirective::default()
            }],
            source_url: opts.source_url.trim().to_owned(),
            notes: opts.notes.trim().to_owned(),
            added: paths::now_secs(),
        };
        save(paths, &m)?;
        Ok(m)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&dir);
    }
    result
}

/// Guesses install stanzas for an archive:
///
/// 1. It has a `GameData` folder (anywhere): install its contents.
/// 2. Otherwise, each top-level folder goes into `GameData`, and loose
///    top-level files go into `GameData/<name>`.
pub fn detect_directives(entries: &[Entry], name: &str) -> Vec<InstallDirective> {
    let has_gamedata = entries.iter().any(|e| {
        e.name
            .split('/')
            .any(|s| s.eq_ignore_ascii_case("GameData"))
    });
    if has_gamedata {
        return vec![InstallDirective {
            find: Some("GameData".into()),
            install_to: "GameData".into(),
            ..InstallDirective::default()
        }];
    }
    let mut dirs = BTreeSet::new();
    let mut loose = false;
    for e in entries {
        match e.name.split_once('/') {
            Some((top, _)) => {
                dirs.insert(top.to_owned());
            }
            None if e.is_dir => {
                dirs.insert(e.name.clone());
            }
            None => loose = true,
        }
    }
    let mut out: Vec<InstallDirective> = dirs
        .into_iter()
        .map(|d| InstallDirective {
            file: Some(d),
            install_to: "GameData".into(),
            ..InstallDirective::default()
        })
        .collect();
    if loose {
        out.push(InstallDirective {
            find_regexp: Some("^[^/]+$".into()),
            install_to: format!("GameData/{}", folder_name(name)),
            ..InstallDirective::default()
        });
    }
    out
}

/// A GameData-safe folder name from a display name.
fn folder_name(name: &str) -> String {
    let s: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if s.is_empty() { "ManualMod".into() } else { s }
}

/// Parses a version string for a manual mod.
pub fn version_of(m: &ManualMod) -> ModVersion {
    ModVersion::new(m.version.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn entries(names: &[&str]) -> Vec<Entry> {
        names
            .iter()
            .map(|n| Entry {
                name: n.trim_end_matches('/').to_owned(),
                is_dir: n.ends_with('/'),
            })
            .collect()
    }

    fn dests(d: &[InstallDirective], e: &[Entry]) -> Vec<String> {
        install::plan(d, e)
            .expect("plans")
            .into_iter()
            .filter(|p| !p.is_dir)
            .map(|p| p.dest)
            .collect()
    }

    #[test]
    fn detects_layouts() {
        let wrapped = entries(&[
            "VolumetricClouds-v5/GameData/EnvironmentalVisualEnhancements/Plugins/EVE.dll",
            "VolumetricClouds-v5/GameData/Scatterer/scatterer.dll",
            "VolumetricClouds-v5/readme.txt",
        ]);
        let d = detect_directives(&wrapped, "Volumetric Clouds");
        assert_eq!(
            dests(&d, &wrapped),
            [
                "GameData/EnvironmentalVisualEnhancements/Plugins/EVE.dll",
                "GameData/Scatterer/scatterer.dll",
            ]
        );

        let bare = entries(&["MyMod/Plugins/a.dll", "MyMod/a.cfg", "patch.cfg"]);
        let d = detect_directives(&bare, "My Patch!");
        assert_eq!(
            dests(&d, &bare),
            [
                "GameData/MyMod/Plugins/a.dll",
                "GameData/MyMod/a.cfg",
                "GameData/MyPatch/patch.cfg"
            ]
        );
    }

    #[test]
    fn adds_7z_archives_through_bsdtar() {
        if std::process::Command::new("bsdtar")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("bsdtar missing; skipped");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = Paths::new(Some(&tmp.path().join("orbis"))).expect("paths");
        let src = tmp.path().join("src/Clouds-5/GameData/EVE");
        fs::create_dir_all(&src).expect("mkdir");
        fs::write(src.join("eve.dll"), b"x").expect("write");
        let archive = tmp.path().join("clouds.7z");
        let ok = std::process::Command::new("bsdtar")
            .args(["--format", "7zip", "-cf"])
            .arg(&archive)
            .arg("-C")
            .arg(tmp.path().join("src"))
            .arg("Clouds-5")
            .status()
            .expect("bsdtar");
        assert!(ok.success());
        let m = add(
            &paths,
            &AddOptions {
                path: archive,
                name: "Clouds".into(),
                ..AddOptions::default()
            },
        )
        .expect("add 7z");
        assert!(m.is_dir);
        let dests: Vec<String> = m
            .plan(&paths)
            .expect("plan")
            .into_iter()
            .filter(|p| !p.is_dir)
            .map(|p| p.dest)
            .collect();
        assert_eq!(dests, ["GameData/EVE/eve.dll"]);
    }

    #[test]
    fn adds_lists_and_deletes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = Paths::new(Some(&tmp.path().join("orbis"))).expect("paths");
        let zip_path = tmp.path().join("clouds.zip");
        {
            let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).expect("create"));
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("GameData/EVE/eve.dll", opts).expect("start");
            w.write_all(b"x").expect("write");
            w.finish().expect("finish");
        }
        let m = add(
            &paths,
            &AddOptions {
                path: zip_path,
                name: "Volumetric Clouds".into(),
                version: "5.0".into(),
                provides: vec!["EnvironmentalVisualEnhancements, Scatterer".into()],
                ..AddOptions::default()
            },
        )
        .expect("add");
        assert_eq!(m.id, "volumetric-clouds");
        assert_eq!(m.provides, ["EnvironmentalVisualEnhancements", "Scatterer"]);
        let module = m.to_module();
        assert_eq!(module.identifier, "manual:volumetric-clouds");
        assert_eq!(m.plan(&paths).expect("plan").len(), 1);
        assert_eq!(
            find(&paths, "manual:volumetric-clouds").expect("find").name,
            "Volumetric Clouds"
        );
        delete(&paths, &m).expect("delete");
        assert!(list(&paths).expect("list").is_empty());
    }
}
