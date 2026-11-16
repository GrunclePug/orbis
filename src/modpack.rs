//! Modpacks: an instance's mod list (and optionally its craft, saves and
//! manual mod files) packed into one file that another machine can import
//! as a new instance.
//!
//! An `.orbispack` is a zip:
//!
//! ```text
//! pack.toml            settings and the mod list (exact versions)
//! manual/<id>/...      manual mod archives, when included
//! saves/<save>/...     saves, when included
//! Ships/...            craft, when included
//! ```
//!
//! CKAN's modpack format (a `.ckan` metapackage) can be exported and
//! imported too, so packs can be swapped with CKAN users.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::install::Source;
use crate::instance::{self, CopyMode, CreateOptions, Instance, LaunchConfig, ModSource};
use crate::manual::{self, AddOptions, ManualMod};
use crate::metadata::{Module, Relationship};
use crate::ops::{self, Event};
use crate::paths::{self, Paths};
use crate::repo::Index;
use crate::resolver::Request;
use crate::version::{GameVersion, ModVersion};

/// The `pack.toml` format version this build writes.
pub const FORMAT: u32 = 1;

/// Contents of `pack.toml`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Pack {
    pub format: u32,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub orbis_version: String,
    pub game_version: GameVersion,
    #[serde(default)]
    pub compat: Vec<GameVersion>,
    #[serde(default)]
    pub allow_testing: bool,
    #[serde(default)]
    pub launch: LaunchConfig,
    /// Saves included in the archive.
    #[serde(default)]
    pub saves: Vec<String>,
    /// Whether the archive includes the `Ships` folder.
    #[serde(default)]
    pub craft: bool,
    #[serde(default)]
    pub mods: Vec<PackMod>,
    #[serde(default)]
    pub manual: Vec<PackManual>,
}

/// One CKAN mod in a pack.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PackMod {
    pub identifier: String,
    /// `None` means the newest compatible release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<ModVersion>,
    /// Installed only as another mod's dependency.
    #[serde(default)]
    pub dependency: bool,
}

/// One manual mod in a pack.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PackManual {
    /// Whether its files are inside the archive.
    pub included: bool,
    #[serde(flatten)]
    pub info: ManualMod,
}

/// What to put in an export.
#[derive(Clone, Debug, Default)]
pub struct ExportOptions {
    pub out: PathBuf,
    pub name: String,
    pub description: String,
    /// Include the `Ships` folder (VAB/SPH craft).
    pub craft: bool,
    /// Saves to include, by folder name.
    pub saves: Vec<String>,
    /// Include manual mod archives. Off by default: many (Patreon builds,
    /// for one) aren't yours to redistribute.
    pub manual_files: bool,
}

/// The user's saves in an instance (stock scenarios and training excluded).
pub fn save_names(inst: &Instance) -> Vec<String> {
    let Ok(entries) = fs::read_dir(inst.game_dir().join("saves")) else {
        return vec![];
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.eq_ignore_ascii_case("scenarios") && !n.eq_ignore_ascii_case("training"))
        .collect();
    out.sort_by_key(|n| n.to_lowercase());
    out
}

/// Builds the pack description of an instance. Returns warnings for manual
/// mods that are installed but no longer in the library.
pub fn describe(
    paths: &Paths,
    inst: &Instance,
    opts: &ExportOptions,
) -> Result<(Pack, Vec<String>)> {
    let reg = inst.registry()?;
    let mut warnings = Vec::new();
    let mut mods = Vec::new();
    let mut manual_mods = Vec::new();
    for (id, m) in &reg.mods {
        match m.source {
            ModSource::Ckan => mods.push(PackMod {
                identifier: id.clone(),
                version: Some(m.module.version.clone()),
                dependency: m.auto_installed,
            }),
            ModSource::Manual => match manual::find(paths, id) {
                Ok(info) => manual_mods.push(PackManual {
                    included: opts.manual_files,
                    info,
                }),
                Err(_) => warnings.push(format!(
                    "{} is installed but no longer in the manual library; it was left out",
                    m.module.display_name()
                )),
            },
        }
    }
    let available: BTreeSet<String> = save_names(inst).into_iter().collect();
    for s in &opts.saves {
        if !available.contains(s) {
            bail!("{} has no save named {s:?}", inst.config.name);
        }
    }
    let pack = Pack {
        format: FORMAT,
        name: if opts.name.trim().is_empty() {
            inst.config.name.clone()
        } else {
            opts.name.trim().to_owned()
        },
        description: opts.description.trim().to_owned(),
        created: paths::now_secs(),
        orbis_version: env!("CARGO_PKG_VERSION").to_owned(),
        game_version: inst.config.game_version.without_build(),
        compat: inst.config.compat.clone(),
        allow_testing: inst.config.allow_testing,
        launch: inst.config.launch.clone(),
        saves: opts.saves.clone(),
        craft: opts.craft,
        mods,
        manual: manual_mods,
    };
    Ok((pack, warnings))
}

/// Writes an `.orbispack`. Returns warnings about anything left out.
pub fn export(paths: &Paths, inst: &Instance, opts: &ExportOptions) -> Result<Vec<String>> {
    let (pack, warnings) = describe(paths, inst, opts)?;
    let tmp = opts.out.with_extension("orbispack.tmp");
    let result = (|| -> Result<()> {
        let file = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        let mut zip = zip::ZipWriter::new(io::BufWriter::new(file));
        let opts_zip = zip::write::SimpleFileOptions::default();
        let text = toml::to_string_pretty(&pack).context("serialize pack.toml")?;
        zip.start_file("pack.toml", opts_zip)
            .context("write pack.toml")?;
        zip.write_all(text.as_bytes()).context("write pack.toml")?;
        let game = inst.game_dir();
        if pack.craft {
            add_dir(&mut zip, &game.join("Ships"), "Ships")?;
        }
        for s in &pack.saves {
            add_dir(&mut zip, &game.join("saves").join(s), &format!("saves/{s}"))?;
        }
        for m in pack.manual.iter().filter(|m| m.included) {
            let src = paths.manual().join(&m.info.id).join(&m.info.file);
            let prefix = format!("manual/{}/{}", m.info.id, m.info.file);
            if m.info.is_dir {
                add_dir(&mut zip, &src, &prefix)?;
            } else {
                add_file(&mut zip, &src, &prefix)?;
            }
        }
        zip.finish().context("finish modpack")?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, &opts.out)
        .with_context(|| format!("move modpack to {}", opts.out.display()))?;
    Ok(warnings)
}

type Zip = zip::ZipWriter<io::BufWriter<File>>;

fn add_file(zip: &mut Zip, src: &Path, name: &str) -> Result<()> {
    zip.start_file(name, zip::write::SimpleFileOptions::default())
        .with_context(|| format!("add {name}"))?;
    let mut f = File::open(src).with_context(|| format!("open {}", src.display()))?;
    io::copy(&mut f, zip).with_context(|| format!("add {name}"))?;
    Ok(())
}

fn add_dir(zip: &mut Zip, dir: &Path, prefix: &str) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for e in Source::Dir(dir.to_path_buf()).entries()? {
        let cloud = e
            .name
            .rsplit('/')
            .next()
            .is_some_and(|n| n.eq_ignore_ascii_case(instance::STEAM_CLOUD_FILE));
        if !e.is_dir && !cloud {
            add_file(zip, &dir.join(&e.name), &format!("{prefix}/{}", e.name))?;
        }
    }
    Ok(())
}

/// Writes a CKAN modpack (a metapackage `.ckan`) listing the mods that
/// weren't installed as dependencies, at their installed versions. Manual
/// mods can't be expressed in it and are left out.
pub fn export_ckan(inst: &Instance, out: &Path, name: &str) -> Result<()> {
    let reg = inst.registry()?;
    let depends: Vec<serde_json::Value> = reg
        .mods
        .iter()
        .filter(|(_, m)| m.source == ModSource::Ckan && !m.auto_installed)
        .map(|(id, m)| serde_json::json!({ "name": id, "version": m.module.version.as_str() }))
        .collect();
    let name = if name.trim().is_empty() {
        inst.config.name.as_str()
    } else {
        name.trim()
    };
    let ident: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_owned();
    let doc = serde_json::json!({
        "spec_version": "v1.6",
        "identifier": format!("installed-{}", if ident.is_empty() { "orbis" } else { &ident }),
        "name": name,
        "abstract": format!("Modpack exported by orbis {}", env!("CARGO_PKG_VERSION")),
        "author": "orbis",
        "version": "1.0",
        "kind": "metapackage",
        "license": "unknown",
        "release_status": "stable",
        "ksp_version": inst.config.game_version.without_build().to_string(),
        "depends": depends,
    });
    let text = serde_json::to_string_pretty(&doc).context("serialize modpack")?;
    paths::write_atomic(out, text.as_bytes())
}

/// Reads a pack: an `.orbispack` zip or a CKAN `.ckan` modpack.
pub fn read(path: &Path) -> Result<Pack> {
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut magic = [0u8; 2];
    let n = f
        .read(&mut magic)
        .with_context(|| format!("read {}", path.display()))?;
    if n == 2 && &magic == b"PK" {
        let mut zip = zip::ZipArchive::new(File::open(path)?)
            .with_context(|| format!("read zip {}", path.display()))?;
        let mut text = String::new();
        zip.by_name("pack.toml")
            .context("not an orbis modpack (no pack.toml)")?
            .read_to_string(&mut text)
            .context("read pack.toml")?;
        let pack: Pack = toml::from_str(&text).context("parse pack.toml")?;
        if pack.format > FORMAT {
            bail!("this modpack needs a newer orbis (format {})", pack.format);
        }
        return Ok(pack);
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let m: Module =
        serde_json::from_str(&text).context("not an orbis modpack or a CKAN .ckan file")?;
    Ok(from_ckan(&m))
}

/// Turns a CKAN metapackage into a pack: its `depends` become the mods.
pub fn from_ckan(m: &Module) -> Pack {
    let mods = m
        .depends
        .iter()
        .filter_map(|r| match r {
            Relationship::Direct(d) => Some(d.clone()),
            Relationship::AnyOf { any_of, .. } => any_of
                .first()
                .and_then(|a| a.alternatives().first().map(|d| (*d).clone())),
        })
        .map(|d| PackMod {
            identifier: d.name,
            version: d.version,
            dependency: false,
        })
        .collect();
    Pack {
        format: FORMAT,
        name: m.display_name().to_owned(),
        description: m.summary.clone(),
        created: 0,
        orbis_version: String::new(),
        game_version: m
            .ksp_version
            .or(m.ksp_version_max)
            .unwrap_or(GameVersion::ANY),
        compat: vec![],
        allow_testing: false,
        launch: LaunchConfig::default(),
        saves: vec![],
        craft: false,
        mods,
        manual: vec![],
    }
}

/// How to import a pack.
#[derive(Clone, Debug)]
pub struct ImportOptions {
    pub path: PathBuf,
    /// Instance name; the pack's name when empty.
    pub name: String,
    /// The game install the new instance links to.
    pub base: PathBuf,
    /// Install the pack's exact versions (falling back to the newest
    /// compatible release when one is gone); otherwise install the newest.
    pub exact: bool,
}

/// What an import did.
#[derive(Debug)]
pub struct ImportReport {
    pub instance: Instance,
    pub warnings: Vec<String>,
}

/// Creates a new instance from a pack and installs its mods.
pub fn import(
    paths: &Paths,
    config: &Config,
    index: &Index,
    opts: &ImportOptions,
    events: &mpsc::Sender<Event>,
) -> Result<ImportReport> {
    let status = |s: String| {
        let _ = events.send(Event::Status(s));
    };
    let pack = read(&opts.path)?;
    let is_zip = is_zip(&opts.path);
    let mut warnings = Vec::new();

    status(format!("Creating instance for {}", pack.name));
    let name = if opts.name.trim().is_empty() {
        pack.name.clone()
    } else {
        opts.name.clone()
    };
    let mut inst = instance::create(
        paths,
        &CreateOptions {
            name,
            source: opts.base.clone(),
            stock_only: true,
            with_saves: false,
            mode: CopyMode::Link,
        },
        &mut |_, _| {},
    )?;
    if !pack.compat.is_empty() {
        inst.config.compat = pack.compat.clone();
    }
    inst.config.allow_testing = pack.allow_testing;
    inst.config.launch = pack.launch.clone();
    inst.config.notes = pack.description.clone();
    inst.save()?;
    let ours = inst.config.game_version.without_build();
    if !pack.game_version.is_any() && !pack.game_version.range().intersects(&ours.range()) {
        warnings.push(format!(
            "the pack was made for KSP {}, this install is {ours}",
            pack.game_version
        ));
    }

    if is_zip {
        let game = inst.game_dir();
        let n = extract_prefix(&opts.path, "saves/", &game.join("saves"))?
            + extract_prefix(&opts.path, "Ships/", &game.join("Ships"))?;
        if n > 0 {
            status(format!("Copied {n} save and craft files"));
        }
    }

    // Manual mods: reuse the library copy, or add the included files.
    let library = manual::list(paths)?;
    let mut manual_to_install = Vec::new();
    let scratch = paths.cache.join(format!("import-{}", paths::now_secs()));
    for pm in &pack.manual {
        let info = &pm.info;
        if let Some(have) = library
            .iter()
            .find(|m| m.id == info.id && m.version == info.version)
        {
            manual_to_install.push(have.clone());
            continue;
        }
        if !(pm.included && is_zip) {
            warnings.push(format!(
                "manual mod {} {} isn't in the pack or your library: add it, then install it",
                info.name, info.version
            ));
            continue;
        }
        let dest = scratch.join(&info.id);
        extract_prefix(&opts.path, &format!("manual/{}/", info.id), &dest)?;
        let added = manual::add(
            paths,
            &AddOptions {
                path: dest.join(&info.file),
                name: info.name.clone(),
                version: info.version.clone(),
                provides: info.provides.clone(),
                depends: info.depends.clone(),
                conflicts: info.conflicts.clone(),
                install: info.install.clone(),
                source_url: info.source_url.clone(),
                notes: info.notes.clone(),
            },
        );
        match added {
            Ok(m) => manual_to_install.push(m),
            Err(e) => warnings.push(format!("manual mod {}: {e:#}", info.name)),
        }
    }
    let _ = fs::remove_dir_all(&scratch);

    // CKAN mods, at the pack's versions when they still exist.
    let compat = inst.compat_versions();
    let mut req = Request::default();
    for pm in &pack.mods {
        if index.versions(&pm.identifier).is_empty() {
            warnings.push(format!(
                "{} isn't in the CKAN index any more; skipped",
                pm.identifier
            ));
            continue;
        }
        let pinned = pm.version.as_ref().filter(|_| opts.exact);
        match pinned {
            Some(v) if index.get(&pm.identifier, v).is_some() => {
                req.install.push((pm.identifier.clone(), Some(v.clone())));
            }
            _ => {
                if index
                    .latest_compatible(&pm.identifier, &compat, inst.config.allow_testing)
                    .is_none()
                {
                    warnings.push(format!(
                        "{} has no compatible release; skipped",
                        pm.identifier
                    ));
                    continue;
                }
                if let Some(v) = pinned {
                    warnings.push(format!(
                        "{} {v} is gone; installing the newest compatible release",
                        pm.identifier
                    ));
                }
                req.install.push((pm.identifier.clone(), None));
            }
        }
    }
    if !req.install.is_empty() {
        status(format!("Resolving {} mods", req.install.len()));
        let plan = ops::plan(&inst, index, &req)?.map_err(|e| {
            anyhow::anyhow!(
                "created {}, but its mods couldn't be resolved: {e}",
                inst.config.name
            )
        })?;
        ops::apply(paths, config, &inst, &plan, events)?;
    }

    // Dependencies stay marked as dependencies.
    let mut reg = inst.registry()?;
    for pm in &pack.mods {
        if let Some(m) = reg.mods.get_mut(&pm.identifier) {
            m.auto_installed = pm.dependency;
        }
    }
    inst.save_registry(&reg)?;

    for m in manual_to_install {
        status(format!("Installing {}", m.name));
        let result = ops::plan_manual(&inst, index, &m).and_then(|p| {
            let plan = p.map_err(|e| anyhow::anyhow!("{e}"))?;
            ops::apply(paths, config, &inst, &plan, events)
        });
        if let Err(e) = result {
            warnings.push(format!("manual mod {}: {e:#}", m.name));
        }
    }
    let _ = events.send(Event::Status("Import finished".into()));
    Ok(ImportReport {
        instance: inst,
        warnings,
    })
}

fn is_zip(path: &Path) -> bool {
    let mut magic = [0u8; 2];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && &magic == b"PK"
}

/// Extracts every entry under `prefix` into `dest`. Returns the number of
/// files written. Entries that would escape `dest` are refused.
fn extract_prefix(zip_path: &Path, prefix: &str, dest: &Path) -> Result<usize> {
    let mut zip = zip::ZipArchive::new(File::open(zip_path)?).context("read modpack")?;
    let mut count = 0;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).context("read modpack entry")?;
        let name = f
            .name()
            .context("read modpack entry name")?
            .replace('\\', "/");
        let Some(rel) = name.strip_prefix(prefix) else {
            continue;
        };
        if rel.is_empty() || f.is_dir() {
            continue;
        }
        if rel.starts_with('/') || rel.split('/').any(|p| p == ".." || p.is_empty()) {
            bail!("modpack entry {name:?} has an unsafe path");
        }
        let out = dest.join(rel);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let mut w = File::create(&out).with_context(|| format!("create {}", out.display()))?;
        io::copy(&mut f, &mut w).with_context(|| format!("extract {name}"))?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_ksp(dir: &Path) {
        fs::create_dir_all(dir.join("GameData/Squad")).expect("mkdir");
        fs::create_dir_all(dir.join("Ships/VAB")).expect("mkdir");
        fs::write(dir.join("Ships/VAB/Stock.craft"), b"stock").expect("write");
        fs::write(dir.join("KSP_x64.exe"), b"").expect("write");
        fs::write(dir.join("readme.txt"), "Version 1.12.5\n").expect("write");
    }

    fn zip_with(path: &Path, files: &[&str]) -> String {
        let mut w = zip::ZipWriter::new(File::create(path).expect("create"));
        for f in files {
            w.start_file(*f, zip::write::SimpleFileOptions::default())
                .expect("start");
            w.write_all(f.as_bytes()).expect("write");
        }
        w.finish().expect("finish");
        format!("file://{}", path.display())
    }

    #[test]
    fn export_then_import_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ksp = tmp.path().join("ksp");
        fake_ksp(&ksp);
        let paths = Paths::new(Some(&tmp.path().join("orbis"))).expect("paths");
        let config = Config::default();
        let mk = |id: &str, file: &str, deps: &[&str]| -> Module {
            let url = zip_with(&tmp.path().join(format!("{id}.zip")), &[file]);
            serde_json::from_value(serde_json::json!({
                "identifier": id, "version": "1.0", "ksp_version": "1.12", "download": url,
                "depends": deps.iter().map(|d| serde_json::json!({"name": d})).collect::<Vec<_>>(),
            }))
            .expect("module")
        };
        let index = Index::from_modules(
            vec![
                mk("Lib", "GameData/Lib/lib.dll", &[]),
                mk("App", "GameData/App/app.dll", &["Lib"]),
            ],
            Default::default(),
            0,
        );
        let (tx, _rx) = mpsc::channel();
        let create = |name: &str| {
            instance::create(
                &paths,
                &CreateOptions {
                    name: name.into(),
                    source: ksp.clone(),
                    stock_only: true,
                    with_saves: false,
                    mode: CopyMode::Link,
                },
                &mut |_, _| {},
            )
            .expect("create")
        };
        let src = create("Source");
        let req = Request {
            install: vec![("App".into(), None)],
            ..Request::default()
        };
        let plan = ops::plan(&src, &index, &req).expect("io").expect("plan");
        ops::apply(&paths, &config, &src, &plan, &tx).expect("apply");

        // A manual mod, a save and some craft.
        let clouds = tmp.path().join("clouds.zip");
        zip_with(&clouds, &["GameData/Clouds/c.dll"]);
        let m = manual::add(
            &paths,
            &AddOptions {
                path: clouds,
                name: "Clouds".into(),
                version: "2.0".into(),
                ..AddOptions::default()
            },
        )
        .expect("add manual");
        let p = ops::plan_manual(&src, &index, &m)
            .expect("io")
            .expect("plan");
        ops::apply(&paths, &config, &src, &p, &tx).expect("apply manual");
        let g = src.game_dir();
        fs::create_dir_all(g.join("saves/Career")).expect("mkdir");
        fs::write(g.join("saves/Career/persistent.sfs"), b"save").expect("write");
        fs::write(g.join("Ships/VAB/Mine.craft"), b"mine").expect("write");
        assert_eq!(save_names(&src), ["Career"]);

        let out = tmp.path().join("pack.orbispack");
        let warnings = export(
            &paths,
            &src,
            &ExportOptions {
                out: out.clone(),
                name: "My Pack".into(),
                craft: true,
                saves: vec!["Career".into()],
                manual_files: true,
                ..ExportOptions::default()
            },
        )
        .expect("export");
        assert!(warnings.is_empty(), "{warnings:?}");
        let pack = read(&out).expect("read");
        assert_eq!(pack.name, "My Pack");
        assert_eq!(pack.mods.len(), 2);
        assert!(
            pack.mods
                .iter()
                .any(|m| m.identifier == "Lib" && m.dependency)
        );

        // Import on a machine without the manual mod in its library.
        manual::delete(&paths, &m).expect("delete manual");
        let report = import(
            &paths,
            &config,
            &index,
            &ImportOptions {
                path: out,
                name: String::new(),
                base: ksp.clone(),
                exact: true,
            },
            &tx,
        )
        .expect("import");
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        let inst = report.instance;
        assert_eq!(inst.config.name, "My Pack");
        let ng = inst.game_dir();
        assert!(ng.join("GameData/App/app.dll").is_file());
        assert!(ng.join("GameData/Clouds/c.dll").is_file());
        assert_eq!(
            fs::read(ng.join("saves/Career/persistent.sfs")).expect("save"),
            b"save"
        );
        assert_eq!(
            fs::read(ng.join("Ships/VAB/Mine.craft")).expect("craft"),
            b"mine"
        );
        let reg = inst.registry().expect("registry");
        assert!(reg.mods["Lib"].auto_installed);
        assert!(!reg.mods["App"].auto_installed);
        assert!(reg.mods.contains_key("manual:clouds"));

        // CKAN format lists only what was asked for.
        let ckan = tmp.path().join("pack.ckan");
        export_ckan(&inst, &ckan, "My Pack").expect("export ckan");
        let back = read(&ckan).expect("read ckan");
        assert_eq!(back.mods.len(), 1);
        assert_eq!(back.mods[0].identifier, "App");
        assert_eq!(
            back.mods[0].version.as_ref().map(ModVersion::as_str),
            Some("1.0")
        );
    }
}
