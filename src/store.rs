//! The mod store: every mod version unpacked once, in `<data>/store/<key>/`.
//!
//! Instances get their files from here with `std::fs::copy`, which reflinks
//! on btrfs and XFS: a mod used by ten instances takes the space of one, yet
//! each instance has its own files, so a mod writing its settings or caches
//! at runtime never touches another instance. On other filesystems the copy
//! is a plain copy, as without the store.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::cancel::Cancel;
use crate::download;
use crate::install::Source;
use crate::instance::{self, ModSource};
use crate::manual::{self, ManualMod};
use crate::paths::{Paths, slug};

/// Where unpacked mods live. Kept in the data directory so it shares a
/// filesystem (and reflinks) with the instances.
pub fn dir(paths: &Paths) -> PathBuf {
    paths.data.join("store")
}

/// The store key of a cached CKAN archive: its file name without `.zip`.
fn archive_key(archive: &Path) -> String {
    archive
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".into())
}

/// The store key of a zipped manual mod.
fn manual_key(m: &ManualMod) -> String {
    format!("manual-{}-{}", m.id, slug(&m.version))
}

/// Unpacks an archive into the store (once) and returns the unpacked
/// folder as an install source.
pub fn unpack(paths: &Paths, archive: &Path, key: &str) -> Result<Source> {
    let dest = dir(paths).join(key);
    if dest.is_dir() {
        return Ok(Source::Dir(dest));
    }
    let partial = dir(paths).join(format!(".{key}.partial"));
    if partial.exists() {
        fs::remove_dir_all(&partial).with_context(|| format!("clear {}", partial.display()))?;
    }
    fs::create_dir_all(&partial).with_context(|| format!("create {}", partial.display()))?;
    if let Err(e) = extract(archive, &partial) {
        let _ = fs::remove_dir_all(&partial);
        return Err(e);
    }
    match fs::rename(&partial, &dest) {
        Ok(()) => {}
        // Another install unpacked it at the same time; use that one.
        Err(_) if dest.is_dir() => {
            let _ = fs::remove_dir_all(&partial);
        }
        Err(e) => return Err(e).with_context(|| format!("move into {}", dest.display())),
    }
    Ok(Source::Dir(dest))
}

/// The install source for a downloaded CKAN archive.
pub fn ckan_source(paths: &Paths, archive: &Path) -> Result<Source> {
    unpack(paths, archive, &archive_key(archive))
}

/// The install source for a manual mod: folders are used as they are,
/// archives go through the store.
pub fn manual_source(paths: &Paths, m: &ManualMod) -> Result<Source> {
    match m.source(paths) {
        Source::Zip(zip) => unpack(paths, &zip, &manual_key(m)),
        dir => Ok(dir),
    }
}

fn extract(archive: &Path, dest: &Path) -> Result<()> {
    let file = File::open(archive).with_context(|| format!("open {}", archive.display()))?;
    let mut zip =
        zip::ZipArchive::new(file).with_context(|| format!("read zip {}", archive.display()))?;
    for i in 0..zip.len() {
        let mut f = zip
            .by_index(i)
            .with_context(|| format!("read zip entry {i}"))?;
        let name = f
            .name()
            .with_context(|| format!("read zip entry {i} name"))?
            .replace('\\', "/");
        let rel = name.trim_end_matches('/');
        if rel.is_empty() {
            continue;
        }
        if rel.starts_with('/') || rel.split('/').any(|p| p == ".." || p.is_empty()) {
            bail!("{} has an unsafe path {name:?}", archive.display());
        }
        let out = dest.join(rel);
        if f.is_dir() || name.ends_with('/') {
            fs::create_dir_all(&out).with_context(|| format!("create {}", out.display()))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let mut w = File::create(&out).with_context(|| format!("create {}", out.display()))?;
        io::copy(&mut f, &mut w).with_context(|| format!("extract {name}"))?;
    }
    Ok(())
}

/// Store keys and download archives that some instance still uses.
fn in_use(paths: &Paths) -> Result<(HashSet<String>, HashSet<PathBuf>)> {
    let mut keys = HashSet::new();
    let mut archives = HashSet::new();
    let library = manual::list(paths)?;
    for inst in instance::list(paths)? {
        for m in inst.registry()?.mods.values() {
            match m.source {
                ModSource::Ckan => {
                    let archive = download::cache_path(&paths.downloads(), &m.module);
                    keys.insert(archive_key(&archive));
                    archives.insert(archive);
                }
                ModSource::Manual => {
                    let id = m
                        .identifier()
                        .strip_prefix(manual::PREFIX)
                        .unwrap_or(m.identifier());
                    if let Some(lib) = library.iter().find(|l| l.id == id) {
                        keys.insert(manual_key(lib));
                    }
                }
            }
        }
    }
    Ok((keys, archives))
}

/// How much the store and the download cache hold.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub store_entries: usize,
    pub store_bytes: u64,
    pub download_files: usize,
    pub download_bytes: u64,
    /// What `clean` would remove.
    pub unused_entries: usize,
    pub unused_bytes: u64,
}

/// Adds up the store and download cache (apparent sizes: reflinked data
/// shared with instances is counted in full).
pub fn usage(paths: &Paths) -> Result<Usage> {
    let (keys, archives) = in_use(paths)?;
    let mut u = Usage::default();
    for (path, key) in entries(&dir(paths)) {
        let bytes = tree_size(&path);
        u.store_entries += 1;
        u.store_bytes += bytes;
        if !keys.contains(&key) {
            u.unused_entries += 1;
            u.unused_bytes += bytes;
        }
    }
    for (path, _) in entries(&paths.downloads()) {
        let bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        u.download_files += 1;
        u.download_bytes += bytes;
        if !archives.contains(&path) {
            u.unused_entries += 1;
            u.unused_bytes += bytes;
        }
    }
    Ok(u)
}

/// Removes unpacked mods and downloads that no instance uses. Returns the
/// number of entries and bytes removed.
pub fn clean(paths: &Paths) -> Result<(usize, u64)> {
    let (keys, archives) = in_use(paths)?;
    let mut removed = 0;
    let mut bytes = 0;
    for (path, key) in entries(&dir(paths)) {
        if !keys.contains(&key) {
            bytes += tree_size(&path);
            fs::remove_dir_all(&path).with_context(|| format!("remove {}", path.display()))?;
            removed += 1;
        }
    }
    for (path, _) in entries(&paths.downloads()) {
        if !archives.contains(&path) {
            bytes += fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
            removed += 1;
        }
    }
    Ok((removed, bytes))
}

/// Filesystems where a copy can share data blocks (reflink).
const REFLINK_FILESYSTEMS: [&str; 3] = ["xfs", "btrfs", "bcachefs"];

/// The filesystem type of a path, from `stat -f`.
pub fn filesystem(path: &Path) -> Option<String> {
    let out = std::process::Command::new("stat")
        .args(["-f", "-c", "%T"])
        .arg(path)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// What sharing space did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shared {
    /// Files replaced by copies that share the store's data.
    pub files: usize,
    pub bytes: u64,
    /// Mods whose unpacked copy couldn't be made (download not cached).
    pub skipped: Vec<String>,
}

/// Makes mods installed before the store existed share disk space with it:
/// every installed file identical to the store's copy is replaced by a
/// reflinked copy of it. Files a mod changed are left alone. Only runs on
/// filesystems with reflinks, where it saves space.
pub fn share(paths: &Paths, cancel: &Cancel, progress: &mut dyn FnMut(String)) -> Result<Shared> {
    let fs_type = filesystem(&paths.data).unwrap_or_default();
    if !REFLINK_FILESYSTEMS.contains(&fs_type.as_str()) {
        bail!(
            "{} is on {fs_type}, which can't share file data (btrfs and XFS can)",
            paths.data.display()
        );
    }
    let mut total = Shared::default();
    for inst in instance::list(paths)? {
        cancel.check()?;
        progress(format!("Sharing space in {}", inst.config.name));
        let r = share_instance(paths, &inst, cancel)?;
        total.files += r.files;
        total.bytes += r.bytes;
        total.skipped.extend(r.skipped);
    }
    Ok(total)
}

/// [`share`] for one instance, without the filesystem check.
pub fn share_instance(paths: &Paths, inst: &instance::Instance, cancel: &Cancel) -> Result<Shared> {
    let game = inst.game_dir();
    let library = manual::list(paths)?;
    let mut out = Shared::default();
    for (id, m) in &inst.registry()?.mods {
        cancel.check()?;
        let (source, directives) = match m.source {
            ModSource::Ckan => {
                if m.module.kind != crate::metadata::Kind::Package {
                    continue;
                }
                let Some(archive) = download::cached(&paths.downloads(), &m.module) else {
                    out.skipped
                        .push(format!("{}: {id} (download not cached)", inst.config.name));
                    continue;
                };
                (ckan_source(paths, &archive)?, m.module.directives())
            }
            ModSource::Manual => {
                let key = id.strip_prefix(manual::PREFIX).unwrap_or(id);
                let Some(lib) = library.iter().find(|l| l.id == key) else {
                    continue;
                };
                // Folder mods already copy from the library, not the store.
                if lib.is_dir {
                    continue;
                }
                (manual_source(paths, lib)?, lib.install.clone())
            }
        };
        let Source::Dir(root) = &source else { continue };
        let Ok(planned) = crate::install::plan(&directives, &source.entries()?) else {
            continue;
        };
        let installed: HashSet<&String> = m.files.iter().collect();
        for p in planned
            .iter()
            .filter(|p| !p.is_dir && installed.contains(&p.dest))
        {
            if crate::install::through_link(&game, &p.dest).is_some() {
                continue;
            }
            let target = game.join(&p.dest);
            let original = root.join(&p.source);
            if !same_content(&target, &original)? {
                continue;
            }
            let tmp = target.with_file_name(format!(
                ".{}.orbis-share",
                target
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
            ));
            fs::copy(&original, &tmp).with_context(|| format!("copy {}", original.display()))?;
            fs::rename(&tmp, &target).with_context(|| format!("replace {}", target.display()))?;
            out.files += 1;
            out.bytes += fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
        }
    }
    Ok(out)
}

/// Whether two files have the same bytes.
fn same_content(a: &Path, b: &Path) -> Result<bool> {
    use std::io::Read;
    let (Ok(ma), Ok(mb)) = (fs::metadata(a), fs::metadata(b)) else {
        return Ok(false);
    };
    if ma.len() != mb.len() || !ma.is_file() || !mb.is_file() {
        return Ok(false);
    }
    let mut fa = File::open(a).with_context(|| format!("open {}", a.display()))?;
    let mut fb = File::open(b).with_context(|| format!("open {}", b.display()))?;
    let mut ba = vec![0; 1 << 16];
    let mut bb = vec![0; 1 << 16];
    loop {
        let n = fa
            .read(&mut ba)
            .with_context(|| format!("read {}", a.display()))?;
        if n == 0 {
            return Ok(true);
        }
        fb.read_exact(&mut bb[..n])
            .with_context(|| format!("read {}", b.display()))?;
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
    }
}

/// Visible entries of a directory: (path, name).
fn entries(dir: &Path) -> Vec<(PathBuf, String)> {
    let Ok(read) = fs::read_dir(dir) else {
        return vec![];
    };
    read.flatten()
        .map(|e| (e.path(), e.file_name().to_string_lossy().into_owned()))
        .filter(|(_, n)| !n.starts_with('.'))
        .map(|(p, n)| {
            let key = n.strip_suffix(".zip").unwrap_or(&n).to_owned();
            (p, key)
        })
        .collect()
}

fn tree_size(path: &Path) -> u64 {
    if path.is_file() {
        return fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    }
    Source::Dir(path.to_path_buf())
        .entries()
        .map(|es| {
            es.iter()
                .filter(|e| !e.is_dir)
                .map(|e| {
                    fs::metadata(path.join(&e.name))
                        .map(|m| m.len())
                        .unwrap_or(0)
                })
                .sum()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn shares_only_unchanged_files() {
        use crate::instance::{CopyMode, CreateOptions, InstalledMod};
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = Paths::new(Some(&tmp.path().join("orbis"))).expect("paths");
        let ksp = tmp.path().join("ksp");
        fs::create_dir_all(ksp.join("GameData/Squad")).expect("mkdir");
        fs::write(ksp.join("KSP_x64.exe"), b"").expect("write");
        fs::write(ksp.join("readme.txt"), "Version 1.12.5\n").expect("write");
        let inst = instance::create(
            &paths,
            &CreateOptions {
                name: "s".into(),
                source: ksp,
                stock_only: true,
                with_saves: false,
                mode: CopyMode::Link,
            },
            &mut |_, _| {},
        )
        .expect("create");
        // A mod installed "before the store": files written straight into the
        // instance, one of them since changed by the mod.
        let module: crate::metadata::Module = serde_json::from_value(serde_json::json!({
            "identifier": "Mod", "version": "1", "download": "https://example.invalid/mod.zip"
        }))
        .expect("module");
        let archive = download::cache_path(&paths.downloads(), &module);
        fs::create_dir_all(archive.parent().expect("parent")).expect("mkdir");
        {
            let mut w = zip::ZipWriter::new(File::create(&archive).expect("create"));
            let o = zip::write::SimpleFileOptions::default();
            w.start_file("GameData/Mod/same.cfg", o).expect("start");
            w.write_all(b"same").expect("write");
            w.start_file("GameData/Mod/settings.cfg", o).expect("start");
            w.write_all(b"default").expect("write");
            w.finish().expect("finish");
        }
        let gd = inst.game_dir().join("GameData/Mod");
        fs::create_dir_all(&gd).expect("mkdir");
        fs::write(gd.join("same.cfg"), b"same").expect("write");
        fs::write(gd.join("settings.cfg"), b"changed by the mod").expect("write");
        let mut reg = inst.registry().expect("registry");
        reg.mods.insert(
            "Mod".into(),
            InstalledMod {
                source: ModSource::Ckan,
                auto_installed: false,
                installed_at: 0,
                files: vec![
                    "GameData/Mod/same.cfg".into(),
                    "GameData/Mod/settings.cfg".into(),
                ],
                dirs: vec!["GameData/Mod".into()],
                module,
            },
        );
        inst.save_registry(&reg).expect("save");

        let r = share_instance(&paths, &inst, &Cancel::default()).expect("share");
        assert_eq!(r.files, 1);
        assert_eq!(fs::read(gd.join("same.cfg")).expect("read"), b"same");
        assert_eq!(
            fs::read(gd.join("settings.cfg")).expect("read"),
            b"changed by the mod"
        );
    }

    #[test]
    fn unpacks_once_and_matches_the_zip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = Paths::new(Some(tmp.path())).expect("paths");
        let zip_path = tmp.path().join("a.zip");
        {
            let mut w = zip::ZipWriter::new(File::create(&zip_path).expect("create"));
            let o = zip::write::SimpleFileOptions::default();
            w.start_file("Mod-1/GameData/Mod/a.cfg", o).expect("start");
            w.write_all(b"a").expect("write");
            w.add_directory("Mod-1/GameData/Mod/Empty/", o)
                .expect("dir");
            w.finish().expect("finish");
        }
        let src = ckan_source(&paths, &zip_path).expect("unpack");
        let Source::Dir(d) = &src else {
            panic!("store gives folders")
        };
        assert_eq!(
            fs::read(d.join("Mod-1/GameData/Mod/a.cfg")).expect("read"),
            b"a"
        );
        assert!(d.join("Mod-1/GameData/Mod/Empty").is_dir());

        // Same plan from the zip and from the store.
        let directives = [crate::metadata::InstallDirective::default_for("Mod")];
        let from_zip = crate::install::plan(
            &directives,
            &Source::Zip(zip_path.clone()).entries().expect("e"),
        )
        .expect("plan zip");
        let from_store =
            crate::install::plan(&directives, &src.entries().expect("e")).expect("plan store");
        let files = |p: &[crate::install::Planned]| -> Vec<String> {
            p.iter()
                .filter(|x| !x.is_dir)
                .map(|x| x.dest.clone())
                .collect()
        };
        assert_eq!(files(&from_zip), files(&from_store));

        // A second call reuses the unpacked copy.
        fs::remove_file(&zip_path).expect("remove zip");
        ckan_source(&paths, &zip_path).expect("reuse");
    }
}
