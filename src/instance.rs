//! Instances: self-contained KSP game directories, each with its own mods,
//! saves and settings (like Prism Launcher instances).
//!
//! Layout of `<data>/instances/<id>/`:
//!
//! ```text
//! instance.toml   settings (name, game version, compatible versions, launch)
//! registry.json   installed mods and the files they own
//! game/           the KSP directory
//! prefix/         an isolated Proton prefix, when the instance uses one
//! ```

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::game;
use crate::install::{self, InstalledFiles};
use crate::metadata::Module;
use crate::paths::{self, Paths, slug};
use crate::version::{GameVersion, ModVersion};

/// Settings stored in `instance.toml`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct InstanceConfig {
    pub name: String,
    /// The game version detected when the instance was created.
    pub game_version: GameVersion,
    /// Extra game versions whose mods count as compatible.
    #[serde(default)]
    pub compat: Vec<GameVersion>,
    /// Offer releases marked `testing` as well as `stable`.
    #[serde(default)]
    pub allow_testing: bool,
    /// Unix seconds.
    pub created: u64,
    /// Where the game files were copied from.
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub last_played: Option<u64>,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub launch: LaunchConfig,
    /// The game install this instance links to. `None` for a full copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<PathBuf>,
}

/// How an instance starts.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct LaunchConfig {
    /// Extra arguments for KSP_x64.exe (for example `-popupwindow`).
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables (for example `DXVK_HUD=fps`).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub prefix: PrefixMode,
}

/// Which Wine prefix the instance runs in.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PrefixMode {
    /// Steam's own KSP prefix. KSP keeps saves and settings in the game
    /// directory, so sharing it between instances is safe.
    #[default]
    Steam,
    /// A prefix inside the instance directory, created on first launch.
    Instance,
}

/// Where an installed mod came from.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModSource {
    Ckan,
    Manual,
}

/// One installed mod.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstalledMod {
    pub source: ModSource,
    /// Installed only to satisfy another mod's dependency.
    pub auto_installed: bool,
    pub installed_at: u64,
    pub files: Vec<String>,
    pub dirs: Vec<String>,
    /// The metadata it was installed from, kept so relationships still work
    /// if the release later disappears from the index.
    pub module: Module,
}

impl InstalledMod {
    pub fn identifier(&self) -> &str {
        &self.module.identifier
    }

    pub fn version(&self) -> &ModVersion {
        &self.module.version
    }

    pub fn installed_files(&self) -> InstalledFiles {
        InstalledFiles {
            files: self.files.clone(),
            dirs: self.dirs.clone(),
        }
    }
}

/// The installed mods of one instance, stored in `registry.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub mods: BTreeMap<String, InstalledMod>,
}

impl Registry {
    /// Identifiers of installed mods that depend on `id` (directly, or
    /// through something it provides).
    pub fn required_by(&self, id: &str) -> Vec<String> {
        let Some(target) = self.mods.get(id) else {
            return vec![];
        };
        let m = &target.module;
        self.mods
            .iter()
            .filter(|(other, _)| other.as_str() != id)
            .filter(|(_, o)| {
                o.module
                    .depends
                    .iter()
                    .any(|r| r.matches(&m.identifier, Some(&m.version), &m.provides))
            })
            .map(|(other, _)| other.clone())
            .collect()
    }

    /// Folded file path -> identifier of the mod that owns it.
    pub fn owners(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for (id, m) in &self.mods {
            for f in &m.files {
                out.insert(install::fold(f), id.clone());
            }
        }
        out
    }
}

/// One instance on disk.
#[derive(Clone, Debug)]
pub struct Instance {
    /// Directory name, used to address the instance.
    pub id: String,
    pub dir: PathBuf,
    pub config: InstanceConfig,
}

impl Instance {
    pub fn game_dir(&self) -> PathBuf {
        self.dir.join("game")
    }

    pub fn prefix_dir(&self) -> PathBuf {
        self.dir.join("prefix")
    }

    fn config_path(dir: &Path) -> PathBuf {
        dir.join("instance.toml")
    }

    fn registry_path(&self) -> PathBuf {
        self.dir.join("registry.json")
    }

    /// Loads the instance in `dir`.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = Self::config_path(dir);
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let config: InstanceConfig =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        let id = dir
            .file_name()
            .context("instance directory has no name")?
            .to_string_lossy()
            .into_owned();
        Ok(Self {
            id,
            dir: dir.to_path_buf(),
            config,
        })
    }

    /// Writes `instance.toml`.
    pub fn save(&self) -> Result<()> {
        if self.config.name.trim().is_empty() {
            bail!("instance name must not be empty");
        }
        let text = toml::to_string_pretty(&self.config).context("serialize instance config")?;
        paths::write_atomic(&Self::config_path(&self.dir), text.as_bytes())
    }

    /// Reads `registry.json` (empty when missing).
    pub fn registry(&self) -> Result<Registry> {
        let path = self.registry_path();
        match File::open(&path) {
            Ok(f) => serde_json::from_reader(BufReader::new(f))
                .with_context(|| format!("parse {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
            Err(e) => Err(e).with_context(|| format!("open {}", path.display())),
        }
    }

    /// Writes `registry.json`.
    pub fn save_registry(&self, reg: &Registry) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(reg).context("serialize registry")?;
        paths::write_atomic(&self.registry_path(), &bytes)
    }

    /// The game versions mods are checked against.
    pub fn compat_versions(&self) -> Vec<GameVersion> {
        let mut v = vec![self.config.game_version.without_build()];
        v.extend(self.config.compat.iter().copied());
        v
    }

    /// DLC present in the game directory.
    pub fn dlc(&self) -> Vec<game::Dlc> {
        game::detect_dlc(&self.game_dir())
    }
}

/// Every instance, sorted by name.
pub fn list(paths: &Paths) -> Result<Vec<Instance>> {
    let dir = paths.instances();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
    };
    let mut out = Vec::new();
    for entry in entries {
        let path = entry.context("read instances directory")?.path();
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        {
            continue;
        }
        if Instance::config_path(&path).is_file() {
            match Instance::load(&path) {
                Ok(i) => out.push(i),
                Err(e) => log::warn!("skip {}: {e:#}", path.display()),
            }
        }
    }
    out.sort_by_key(|i| i.config.name.to_lowercase());
    Ok(out)
}

/// Finds an instance by id or (case-insensitive) name.
pub fn find(paths: &Paths, key: &str) -> Result<Instance> {
    let all = list(paths)?;
    if let Some(i) = all
        .iter()
        .find(|i| i.id == key)
        .or_else(|| all.iter().find(|i| i.config.name.eq_ignore_ascii_case(key)))
    {
        return Ok(i.clone());
    }
    let names: Vec<&str> = all.iter().map(|i| i.config.name.as_str()).collect();
    bail!("no instance {key:?} (have: {})", names.join(", "))
}

/// How an instance gets its game files.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CopyMode {
    /// Link the stock game from the source install and own only mods,
    /// saves and settings. The source is never written to, and the instance
    /// follows it when it updates.
    #[default]
    Link,
    /// A full, independent copy that keeps its game version. On reflink
    /// filesystems (btrfs, XFS) the copy shares data blocks and is instant.
    Copy,
    /// Hard links, for filesystems without reflinks. Stock files are shared
    /// with the source, so never edit them in place.
    Hardlink,
}

/// Options for creating an instance.
#[derive(Clone, Debug)]
pub struct CreateOptions {
    pub name: String,
    /// A KSP directory (usually Steam's) or another instance's game dir.
    pub source: PathBuf,
    /// Copy only the stock game (skip non-stock GameData, saves, logs).
    pub stock_only: bool,
    /// Copy user saves too.
    pub with_saves: bool,
    pub mode: CopyMode,
}

/// Progress while copying: (files done, files total).
pub type CopyProgress<'a> = &'a mut dyn FnMut(usize, usize);

/// Creates an instance by copying a game directory.
pub fn create(paths: &Paths, opts: &CreateOptions, progress: CopyProgress) -> Result<Instance> {
    if opts.name.trim().is_empty() {
        bail!("instance name must not be empty");
    }
    let version = game::detect_version(&opts.source)?;
    let root = paths.instances();
    fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
    let id = unique_id(&root, &slug(&opts.name));
    let dir = root.join(&id);
    let partial = root.join(format!(".{id}.partial"));
    if partial.exists() {
        fs::remove_dir_all(&partial).with_context(|| format!("clear {}", partial.display()))?;
    }
    let base = match opts.mode {
        CopyMode::Link => Some(
            fs::canonicalize(&opts.source)
                .with_context(|| format!("resolve {}", opts.source.display()))?,
        ),
        CopyMode::Copy | CopyMode::Hardlink => None,
    };
    let result = (|| -> Result<Instance> {
        match &base {
            Some(base) => build_linked(base, &partial.join("game"), opts.with_saves, progress)?,
            None => {
                let files = collect(&opts.source, opts)?;
                copy_tree(
                    &opts.source,
                    &partial.join("game"),
                    &files,
                    opts.mode,
                    progress,
                )?;
            }
        }
        for d in [
            "Ships/VAB",
            "Ships/SPH",
            "Ships/@thumbs/VAB",
            "Ships/@thumbs/SPH",
            "saves/scenarios",
            "saves/training",
        ] {
            fs::create_dir_all(partial.join("game").join(d))
                .with_context(|| format!("create {d}"))?;
        }
        let inst = Instance {
            id: id.clone(),
            dir: partial.clone(),
            config: InstanceConfig {
                name: opts.name.trim().to_owned(),
                game_version: version,
                compat: game::default_compat(version),
                allow_testing: false,
                created: paths::now_secs(),
                source: opts.source.display().to_string(),
                last_played: None,
                notes: String::new(),
                launch: LaunchConfig::default(),
                base: base.clone(),
            },
        };
        inst.save()?;
        inst.save_registry(&Registry::default())?;
        Ok(inst)
    })();
    match result {
        Ok(mut inst) => {
            fs::rename(&partial, &dir)
                .with_context(|| format!("move instance into {}", dir.display()))?;
            inst.dir = dir;
            Ok(inst)
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&partial);
            Err(e)
        }
    }
}

/// Copies an instance: game directory, registry and settings.
pub fn clone_instance(
    paths: &Paths,
    src: &Instance,
    name: &str,
    with_saves: bool,
    progress: CopyProgress,
) -> Result<Instance> {
    let opts = CreateOptions {
        name: name.to_owned(),
        source: src.game_dir(),
        stock_only: false,
        with_saves,
        // Copying a linked instance copies its links as links.
        mode: CopyMode::Copy,
    };
    let mut inst = create(paths, &opts, progress)?;
    inst.config = InstanceConfig {
        name: opts.name.trim().to_owned(),
        created: inst.config.created,
        source: format!("clone of {}", src.config.name),
        last_played: None,
        ..src.config.clone()
    };
    inst.save()?;
    inst.save_registry(&src.registry()?)?;
    Ok(inst)
}

/// Deletes an instance directory and everything in it.
pub fn delete(inst: &Instance) -> Result<()> {
    if !Instance::config_path(&inst.dir).is_file() {
        bail!("{} does not look like an instance", inst.dir.display());
    }
    fs::remove_dir_all(&inst.dir).with_context(|| format!("delete {}", inst.dir.display()))
}

/// Top-level entries a linked instance owns: copied at creation, never linked.
const OWNED: [&str; 7] = [
    "GameData",
    "saves",
    "Ships",
    "Missions",
    "UserLoadingScreens",
    "settings.cfg",
    "Physics.cfg",
];

/// Top-level entries the game or other tools write: neither copied nor linked.
const GENERATED: [&str; 7] = [
    "Logs",
    "Screenshots",
    "thumbs",
    "KSP.log",
    "PartDatabase.cfg",
    "CKAN",
    "orbis-launch.log",
];

/// Stock GameData folders a linked instance links from its base.
const STOCK_GAMEDATA: [&str; 2] = ["Squad", "SquadExpansion"];

fn is_in(list: &[&str], name: &str) -> bool {
    list.iter().any(|n| n.eq_ignore_ascii_case(name))
}

/// Builds a linked game directory: owned entries copied from `base`, stock
/// GameData and every other top-level entry symlinked to it.
fn build_linked(base: &Path, game: &Path, with_saves: bool, progress: CopyProgress) -> Result<()> {
    let opts = CreateOptions {
        name: String::new(),
        source: base.to_path_buf(),
        stock_only: true,
        with_saves,
        mode: CopyMode::Copy,
    };
    let owned: Vec<(String, fs::FileType)> = collect(base, &opts)?
        .into_iter()
        .filter(|(rel, _)| {
            let first = rel.split('/').next().unwrap_or("");
            is_in(&OWNED, first) && !first.eq_ignore_ascii_case("GameData")
        })
        .collect();
    copy_tree(base, game, &owned, CopyMode::Copy, progress)?;
    fs::create_dir_all(game.join("GameData")).context("create GameData")?;
    link_missing(base, game)?;
    Ok(())
}

/// Creates the links a linked game directory should have and removes links
/// whose target is gone. Returns how many links changed.
fn link_missing(base: &Path, game: &Path) -> Result<usize> {
    let mut changed = 0;
    let mut wanted: Vec<(PathBuf, PathBuf)> = Vec::new();
    for entry in fs::read_dir(base).with_context(|| format!("read {}", base.display()))? {
        let name = entry
            .with_context(|| format!("read {}", base.display()))?
            .file_name();
        let n = name.to_string_lossy();
        if is_in(&OWNED, &n) || is_in(&GENERATED, &n) {
            continue;
        }
        wanted.push((base.join(&name), game.join(&name)));
    }
    for stock in STOCK_GAMEDATA {
        let target = base.join("GameData").join(stock);
        if target.is_dir() {
            wanted.push((target, game.join("GameData").join(stock)));
        }
    }
    for (target, link) in wanted {
        match fs::symlink_metadata(&link) {
            Ok(m) if m.file_type().is_symlink() => {
                if fs::read_link(&link).ok().as_deref() == Some(target.as_path()) {
                    continue;
                }
                fs::remove_file(&link).with_context(|| format!("replace {}", link.display()))?;
            }
            // Something real is there; the user put it there on purpose.
            Ok(_) => continue,
            Err(_) => {}
        }
        std::os::unix::fs::symlink(&target, &link)
            .with_context(|| format!("link {}", link.display()))?;
        changed += 1;
    }
    for dir in [game.to_path_buf(), game.join("GameData")] {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let dangling = entry.file_type().is_ok_and(|t| t.is_symlink()) && !p.exists();
            if dangling {
                fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))?;
                changed += 1;
            }
        }
    }
    Ok(changed)
}

/// Brings a linked instance in line with its base install: adds links for
/// new files, drops links to removed ones, and records the base's current
/// game version. Does nothing for a full copy. Run it before launching.
pub fn sync_links(inst: &mut Instance) -> Result<()> {
    let Some(base) = inst.config.base.clone() else {
        return Ok(());
    };
    if !game::is_game_dir(&base) {
        bail!(
            "{} links to {}, which is no longer a KSP install",
            inst.config.name,
            base.display()
        );
    }
    let changed = link_missing(&base, &inst.game_dir())?;
    if changed > 0 {
        log::info!(
            "{}: updated {changed} links to {}",
            inst.config.name,
            base.display()
        );
    }
    let version = game::detect_version(&inst.game_dir())?;
    if version != inst.config.game_version {
        log::info!(
            "{}: game version {} -> {version}",
            inst.config.name,
            inst.config.game_version
        );
        inst.config.game_version = version;
        inst.save()?;
    }
    Ok(())
}

fn unique_id(root: &Path, base: &str) -> String {
    let mut id = base.to_owned();
    let mut n = 2;
    while root.join(&id).exists() {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

/// Whether a relative path (with `/`) is copied into a new instance.
/// Steam Cloud's marker file: it belongs to the Steam install, and copying
/// it would make Steam treat an instance's saves as cloud saves.
pub const STEAM_CLOUD_FILE: &str = "steam_autocloud.vdf";

fn wanted(rel: &str, opts: &CreateOptions) -> bool {
    let lower = rel.to_lowercase();
    if lower.rsplit('/').next() == Some(STEAM_CLOUD_FILE) {
        return false;
    }
    let first = lower.split('/').next().unwrap_or("");
    if [
        "logs",
        "ksp.log",
        "ckan",
        "partdatabase.cfg",
        "orbis-launch.log",
    ]
    .contains(&first)
    {
        return false;
    }
    if (first == "screenshots" || first == "thumbs") && lower.contains('/') {
        return false;
    }
    if first == "saves" && !opts.with_saves {
        let second = lower.split('/').nth(1);
        if second.is_some_and(|s| s != "scenarios" && s != "training") {
            return false;
        }
    }
    if opts.stock_only && first == "gamedata" {
        let second = lower.split('/').nth(1);
        if second.is_some_and(|s| s != "squad" && s != "squadexpansion") {
            return false;
        }
    }
    true
}

/// Relative paths to copy, directories before their contents.
fn collect(src: &Path, opts: &CreateOptions) -> Result<Vec<(String, fs::FileType)>> {
    let mut out = Vec::new();
    let mut stack = vec![String::new()];
    while let Some(rel) = stack.pop() {
        let dir = if rel.is_empty() {
            src.to_path_buf()
        } else {
            src.join(&rel)
        };
        let mut entries: Vec<_> = fs::read_dir(&dir)
            .with_context(|| format!("read {}", dir.display()))?
            .collect::<Result<_, _>>()
            .with_context(|| format!("read {}", dir.display()))?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if !wanted(&child, opts) {
                continue;
            }
            let ft = e.file_type().with_context(|| format!("stat {child}"))?;
            if ft.is_dir() {
                stack.push(child.clone());
            }
            out.push((child, ft));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn copy_tree(
    src: &Path,
    dest: &Path,
    files: &[(String, fs::FileType)],
    mode: CopyMode,
    progress: CopyProgress,
) -> Result<()> {
    fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    let total = files.len();
    for (i, (rel, ft)) in files.iter().enumerate() {
        let from = src.join(rel);
        let to = dest.join(rel);
        if ft.is_dir() {
            fs::create_dir_all(&to).with_context(|| format!("create {}", to.display()))?;
        } else if ft.is_symlink() {
            let target =
                fs::read_link(&from).with_context(|| format!("read link {}", from.display()))?;
            std::os::unix::fs::symlink(&target, &to)
                .with_context(|| format!("link {}", to.display()))?;
        } else if mode == CopyMode::Hardlink && fs::hard_link(&from, &to).is_ok() {
            // Linked.
        } else {
            fs::copy(&from, &to).with_context(|| format!("copy {rel}"))?;
        }
        if i % 64 == 0 || i + 1 == total {
            progress(i + 1, total);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_ksp(dir: &Path) {
        for d in [
            "GameData/Squad/Parts",
            "GameData/SquadExpansion/Serenity",
            "GameData/SomeMod",
            "saves/scenarios",
            "saves/MyCareer",
            "Logs",
            "CKAN",
        ] {
            fs::create_dir_all(dir.join(d)).expect("mkdir");
        }
        for f in [
            "KSP_x64.exe",
            "GameData/Squad/Parts/part.cfg",
            "GameData/SomeMod/mod.dll",
            "GameData/ModuleManager.ConfigCache",
            "saves/MyCareer/persistent.sfs",
            "saves/scenarios/s.sfs",
            "saves/steam_autocloud.vdf",
            "Logs/a.log",
            "KSP.log",
            "settings.cfg",
        ] {
            fs::write(dir.join(f), f).expect("write");
        }
        fs::write(dir.join("readme.txt"), "Version 1.12.5\n").expect("write");
        fs::write(dir.join("buildID64.txt"), "build id = 03190\n").expect("write");
    }

    #[test]
    fn creates_linked_instance() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ksp = tmp.path().join("ksp");
        fake_ksp(&ksp);
        fs::create_dir_all(ksp.join("KSP_x64_Data")).expect("mkdir");
        let paths = Paths::new(Some(&tmp.path().join("orbis"))).expect("paths");
        let opts = CreateOptions {
            name: "Linked".into(),
            source: ksp.clone(),
            stock_only: true,
            with_saves: false,
            mode: CopyMode::Link,
        };
        let mut inst = create(&paths, &opts, &mut |_, _| {}).expect("create");
        let g = inst.game_dir();
        let is_link = |p: &Path| fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink());
        assert_eq!(
            inst.config.base.as_deref(),
            Some(fs::canonicalize(&ksp).expect("canon").as_path())
        );
        assert!(is_link(&g.join("KSP_x64.exe")));
        assert!(is_link(&g.join("KSP_x64_Data")));
        assert!(is_link(&g.join("GameData/Squad")));
        assert!(is_link(&g.join("GameData/SquadExpansion")));
        assert!(!is_link(&g.join("GameData")));
        assert!(!is_link(&g.join("settings.cfg")) && g.join("settings.cfg").is_file());
        assert!(g.join("saves/scenarios/s.sfs").is_file() && !is_link(&g.join("saves")));
        assert!(!g.join("saves/steam_autocloud.vdf").exists());
        assert!(!g.join("saves/MyCareer").exists());
        assert!(!g.join("GameData/SomeMod").exists());
        assert!(!g.join("KSP.log").exists() && !g.join("Logs").exists());
        assert_eq!(inst.config.game_version.to_string(), "1.12.5.3190");
        assert_eq!(game::detect_dlc(&g).len(), 0);

        // The base gains a file and loses one; sync follows it.
        fs::write(ksp.join("NewThing.dll"), b"x").expect("write");
        fs::remove_file(ksp.join("buildID64.txt")).expect("remove");
        fs::write(ksp.join("readme.txt"), "Version 1.12.6\n").expect("write");
        sync_links(&mut inst).expect("sync");
        assert!(is_link(&g.join("NewThing.dll")));
        assert!(fs::symlink_metadata(g.join("buildID64.txt")).is_err());
        assert_eq!(inst.config.game_version.to_string(), "1.12.6");

        // A real file the user put in place of a link is left alone.
        fs::remove_file(g.join("NewThing.dll")).expect("remove");
        fs::write(g.join("NewThing.dll"), b"mine").expect("write");
        sync_links(&mut inst).expect("sync");
        assert_eq!(fs::read(g.join("NewThing.dll")).expect("read"), b"mine");

        // Deleting the instance removes links, not what they point to.
        delete(&inst).expect("delete");
        assert!(ksp.join("KSP_x64.exe").is_file());
        assert!(ksp.join("GameData/Squad/Parts/part.cfg").is_file());
    }

    #[test]
    fn creates_stock_instance_and_clones() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ksp = tmp.path().join("ksp");
        fake_ksp(&ksp);
        let paths = Paths::new(Some(&tmp.path().join("orbis"))).expect("paths");
        let opts = CreateOptions {
            name: "Stock Play".into(),
            source: ksp.clone(),
            stock_only: true,
            with_saves: false,
            mode: CopyMode::Copy,
        };
        let inst = create(&paths, &opts, &mut |_, _| {}).expect("create");
        let g = inst.game_dir();
        assert_eq!(inst.id, "stock-play");
        assert_eq!(inst.config.game_version.to_string(), "1.12.5.3190");
        assert!(g.join("GameData/Squad/Parts/part.cfg").is_file());
        assert!(g.join("settings.cfg").is_file());
        assert!(g.join("saves/scenarios/s.sfs").is_file());
        assert!(!g.join("GameData/SomeMod").exists());
        assert!(!g.join("GameData/ModuleManager.ConfigCache").exists());
        assert!(!g.join("saves/MyCareer").exists());
        assert!(!g.join("Logs").exists());
        assert!(!g.join("KSP.log").exists());
        assert!(g.join("Ships/VAB").is_dir());

        let again = create(&paths, &opts, &mut |_, _| {}).expect("second");
        assert_eq!(again.id, "stock-play-2");

        let copy = clone_instance(&paths, &inst, "Copy", true, &mut |_, _| {}).expect("clone");
        assert_eq!(copy.config.name, "Copy");
        assert_eq!(copy.config.game_version, inst.config.game_version);
        assert_eq!(list(&paths).expect("list").len(), 3);
        assert_eq!(find(&paths, "copy").expect("find").id, "copy");

        delete(&copy).expect("delete");
        assert_eq!(list(&paths).expect("list").len(), 2);
    }
}
