//! High-level operations on an instance, shared by the CLI and the GUI.

use std::collections::HashMap;
use std::sync::mpsc;

use anyhow::{Context, Result, bail};

use crate::backup;
use crate::cancel::Cancel;
use crate::config::Config;
use crate::download::{self, DownloadEvent};
use crate::history;
use crate::install::{self, Source};
use crate::instance::{InstalledMod, Instance, ModSource, Registry};
use crate::manual::{self, ManualMod};
use crate::metadata::{Kind, Module};
use crate::paths::{self, Paths};
use crate::repo::Index;
use crate::resolver::{Change, Plan, Present, Reason, Request, ResolveError, Resolver};
use crate::store;

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
    cancel: &Cancel,
) -> Result<()> {
    if plan.is_empty() {
        return Ok(());
    }
    if let Some(b) = backup::snapshot(inst, "before changes", config.backups_keep)
        .context("back up saves before changing mods (set backups_keep = 0 to turn backups off)")?
    {
        let _ = events.send(Event::Status(format!(
            "Backed up {} save(s)",
            b.saves.len()
        )));
    }
    let before = history::snapshot(&inst.registry()?);
    let result = apply_changes(paths, config, inst, plan, events, cancel);
    let after = history::snapshot(&inst.registry()?);
    let mut summary: Vec<String> = plan.changes.iter().map(ToString::to_string).collect();
    if let Err(e) = &result {
        summary.push(format!("failed: {e:#}"));
    }
    history::record(inst, before, after, summary)?;
    result
}

fn apply_changes(
    paths: &Paths,
    config: &Config,
    inst: &Instance,
    plan: &Plan,
    events: &mpsc::Sender<Event>,
    cancel: &Cancel,
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
            let r =
                download::download_modules(&dir, &to_fetch, config.download_workers, &tx, cancel);
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

    cancel.check()?;
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
        // Stop between mods, never in the middle of one.
        cancel.check()?;
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
            let src = store::manual_source(paths, &m)?;
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
            let src = store::ckan_source(paths, archive)
                .with_context(|| format!("unpack {id} {}", module.version))?;
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

/// Brings an instance back to a recorded mod list. Returns warnings for
/// anything that couldn't be restored.
pub fn restore(
    paths: &Paths,
    config: &Config,
    inst: &Instance,
    index: &Index,
    target: &[history::SnapMod],
    events: &mpsc::Sender<Event>,
    cancel: &Cancel,
) -> Result<Vec<String>> {
    // Mods the target chose count as chosen while planning, so they aren't
    // removed as orphans along the way; demotions wait until the end.
    history::promote_chosen(inst, target)?;
    let mut warnings: Vec<String> = Vec::new();
    // Each pass removes what the target drops, then installs manual mods
    // (target CKAN mods may depend on what they provide), then works out the
    // CKAN installs again. Steps
    // that fail get another pass (a manual mod may conflict with a CKAN mod
    // the CKAN side removes); only failures in the last pass are reported.
    let mut failures: Vec<String> = Vec::new();
    let mut ckan_error = None;
    for _ in 0..3 {
        let rp = history::restore_plan(paths, inst, index, target)?;
        for w in &rp.warnings {
            if !warnings.contains(w) {
                warnings.push(w.clone());
            }
        }
        if rp.is_empty() {
            break;
        }
        failures.clear();
        ckan_error = None;
        // Removals first: a mod the target drops may conflict with a manual
        // mod the target adds. Mods a target manual mod provides are left
        // for it to replace (which keeps their dependents), and removals that
        // would take a target mod with them wait for the combined CKAN step.
        let replaced: Vec<&String> = rp.manual.iter().flat_map(|m| &m.provides).collect();
        let removals = Request {
            remove: rp
                .request
                .remove
                .iter()
                .filter(|id| !replaced.contains(id))
                .cloned()
                .collect(),
            ..Request::default()
        };
        if !removals.remove.is_empty()
            && let Ok(p) = plan(inst, index, &removals)?
            && drops_target(&p, target).is_none()
        {
            apply(paths, config, inst, &p, events, cancel)?;
        }
        for m in &rp.manual {
            cancel.check()?;
            let result = plan_manual(inst, index, m, &HashMap::new()).and_then(|p| {
                let p = p.map_err(|e| anyhow::anyhow!("{e}"))?;
                if let Some(id) = drops_target(&p, target) {
                    bail!("installing it would remove {id}, which that point keeps");
                }
                apply(paths, config, inst, &p, events, cancel)
            });
            if let Err(e) = result {
                if crate::cancel::is_cancelled(&e) {
                    return Err(e);
                }
                failures.push(format!("manual mod {}: {e:#}", m.name));
            }
        }
        let rp = history::restore_plan(paths, inst, index, target)?;
        if !rp.request.install.is_empty() || !rp.request.remove.is_empty() {
            match plan(inst, index, &rp.request)? {
                Ok(p) => {
                    // Never delete a mod the target keeps: fail instead.
                    if let Some(id) = drops_target(&p, target) {
                        bail!(
                            "restoring would remove {id}, which that point keeps; stopped before removing it"
                        );
                    }
                    apply(paths, config, inst, &p, events, cancel)?;
                }
                Err(e) => ckan_error = Some(e),
            }
        }
    }
    if let Some(e) = ckan_error {
        bail!("{e}");
    }
    warnings.extend(failures);
    if !history::restore_plan(paths, inst, index, target)?.is_empty() {
        warnings.push("some mods could not be put back exactly; check the Installed list".into());
    }
    history::restore_flags(inst, target)?;
    Ok(warnings)
}

/// A mod the target keeps that the plan would remove (other than a removal
/// asked for directly, such as reinstalling a manual mod at another version).
fn drops_target<'a>(p: &'a Plan, target: &[history::SnapMod]) -> Option<&'a str> {
    p.changes.iter().find_map(|c| match c {
        Change::Remove {
            identifier, reason, ..
        } if *reason != Reason::Requested && target.iter().any(|t| &t.identifier == identifier) => {
            Some(identifier.as_str())
        }
        _ => None,
    })
}

/// Turns a hand-added GameData folder into a tracked manual mod: copies it
/// into the library and records the files already in place as installed.
pub fn adopt(
    paths: &Paths,
    inst: &Instance,
    folder: &str,
    opts: &manual::AddOptions,
) -> Result<ManualMod> {
    let mut reg = inst.registry()?;
    let (unmanaged, _) = manual::unmanaged(&inst.game_dir(), &reg.owners())?;
    if !unmanaged.iter().any(|u| u.folder == folder) {
        bail!(
            "GameData/{folder} isn't an unmanaged folder in {}",
            inst.config.name
        );
    }
    let m = manual::add_folder(paths, &inst.game_dir().join("GameData"), folder, opts)?;
    let planned = m.plan(paths)?;
    let before = history::snapshot(&reg);
    reg.mods.insert(
        m.identifier(),
        InstalledMod {
            source: ModSource::Manual,
            auto_installed: false,
            installed_at: paths::now_secs(),
            files: planned
                .iter()
                .filter(|p| !p.is_dir)
                .map(|p| p.dest.clone())
                .collect(),
            dirs: install::dirs_of(&planned),
            module: m.to_module(),
        },
    );
    inst.save_registry(&reg)?;
    history::record(
        inst,
        before,
        history::snapshot(&reg),
        vec![format!("adopt GameData/{folder} as {}", m.identifier())],
    )?;
    Ok(m)
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
/// Plans installing a manual mod. `choices` answers earlier provider
/// questions, as in [`Request::choices`].
pub fn plan_manual(
    inst: &Instance,
    index: &Index,
    m: &ManualMod,
    choices: &HashMap<String, String>,
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
        choices: choices.clone(),
        ..Request::default()
    };
    let mut plan = match resolver.resolve(&present, &req) {
        Ok(p) => p,
        Err(e) => return Ok(Err(e)),
    };
    // The resolver sees the manual mod as already present, so it doesn't
    // check its conflicts: do that against the mods the plan leaves.
    let added = m.to_module();
    // Removed and upgraded mods leave their old metadata behind.
    let removed: Vec<&str> = plan
        .changes
        .iter()
        .filter_map(|c| match c {
            Change::Remove { identifier, .. } => Some(identifier.as_str()),
            Change::Upgrade { module, .. } => Some(module.identifier.as_str()),
            Change::Install { .. } => None,
        })
        .collect();
    let mut after: Vec<&crate::metadata::Module> = present
        .iter()
        .map(|p| &p.module)
        .filter(|p| p.identifier != added.identifier && !removed.contains(&p.identifier.as_str()))
        .collect();
    after.extend(plan.changes.iter().filter_map(|c| match c {
        Change::Install { module, .. } | Change::Upgrade { module, .. } => Some(module),
        Change::Remove { .. } => None,
    }));
    for o in after {
        let clash = added
            .conflicts
            .iter()
            .any(|r| r.matches(&o.identifier, Some(&o.version), &o.provides))
            || o.conflicts
                .iter()
                .any(|r| r.matches(&added.identifier, Some(&added.version), &added.provides));
        if clash {
            return Ok(Err(ResolveError::Conflict {
                a: added.identifier.clone(),
                b: o.identifier.clone(),
            }));
        }
    }
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
        // Cancelled before it starts: nothing changes.
        let stop = Cancel::default();
        stop.cancel();
        let err = apply(&paths, &config, &inst, &p, &tx, &stop).expect_err("cancelled");
        assert!(crate::cancel::is_cancelled(&err));
        assert!(inst.registry().expect("registry").mods.is_empty());
        assert!(!inst.game_dir().join("GameData/App").exists());
        apply(&paths, &config, &inst, &p, &tx, &Cancel::default()).expect("apply");
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
        apply(&paths, &config, &inst, &p, &tx, &Cancel::default()).expect("upgrade");
        assert!(!g.join("GameData/App/app.dll").exists());
        assert!(g.join("GameData/App/app2.dll").is_file());

        let req = Request {
            remove: vec!["App".into()],
            ..Request::default()
        };
        let p = plan(&inst, &index, &req).expect("io").expect("resolves");
        assert_eq!(p.changes.len(), 2, "Lib goes too: {:?}", p.changes);
        apply(&paths, &config, &inst, &p, &tx, &Cancel::default()).expect("remove");
        assert!(!g.join("GameData/App").exists());
        assert!(!g.join("GameData/Lib").exists());
        assert!(inst.registry().expect("registry").mods.is_empty());

        // History: install 1.0, upgrade to 2.0, remove; roll back to 1.0.
        let entries = history::list(&inst).expect("history");
        assert_eq!(entries.len(), 3);
        assert!(entries[0].summary[0].starts_with("remove App"));
        let first = entries.last().expect("first entry").after.clone();
        let warnings = restore(
            &paths,
            &config,
            &inst,
            &index,
            &first,
            &tx,
            &Cancel::default(),
        )
        .expect("restore");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(g.join("GameData/App/app.dll").is_file());
        let reg = inst.registry().expect("registry");
        assert_eq!(reg.mods["App"].module.version.as_str(), "1.0");
        assert!(reg.mods["Lib"].auto_installed);
        assert!(!reg.mods["App"].auto_installed);
    }

    #[test]
    fn manual_mods_respect_conflicts_and_choices() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ksp = tmp.path().join("ksp");
        fake_ksp(&ksp);
        let paths = Paths::new(Some(&tmp.path().join("orbis"))).expect("paths");
        let inst = crate::instance::create(
            &paths,
            &CreateOptions {
                name: "t".into(),
                source: ksp,
                stock_only: true,
                with_saves: false,
                mode: CopyMode::Link,
            },
            &mut |_, _| {},
        )
        .expect("create");
        let mk = |id: &str, file: &str, provides: &[&str]| -> Module {
            let url = zip_with(&tmp.path().join(format!("{id}.zip")), &[file]);
            serde_json::from_value(serde_json::json!({
                "identifier": id, "version": "1", "ksp_version": "1.12", "download": url,
                "provides": provides,
            }))
            .expect("module")
        };
        let index = Index::from_modules(
            vec![
                mk("Old", "GameData/Old/o.dll", &[]),
                mk("P1", "GameData/P1/p.dll", &["Virt"]),
                mk("P2", "GameData/P2/p.dll", &["Virt"]),
            ],
            HashMap::new(),
            0,
        );
        let (tx, _rx) = mpsc::channel();
        let req = Request {
            install: vec![("Old".into(), None)],
            ..Request::default()
        };
        let p = plan(&inst, &index, &req).expect("io").expect("plan");
        apply(
            &paths,
            &Config::default(),
            &inst,
            &p,
            &tx,
            &Cancel::default(),
        )
        .expect("apply");

        let zip = tmp.path().join("m.zip");
        zip_with(&zip, &["GameData/M/m.dll"]);
        let m = manual::add(
            &paths,
            &manual::AddOptions {
                path: zip,
                name: "M".into(),
                conflicts: vec!["Old".into()],
                ..manual::AddOptions::default()
            },
        )
        .expect("add");
        let none = HashMap::new();
        let err = plan_manual(&inst, &index, &m, &none)
            .expect("io")
            .expect_err("conflicts with Old");
        assert!(matches!(err, ResolveError::Conflict { .. }), "{err:?}");

        // A manual mod needing a virtual: the provider question is answered
        // by choices.
        let mut m2 = m.clone();
        m2.conflicts.clear();
        m2.depends = vec!["Virt".into()];
        let err = plan_manual(&inst, &index, &m2, &none)
            .expect("io")
            .expect_err("asks");
        let ResolveError::Choice { relationship, .. } = err else {
            panic!("{err:?}")
        };
        let choices: HashMap<String, String> = [(relationship, "P2".to_owned())].into();
        let p = plan_manual(&inst, &index, &m2, &choices)
            .expect("io")
            .expect("plans");
        assert!(p.changes.iter().any(|c| c.identifier() == "P2"));

        // An installed mod whose old version conflicts with what the manual
        // mod provides, but whose upgrade (pulled in by the manual mod)
        // doesn't, is no conflict.
        let mut old_lib = mk("Lib", "GameData/Lib/l1.dll", &[]);
        old_lib.conflicts = vec![crate::metadata::Relationship::Direct(
            crate::metadata::Direct::named("Shiny"),
        )];
        let mut new_lib = mk("Lib", "GameData/Lib/l2.dll", &[]);
        new_lib.version = crate::version::ModVersion::new("2");
        let mut need = mk("Need", "GameData/Need/n.dll", &[]);
        need.depends = vec![crate::metadata::Relationship::Direct(
            crate::metadata::Direct {
                name: "Lib".into(),
                version: None,
                min_version: Some(crate::version::ModVersion::new("2")),
                max_version: None,
            },
        )];
        let index2 = Index::from_modules(vec![old_lib, new_lib, need], HashMap::new(), 0);
        let req = Request {
            install: vec![("Lib".into(), Some(crate::version::ModVersion::new("1")))],
            ..Request::default()
        };
        let p = plan(&inst, &index2, &req).expect("io").expect("plan");
        apply(
            &paths,
            &Config::default(),
            &inst,
            &p,
            &tx,
            &Cancel::default(),
        )
        .expect("apply");
        let mut m3 = m.clone();
        m3.conflicts.clear();
        m3.provides = vec!["Shiny".into()];
        m3.depends = vec!["Need".into()];
        let p = plan_manual(&inst, &index2, &m3, &none)
            .expect("io")
            .expect("the upgrade clears the conflict");
        assert!(
            p.changes
                .iter()
                .any(|c| matches!(c, Change::Upgrade { .. }))
        );
    }

    #[test]
    fn restores_mods_that_depend_on_a_manual_provider() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ksp = tmp.path().join("ksp");
        fake_ksp(&ksp);
        let paths = Paths::new(Some(&tmp.path().join("orbis"))).expect("paths");
        let inst = crate::instance::create(
            &paths,
            &CreateOptions {
                name: "t".into(),
                source: ksp,
                stock_only: true,
                with_saves: false,
                mode: CopyMode::Link,
            },
            &mut |_, _| {},
        )
        .expect("create");
        let mk = |id: &str, file: &str, deps: &[&str]| -> Module {
            let url = zip_with(&tmp.path().join(format!("{id}.zip")), &[file]);
            serde_json::from_value(serde_json::json!({
                "identifier": id, "version": "1", "ksp_version": "1.12", "download": url,
                "depends": deps.iter().map(|d| serde_json::json!({"name": d})).collect::<Vec<_>>(),
            }))
            .expect("module")
        };
        let index = Index::from_modules(
            vec![
                mk("X", "GameData/X/x.dll", &[]),
                mk("A", "GameData/A/a.dll", &["X"]),
                mk("B", "GameData/B/b.dll", &[]),
            ],
            HashMap::new(),
            0,
        );
        let zip = tmp.path().join("m.zip");
        zip_with(&zip, &["GameData/M/m.dll"]);
        let m = manual::add(
            &paths,
            &manual::AddOptions {
                path: zip,
                name: "M".into(),
                provides: vec!["X".into()],
                ..manual::AddOptions::default()
            },
        )
        .expect("add");
        let target = vec![
            history::SnapMod {
                identifier: "A".into(),
                version: crate::version::ModVersion::new("1"),
                source: ModSource::Ckan,
                auto: false,
            },
            history::SnapMod {
                identifier: m.identifier(),
                version: crate::version::ModVersion::new(m.version.clone()),
                source: ModSource::Manual,
                auto: false,
            },
        ];
        // Now: A with X from CKAN. Target: A with X provided by M.
        let (tx, _rx) = mpsc::channel();
        let req = Request {
            install: vec![("A".into(), None)],
            ..Request::default()
        };
        let p = plan(&inst, &index, &req).expect("io").expect("plan");
        apply(
            &paths,
            &Config::default(),
            &inst,
            &p,
            &tx,
            &Cancel::default(),
        )
        .expect("apply");
        let warnings = restore(
            &paths,
            &Config::default(),
            &inst,
            &index,
            &target,
            &tx,
            &Cancel::default(),
        )
        .expect("restore");
        assert!(warnings.is_empty(), "{warnings:?}");
        let ids = |inst: &crate::instance::Instance| -> Vec<String> {
            let mut ids: Vec<String> = inst
                .registry()
                .expect("registry")
                .mods
                .into_keys()
                .collect();
            ids.sort_unstable();
            ids
        };
        assert_eq!(ids(&inst), ["A", "manual:m"]);

        // From an empty instance, A can only resolve once M is in place.
        let empty = crate::instance::create(
            &paths,
            &CreateOptions {
                name: "empty".into(),
                source: tmp.path().join("ksp"),
                stock_only: true,
                with_saves: false,
                mode: CopyMode::Link,
            },
            &mut |_, _| {},
        )
        .expect("create");
        let warnings = restore(
            &paths,
            &Config::default(),
            &empty,
            &index,
            &target,
            &tx,
            &Cancel::default(),
        )
        .expect("restore into an empty instance");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(ids(&empty), ["A", "manual:m"]);

        // From {B}, where M conflicts with B: B has to go before M can come.
        let with_b = crate::instance::create(
            &paths,
            &CreateOptions {
                name: "with-b".into(),
                source: tmp.path().join("ksp"),
                stock_only: true,
                with_saves: false,
                mode: CopyMode::Link,
            },
            &mut |_, _| {},
        )
        .expect("create");
        let req = Request {
            install: vec![("B".into(), None)],
            ..Request::default()
        };
        let p = plan(&with_b, &index, &req).expect("io").expect("plan");
        apply(
            &paths,
            &Config::default(),
            &with_b,
            &p,
            &tx,
            &Cancel::default(),
        )
        .expect("apply");
        let mut m_conflicting = m.clone();
        m_conflicting.conflicts = vec!["B".into()];
        manual::save(&paths, &m_conflicting).expect("save");
        // A has since left CKAN: replacing X with M must keep it, since it
        // can't be reinstalled.
        let gone = crate::instance::create(
            &paths,
            &CreateOptions {
                name: "gone".into(),
                source: tmp.path().join("ksp"),
                stock_only: true,
                with_saves: false,
                mode: CopyMode::Link,
            },
            &mut |_, _| {},
        )
        .expect("create");
        let req = Request {
            install: vec![("A".into(), None)],
            ..Request::default()
        };
        let p = plan(&gone, &index, &req).expect("io").expect("plan");
        apply(
            &paths,
            &Config::default(),
            &gone,
            &p,
            &tx,
            &Cancel::default(),
        )
        .expect("apply");
        let without_a =
            Index::from_modules(vec![mk("X", "GameData/X/x.dll", &[])], HashMap::new(), 0);
        manual::save(&paths, &m).expect("save");
        let warnings = restore(
            &paths,
            &Config::default(),
            &gone,
            &without_a,
            &target,
            &tx,
            &Cancel::default(),
        )
        .expect("restore without A in CKAN");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(ids(&gone), ["A", "manual:m"]);
        assert!(gone.game_dir().join("GameData/A/a.dll").is_file());
        manual::save(&paths, &m_conflicting).expect("save");

        // A was a dependency of B; the target keeps A as a chosen mod but
        // drops B. A must survive B's removal even though A left CKAN.
        let chain = crate::instance::create(
            &paths,
            &CreateOptions {
                name: "chain".into(),
                source: tmp.path().join("ksp"),
                stock_only: true,
                with_saves: false,
                mode: CopyMode::Link,
            },
            &mut |_, _| {},
        )
        .expect("create");
        let with_b_on_a = Index::from_modules(
            vec![
                mk("X", "GameData/X/x.dll", &[]),
                mk("A", "GameData/A/a.dll", &["X"]),
                mk("B", "GameData/B/b2.dll", &["A"]),
            ],
            HashMap::new(),
            0,
        );
        let req = Request {
            install: vec![("B".into(), None)],
            ..Request::default()
        };
        let p = plan(&chain, &with_b_on_a, &req).expect("io").expect("plan");
        apply(
            &paths,
            &Config::default(),
            &chain,
            &p,
            &tx,
            &Cancel::default(),
        )
        .expect("apply");
        assert!(chain.registry().expect("registry").mods["A"].auto_installed);
        manual::save(&paths, &m).expect("save");
        let warnings = restore(
            &paths,
            &Config::default(),
            &chain,
            &without_a,
            &target,
            &tx,
            &Cancel::default(),
        )
        .expect("restore keeps A");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(ids(&chain), ["A", "manual:m"]);
        assert!(chain.game_dir().join("GameData/A/a.dll").is_file());
        assert!(!chain.registry().expect("registry").mods["A"].auto_installed);
        manual::save(&paths, &m_conflicting).expect("save");

        // Sol's round-6 case: A is chosen now and a dependency (of B) in the
        // target, and A left CKAN. Replacing X with M must not orphan A.
        let demote = crate::instance::create(
            &paths,
            &CreateOptions {
                name: "demote".into(),
                source: tmp.path().join("ksp"),
                stock_only: true,
                with_saves: false,
                mode: CopyMode::Link,
            },
            &mut |_, _| {},
        )
        .expect("create");
        let req = Request {
            install: vec![("A".into(), None)],
            ..Request::default()
        };
        let p = plan(&demote, &with_b_on_a, &req)
            .expect("io")
            .expect("plan");
        apply(
            &paths,
            &Config::default(),
            &demote,
            &p,
            &tx,
            &Cancel::default(),
        )
        .expect("apply");
        let b_only = Index::from_modules(
            vec![mk("B", "GameData/B/b2.dll", &["A"])],
            HashMap::new(),
            0,
        );
        let snap = |id: &str, source: ModSource, auto: bool| history::SnapMod {
            identifier: id.into(),
            version: crate::version::ModVersion::new(if source == ModSource::Manual {
                m.version.clone()
            } else {
                "1".into()
            }),
            source,
            auto,
        };
        let demoted_target = vec![
            snap("B", ModSource::Ckan, false),
            snap("A", ModSource::Ckan, true),
            snap(&m.identifier(), ModSource::Manual, false),
        ];
        manual::save(&paths, &m).expect("save");
        let warnings = restore(
            &paths,
            &Config::default(),
            &demote,
            &b_only,
            &demoted_target,
            &tx,
            &Cancel::default(),
        )
        .expect("restore keeps A while demoting it");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(ids(&demote), ["A", "B", "manual:m"]);
        assert!(demote.game_dir().join("GameData/A/a.dll").is_file());
        assert!(demote.registry().expect("registry").mods["A"].auto_installed);
        manual::save(&paths, &m_conflicting).expect("save");

        // No CKAN provider for X: only M can satisfy A.
        let no_x = Index::from_modules(
            vec![
                mk("A", "GameData/A/a.dll", &["X"]),
                mk("B", "GameData/B/b.dll", &[]),
            ],
            HashMap::new(),
            0,
        );
        let warnings = restore(
            &paths,
            &Config::default(),
            &with_b,
            &no_x,
            &target,
            &tx,
            &Cancel::default(),
        )
        .expect("restore over a conflicting mod");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(ids(&with_b), ["A", "manual:m"]);
    }

    #[test]
    fn adopts_hand_added_folders() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ksp = tmp.path().join("ksp");
        fake_ksp(&ksp);
        let paths = Paths::new(Some(&tmp.path().join("orbis"))).expect("paths");
        let inst = crate::instance::create(
            &paths,
            &CreateOptions {
                name: "t".into(),
                source: ksp,
                stock_only: true,
                with_saves: false,
                mode: CopyMode::Link,
            },
            &mut |_, _| {},
        )
        .expect("create");
        let gd = inst.game_dir().join("GameData");
        fs::create_dir_all(gd.join("HandMod/Plugins")).expect("mkdir");
        fs::write(gd.join("HandMod/Plugins/h.dll"), b"h").expect("write");
        fs::write(gd.join("ModuleManager.ConfigCache"), b"cache").expect("write");
        fs::write(gd.join("stray.cfg"), b"x").expect("write");
        let (folders, loose) = manual::unmanaged(&inst.game_dir(), &HashMap::new()).expect("scan");
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].folder, "HandMod");
        assert_eq!(loose, ["stray.cfg"]);

        let m = adopt(
            &paths,
            &inst,
            "HandMod",
            &manual::AddOptions {
                version: "0.9".into(),
                ..manual::AddOptions::default()
            },
        )
        .expect("adopt");
        assert_eq!(m.identifier(), "manual:handmod");
        let reg = inst.registry().expect("registry");
        assert_eq!(
            reg.mods["manual:handmod"].files,
            ["GameData/HandMod/Plugins/h.dll"]
        );
        let (folders, _) = manual::unmanaged(&inst.game_dir(), &reg.owners()).expect("scan");
        assert!(folders.is_empty());

        // Removing it removes the files; the library keeps a copy.
        let (tx, _rx) = mpsc::channel();
        let req = Request {
            remove: vec!["manual:handmod".into()],
            ..Request::default()
        };
        let index = Index::default();
        let p = plan(&inst, &index, &req).expect("io").expect("plan");
        apply(
            &paths,
            &Config::default(),
            &inst,
            &p,
            &tx,
            &Cancel::default(),
        )
        .expect("remove");
        assert!(!gd.join("HandMod").exists());
        assert!(m.plan(&paths).expect("library copy").len() > 1);
    }
}
