//! High-level operations on an instance, shared by the CLI and the GUI.

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc;

use anyhow::{Context, Result, bail};

use crate::backup;
use crate::cancel::Cancel;
use crate::config::Config;
use crate::download::{self, DownloadEvent};
use crate::history;
use crate::install;
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

/// Carries out a plan, all or nothing: downloads, then a check of every
/// install against the files that will be there, then removals and
/// installs. Removed files are set aside until everything succeeded; a
/// failure or cancel part-way puts the instance back exactly as it was.
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
    crate::running::refuse_if_running(inst)?;
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
    let stash = inst.dir.join(STASH);
    clear_committed_stash(inst, &stash)?;
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
    let original = inst.registry()?;

    // Work out every install before touching the instance: unpack it, plan
    // its files, and check them against what will be installed by then.
    let leaving: Vec<&String> = plan
        .changes
        .iter()
        .filter_map(|c| match c {
            Change::Remove { identifier, .. } => Some(identifier),
            Change::Upgrade { module, .. } => Some(&module.identifier),
            Change::Install { .. } => None,
        })
        .collect();
    let mut owners: HashMap<String, String> = original
        .mods
        .iter()
        .filter(|(id, _)| !leaving.contains(id))
        .flat_map(|(id, m)| m.files.iter().map(move |f| (install::fold(f), id.clone())))
        .collect();
    // Paths the leaving mods vacate: their files exactly as recorded, and
    // their folders (emptied and removed during the swap).
    let freed: std::collections::HashSet<&str> = original
        .mods
        .iter()
        .filter(|(id, _)| leaving.contains(id))
        .flat_map(|(_, m)| m.files.iter().chain(&m.dirs).map(String::as_str))
        .collect();
    let mut prepared = Vec::new();
    let mut problems = Vec::new();
    for c in &plan.changes {
        let (module, reason) = match c {
            Change::Install { module, reason } | Change::Upgrade { module, reason, .. } => {
                (module, reason)
            }
            Change::Remove { .. } => continue,
        };
        cancel.check()?;
        let id = &module.identifier;
        let _ = events.send(Event::Status(format!("Preparing {id} {}", module.version)));
        let (src, directives, source) = if let Some(mid) = id.strip_prefix(manual::PREFIX) {
            let m = manual::find(paths, mid)?;
            (
                Some(store::manual_source(paths, &m)?),
                m.install.clone(),
                ModSource::Manual,
            )
        } else if module.kind == Kind::Metapackage {
            (None, Vec::new(), ModSource::Ckan)
        } else {
            let archive = archives
                .get(id)
                .with_context(|| format!("{id} was not downloaded"))?;
            let src = store::ckan_source(paths, archive)
                .with_context(|| format!("unpack {id} {}", module.version))?;
            (Some(src), module.directives(), ModSource::Ckan)
        };
        let planned = match &src {
            Some(src) => install::plan(&directives, &src.entries()?)
                .with_context(|| format!("plan {id} {}", module.version))?,
            None => Vec::new(),
        };
        for f in planned.iter().filter(|f| !f.is_dir) {
            let key = install::fold(&f.dest);
            if let Some(owner) = owners.get(&key).filter(|o| *o != id) {
                problems.push(format!("{id}: {} is owned by {owner}", f.dest));
            } else if !freed.contains(f.dest.as_str()) && game.join(&f.dest).exists() {
                problems.push(format!(
                    "{id}: {} already exists and is not managed by orbis",
                    f.dest
                ));
            } else if let Some(link) = install::through_link(&game, &f.dest) {
                problems.push(format!(
                    "{id}: {} would be written through {link}, which links to the base game install",
                    f.dest
                ));
            }
            owners.insert(key, id.clone());
        }
        prepared.push((module, reason, src, planned, source));
    }
    if !problems.is_empty() {
        problems.truncate(20);
        bail!(
            "nothing was changed; these files are in the way:\n  {}",
            problems.join("\n  ")
        );
    }
    cancel.check()?;
    // The game may have been started while this downloaded.
    crate::running::refuse_if_running(inst)?;

    // Set the leaving mods' files aside rather than deleting them, so a
    // failure part-way puts everything back. Every move is recorded as it
    // happens.
    let mut reg = original.clone();
    let mut stashed: Vec<(String, Vec<String>)> = Vec::new();
    let mut vacated: Vec<String> = Vec::new();
    let mut added: Vec<install::InstalledFiles> = Vec::new();
    let mut auto_before: HashMap<String, bool> = HashMap::new();
    let result = (|| -> Result<()> {
        std::fs::create_dir_all(&stash).with_context(|| format!("create {}", stash.display()))?;
        // What the instance held before: if the apply dies before saving, the
        // registry still matches this, which is how the next apply knows.
        let before = serde_json::to_vec(&installed_files(&original)).context("serialize")?;
        paths::write_atomic(&stash.join(BEFORE), &before)?;
        for id in &leaving {
            let Some(old) = reg.mods.remove(*id) else {
                continue;
            };
            let _ = events.send(Event::Status(format!("Removing {id} {}", old.version())));
            auto_before.insert((*id).clone(), old.auto_installed);
            stashed.push(((*id).clone(), Vec::new()));
            let moved = &mut stashed.last_mut().expect("just pushed").1;
            set_aside(&game, &stash, id, &old.files, moved)?;
        }
        // Folders the leaving mods emptied go too, so a new version can put
        // a file where an old folder was.
        let mut dirs: Vec<&String> = leaving
            .iter()
            .filter_map(|id| original.mods.get(*id))
            .flat_map(|m| &m.dirs)
            .collect();
        dirs.sort_by(|a, b| {
            b.matches('/')
                .count()
                .cmp(&a.matches('/').count())
                .then_with(|| a.cmp(b))
        });
        dirs.dedup();
        for d in dirs {
            if install::through_link(&game, d).is_none()
                && std::fs::remove_dir(game.join(d)).is_ok()
            {
                vacated.push(d.clone());
            }
        }
        for (module, reason, src, planned, source) in &prepared {
            cancel.check()?;
            let id = &module.identifier;
            let _ = events.send(Event::Status(format!("Installing {id} {}", module.version)));
            let files = match src {
                Some(src) => install::install(&game, src, planned, &reg.owners(), false)
                    .with_context(|| format!("install {id} {}", module.version))?,
                None => install::InstalledFiles::default(),
            };
            added.push(files.clone());
            let auto = auto_before
                .get(id)
                .copied()
                .unwrap_or(matches!(reason, Reason::Dependency(_)));
            reg.mods.insert(
                id.clone(),
                InstalledMod {
                    source: *source,
                    auto_installed: auto,
                    installed_at: paths::now_secs(),
                    files: files.files,
                    dirs: files.dirs,
                    module: (*module).clone(),
                },
            );
        }
        inst.save_registry(&reg)
    })();
    match result {
        Ok(()) => {
            // Committed: the set-aside files can go, then the folders they
            // leave empty. A stash that can't be removed now is cleared on
            // the next change.
            if let Err(e) = remove_stash(&stash) {
                log::warn!("remove {}: {e:#}", stash.display());
            }
            for (id, _) in &stashed {
                if let Some(old) = original.mods.get(id) {
                    // Folders only: a new version may reuse the old paths.
                    let dirs = install::InstalledFiles {
                        files: Vec::new(),
                        dirs: old.dirs.clone(),
                    };
                    install::uninstall(&game, &dirs);
                }
            }
            let _ = events.send(Event::Status("Done".into()));
            Ok(())
        }
        Err(e) => {
            for files in added.iter().rev() {
                install::uninstall(&game, files);
            }
            for d in vacated.iter().rev() {
                let _ = std::fs::create_dir_all(game.join(d));
            }
            let mut lost = Vec::new();
            for (id, moved) in &stashed {
                if let Err(err) = put_back(&game, &stash, id, moved) {
                    lost.push(format!("{id}: {err:#}"));
                }
            }
            if lost.is_empty() {
                if let Err(err) = remove_stash(&stash) {
                    log::warn!("remove {}: {err:#}", stash.display());
                }
                Err(e.context("nothing was changed"))
            } else {
                Err(e.context(format!(
                    "couldn't put back every file; the rest are in {}: {}",
                    stash.display(),
                    lost.join("; ")
                )))
            }
        }
    }
}

/// Moves a mod's files from the game into `stash/<id>/`, keeping their
/// paths, and records each one in `moved` as soon as it has moved, so a
/// failure part-way can put back exactly what moved. Missing files are
/// skipped.
fn set_aside(
    game: &std::path::Path,
    stash: &std::path::Path,
    id: &str,
    files: &[String],
    moved: &mut Vec<String>,
) -> Result<()> {
    for f in files {
        if install::through_link(game, f).is_some() {
            continue;
        }
        let from = game.join(f);
        if !from.exists() {
            continue;
        }
        let to = stash.join(paths::slug(id)).join(f);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::rename(&from, &to).with_context(|| format!("set aside {f}"))?;
        moved.push(f.clone());
    }
    Ok(())
}

/// Where an apply sets the leaving mods' files aside, inside the instance.
const STASH: &str = ".apply-stash";
/// Inside the stash: each installed mod's version and files before the
/// apply moved anything.
const BEFORE: &str = "installed-before.json";

/// Each installed mod's version and files: what decides whether an apply
/// was saved. Flags such as "installed as a dependency" are left out, since
/// they change without any file moving.
fn installed_files(reg: &Registry) -> BTreeMap<String, (String, Vec<String>)> {
    reg.mods
        .iter()
        .map(|(id, m)| (id.clone(), (m.version().to_string(), m.files.clone())))
        .collect()
}

/// Whether a folder holds any file in its subfolders. Set-aside files
/// always sit in `<stash>/<mod>/...`; files at the top are the stash's own
/// marker (or a half-written one) and don't count.
fn holds_files(dir: &std::path::Path, top: bool) -> Result<bool> {
    for e in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let e = e.with_context(|| format!("read {}", dir.display()))?;
        let ft = e
            .file_type()
            .with_context(|| format!("stat {}", e.path().display()))?;
        if ft.is_dir() {
            if holds_files(&e.path(), false)? {
                return Ok(true);
            }
        } else if !top {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Removes a stash: the set-aside files first, the marker last, so a
/// removal that fails part-way still reads correctly next time.
fn remove_stash(stash: &std::path::Path) -> Result<()> {
    for e in std::fs::read_dir(stash).with_context(|| format!("read {}", stash.display()))? {
        let e = e.with_context(|| format!("read {}", stash.display()))?;
        if e.file_name() != BEFORE {
            let p = e.path();
            if p.is_dir() {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            }
            .with_context(|| format!("remove {}", p.display()))?;
        }
    }
    std::fs::remove_dir_all(stash).with_context(|| format!("remove {}", stash.display()))
}

/// Deals with a stash an earlier apply left behind. It holds files only if
/// that apply set some aside and never finished: when the instance still
/// has exactly the mods and files it had before, the apply was interrupted
/// before saving, and the user is told how to recover. Otherwise the change
/// was saved (anything since, such as marking a dependency, doesn't matter)
/// and the stash is leftover old files, or empty: it is deleted.
fn clear_committed_stash(inst: &Instance, stash: &std::path::Path) -> Result<()> {
    if !stash.exists() {
        return Ok(());
    }
    let has_files = holds_files(stash, true)?;
    let before = std::fs::read(stash.join(BEFORE))
        .ok()
        .and_then(|b| serde_json::from_slice::<BTreeMap<String, (String, Vec<String>)>>(&b).ok());
    let now = installed_files(&inst.registry()?);
    // No record (a stash from before 0.13.2, or one written moments before
    // a crash) can't prove the change was saved: keep the files.
    let interrupted = has_files && before.is_none_or(|b| b == now);
    if !interrupted {
        return remove_stash(stash)
            .with_context(|| format!("clear the leftover {}", stash.display()));
    }
    bail!(
        "an earlier change to {} was interrupted before it was saved. The mod list still \
         describes the mods it was replacing, whose files are in {} (one folder per mod, \
         paths kept). Move each folder's contents back into the instance's game/ folder and \
         delete {}; files the interrupted change had installed may be left in GameData, where \
         the Manual mods tab lists them",
        inst.config.name,
        stash.display(),
        stash.display()
    )
}

/// Moves files set aside by [`set_aside`] back into the game.
fn put_back(
    game: &std::path::Path,
    stash: &std::path::Path,
    id: &str,
    files: &[String],
) -> Result<()> {
    for f in files {
        let to = game.join(f);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::rename(stash.join(paths::slug(id)).join(f), &to)
            .with_context(|| format!("put back {f}"))?;
    }
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
    crate::running::refuse_if_running(inst)?;
    // An interrupted apply must be sorted out before the registry changes.
    clear_committed_stash(inst, &inst.dir.join(STASH))?;
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
    fn a_failed_change_leaves_the_instance_as_it_was() {
        use std::os::unix::fs::PermissionsExt;
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
        let mk = |id: &str, v: &str, files: &[&str]| -> Module {
            let url = zip_with(&tmp.path().join(format!("{id}-{v}.zip")), files);
            serde_json::from_value(serde_json::json!({
                "identifier": id, "version": v, "ksp_version": "1.12", "download": url,
            }))
            .expect("module")
        };
        let index = Index::from_modules(
            vec![
                mk("App", "1.0", &["GameData/App/app.dll"]),
                mk(
                    "App",
                    "2.0",
                    &["GameData/App/app.dll", "GameData/App/stray.cfg"],
                ),
                mk("Good", "1.0", &["GameData/Good/g.dll"]),
                mk("Locked", "1.0", &["GameData/Locked/l.dll"]),
            ],
            HashMap::new(),
            0,
        );
        let (tx, _rx) = mpsc::channel();
        let run = |req: Request| {
            let p = plan(&inst, &index, &req).expect("io").expect("resolves");
            apply(&paths, &config, &inst, &p, &tx, &Cancel::default())
        };
        run(Request {
            install: vec![("App".into(), Some(crate::version::ModVersion::new("1.0")))],
            ..Request::default()
        })
        .expect("install 1.0");
        let g = inst.game_dir();
        let unchanged = |what: &str| {
            let reg = inst.registry().expect("registry");
            assert_eq!(reg.mods["App"].module.version.as_str(), "1.0", "{what}");
            assert!(g.join("GameData/App/app.dll").is_file(), "{what}");
            assert!(!reg.mods.contains_key("Good"), "{what}");
            assert!(!g.join("GameData/Good").exists(), "{what}");
            assert!(!inst.dir.join(".apply-stash").exists(), "{what}");
        };

        // A stray file in the new version's way: refused before anything moves.
        fs::write(g.join("GameData/App/stray.cfg"), "mine").expect("write");
        let err = run(Request {
            upgrade: vec!["App".into()],
            install: vec![("Good".into(), None)],
            ..Request::default()
        })
        .expect_err("conflict");
        assert!(
            format!("{err:#}").contains("nothing was changed"),
            "{err:#}"
        );
        unchanged("conflict");
        assert_eq!(
            fs::read_to_string(g.join("GameData/App/stray.cfg")).expect("kept"),
            "mine"
        );
        fs::remove_file(g.join("GameData/App/stray.cfg")).expect("rm");

        // A failure while installing: the upgrade already done is undone.
        fs::create_dir_all(g.join("GameData/Locked")).expect("mkdir");
        fs::set_permissions(g.join("GameData/Locked"), fs::Permissions::from_mode(0o555))
            .expect("chmod");
        let err = run(Request {
            upgrade: vec!["App".into()],
            install: vec![("Good".into(), None), ("Locked".into(), None)],
            ..Request::default()
        })
        .expect_err("locked folder");
        fs::set_permissions(g.join("GameData/Locked"), fs::Permissions::from_mode(0o755))
            .expect("chmod back");
        assert!(
            format!("{err:#}").contains("nothing was changed"),
            "{err:#}"
        );
        unchanged("install failure");
        assert!(!g.join("GameData/App/stray.cfg").exists());

        // While the game runs, nothing changes.
        crate::running::mark(&inst.dir, std::process::id(), "direct").expect("mark");
        let err = run(Request {
            install: vec![("Good".into(), None)],
            ..Request::default()
        })
        .expect_err("running");
        assert!(format!("{err:#}").contains("is running"), "{err:#}");
        assert!(crate::instance::delete(&inst).is_err());
        crate::running::clear(&inst.dir, std::process::id());
        unchanged("running");
    }

    #[test]
    fn apply_handles_partial_set_aside_folder_swaps_and_leftovers() {
        use std::os::unix::fs::PermissionsExt;
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
        let mk = |id: &str, v: &str, files: &[&str]| -> Module {
            let url = zip_with(&tmp.path().join(format!("{id}-{v}.zip")), files);
            serde_json::from_value(serde_json::json!({
                "identifier": id, "version": v, "ksp_version": "1.12", "download": url,
            }))
            .expect("module")
        };
        let index = Index::from_modules(
            vec![
                mk(
                    "Two",
                    "1.0",
                    &["GameData/Two/a/one.dll", "GameData/Two/b/two.dll"],
                ),
                mk("Dir", "1.0", &["GameData/Dir/cache/item.cfg"]),
                mk("Dir", "2.0", &["GameData/Dir/cache"]),
                mk(
                    "Dup",
                    "1.0",
                    &["GameData/Dup/Readme.txt", "GameData/Dup/README.txt"],
                ),
            ],
            HashMap::new(),
            0,
        );
        let (tx, _rx) = mpsc::channel();
        let run = |req: Request| {
            let p = plan(&inst, &index, &req).expect("io").expect("resolves");
            apply(&paths, &config, &inst, &p, &tx, &Cancel::default())
        };
        let g = inst.game_dir();
        let stash = inst.dir.join(STASH);

        // Files differing only in case within one mod install together.
        run(Request {
            install: vec![("Dup".into(), None), ("Two".into(), None)],
            ..Request::default()
        })
        .expect("install");

        // Setting aside fails on the second file: the first comes back.
        fs::set_permissions(g.join("GameData/Two/b"), fs::Permissions::from_mode(0o555))
            .expect("chmod");
        let err = run(Request {
            remove: vec!["Two".into()],
            ..Request::default()
        })
        .expect_err("can't move two.dll");
        fs::set_permissions(g.join("GameData/Two/b"), fs::Permissions::from_mode(0o755))
            .expect("chmod back");
        assert!(
            format!("{err:#}").contains("nothing was changed"),
            "{err:#}"
        );
        assert!(
            g.join("GameData/Two/a/one.dll").is_file(),
            "the moved file is back"
        );
        assert!(g.join("GameData/Two/b/two.dll").is_file());
        assert!(inst.registry().expect("registry").mods.contains_key("Two"));
        assert!(!stash.exists());

        // An upgrade may put a file where the old version had a folder.
        run(Request {
            install: vec![("Dir".into(), Some(crate::version::ModVersion::new("1.0")))],
            ..Request::default()
        })
        .expect("Dir 1.0");
        run(Request {
            upgrade: vec!["Dir".into()],
            ..Request::default()
        })
        .expect("Dir 2.0");
        assert!(g.join("GameData/Dir/cache").is_file());

        // A stash left by a change that was saved is cleared on the next
        // one, even after the registry changed without files moving.
        let before_dup = serde_json::to_vec(&{
            let mut r = inst.registry().expect("registry");
            r.mods.remove("Dir");
            installed_files(&r)
        })
        .expect("json");
        fs::create_dir_all(stash.join("old/GameData")).expect("mkdir");
        fs::write(stash.join("old/GameData/x.dll"), "old").expect("write");
        fs::write(stash.join(BEFORE), &before_dup).expect("write");
        let mut reg = inst.registry().expect("registry");
        reg.mods.get_mut("Dup").expect("Dup").auto_installed = true;
        inst.save_registry(&reg).expect("mark as dependency");
        run(Request {
            remove: vec!["Dup".into()],
            ..Request::default()
        })
        .expect("leftover from a finished change is cleared");
        assert!(!stash.exists());

        // An empty stash (a rollback whose cleanup failed) is just cleared.
        fs::create_dir_all(stash.join("dup/GameData/Dup")).expect("mkdir");
        let now = serde_json::to_vec(&installed_files(&inst.registry().expect("r"))).expect("json");
        fs::write(stash.join(BEFORE), &now).expect("write");
        run(Request {
            install: vec![("Dup".into(), None)],
            ..Request::default()
        })
        .expect("empty leftover is cleared");
        assert!(!stash.exists());

        // One left by an interrupted change stops the next change, flag
        // edits or not.
        let now = serde_json::to_vec(&installed_files(&inst.registry().expect("r"))).expect("json");
        fs::create_dir_all(stash.join("two/GameData/Two")).expect("mkdir");
        fs::write(stash.join("two/GameData/Two/old.dll"), "old").expect("write");
        fs::write(stash.join(BEFORE), &now).expect("write");
        let mut reg = inst.registry().expect("registry");
        reg.mods.get_mut("Two").expect("Two").auto_installed = true;
        inst.save_registry(&reg).expect("flag edit");
        let err = run(Request {
            remove: vec!["Two".into()],
            ..Request::default()
        })
        .expect_err("interrupted change");
        assert!(format!("{err:#}").contains("interrupted"), "{err:#}");
        assert!(g.join("GameData/Two/a/one.dll").is_file());
        assert!(
            stash.join("two/GameData/Two/old.dll").is_file(),
            "kept for recovery"
        );

        // A stash from before the record existed is never taken as finished.
        fs::remove_file(stash.join(BEFORE)).expect("rm marker");
        fs::write(stash.join(".installed-before.json.tmp"), "half").expect("write");
        let err = run(Request {
            remove: vec!["Two".into()],
            ..Request::default()
        })
        .expect_err("no record: interrupted");
        assert!(format!("{err:#}").contains("interrupted"), "{err:#}");
        assert!(stash.join("two/GameData/Two/old.dll").is_file());
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
