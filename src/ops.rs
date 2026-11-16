//! High-level operations on an instance, shared by the CLI and the GUI.

use std::collections::HashMap;
use std::sync::mpsc;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::download::{self, DownloadEvent};
use crate::install::{self, Source};
use crate::instance::{InstalledMod, Instance, ModSource, Registry};
use crate::manual::{self, ManualMod};
use crate::metadata::{Kind, Module};
use crate::paths::{self, Paths};
use crate::repo::Index;
use crate::resolver::{Change, Plan, Present, Reason, Request, ResolveError, Resolver};

/// Progress reported while applying changes.
#[derive(Debug)]
pub enum Event {
    Status(String),
    Download(DownloadEvent),
}

/// Everything present in an instance, as the resolver sees it: installed
/// mods (CKAN and manual) plus detected DLC.
pub fn present(inst: &Instance, reg: &Registry) -> Vec<Present> {
    let mut out: Vec<Present> = reg
        .mods
        .values()
        .map(|m| Present {
            module: m.module.clone(),
            auto_installed: m.auto_installed,
            is_dlc: false,
            unknown_version: false,
        })
        .collect();
    for dlc in inst.dlc() {
        out.push(Present::dlc(&dlc.identifier, dlc.version));
    }
    out
}

/// Plans a request against an instance.
pub fn plan(inst: &Instance, index: &Index, req: &Request) -> Result<Result<Plan, ResolveError>> {
    let reg = inst.registry()?;
    let compat = inst.compat_versions();
    let resolver = Resolver {
        index,
        compat: &compat,
        allow_testing: inst.config.allow_testing,
    };
    Ok(resolver.resolve(&present(inst, &reg), req))
}

/// Identifiers with a newer compatible release than the installed one.
pub fn upgradable(inst: &Instance, index: &Index, reg: &Registry) -> Vec<(String, Module)> {
    let compat = inst.compat_versions();
    reg.mods
        .iter()
        .filter(|(_, m)| m.source == ModSource::Ckan)
        .filter_map(|(id, m)| {
            let latest = index.latest_compatible(id, &compat, inst.config.allow_testing)?;
            (latest.version > *m.version()).then(|| (id.clone(), latest.clone()))
        })
        .collect()
}

/// Carries out a plan: downloads, then removals, then installs. The
/// registry is saved after every step, so a failure part-way leaves it
/// matching the files on disk.
pub fn apply(
    paths: &Paths,
    config: &Config,
    inst: &Instance,
    plan: &Plan,
    events: &mpsc::Sender<Event>,
) -> Result<()> {
    let to_fetch: Vec<&Module> = plan
        .changes
        .iter()
        .filter_map(|c| match c {
            Change::Install { module, .. } | Change::Upgrade { module, .. } => Some(module),
            Change::Remove { .. } => None,
        })
        .filter(|m| m.kind == Kind::Package && !m.identifier.starts_with(manual::PREFIX))
        .collect();
    let archives: HashMap<String, std::path::PathBuf> = if to_fetch.is_empty() {
        HashMap::new()
    } else {
        let _ = events.send(Event::Status(format!(
            "Downloading {} mod(s)",
            to_fetch.len()
        )));
        let (tx, rx) = mpsc::channel();
        let dir = paths.downloads();
        let result = std::thread::scope(|s| {
            let forward = s.spawn(|| {
                for e in rx {
                    let _ = events.send(Event::Download(e));
                }
            });
            let r = download::download_modules(&dir, &to_fetch, config.download_workers, &tx);
            drop(tx);
            let _ = forward.join();
            r
        })?;
        to_fetch
            .iter()
            .zip(result)
            .map(|(m, p)| (m.identifier.clone(), p))
            .collect()
    };

    let game = inst.game_dir();
    let mut reg = inst.registry()?;
    let mut auto_before: HashMap<String, bool> = HashMap::new();
    for c in &plan.changes {
        if let Change::Remove { identifier, .. }
        | Change::Upgrade {
            module: Module { identifier, .. },
            ..
        } = c
            && let Some(old) = reg.mods.remove(identifier)
        {
            let _ = events.send(Event::Status(format!(
                "Removing {identifier} {}",
                old.version()
            )));
            install::uninstall(&game, &old.installed_files());
            auto_before.insert(identifier.clone(), old.auto_installed);
            inst.save_registry(&reg)?;
        }
    }
    for c in &plan.changes {
        let (module, reason) = match c {
            Change::Install { module, reason } | Change::Upgrade { module, reason, .. } => {
                (module, reason)
            }
            Change::Remove { .. } => continue,
        };
        let id = &module.identifier;
        let _ = events.send(Event::Status(format!("Installing {id} {}", module.version)));
        let (files, source) = if let Some(mid) = id.strip_prefix(manual::PREFIX) {
            let m = manual::find(paths, mid)?;
            let src = m.source(paths);
            (
                install_source(inst, &reg, &src, &m.install)?,
                ModSource::Manual,
            )
        } else if module.kind == Kind::Metapackage {
            (install::InstalledFiles::default(), ModSource::Ckan)
        } else {
            let archive = archives
                .get(id)
                .with_context(|| format!("{id} was not downloaded"))?;
            let src = Source::Zip(archive.clone());
            let files = install_source(inst, &reg, &src, &module.directives())
                .with_context(|| format!("install {id} {}", module.version))?;
            (files, ModSource::Ckan)
        };
        let auto = auto_before
            .get(id)
            .copied()
            .unwrap_or(matches!(reason, Reason::Dependency(_)));
        reg.mods.insert(
            id.clone(),
            InstalledMod {
                source,
                auto_installed: auto,
                installed_at: paths::now_secs(),
                files: files.files,
                dirs: files.dirs,
                module: module.clone(),
            },
        );
        inst.save_registry(&reg)?;
    }
    let _ = events.send(Event::Status("Done".into()));
    Ok(())
}

fn install_source(
    inst: &Instance,
    reg: &Registry,
    src: &Source,
    directives: &[crate::metadata::InstallDirective],
) -> Result<install::InstalledFiles> {
    let planned = install::plan(directives, &src.entries()?)?;
    install::install(&inst.game_dir(), src, &planned, &reg.owners(), false)
}

/// Plans installing a manual mod (resolving its CKAN dependencies).
pub fn plan_manual(
    inst: &Instance,
    index: &Index,
    m: &ManualMod,
) -> Result<Result<Plan, ResolveError>> {
    let reg = inst.registry()?;
    let compat = inst.compat_versions();
    let resolver = Resolver {
        index,
        compat: &compat,
        allow_testing: inst.config.allow_testing,
    };
    let mut present = present(inst, &reg);
    if reg.mods.contains_key(&m.identifier()) {
        bail!("{} is already installed in {}", m.name, inst.config.name);
    }
    // The resolver only knows index modules, so plan the manual mod's
    // dependencies as plain installs, then put the manual mod first.
    let deps: Vec<(String, Option<crate::version::ModVersion>)> = m
        .depends
        .iter()
        .filter(|d| {
            !present
                .iter()
                .any(|p| p.module.identifier == **d || p.module.provides.contains(d))
        })
        .map(|d| (d.clone(), None))
        .collect();
    present.push(Present {
        module: m.to_module(),
        auto_installed: false,
        is_dlc: false,
        unknown_version: false,
    });
    // CKAN mods the manual mod stands in for are removed.
    let replaced: Vec<String> = m
        .provides
        .iter()
        .filter(|p| {
            reg.mods
                .get(*p)
                .is_some_and(|i| i.source == ModSource::Ckan)
        })
        .cloned()
        .collect();
    let req = Request {
        install: deps,
        remove: replaced,
        ..Request::default()
    };
    let mut plan = match resolver.resolve(&present, &req) {
        Ok(p) => p,
        Err(e) => return Ok(Err(e)),
    };
    for c in &mut plan.changes {
        match c {
            Change::Install { reason, .. } => *reason = Reason::Dependency(m.identifier()),
            Change::Remove { reason, .. } if *reason == Reason::Requested => {
                *reason = Reason::ReplacedBy(m.identifier());
            }
            _ => {}
        }
    }
    plan.changes.push(Change::Install {
        module: m.to_module(),
        reason: Reason::Requested,
    });
    Ok(Ok(plan))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::{CopyMode, CreateOptions};
    use std::fs;
    use std::io::Write;

    fn fake_ksp(dir: &std::path::Path) {
        fs::create_dir_all(dir.join("GameData/Squad")).expect("mkdir");
        fs::write(dir.join("KSP_x64.exe"), b"").expect("write");
        fs::write(dir.join("readme.txt"), "Version 1.12.5\n").expect("write");
    }

    fn zip_with(path: &std::path::Path, files: &[&str]) -> String {
        let mut w = zip::ZipWriter::new(fs::File::create(path).expect("create"));
        for f in files {
            w.start_file(*f, zip::write::SimpleFileOptions::default())
                .expect("start");
            w.write_all(f.as_bytes()).expect("write");
        }
        w.finish().expect("finish");
        format!("file://{}", path.display())
    }

    #[test]
    fn install_upgrade_remove_end_to_end() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ksp = tmp.path().join("ksp");
        fake_ksp(&ksp);
        let paths = Paths::new(Some(&tmp.path().join("orbis"))).expect("paths");
        let config = Config::default();
        let inst = crate::instance::create(
            &paths,
            &CreateOptions {
                name: "t".into(),
                source: ksp,
                stock_only: true,
                with_saves: false,
                mode: CopyMode::Copy,
            },
            &mut |_, _| {},
        )
        .expect("create");

        let mk = |id: &str, v: &str, file: &str, deps: &[&str]| -> Module {
            let url = zip_with(&tmp.path().join(format!("{id}-{v}.zip")), &[file]);
            serde_json::from_value(serde_json::json!({
                "identifier": id, "version": v, "ksp_version": "1.12", "download": url,
                "depends": deps.iter().map(|d| serde_json::json!({"name": d})).collect::<Vec<_>>(),
            }))
            .expect("module")
        };
        let index = Index::from_modules(
            vec![
                mk("Lib", "1.0", "GameData/Lib/lib.dll", &[]),
                mk("App", "1.0", "GameData/App/app.dll", &["Lib"]),
                mk("App", "2.0", "GameData/App/app2.dll", &["Lib"]),
            ],
            HashMap::new(),
            0,
        );
        let (tx, _rx) = mpsc::channel();
        let req = Request {
            install: vec![("App".into(), Some(crate::version::ModVersion::new("1.0")))],
            ..Request::default()
        };
        let p = plan(&inst, &index, &req).expect("io").expect("resolves");
        apply(&paths, &config, &inst, &p, &tx).expect("apply");
        let g = inst.game_dir();
        assert!(g.join("GameData/App/app.dll").is_file());
        assert!(g.join("GameData/Lib/lib.dll").is_file());
        let reg = inst.registry().expect("registry");
        assert!(reg.mods["Lib"].auto_installed);

        assert_eq!(upgradable(&inst, &index, &reg).len(), 1);
        let req = Request {
            upgrade: vec!["App".into()],
            ..Request::default()
        };
        let p = plan(&inst, &index, &req).expect("io").expect("resolves");
        apply(&paths, &config, &inst, &p, &tx).expect("upgrade");
        assert!(!g.join("GameData/App/app.dll").exists());
        assert!(g.join("GameData/App/app2.dll").is_file());

        let req = Request {
            remove: vec!["App".into()],
            ..Request::default()
        };
        let p = plan(&inst, &index, &req).expect("io").expect("resolves");
        assert_eq!(p.changes.len(), 2, "Lib goes too: {:?}", p.changes);
        apply(&paths, &config, &inst, &p, &tx).expect("remove");
        assert!(!g.join("GameData/App").exists());
        assert!(!g.join("GameData/Lib").exists());
        assert!(inst.registry().expect("registry").mods.is_empty());
    }
}
