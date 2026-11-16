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
