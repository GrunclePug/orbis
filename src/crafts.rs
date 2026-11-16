//! The craft library: every craft from every instance in one place, shared
//! with each instance that has the parts to build it.
//!
//! Layout of `<data>/crafts/`:
//!
//! ```text
//! library.json        every craft: id, name, building, revisions, deletion
//! blobs/<sha256>      the bytes of each revision
//! thumbs/<sha256>.png the thumbnail KSP drew for a revision, when found
//! ```
//!
//! Each instance keeps `crafts.json`: where each library craft sits in its
//! saves (`saves/<save>/Ships/<VAB|SPH>/<path>`) and the bytes orbis last saw
//! there. Syncing compares every location with that baseline:
//!
//! * unchanged: orbis may replace it with the library's newest revision, or
//!   remove it when that revision needs parts the instance lacks;
//! * changed: the file is a new revision (or, when the library also changed
//!   since, a separate craft, so neither edit is lost);
//! * gone: the craft was deleted in KSP, and is deleted everywhere (to the
//!   trash) unless it changed elsewhere meanwhile; when several go missing
//!   from one save at once, they wait for confirmation instead.
//!
//! New files become new craft, or locations of the craft whose name and
//! bytes they match. Running instances are left alone until they stop.
//! Identity is an id, never the name: saving under a new name in KSP makes a
//! new craft.

pub mod parts;

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::download::hex;
use crate::instance::{self, Instance};
use crate::paths::{self, Paths};
use crate::running;
use parts::PartIndex;

/// Missing craft in one save beyond this many wait for confirmation instead
/// of counting as deletions.
const MAX_SILENT_DELETES: usize = 3;

/// The editor a craft belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Facility {
    #[serde(rename = "VAB")]
    Vab,
    #[serde(rename = "SPH")]
    Sph,
}

impl Facility {
    pub const ALL: [Facility; 2] = [Facility::Vab, Facility::Sph];

    /// The folder name under `Ships/`, also KSP's name for it.
    pub fn dir(self) -> &'static str {
        match self {
            Facility::Vab => "VAB",
            Facility::Sph => "SPH",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "VAB" => Some(Facility::Vab),
            "SPH" => Some(Facility::Sph),
            _ => None,
        }
    }
}

/// One saved version of a craft.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    /// SHA-256 of the file.
    pub hash: String,
    /// When orbis collected it (seconds since the epoch).
    pub saved: u64,
    /// Where it came from: `<instance>/<save>`, `upload` or `import`.
    pub from: String,
    /// Part names, in KSP's form.
    pub parts: Vec<String>,
}

/// When and where a craft was deleted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deleted {
    pub at: u64,
    pub from: String,
}

/// A craft in the library.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Craft {
    pub id: String,
    /// Its name and file name in every instance (unique per building among
    /// live craft, ignoring case).
    pub name: String,
    pub facility: Facility,
    pub created: u64,
    /// Oldest first; never empty.
    pub revisions: Vec<Revision>,
    /// Set while the craft is in the trash.
    #[serde(default)]
    pub deleted: Option<Deleted>,
}

impl Craft {
    /// The newest revision.
    pub fn head(&self) -> &Revision {
        self.revisions
            .last()
            .expect("a craft always has at least one revision")
    }

    pub fn live(&self) -> bool {
        self.deleted.is_none()
    }

    fn has(&self, hash: &str) -> bool {
        self.revisions.iter().any(|r| r.hash == hash)
    }
}

/// What's left of a craft after the trash is emptied: enough to recognize
/// leftover copies of it, so they never come back as new craft.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Purged {
    pub id: String,
    pub name: String,
    pub facility: Facility,
    pub hashes: Vec<String>,
}

/// Every craft orbis knows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Library {
    pub next_id: u64,
    pub crafts: Vec<Craft>,
    #[serde(default)]
    pub purged: Vec<Purged>,
}

fn root(paths: &Paths) -> PathBuf {
    paths.data.join("crafts")
}

fn blob_path(paths: &Paths, hash: &str) -> PathBuf {
    root(paths).join("blobs").join(hash)
}

/// Where the thumbnail of a revision is kept (it may not exist).
pub fn thumb_path(paths: &Paths, hash: &str) -> PathBuf {
    root(paths).join("thumbs").join(format!("{hash}.png"))
}

fn sha(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// The bytes of a revision.
pub fn read_blob(paths: &Paths, hash: &str) -> Result<Vec<u8>> {
    let p = blob_path(paths, hash);
    fs::read(&p).with_context(|| format!("read {}", p.display()))
}

fn store_blob(paths: &Paths, bytes: &[u8]) -> Result<String> {
    let hash = sha(bytes);
    let p = blob_path(paths, &hash);
    if !p.is_file() {
        paths::write_atomic(&p, bytes)?;
    }
    Ok(hash)
}

/// Holds the library lock; every change to the library takes it, so the
/// GUI, the CLI and the Steam wrapper never sync at the same time.
pub struct Lock {
    _file: File,
}

pub fn lock(paths: &Paths) -> Result<Lock> {
    let dir = root(paths);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let p = dir.join(".lock");
    let f = File::create(&p).with_context(|| format!("open {}", p.display()))?;
    f.lock().with_context(|| format!("lock {}", p.display()))?;
    Ok(Lock { _file: f })
}

impl Library {
    /// Loads the library (empty when it doesn't exist yet).
    pub fn load(paths: &Paths) -> Result<Self> {
        let p = root(paths).join("library.json");
        match fs::read_to_string(&p) {
            Ok(t) => serde_json::from_str(&t).with_context(|| format!("parse {}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", p.display())),
        }
    }

    fn save(&self, paths: &Paths) -> Result<()> {
        let json = serde_json::to_vec_pretty(self).context("serialize craft library")?;
        paths::write_atomic(&root(paths).join("library.json"), &json)
    }

    pub fn get(&self, id: &str) -> Option<&Craft> {
        self.crafts.iter().find(|c| c.id == id)
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.crafts.iter().position(|c| c.id == id)
    }

    /// The live craft with this name in this building.
    fn live_named(&self, fac: Facility, name: &str) -> Option<usize> {
        self.crafts
            .iter()
            .position(|c| c.live() && c.facility == fac && c.name.eq_ignore_ascii_case(name))
    }

    /// Whether these bytes, under this name, are a copy of a deleted craft
    /// (in the trash, or purged from it). Returns its name.
    fn deleted_copy(&self, fac: Facility, name: &str, hash: &str) -> Option<String> {
        let same = |f: Facility, n: &str| f == fac && n.eq_ignore_ascii_case(name);
        self.crafts
            .iter()
            .find(|c| !c.live() && same(c.facility, &c.name) && c.has(hash))
            .map(|c| c.name.clone())
            .or_else(|| {
                self.purged
                    .iter()
                    .find(|p| same(p.facility, &p.name) && p.hashes.iter().any(|h| h == hash))
                    .map(|p| p.name.clone())
            })
    }

    /// The craft a user means: an id, or a name (live craft first).
    pub fn find(&self, key: &str) -> Result<&Craft> {
        self.get(key)
            .or_else(|| {
                self.crafts
                    .iter()
                    .find(|c| c.live() && c.name.eq_ignore_ascii_case(key))
            })
            .or_else(|| {
                self.crafts
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(key))
            })
            .with_context(|| format!("no craft {key:?} in the library"))
    }

    /// `base`, or `base (<hint>)`, `base (<hint> 2)`, ... whichever no live
    /// craft has and `free` accepts.
    fn unique_name(
        &self,
        fac: Facility,
        base: &str,
        hint: &str,
        free: &dyn Fn(&str) -> bool,
    ) -> String {
        if self.live_named(fac, base).is_none() && free(base) {
            return base.to_owned();
        }
        for n in 1.. {
            let name = match (hint.is_empty(), n) {
                (false, 1) => format!("{base} ({hint})"),
                (false, _) => format!("{base} ({hint} {n})"),
                (true, _) => format!("{base} ({})", n + 1),
            };
            if self.live_named(fac, &name).is_none() && free(&name) {
                return name;
            }
        }
        unreachable!("the loop above always returns")
    }

    /// Adds a new craft named `name` (made unique with `hint` and `free`),
    /// rewriting the file's ship name to match. Returns its index and stored
    /// bytes.
    #[expect(clippy::too_many_arguments, reason = "one call shape, all needed")]
    fn add(
        &mut self,
        paths: &Paths,
        fac: Facility,
        name: &str,
        hint: &str,
        free: &dyn Fn(&str) -> bool,
        bytes: &[u8],
        from: &str,
    ) -> Result<(usize, Vec<u8>)> {
        let name = self.unique_name(fac, name, hint, free);
        let bytes = if parts::read_craft(&String::from_utf8_lossy(bytes)).ship == name {
            bytes.to_vec()
        } else {
            set_ship_name(bytes, &name)
        };
        let hash = store_blob(paths, &bytes)?;
        let now = paths::now_secs();
        self.next_id += 1;
        self.crafts.push(Craft {
            id: format!("c{}", self.next_id),
            name,
            facility: fac,
            created: now,
            revisions: vec![revision(&bytes, hash, from)],
            deleted: None,
        });
        Ok((self.crafts.len() - 1, bytes))
    }

    /// Deletes stored files no revision refers to any more.
    fn prune(&mut self, paths: &Paths) -> Result<()> {
        let used: BTreeSet<&str> = self
            .crafts
            .iter()
            .flat_map(|c| c.revisions.iter().map(|r| r.hash.as_str()))
            .collect();
        for (dir, ext) in [("blobs", ""), ("thumbs", ".png")] {
            let Ok(entries) = fs::read_dir(root(paths).join(dir)) else {
                continue;
            };
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let Some(hash) = name.strip_suffix(ext) else {
                    continue;
                };
                if !hash.starts_with('.') && !used.contains(hash) {
                    let _ = fs::remove_file(e.path());
                }
            }
        }
        Ok(())
    }
}

fn revision(bytes: &[u8], hash: String, from: &str) -> Revision {
    Revision {
        hash,
        saved: paths::now_secs(),
        from: from.to_owned(),
        parts: parts::read_craft(&String::from_utf8_lossy(bytes))
            .parts
            .into_iter()
            .collect(),
    }
}

/// Rewrites the first top-level `ship =` line, keeping its line ending.
fn set_ship_name(bytes: &[u8], name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + name.len());
    let mut done = false;
    for line in bytes.split_inclusive(|&b| b == b'\n') {
        let is_ship = !done
            && line.starts_with(b"ship")
            && line[4..]
                .iter()
                .find(|b| !b.is_ascii_whitespace())
                .is_some_and(|&b| b == b'=');
        if is_ship {
            let ending: &[u8] = if line.ends_with(b"\r\n") {
                b"\r\n"
            } else if line.ends_with(b"\n") {
                b"\n"
            } else {
                b""
            };
            out.extend_from_slice(format!("ship = {name}").as_bytes());
            out.extend_from_slice(ending);
            done = true;
        } else {
            out.extend_from_slice(line);
        }
    }
    if !done {
        let mut with = format!("ship = {name}\n").into_bytes();
        with.extend_from_slice(&out);
        return with;
    }
    out
}

/// Turns a name into something safe as a file name in KSP.
fn file_safe(name: &str) -> String {
    let s: String = name
        .trim()
        .chars()
        .map(|c| {
            if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    if s.is_empty() { "Craft".into() } else { s }
}

// ---- per-instance state -----------------------------------------------------

/// Where a library craft sits in an instance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub save: String,
    pub facility: Facility,
    /// Path under `Ships/<facility>/`, with `/` separators.
    pub path: String,
    pub id: String,
    /// SHA-256 of the file when orbis last wrote or read it.
    pub hash: String,
}

impl Location {
    fn is_in(&self, save: &str, fac: Facility) -> bool {
        self.save == save && self.facility == fac
    }
}

/// An instance's `crafts.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceState {
    #[serde(default)]
    pub locations: Vec<Location>,
    /// Locations whose file went missing in a way that needs confirming.
    #[serde(default)]
    pub pending: Vec<Location>,
}

fn state_path(instance_dir: &Path) -> PathBuf {
    instance_dir.join("crafts.json")
}

/// Reads an instance's craft state (empty when it has none yet).
pub fn load_state(instance_dir: &Path) -> Result<InstanceState> {
    let p = state_path(instance_dir);
    match fs::read_to_string(&p) {
        Ok(t) => serde_json::from_str(&t).with_context(|| format!("parse {}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(InstanceState::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", p.display())),
    }
}

fn save_state(instance_dir: &Path, state: &InstanceState) -> Result<()> {
    let json = serde_json::to_vec_pretty(state).context("serialize craft state")?;
    paths::write_atomic(&state_path(instance_dir), &json)
}

/// Forgets what orbis knew about these saves' craft, so files that changed
/// or vanished because the saves were replaced (a backup restore) are read
/// afresh instead of counting as edits or deletions.
pub fn forget_saves(instance_dir: &Path, saves: &[String]) -> Result<()> {
    let mut state = load_state(instance_dir)?;
    let before = (state.locations.len(), state.pending.len());
    state.locations.retain(|l| !saves.contains(&l.save));
    state.pending.retain(|l| !saves.contains(&l.save));
    if before != (state.locations.len(), state.pending.len()) {
        save_state(instance_dir, &state)?;
    }
    Ok(())
}

/// The part index from the last sync, without rebuilding it.
pub fn cached_index(inst: &Instance) -> Option<PartIndex> {
    let text = fs::read_to_string(inst.dir.join("parts.json")).ok()?;
    serde_json::from_str(&text).ok()
}

// ---- scanning ---------------------------------------------------------------

/// Craft files KSP or mods make on their own, never shared.
fn ignored_file(name: &str) -> bool {
    name.eq_ignore_ascii_case("Auto-Saved Ship.craft")
}

fn ignored_dir(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with("_backup") || name.starts_with('.')
}

struct Found {
    path: String,
    hash: String,
    bytes: Vec<u8>,
}

/// Every craft file under `dir`; an error when anything can't be read, so a
/// partial view never looks like deleted craft.
fn scan_dir(dir: &Path) -> Result<Vec<Found>> {
    let mut out = Vec::new();
    let mut stack = vec![String::new()];
    while let Some(rel) = stack.pop() {
        let here = if rel.is_empty() {
            dir.to_path_buf()
        } else {
            dir.join(&rel)
        };
        for e in fs::read_dir(&here).with_context(|| format!("read {}", here.display()))? {
            let e = e.with_context(|| format!("read {}", here.display()))?;
            let name = e.file_name().to_string_lossy().into_owned();
            let child = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let ft = e.file_type().with_context(|| format!("stat {child}"))?;
            if ft.is_dir() {
                if !ignored_dir(&name) {
                    stack.push(child);
                }
            } else if ft.is_file()
                && name.to_ascii_lowercase().ends_with(".craft")
                && !ignored_file(&name)
            {
                let bytes = fs::read(e.path()).with_context(|| format!("read {child}"))?;
                out.push(Found {
                    path: child,
                    hash: sha(&bytes),
                    bytes,
                });
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// The saves of a game directory: folders with a `persistent.sfs`, apart
/// from the stock scenarios and training.
fn saves(game: &Path) -> Result<Vec<String>> {
    let dir = game.join("saves");
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
    };
    let mut out = Vec::new();
    for e in entries {
        let e = e.with_context(|| format!("read {}", dir.display()))?;
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.')
            || name.eq_ignore_ascii_case("scenarios")
            || name.eq_ignore_ascii_case("training")
        {
            continue;
        }
        if e.path().join("persistent.sfs").is_file() {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

fn stem(path: &str) -> &str {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.len()
        .checked_sub(".craft".len())
        .and_then(|n| file.get(..n))
        .filter(|_| file.to_ascii_lowercase().ends_with(".craft"))
        .unwrap_or(file)
}

fn with_stem(path: &str, name: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{}.craft", file_safe(name)),
        None => format!("{}.craft", file_safe(name)),
    }
}

/// The thumbnail KSP keeps for a craft in a save: `thumbs/<save>_<VAB|SPH>_<name>.png`.
fn game_thumb(game: &Path, save: &str, fac: Facility, name: &str) -> PathBuf {
    game.join("thumbs")
        .join(format!("{save}_{}_{name}.png", fac.dir()))
}

/// Keeps KSP's thumbnail for a revision when it is at least as new as the craft.
fn grab_thumb(paths: &Paths, thumb: &Path, craft_file: &Path, hash: &str) {
    let modified = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
    let (Some(t), Some(c)) = (modified(thumb), modified(craft_file)) else {
        return;
    };
    if t + std::time::Duration::from_secs(60) < c {
        return;
    }
    let dest = thumb_path(paths, hash);
    if !dest.exists()
        && let Ok(bytes) = fs::read(thumb)
        && let Err(e) = paths::write_atomic(&dest, &bytes)
    {
        log::warn!("keep thumbnail: {e:#}");
    }
}

// ---- syncing ----------------------------------------------------------------

/// What a sync did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// New craft in the library.
    pub added: usize,
    /// New revisions of existing craft.
    pub revised: usize,
    /// Copies written into instances (new or updated).
    pub deployed: usize,
    /// Copies removed because the instance lacks parts, or the craft was deleted.
    pub withdrawn: usize,
    /// Things worth telling the user.
    pub notes: Vec<String>,
    /// Instances skipped because their game is running.
    pub running: Vec<String>,
}

impl Report {
    /// A one-line summary, empty when nothing happened.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        for (n, what) in [
            (self.added, "new"),
            (self.revised, "updated"),
            (self.deployed, "copied to instances"),
            (self.withdrawn, "removed from instances"),
        ] {
            if n > 0 {
                parts.push(format!("{n} {what}"));
            }
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("Crafts: {}", parts.join(", "))
        }
    }
}

struct Work {
    inst: Instance,
    state: InstanceState,
    index: PartIndex,
    saves: Vec<String>,
}

/// Collects new and changed craft from every stopped instance into the
/// library, then brings each stopped instance's saves up to date.
pub fn sync(paths: &Paths) -> Result<Report> {
    let _lock = lock(paths)?;
    let mut lib = Library::load(paths)?;
    let mut report = Report::default();
    let mut work = Vec::new();
    for inst in instance::list(paths)? {
        if running::is_running(&inst.dir) {
            report.running.push(inst.config.name.clone());
            continue;
        }
        let prepared = (|| -> Result<Work> {
            Ok(Work {
                index: parts::index(&inst.game_dir(), &inst.dir.join("parts.json"))?,
                state: load_state(&inst.dir)?,
                saves: saves(&inst.game_dir())?,
                inst: inst.clone(),
            })
        })();
        match prepared {
            Ok(w) => work.push(w),
            Err(e) => report
                .notes
                .push(format!("Skipped {}: {e:#}", inst.config.name)),
        }
    }
    for w in &mut work {
        collect(paths, &mut lib, w, &mut report)?;
    }
    lib.save(paths)?;
    for w in &mut work {
        deploy(paths, &lib, w, &mut report)?;
        save_state(&w.inst.dir, &w.state)?;
    }
    lib.prune(paths)?;
    lib.save(paths)?;
    Ok(report)
}

/// Reads one instance's saves into the library.
fn collect(paths: &Paths, lib: &mut Library, w: &mut Work, report: &mut Report) -> Result<()> {
    let game = w.inst.game_dir();
    let existing: BTreeSet<&String> = w.saves.iter().collect();
    // A save that is gone took its craft with it; that's not a deletion.
    w.state.locations.retain(|l| existing.contains(&l.save));
    w.state.pending.retain(|l| existing.contains(&l.save));
    for save in w.saves.clone() {
        let label = format!("{}/{save}", w.inst.config.name);
        let mut missing = Vec::new();
        for fac in Facility::ALL {
            let dir = game.join("saves").join(&save).join("Ships").join(fac.dir());
            if !dir.is_dir() {
                continue;
            }
            let found = match scan_dir(&dir) {
                Ok(f) => f,
                Err(e) => {
                    report
                        .notes
                        .push(format!("Couldn't read {label} {}: {e:#}", fac.dir()));
                    continue;
                }
            };
            missing.extend(collect_dir(
                paths,
                lib,
                w,
                report,
                &CollectCtx {
                    game: &game,
                    dir: &dir,
                    save: &save,
                    fac,
                    label: &label,
                    found,
                },
            )?);
        }
        settle_missing(lib, w, report, &label, missing);
    }
    Ok(())
}

/// Decides what craft that vanished from one save mean: deletions, or, when
/// many vanished at once, a question for the user.
fn settle_missing(
    lib: &mut Library,
    w: &mut Work,
    report: &mut Report,
    label: &str,
    missing: Vec<Location>,
) {
    if missing.len() > MAX_SILENT_DELETES {
        report.notes.push(format!(
            "{} craft went missing from {label} at once; confirm or put them back on the Crafts tab",
            missing.len()
        ));
        w.state.pending.extend(missing);
        return;
    }
    for loc in missing {
        let Some(ci) = lib.index_of(&loc.id) else {
            continue;
        };
        let c = &mut lib.crafts[ci];
        if !c.live() {
            continue;
        }
        if c.head().hash == loc.hash {
            c.deleted = Some(Deleted {
                at: paths::now_secs(),
                from: label.to_owned(),
            });
            report.notes.push(format!(
                "Deleted {} (in {label}); it's in the trash",
                c.name
            ));
        } else {
            report.notes.push(format!(
                "Kept {}: deleted in {label}, but changed elsewhere",
                c.name
            ));
        }
    }
}

struct CollectCtx<'a> {
    game: &'a Path,
    dir: &'a Path,
    save: &'a str,
    fac: Facility,
    label: &'a str,
    found: Vec<Found>,
}

/// Reads one save's VAB or SPH folder into the library. Returns the tracked
/// craft that are gone from it.
fn collect_dir(
    paths: &Paths,
    lib: &mut Library,
    w: &mut Work,
    report: &mut Report,
    cx: &CollectCtx<'_>,
) -> Result<Vec<Location>> {
    let (save, fac, label, found) = (cx.save, cx.fac, cx.label, &cx.found);
    let find = |path: &str| found.iter().position(|f| f.path.eq_ignore_ascii_case(path));
    let mut claimed = vec![false; found.len()];
    // A new name must be free on disk too, so a rename never overwrites.
    let free_beside = |path: &str| {
        let path = path.to_owned();
        move |name: &str| {
            let to = with_stem(&path, name);
            to.eq_ignore_ascii_case(&path) || !occupied(cx.dir, &to)
        }
    };
    // A pending file that came back is just a file again.
    w.state
        .pending
        .retain(|p| !(p.is_in(save, fac) && find(&p.path).is_some()));

    let mut keep: Vec<Location> = Vec::new();
    let mut missing: Vec<Location> = Vec::new();
    let mut add_locations: Vec<Location> = Vec::new();
    let locations = std::mem::take(&mut w.state.locations);
    for mut loc in locations {
        if !loc.is_in(save, fac) {
            keep.push(loc);
            continue;
        }
        let Some(fi) = find(&loc.path) else {
            missing.push(loc);
            continue;
        };
        claimed[fi] = true;
        let f = &found[fi];
        loc.path.clone_from(&f.path);
        if f.hash == loc.hash {
            keep.push(loc);
            continue;
        }
        let Some(ci) = lib.index_of(&loc.id) else {
            // The library lost it: read the file as new below.
            claimed[fi] = false;
            continue;
        };
        let craft = &lib.crafts[ci];
        if !craft.live() {
            // Changed after being deleted: the edit wins, if the name is free.
            if lib.live_named(fac, &craft.name).is_none() {
                let hash = store_blob(paths, &f.bytes)?;
                let c = &mut lib.crafts[ci];
                c.deleted = None;
                c.revisions.push(revision(&f.bytes, hash.clone(), label));
                report.notes.push(format!(
                    "Kept {}: it was deleted, but {label} changed it",
                    c.name
                ));
                report.revised += 1;
                grab_thumb(
                    paths,
                    &game_thumb(cx.game, save, fac, stem(&f.path)),
                    &cx.dir.join(&f.path),
                    &hash,
                );
                loc.hash = hash;
                keep.push(loc);
            } else {
                claimed[fi] = false;
            }
            continue;
        }
        if craft.head().hash == loc.hash {
            let hash = store_blob(paths, &f.bytes)?;
            lib.crafts[ci]
                .revisions
                .push(revision(&f.bytes, hash.clone(), label));
            report.revised += 1;
            grab_thumb(
                paths,
                &game_thumb(cx.game, save, fac, stem(&f.path)),
                &cx.dir.join(&f.path),
                &hash,
            );
            loc.hash = hash;
            keep.push(loc);
        } else if craft.head().hash == f.hash {
            // Both sides made the same change.
            loc.hash.clone_from(&f.hash);
            keep.push(loc);
        } else {
            // Changed here and elsewhere: keep this version as its own craft,
            // or join an earlier copy of the very same change.
            let base = craft.name.clone();
            let prefix = format!("{base} (");
            let earlier = lib.crafts.iter().position(|o| {
                o.live()
                    && o.facility == fac
                    && o.id != loc.id
                    && o.name.starts_with(&prefix)
                    && sha(&set_ship_name(&f.bytes, &o.name)) == o.head().hash
                    && !keep
                        .iter()
                        .chain(&add_locations)
                        .any(|l| l.is_in(save, fac) && l.id == o.id)
            });
            if let Some(oi) = earlier {
                let o = &lib.crafts[oi];
                let bytes = set_ship_name(&f.bytes, &o.name);
                let new_path = with_stem(&f.path, &o.name);
                if free_beside(&f.path)(&o.name) {
                    rename_in_place(cx.dir, &f.path, &new_path, &bytes)?;
                    add_locations.push(Location {
                        save: save.to_owned(),
                        facility: fac,
                        path: new_path,
                        id: o.id.clone(),
                        hash: o.head().hash.clone(),
                    });
                    continue;
                }
            }
            let (ni, bytes) = lib.add(
                paths,
                fac,
                &base,
                save,
                &free_beside(&f.path),
                &f.bytes,
                label,
            )?;
            let new_name = lib.crafts[ni].name.clone();
            let new_path = with_stem(&f.path, &new_name);
            rename_in_place(cx.dir, &f.path, &new_path, &bytes)?;
            report.notes.push(format!(
                "{base} changed in two places; kept {label}'s version as {new_name}"
            ));
            report.added += 1;
            add_locations.push(Location {
                save: save.to_owned(),
                facility: fac,
                path: new_path,
                id: lib.crafts[ni].id.clone(),
                hash: sha(&bytes),
            });
        }
    }

    // Moved within the save: same bytes at a new path.
    let mut still_missing = Vec::new();
    for mut loc in missing {
        let moved = found
            .iter()
            .enumerate()
            .find(|(i, f)| {
                !claimed[*i]
                    && f.hash == loc.hash
                    && stem(&f.path).eq_ignore_ascii_case(stem(&loc.path))
            })
            .map(|(i, _)| i);
        match moved {
            Some(i) => {
                claimed[i] = true;
                loc.path.clone_from(&found[i].path);
                keep.push(loc);
            }
            None => still_missing.push(loc),
        }
    }

    // Files orbis doesn't track yet.
    let mut located: BTreeSet<String> = keep
        .iter()
        .chain(&add_locations)
        .filter(|l| l.is_in(save, fac))
        .map(|l| l.id.clone())
        .collect();
    for (i, f) in found.iter().enumerate() {
        if claimed[i] {
            continue;
        }
        let name = stem(&f.path).to_owned();
        let live = lib.live_named(fac, &name);
        if let Some(ci) = live {
            let c = &lib.crafts[ci];
            if c.has(&f.hash) && !located.contains(&c.id) {
                located.insert(c.id.clone());
                add_locations.push(Location {
                    save: save.to_owned(),
                    facility: fac,
                    path: f.path.clone(),
                    id: c.id.clone(),
                    hash: f.hash.clone(),
                });
                continue;
            }
        }
        if let Some(gone) = lib.deleted_copy(fac, &name, &f.hash) {
            // A leftover copy of a deleted craft never brings it back.
            remove_craft_file(cx.dir, &f.path)?;
            report.notes.push(format!(
                "Removed a leftover copy of deleted {gone} from {label}"
            ));
            continue;
        }
        if live.is_some() {
            // A different craft with a taken name: add it under a new one.
            let (ni, bytes) = lib.add(
                paths,
                fac,
                &name,
                save,
                &free_beside(&f.path),
                &f.bytes,
                label,
            )?;
            let new_name = lib.crafts[ni].name.clone();
            let new_path = with_stem(&f.path, &new_name);
            rename_in_place(cx.dir, &f.path, &new_path, &bytes)?;
            report.notes.push(format!(
                "{label} has a different craft called {name}; it's {new_name} in the library"
            ));
            report.added += 1;
            located.insert(lib.crafts[ni].id.clone());
            add_locations.push(Location {
                save: save.to_owned(),
                facility: fac,
                path: new_path,
                id: lib.crafts[ni].id.clone(),
                hash: sha(&bytes),
            });
            continue;
        }
        let (ni, bytes) = lib.add(paths, fac, &name, save, &|_| true, &f.bytes, label)?;
        if bytes != f.bytes {
            // Its ship name didn't match the file name.
            write_craft(cx.dir, &f.path, &bytes)?;
        }
        let hash = lib.crafts[ni].head().hash.clone();
        grab_thumb(
            paths,
            &game_thumb(cx.game, save, fac, &name),
            &cx.dir.join(&f.path),
            &hash,
        );
        report.added += 1;
        located.insert(lib.crafts[ni].id.clone());
        add_locations.push(Location {
            save: save.to_owned(),
            facility: fac,
            path: f.path.clone(),
            id: lib.crafts[ni].id.clone(),
            hash,
        });
    }
    keep.extend(add_locations);
    w.state.locations = keep;
    Ok(still_missing)
}

fn write_craft(dir: &Path, rel: &str, bytes: &[u8]) -> Result<()> {
    paths::write_atomic(&dir.join(rel), bytes)
}

/// Removes a craft file and KSP's cached summary of it.
fn remove_craft_file(dir: &Path, rel: &str) -> Result<()> {
    let p = dir.join(rel);
    fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))?;
    let _ = fs::remove_file(p.with_extension("loadmeta"));
    Ok(())
}

/// Moves a craft file to a new name with new bytes; never overwrites
/// another file.
fn rename_in_place(dir: &Path, from: &str, to: &str, bytes: &[u8]) -> Result<()> {
    let same = from.eq_ignore_ascii_case(to);
    if !same && occupied(dir, to) {
        bail!("{} already exists", dir.join(to).display());
    }
    write_craft(dir, to, bytes)?;
    if !same {
        remove_craft_file(dir, from)?;
    }
    Ok(())
}

/// Whether a file exists at `rel` under `dir`, ignoring case as Windows does.
fn occupied(dir: &Path, rel: &str) -> bool {
    let (parent, file) = match rel.rsplit_once('/') {
        Some((p, f)) => (dir.join(p), f),
        None => (dir.to_path_buf(), rel),
    };
    taken(&parent, file)
}

/// Whether a revision can be built in an instance with this index.
pub fn fits(index: &PartIndex, rev: &Revision) -> bool {
    index.missing(&rev.parts).is_empty()
}

/// Brings one instance's saves up to date with the library.
fn deploy(paths: &Paths, lib: &Library, w: &mut Work, report: &mut Report) -> Result<()> {
    let game = w.inst.game_dir();
    let label_of = |save: &str| format!("{}/{save}", w.inst.config.name);
    let locations = std::mem::take(&mut w.state.locations);
    let mut keep = Vec::new();
    for mut loc in locations {
        let dir = game
            .join("saves")
            .join(&loc.save)
            .join("Ships")
            .join(loc.facility.dir());
        // Only touch a file that is still what orbis last saw.
        let current = fs::read(dir.join(&loc.path)).ok().map(|b| sha(&b));
        let unchanged = current.as_deref() == Some(loc.hash.as_str());
        let Some(craft) = lib.get(&loc.id) else {
            if unchanged && lib.purged.iter().any(|p| p.id == loc.id) {
                remove_craft_file(&dir, &loc.path)?;
                report.withdrawn += 1;
            }
            continue;
        };
        if !unchanged {
            keep.push(loc);
            continue;
        }
        if !craft.live() {
            remove_craft_file(&dir, &loc.path)?;
            report.withdrawn += 1;
            continue;
        }
        let head = craft.head();
        if !fits(&w.index, head) {
            remove_craft_file(&dir, &loc.path)?;
            report.withdrawn += 1;
            continue;
        }
        if head.hash != loc.hash {
            write_craft(&dir, &loc.path, &read_blob(paths, &head.hash)?)?;
            loc.hash.clone_from(&head.hash);
            report.deployed += 1;
        }
        keep.push(loc);
    }
    for save in &w.saves {
        for fac in Facility::ALL {
            let dir = game.join("saves").join(save).join("Ships").join(fac.dir());
            for craft in lib.crafts.iter().filter(|c| c.live() && c.facility == fac) {
                let here = |l: &Location| l.is_in(save, fac) && l.id == craft.id;
                if keep.iter().any(here) || w.state.pending.iter().any(here) {
                    continue;
                }
                let head = craft.head();
                if !fits(&w.index, head) {
                    continue;
                }
                let path = format!("{}.craft", file_safe(&craft.name));
                if taken(&dir, &path) {
                    report.notes.push(format!(
                        "Didn't copy {} into {}: another file has its name",
                        craft.name,
                        label_of(save)
                    ));
                    continue;
                }
                write_craft(&dir, &path, &read_blob(paths, &head.hash)?)?;
                let thumb = thumb_path(paths, &head.hash);
                let dest = game_thumb(&game, save, fac, &craft.name);
                if thumb.is_file() && !dest.exists() {
                    let _ = fs::create_dir_all(game.join("thumbs"));
                    let _ = fs::copy(&thumb, &dest);
                }
                report.deployed += 1;
                keep.push(Location {
                    save: save.clone(),
                    facility: fac,
                    path,
                    id: craft.id.clone(),
                    hash: head.hash.clone(),
                });
            }
        }
    }
    w.state.locations = keep;
    Ok(())
}

/// Whether a file with this name (ignoring case, as Windows does) exists.
fn taken(dir: &Path, rel: &str) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(rel))
}

// ---- library operations ------------------------------------------------------

/// Adds a craft file to the library. Its building comes from the file's
/// `type`, then its folder, then defaults to the VAB.
pub fn add_file(paths: &Paths, file: &Path) -> Result<String> {
    let _lock = lock(paths)?;
    let mut lib = Library::load(paths)?;
    let bytes = fs::read(file).with_context(|| format!("read {}", file.display()))?;
    let info = parts::read_craft(&String::from_utf8_lossy(&bytes));
    if info.parts.is_empty() {
        bail!("{} doesn't look like a craft file", file.display());
    }
    let fac = facility_of(file, &info);
    let name = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| info.ship.clone());
    let (i, _) = lib.add(
        paths,
        fac,
        &file_safe(&name),
        "",
        &|_| true,
        &bytes,
        "upload",
    )?;
    let name = lib.crafts[i].name.clone();
    lib.save(paths)?;
    Ok(name)
}

fn facility_of(file: &Path, info: &parts::CraftInfo) -> Facility {
    Facility::from_name(&info.kind)
        .or_else(|| {
            file.parent()
                .and_then(|p| p.file_name())
                .and_then(|n| Facility::from_name(&n.to_string_lossy()))
        })
        .unwrap_or(Facility::Vab)
}

/// What an import did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Imported {
    pub added: Vec<String>,
    /// Files already in the library.
    pub skipped: usize,
}

/// Adds every craft under `dir` (for example an old KSP folder's `saves`),
/// skipping autosaves, mod backup folders and craft already in the library.
/// Thumbnails are picked up from a `thumbs` folder beside `saves`.
pub fn import(paths: &Paths, dir: &Path) -> Result<Imported> {
    let _lock = lock(paths)?;
    let mut lib = Library::load(paths)?;
    let found = scan_dir(dir)?;
    let mut out = Imported::default();
    for f in found {
        let full = dir.join(&f.path);
        let info = parts::read_craft(&String::from_utf8_lossy(&f.bytes));
        if info.parts.is_empty() {
            continue;
        }
        let fac = facility_of(&full, &info);
        if lib
            .crafts
            .iter()
            .any(|c| c.facility == fac && c.has(&f.hash))
        {
            out.skipped += 1;
            continue;
        }
        // `.../saves/<save>/Ships/<fac>/<name>.craft` names its save.
        let save = full
            .components()
            .collect::<Vec<_>>()
            .windows(2)
            .find(|w| w[1].as_os_str() == "Ships")
            .map(|w| w[0].as_os_str().to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = stem(&f.path).to_owned();
        let (i, _) = lib.add(
            paths,
            fac,
            &file_safe(&name),
            &save,
            &|_| true,
            &f.bytes,
            "import",
        )?;
        let hash = lib.crafts[i].head().hash.clone();
        if let Some(game) = full
            .ancestors()
            .find(|a| a.file_name().is_some_and(|n| n == "saves"))
            .and_then(Path::parent)
        {
            grab_thumb(paths, &game_thumb(game, &save, fac, &name), &full, &hash);
        }
        out.added.push(lib.crafts[i].name.clone());
    }
    lib.save(paths)?;
    Ok(out)
}

/// Moves a craft to the trash; the next sync removes it from instances.
pub fn delete(paths: &Paths, id: &str) -> Result<()> {
    let _lock = lock(paths)?;
    let mut lib = Library::load(paths)?;
    let i = lib.index_of(id).with_context(|| format!("no craft {id}"))?;
    if lib.crafts[i].deleted.is_none() {
        lib.crafts[i].deleted = Some(Deleted {
            at: paths::now_secs(),
            from: "orbis".into(),
        });
    }
    lib.save(paths)
}

/// Brings a craft back from the trash (renamed if its name was taken since).
/// Returns its name.
pub fn restore(paths: &Paths, id: &str) -> Result<String> {
    let _lock = lock(paths)?;
    let mut lib = Library::load(paths)?;
    let i = lib.index_of(id).with_context(|| format!("no craft {id}"))?;
    if lib.crafts[i].live() {
        return Ok(lib.crafts[i].name.clone());
    }
    let (fac, name) = (lib.crafts[i].facility, lib.crafts[i].name.clone());
    let free = lib.unique_name(fac, &name, "restored", &|_| true);
    if free != name {
        let bytes = set_ship_name(&read_blob(paths, &lib.crafts[i].head().hash)?, &free);
        let hash = store_blob(paths, &bytes)?;
        let c = &mut lib.crafts[i];
        c.revisions.push(revision(&bytes, hash, "orbis"));
        c.name.clone_from(&free);
    }
    lib.crafts[i].deleted = None;
    lib.save(paths)?;
    Ok(free)
}

/// Saves a revision of a craft as a new craft. Returns the new name.
pub fn fork(paths: &Paths, id: &str, hash: &str, name: &str) -> Result<String> {
    let _lock = lock(paths)?;
    let mut lib = Library::load(paths)?;
    let c = lib.get(id).with_context(|| format!("no craft {id}"))?;
    if !c.has(hash) {
        bail!("{} has no revision {hash}", c.name);
    }
    let fac = c.facility;
    let from = format!("copy of {}", c.name);
    let bytes = read_blob(paths, hash)?;
    let (i, _) = lib.add(paths, fac, &file_safe(name), "", &|_| true, &bytes, &from)?;
    let thumb = thumb_path(paths, hash);
    if thumb.is_file() {
        let _ = fs::copy(&thumb, thumb_path(paths, &lib.crafts[i].head().hash));
    }
    let name = lib.crafts[i].name.clone();
    lib.save(paths)?;
    Ok(name)
}

/// Removes everything in the trash for good. Returns how many craft.
pub fn empty_trash(paths: &Paths) -> Result<usize> {
    let _lock = lock(paths)?;
    let mut lib = Library::load(paths)?;
    let (live, gone): (Vec<Craft>, Vec<Craft>) = lib.crafts.drain(..).partition(Craft::live);
    let n = gone.len();
    lib.crafts = live;
    lib.purged.extend(gone.into_iter().map(|c| Purged {
        hashes: c.revisions.iter().map(|r| r.hash.clone()).collect(),
        id: c.id,
        name: c.name,
        facility: c.facility,
    }));
    lib.prune(paths)?;
    lib.save(paths)?;
    Ok(n)
}

/// A craft that went missing and waits for a decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingCraft {
    pub instance: String,
    pub instance_name: String,
    pub location: Location,
    pub name: String,
}

/// Every pending craft, across instances.
pub fn pending(paths: &Paths) -> Result<Vec<PendingCraft>> {
    let lib = Library::load(paths)?;
    let mut out = Vec::new();
    for inst in instance::list(paths)? {
        for loc in load_state(&inst.dir)?.pending {
            let name = lib
                .get(&loc.id)
                .map_or_else(|| loc.path.clone(), |c| c.name.clone());
            out.push(PendingCraft {
                instance: inst.id.clone(),
                instance_name: inst.config.name.clone(),
                location: loc,
                name,
            });
        }
    }
    Ok(out)
}

/// Settles a pending craft: delete it everywhere, or put it back (the next
/// sync copies it into the save again).
pub fn resolve(paths: &Paths, p: &PendingCraft, delete_it: bool) -> Result<()> {
    let _lock = lock(paths)?;
    let inst = instance::find(paths, &p.instance)?;
    let mut state = load_state(&inst.dir)?;
    state.pending.retain(|l| l != &p.location);
    save_state(&inst.dir, &state)?;
    if delete_it {
        let mut lib = Library::load(paths)?;
        if let Some(i) = lib.index_of(&p.location.id)
            && lib.crafts[i].deleted.is_none()
        {
            lib.crafts[i].deleted = Some(Deleted {
                at: paths::now_secs(),
                from: format!("{}/{}", p.instance_name, p.location.save),
            });
            lib.save(paths)?;
        }
    }
    Ok(())
}

// ---- explaining ---------------------------------------------------------------

/// Which mod (and in which instance) provides each part orbis has seen.
#[derive(Clone, Debug, Default)]
pub struct Providers(HashMap<String, (String, String)>);

/// Builds the part -> (mod, instance) map from every instance's last index.
pub fn providers(paths: &Paths) -> Providers {
    let mut map = HashMap::new();
    let Ok(list) = instance::list(paths) else {
        return Providers(map);
    };
    for inst in list {
        let Some(idx) = cached_index(&inst) else {
            continue;
        };
        let mut folder_mod: HashMap<String, String> = HashMap::new();
        if let Ok(reg) = inst.registry() {
            for (file, id) in reg.owners() {
                if let Some(folder) = file
                    .strip_prefix("gamedata/")
                    .and_then(|r| r.split('/').next())
                {
                    folder_mod.entry(folder.to_owned()).or_insert(id);
                }
            }
        }
        for (part, folder) in idx.parts {
            let m = folder_mod
                .get(&folder.to_lowercase())
                .cloned()
                .unwrap_or(folder);
            map.entry(part)
                .or_insert_with(|| (m, inst.config.name.clone()));
        }
    }
    Providers(map)
}

/// Says which mods the missing parts come from: "NearFuturePropulsion (3
/// parts, in Career B); 2 parts no instance has".
pub fn explain(missing: &[String], providers: &Providers) -> String {
    let mut by_mod: Vec<(String, String, usize)> = Vec::new();
    let mut unknown = 0;
    for p in missing {
        match providers.0.get(p) {
            Some((m, inst)) => match by_mod.iter_mut().find(|(bm, _, _)| bm == m) {
                Some(e) => e.2 += 1,
                None => by_mod.push((m.clone(), inst.clone(), 1)),
            },
            None => unknown += 1,
        }
    }
    let plural = |n: usize| if n == 1 { "part" } else { "parts" };
    let mut out: Vec<String> = by_mod
        .into_iter()
        .map(|(m, inst, n)| format!("{m} ({n} {}, in {inst})", plural(n)))
        .collect();
    if unknown > 0 {
        out.push(format!("{unknown} {} no instance has", plural(unknown)));
    }
    out.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::{CopyMode, CreateOptions};

    struct World {
        _tmp: tempfile::TempDir,
        paths: Paths,
        ksp: PathBuf,
    }

    fn world() -> World {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ksp = tmp.path().join("ksp");
        fs::create_dir_all(ksp.join("GameData/Squad/Parts")).expect("mkdir");
        fs::write(ksp.join("readme.txt"), "Version 1.12.5\n").expect("w");
        fs::write(ksp.join("buildID64.txt"), "build id = 03190\n").expect("w");
        fs::write(ksp.join("KSP_x64.exe"), "").expect("w");
        fs::write(
            ksp.join("GameData/Squad/Parts/stock.cfg"),
            "PART\n{\n\tname = pod\n}\nPART\n{\n\tname = tank_1\n}\n",
        )
        .expect("w");
        let paths = Paths::new(Some(&tmp.path().join("orbis"))).expect("paths");
        World {
            _tmp: tmp,
            paths,
            ksp,
        }
    }

    /// An instance with one save `S` and the given mods (folder, parts).
    fn instance(w: &World, name: &str, mods: &[(&str, &[&str])]) -> Instance {
        let opts = CreateOptions {
            name: name.into(),
            source: w.ksp.clone(),
            stock_only: true,
            with_saves: false,
            mode: CopyMode::Link,
        };
        let inst = instance::create(&w.paths, &opts, &mut |_, _| {}).expect("create");
        for (folder, parts) in mods {
            let dir = inst.game_dir().join("GameData").join(folder);
            fs::create_dir_all(&dir).expect("mkdir");
            let cfg: String = parts
                .iter()
                .map(|p| format!("PART\n{{\n\tname = {p}\n}}\n"))
                .collect();
            fs::write(dir.join("parts.cfg"), cfg).expect("w");
        }
        add_save(&inst, "S");
        inst
    }

    fn add_save(inst: &Instance, save: &str) {
        let s = inst.game_dir().join("saves").join(save);
        fs::create_dir_all(s.join("Ships/VAB")).expect("mkdir");
        fs::write(s.join("persistent.sfs"), "GAME {}").expect("w");
    }

    fn craft(name: &str, parts: &[&str]) -> String {
        let mut s = format!("ship = {name}\nversion = 1.12.5\ntype = VAB\n");
        for (i, p) in parts.iter().enumerate() {
            s.push_str(&format!("PART\n{{\n\tpart = {p}_{}\n}}\n", 4000 + i));
        }
        s
    }

    fn vab(inst: &Instance) -> PathBuf {
        inst.game_dir().join("saves/S/Ships/VAB")
    }

    fn put(inst: &Instance, file: &str, text: &str) {
        let p = vab(inst).join(file);
        fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        fs::write(p, text).expect("write craft");
    }

    fn get(inst: &Instance, file: &str) -> Option<String> {
        fs::read_to_string(vab(inst).join(file)).ok()
    }

    fn files(inst: &Instance) -> Vec<String> {
        let mut v: Vec<String> = scan_dir(&vab(inst))
            .expect("scan")
            .into_iter()
            .map(|f| f.path)
            .collect();
        v.sort();
        v
    }

    fn lib(w: &World) -> Library {
        Library::load(&w.paths).expect("library")
    }

    fn live_names(w: &World) -> Vec<String> {
        let mut v: Vec<String> = lib(w)
            .crafts
            .into_iter()
            .filter(Craft::live)
            .map(|c| c.name)
            .collect();
        v.sort();
        v
    }

    #[test]
    fn shares_craft_only_where_its_parts_exist() {
        let w = world();
        let a = instance(&w, "A", &[("NearFuture", &["nf_ion"])]);
        let b = instance(&w, "B", &[]);
        let c = instance(&w, "C", &[("NearFuture", &["nf_ion"])]);
        put(
            &a,
            "Orbiter I.craft",
            &craft("Orbiter I", &["pod", "tank.1"]),
        );
        put(&a, "Ion.craft", &craft("Ion", &["pod", "nf.ion"]));
        let r = sync(&w.paths).expect("sync");
        assert_eq!(r.added, 2);
        assert_eq!(live_names(&w), ["Ion", "Orbiter I"]);
        assert_eq!(files(&b), ["Orbiter I.craft"]);
        assert_eq!(files(&c), ["Ion.craft", "Orbiter I.craft"]);
        assert_eq!(get(&c, "Ion.craft"), get(&a, "Ion.craft"));
        // A second sync changes nothing.
        let r = sync(&w.paths).expect("sync again");
        assert_eq!((r.added, r.revised, r.deployed, r.withdrawn), (0, 0, 0, 0));
    }

    #[test]
    fn saving_over_with_a_mod_part_hides_it_where_the_mod_is_missing() {
        let w = world();
        let a = instance(&w, "A", &[("NearFuture", &["nf.ion"])]);
        let b = instance(&w, "B", &[]);
        put(&a, "Orbiter I.craft", &craft("Orbiter I", &["pod"]));
        sync(&w.paths).expect("sync");
        assert_eq!(files(&b), ["Orbiter I.craft"]);
        put(
            &a,
            "Orbiter I.craft",
            &craft("Orbiter I", &["pod", "nf.ion"]),
        );
        let r = sync(&w.paths).expect("sync");
        assert_eq!((r.revised, r.withdrawn), (1, 1));
        assert!(files(&b).is_empty());
        let l = lib(&w);
        let orbiter = l.find("Orbiter I").expect("craft");
        assert_eq!(orbiter.revisions.len(), 2);
        // The stock version lives on as its own craft.
        let first = orbiter.revisions[0].hash.clone();
        let name = fork(&w.paths, &orbiter.id, &first, "Orbiter I Stock").expect("fork");
        assert_eq!(name, "Orbiter I Stock");
        sync(&w.paths).expect("sync");
        assert_eq!(files(&b), ["Orbiter I Stock.craft"]);
        let text = get(&b, "Orbiter I Stock.craft").expect("copied");
        assert!(text.starts_with("ship = Orbiter I Stock\n"));
        assert_eq!(files(&a), ["Orbiter I Stock.craft", "Orbiter I.craft"]);
    }

    #[test]
    fn save_as_a_new_name_is_a_new_craft() {
        let w = world();
        let a = instance(&w, "A", &[]);
        let b = instance(&w, "B", &[]);
        put(&a, "Orbiter I.craft", &craft("Orbiter I", &["pod"]));
        sync(&w.paths).expect("sync");
        put(
            &a,
            "Orbiter II.craft",
            &craft("Orbiter II", &["pod", "tank.1"]),
        );
        sync(&w.paths).expect("sync");
        assert_eq!(live_names(&w), ["Orbiter I", "Orbiter II"]);
        assert_eq!(files(&b), ["Orbiter I.craft", "Orbiter II.craft"]);
    }

    #[test]
    fn deleting_in_game_deletes_everywhere_until_restored() {
        let w = world();
        let a = instance(&w, "A", &[]);
        let b = instance(&w, "B", &[]);
        put(&a, "Orbiter I.craft", &craft("Orbiter I", &["pod"]));
        sync(&w.paths).expect("sync");
        fs::remove_file(vab(&a).join("Orbiter I.craft")).expect("rm");
        let r = sync(&w.paths).expect("sync");
        assert_eq!(r.withdrawn, 1);
        assert!(live_names(&w).is_empty());
        assert!(files(&a).is_empty() && files(&b).is_empty());
        let id = lib(&w).find("Orbiter I").expect("in trash").id.clone();
        restore(&w.paths, &id).expect("restore");
        sync(&w.paths).expect("sync");
        assert_eq!(files(&a), ["Orbiter I.craft"]);
        assert_eq!(files(&b), ["Orbiter I.craft"]);
    }

    #[test]
    fn an_edit_elsewhere_beats_a_delete() {
        let w = world();
        let a = instance(&w, "A", &[]);
        let b = instance(&w, "B", &[]);
        put(&a, "Orbiter I.craft", &craft("Orbiter I", &["pod"]));
        sync(&w.paths).expect("sync");
        fs::remove_file(vab(&a).join("Orbiter I.craft")).expect("rm");
        let edited = craft("Orbiter I", &["pod", "tank.1"]);
        put(&b, "Orbiter I.craft", &edited);
        let r = sync(&w.paths).expect("sync");
        assert!(
            r.notes.iter().any(|n| n.starts_with("Kept Orbiter I")),
            "{r:?}"
        );
        assert_eq!(live_names(&w), ["Orbiter I"]);
        assert_eq!(get(&a, "Orbiter I.craft").as_deref(), Some(edited.as_str()));
    }

    #[test]
    fn different_edits_in_two_places_keep_both() {
        let w = world();
        let a = instance(&w, "A", &[]);
        let b = instance(&w, "B", &[]);
        put(&a, "Orbiter I.craft", &craft("Orbiter I", &["pod"]));
        sync(&w.paths).expect("sync");
        let ea = craft("Orbiter I", &["pod", "tank.1"]);
        put(&a, "Orbiter I.craft", &ea);
        put(&b, "Orbiter I.craft", &craft("Orbiter I", &["pod", "pod"]));
        sync(&w.paths).expect("sync");
        assert_eq!(live_names(&w), ["Orbiter I", "Orbiter I (S)"]);
        assert_eq!(files(&a), ["Orbiter I (S).craft", "Orbiter I.craft"]);
        assert_eq!(files(&b), ["Orbiter I (S).craft", "Orbiter I.craft"]);
        assert_eq!(get(&b, "Orbiter I.craft").as_deref(), Some(ea.as_str()));
        let theirs = get(&b, "Orbiter I (S).craft").expect("kept");
        assert!(theirs.starts_with("ship = Orbiter I (S)\n"));
        assert_eq!(
            get(&a, "Orbiter I (S).craft").as_deref(),
            Some(theirs.as_str())
        );
    }

    #[test]
    fn identical_edits_converge() {
        let w = world();
        let a = instance(&w, "A", &[]);
        let b = instance(&w, "B", &[]);
        put(&a, "Orbiter I.craft", &craft("Orbiter I", &["pod"]));
        sync(&w.paths).expect("sync");
        let e = craft("Orbiter I", &["pod", "tank.1"]);
        put(&a, "Orbiter I.craft", &e);
        put(&b, "Orbiter I.craft", &e);
        let r = sync(&w.paths).expect("sync");
        assert_eq!((r.added, r.revised), (0, 1));
        assert_eq!(live_names(&w), ["Orbiter I"]);
    }

    #[test]
    fn many_missing_at_once_wait_for_confirmation() {
        let w = world();
        let a = instance(&w, "A", &[]);
        let b = instance(&w, "B", &[]);
        for n in 1..=5 {
            put(
                &a,
                &format!("C{n}.craft"),
                &craft(&format!("C{n}"), &["pod"]),
            );
        }
        sync(&w.paths).expect("sync");
        for n in 1..=5 {
            fs::remove_file(vab(&b).join(format!("C{n}.craft"))).expect("rm");
        }
        let r = sync(&w.paths).expect("sync");
        assert!(
            r.notes.iter().any(|n| n.contains("5 craft went missing")),
            "{r:?}"
        );
        assert_eq!(live_names(&w).len(), 5);
        assert!(files(&b).is_empty(), "not put back while pending");
        let p = pending(&w.paths).expect("pending");
        assert_eq!(p.len(), 5);
        let back = p.iter().find(|p| p.name == "C1").expect("C1");
        resolve(&w.paths, back, false).expect("put back");
        let gone = p.iter().find(|p| p.name == "C2").expect("C2");
        resolve(&w.paths, gone, true).expect("delete");
        sync(&w.paths).expect("sync");
        assert_eq!(files(&b), ["C1.craft"]);
        assert!(!files(&a).contains(&"C2.craft".to_owned()));
        assert_eq!(pending(&w.paths).expect("pending").len(), 3);
    }

    #[test]
    fn a_missing_save_or_forgotten_save_is_not_a_deletion() {
        let w = world();
        let a = instance(&w, "A", &[]);
        let b = instance(&w, "B", &[]);
        put(&a, "Orbiter I.craft", &craft("Orbiter I", &["pod"]));
        sync(&w.paths).expect("sync");
        fs::remove_dir_all(b.game_dir().join("saves/S")).expect("rm save");
        sync(&w.paths).expect("sync");
        assert_eq!(live_names(&w), ["Orbiter I"]);
        add_save(&b, "S");
        sync(&w.paths).expect("sync");
        fs::remove_file(vab(&b).join("Orbiter I.craft")).expect("rm");
        forget_saves(&b.dir, &["S".to_owned()]).expect("forget");
        sync(&w.paths).expect("sync");
        assert_eq!(live_names(&w), ["Orbiter I"]);
        assert_eq!(files(&b), ["Orbiter I.craft"]);
    }

    #[test]
    fn running_instances_are_left_alone() {
        let w = world();
        let a = instance(&w, "A", &[]);
        let b = instance(&w, "B", &[]);
        running::mark(&b.dir, std::process::id(), "direct").expect("mark");
        put(&a, "Orbiter I.craft", &craft("Orbiter I", &["pod"]));
        put(&b, "Mine.craft", &craft("Mine", &["pod"]));
        let r = sync(&w.paths).expect("sync");
        assert_eq!(r.running, ["B"]);
        assert_eq!(live_names(&w), ["Orbiter I"]);
        assert_eq!(files(&b), ["Mine.craft"]);
        running::clear(&b.dir, std::process::id());
        sync(&w.paths).expect("sync");
        assert_eq!(live_names(&w), ["Mine", "Orbiter I"]);
        assert_eq!(files(&a), ["Mine.craft", "Orbiter I.craft"]);
    }

    #[test]
    fn identical_copies_are_one_craft_and_different_ones_are_renamed() {
        let w = world();
        let a = instance(&w, "A", &[]);
        let b = instance(&w, "B", &[]);
        put(&a, "Same.craft", &craft("Same", &["pod"]));
        put(&b, "Same.craft", &craft("Same", &["pod"]));
        put(&a, "Alpha.craft", &craft("Alpha", &["pod"]));
        put(&b, "Alpha.craft", &craft("Alpha", &["tank.1"]));
        sync(&w.paths).expect("sync");
        assert_eq!(live_names(&w), ["Alpha", "Alpha (S)", "Same"]);
        assert_eq!(files(&b), ["Alpha (S).craft", "Alpha.craft", "Same.craft"]);
        assert!(
            get(&b, "Alpha (S).craft")
                .expect("renamed")
                .contains("tank.1_")
        );
        assert!(get(&b, "Alpha.craft").expect("copied").contains("pod_"));
    }

    #[test]
    fn leftover_copies_of_deleted_craft_are_removed() {
        let w = world();
        let a = instance(&w, "A", &[]);
        put(&a, "Orbiter I.craft", &craft("Orbiter I", &["pod"]));
        sync(&w.paths).expect("sync");
        let id = lib(&w).find("Orbiter I").expect("craft").id.clone();
        delete(&w.paths, &id).expect("delete");
        sync(&w.paths).expect("sync");
        assert!(files(&a).is_empty());
        let b = instance(&w, "B", &[]);
        put(&b, "Orbiter I.craft", &craft("Orbiter I", &["pod"]));
        let r = sync(&w.paths).expect("sync");
        assert!(files(&b).is_empty(), "{r:?}");
        assert!(live_names(&w).is_empty());
    }

    #[test]
    fn autosaves_backups_and_sidecars_are_ignored() {
        let w = world();
        let a = instance(&w, "A", &[]);
        put(&a, "Auto-Saved Ship.craft", &craft("Auto", &["pod"]));
        put(
            &a,
            "Orbiter I_Backup/1.craft",
            &craft("Orbiter I", &["pod"]),
        );
        put(
            &a,
            "Orbiter I.craft.original",
            &craft("Orbiter I", &["pod"]),
        );
        put(&a, "Orbiter I.loadmeta", "x");
        sync(&w.paths).expect("sync");
        assert!(live_names(&w).is_empty());
    }

    #[test]
    fn moving_a_craft_into_a_folder_keeps_it() {
        let w = world();
        let a = instance(&w, "A", &[]);
        let b = instance(&w, "B", &[]);
        put(&a, "Orbiter I.craft", &craft("Orbiter I", &["pod"]));
        sync(&w.paths).expect("sync");
        fs::create_dir_all(vab(&b).join("Old")).expect("mkdir");
        fs::rename(
            vab(&b).join("Orbiter I.craft"),
            vab(&b).join("Old/Orbiter I.craft"),
        )
        .expect("move");
        let r = sync(&w.paths).expect("sync");
        assert_eq!((r.added, r.withdrawn), (0, 0));
        assert_eq!(files(&b), ["Old/Orbiter I.craft"]);
        put(
            &b,
            "Old/Orbiter I.craft",
            &craft("Orbiter I", &["pod", "tank.1"]),
        );
        sync(&w.paths).expect("sync");
        assert!(
            get(&a, "Orbiter I.craft")
                .expect("updated")
                .contains("tank.1_")
        );
    }

    #[test]
    fn imports_old_craft_with_thumbnails() {
        let w = world();
        let old = w._tmp.path().join("old");
        let ships = old.join("saves/Career/Ships");
        fs::create_dir_all(ships.join("VAB/Orbiter I_Backup")).expect("mkdir");
        fs::create_dir_all(ships.join("SPH")).expect("mkdir");
        fs::create_dir_all(old.join("thumbs")).expect("mkdir");
        fs::write(
            ships.join("VAB/Orbiter I.craft"),
            craft("Orbiter I", &["pod"]),
        )
        .expect("w");
        fs::write(
            ships.join("VAB/Orbiter I_Backup/x.craft"),
            craft("Orbiter I", &["tank"]),
        )
        .expect("w");
        fs::write(
            ships.join("SPH/Plane.craft"),
            craft("Plane", &["pod"]).replace("VAB", "SPH"),
        )
        .expect("w");
        fs::write(old.join("thumbs/Career_VAB_Orbiter I.png"), "png").expect("w");
        let r = import(&w.paths, &old.join("saves")).expect("import");
        assert_eq!(r.added.len(), 2);
        let l = lib(&w);
        let o = l.find("Orbiter I").expect("orbiter");
        assert!(thumb_path(&w.paths, &o.head().hash).is_file());
        assert_eq!(l.find("Plane").expect("plane").facility, Facility::Sph);
        let again = import(&w.paths, &old.join("saves")).expect("import again");
        assert_eq!((again.added.len(), again.skipped), (0, 2));
    }

    #[test]
    fn a_conflict_rename_never_overwrites_a_file() {
        let w = world();
        let a = instance(&w, "A", &[]);
        let b = instance(&w, "B", &[]);
        put(&a, "C.craft", &craft("C", &["pod"]));
        sync(&w.paths).expect("sync");
        put(&a, "C.craft", &craft("C", &["pod", "tank.1"]));
        put(&b, "C.craft", &craft("C", &["pod", "pod"]));
        let mine = craft("C (S)", &["tank.1", "tank.1"]);
        put(&b, "C (S).craft", &mine);
        sync(&w.paths).expect("sync");
        assert_eq!(live_names(&w), ["C", "C (S 2)", "C (S)"]);
        assert_eq!(get(&b, "C (S).craft").as_deref(), Some(mine.as_str()));
        assert_eq!(files(&b), ["C (S 2).craft", "C (S).craft", "C.craft"]);
    }

    #[test]
    fn the_missing_limit_counts_both_buildings_of_a_save() {
        let w = world();
        let a = instance(&w, "A", &[]);
        let b = instance(&w, "B", &[]);
        for (dir, n) in [("VAB", 1), ("VAB", 2), ("SPH", 3), ("SPH", 4)] {
            let p = a.game_dir().join(format!("saves/S/Ships/{dir}/C{n}.craft"));
            fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            fs::write(p, craft(&format!("C{n}"), &["pod"]).replace("VAB", dir)).expect("w");
        }
        sync(&w.paths).expect("sync");
        for (dir, n) in [("VAB", 1), ("VAB", 2), ("SPH", 3), ("SPH", 4)] {
            fs::remove_file(b.game_dir().join(format!("saves/S/Ships/{dir}/C{n}.craft")))
                .expect("rm");
        }
        sync(&w.paths).expect("sync");
        assert_eq!(live_names(&w).len(), 4);
        assert_eq!(pending(&w.paths).expect("pending").len(), 4);
    }

    #[test]
    fn copies_of_purged_craft_stay_gone() {
        let w = world();
        let a = instance(&w, "A", &[]);
        let b = instance(&w, "B", &[]);
        put(&a, "C.craft", &craft("C", &["pod"]));
        sync(&w.paths).expect("sync");
        running::mark(&b.dir, std::process::id(), "direct").expect("mark");
        let id = lib(&w).find("C").expect("craft").id.clone();
        delete(&w.paths, &id).expect("delete");
        sync(&w.paths).expect("sync");
        assert_eq!(empty_trash(&w.paths).expect("empty"), 1);
        running::clear(&b.dir, std::process::id());
        sync(&w.paths).expect("sync");
        sync(&w.paths).expect("sync again");
        assert!(files(&b).is_empty());
        assert!(live_names(&w).is_empty());
        // An untracked leftover is recognized too.
        let c = instance(&w, "C", &[]);
        put(&c, "C.craft", &craft("C", &["pod"]));
        sync(&w.paths).expect("sync");
        assert!(files(&c).is_empty() && live_names(&w).is_empty());
    }

    #[test]
    fn a_leftover_copy_never_takes_a_new_craft_name() {
        let w = world();
        let a = instance(&w, "A", &[]);
        let old = craft("X", &["pod"]);
        put(&a, "X.craft", &old);
        sync(&w.paths).expect("sync");
        let id = lib(&w).find("X").expect("craft").id.clone();
        delete(&w.paths, &id).expect("delete");
        sync(&w.paths).expect("sync");
        let new = craft("X", &["tank.1"]);
        put(&a, "X.craft", &new);
        sync(&w.paths).expect("sync");
        let b = instance(&w, "B", &[]);
        put(&b, "X.craft", &old);
        sync(&w.paths).expect("sync");
        assert_eq!(live_names(&w), ["X"]);
        assert_eq!(get(&b, "X.craft").as_deref(), Some(new.as_str()));
    }

    #[test]
    fn the_same_conflicting_edit_in_two_places_is_one_craft() {
        let w = world();
        let a = instance(&w, "A", &[]);
        let b = instance(&w, "B", &[]);
        let c = instance(&w, "C", &[]);
        put(&a, "O.craft", &craft("O", &["pod"]));
        sync(&w.paths).expect("sync");
        put(&a, "O.craft", &craft("O", &["pod", "tank.1"]));
        put(&b, "O.craft", &craft("O", &["pod", "pod"]));
        put(&c, "O.craft", &craft("O", &["pod", "pod"]));
        sync(&w.paths).expect("sync");
        assert_eq!(live_names(&w), ["O", "O (S)"]);
        assert_eq!(files(&c), ["O (S).craft", "O.craft"]);
    }

    #[test]
    fn ship_names_are_rewritten_keeping_line_endings() {
        let cases = [
            ("ship = A\r\nversion = 1\r\n", "ship = B\r\nversion = 1\r\n"),
            ("ship = A\nPART\n{\n}\n", "ship = B\nPART\n{\n}\n"),
            ("version = 1\n", "ship = B\nversion = 1\n"),
        ];
        for (input, want) in cases {
            assert_eq!(
                String::from_utf8(set_ship_name(input.as_bytes(), "B")).expect("utf8"),
                want
            );
        }
    }

    #[test]
    fn explains_missing_parts() {
        let mut p = Providers::default();
        p.0.insert("a".into(), ("NearFuture".into(), "Career".into()));
        p.0.insert("b".into(), ("NearFuture".into(), "Career".into()));
        let missing = ["a".to_owned(), "b".to_owned(), "c".to_owned()];
        assert_eq!(
            explain(&missing, &p),
            "NearFuture (2 parts, in Career); 1 part no instance has"
        );
    }
}
