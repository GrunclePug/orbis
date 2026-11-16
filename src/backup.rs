//! Save backups: snapshots of an instance's `saves` folder, taken before mod
//! changes and launches, kept in `<instance>/backups/<id>/`.
//!
//! Copies go through `std::fs::copy`, which reflinks on btrfs and XFS, so a
//! backup there costs no space until the save changes.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::install::Source;
use crate::instance::{Instance, STEAM_CLOUD_FILE};
use crate::paths;

/// One backup.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Backup {
    /// Directory name (sortable: creation time, then reason).
    pub id: String,
    pub created: u64,
    pub reason: String,
    /// Save folders it contains.
    pub saves: Vec<String>,
    pub bytes: u64,
    /// Size and newest modification time of the saves.
    pub fingerprint: (u64, u64),
    /// SHA-256 of every save file's path and contents, to skip duplicates.
    /// Empty in backups made before it existed.
    #[serde(default)]
    pub content: String,
}

fn root(inst: &Instance) -> PathBuf {
    inst.dir.join("backups")
}

/// The user's save folders (stock scenarios and training excluded).
fn user_saves(saves: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(saves) else {
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

/// Every file under the user's saves, relative to `saves`.
fn save_files(saves: &Path, names: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for n in names {
        for e in Source::Dir(saves.join(n)).entries()? {
            let cloud = e
                .name
                .rsplit('/')
                .next()
                .is_some_and(|f| f.eq_ignore_ascii_case(STEAM_CLOUD_FILE));
            if !e.is_dir && !cloud {
                out.push(format!("{n}/{}", e.name));
            }
        }
    }
    Ok(out)
}

/// A hash of the saves' paths and contents: two backups are the same only
/// when every byte is.
fn content_hash(saves: &Path, files: &[String]) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut sorted: Vec<&String> = files.iter().collect();
    sorted.sort();
    let mut h = Sha256::new();
    for f in sorted {
        let bytes = fs::read(saves.join(f)).with_context(|| format!("read {f}"))?;
        h.update(f.as_bytes());
        h.update([0]);
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    Ok(crate::download::hex(&h.finalize()))
}

fn fingerprint(saves: &Path, files: &[String]) -> (u64, u64) {
    let mut size = 0;
    let mut newest = 0;
    for f in files {
        if let Ok(m) = fs::metadata(saves.join(f)) {
            size += m.len();
            let t = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            newest = newest.max(t);
        }
    }
    (size, newest)
}

/// Every backup of an instance, newest first.
pub fn list(inst: &Instance) -> Result<Vec<Backup>> {
    let dir = root(inst);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let meta = e.path().join("backup.toml");
        let Ok(text) = fs::read_to_string(&meta) else {
            continue;
        };
        match toml::from_str::<Backup>(&text) {
            Ok(b) => out.push(b),
            Err(err) => log::warn!("skip {}: {err}", meta.display()),
        }
    }
    out.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(out)
}

/// Backs up the instance's saves. Returns `None` when there are no saves,
/// or when nothing changed since the newest backup.
pub fn create(inst: &Instance, reason: &str) -> Result<Option<Backup>> {
    let saves = inst.game_dir().join("saves");
    let names = user_saves(&saves);
    if names.is_empty() {
        return Ok(None);
    }
    let files = save_files(&saves, &names)?;
    let print = fingerprint(&saves, &files);
    let content = content_hash(&saves, &files)?;
    if list(inst)?
        .first()
        .is_some_and(|b| b.content == content && b.saves == names)
    {
        return Ok(None);
    }
    let created = paths::now_secs();
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let base_id = format!("{millis:014}-{}", paths::slug(reason));
    let mut id = base_id.clone();
    let mut n = 2;
    while root(inst).join(&id).exists() {
        id = format!("{base_id}-{n}");
        n += 1;
    }
    let dir = root(inst).join(&id);
    let result = (|| -> Result<Backup> {
        for f in &files {
            let to = dir.join("saves").join(f);
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            fs::copy(saves.join(f), &to).with_context(|| format!("copy {f}"))?;
        }
        let b = Backup {
            id: id.clone(),
            created,
            reason: reason.to_owned(),
            saves: names,
            bytes: print.0,
            fingerprint: print,
            content,
        };
        let text = toml::to_string_pretty(&b).context("serialize backup")?;
        paths::write_atomic(&dir.join("backup.toml"), text.as_bytes())?;
        Ok(b)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&dir);
    }
    result.map(Some)
}

/// Deletes all but the newest `keep` backups.
pub fn prune(inst: &Instance, keep: usize) -> Result<usize> {
    let all = list(inst)?;
    let mut removed = 0;
    for b in all.iter().skip(keep) {
        fs::remove_dir_all(root(inst).join(&b.id))
            .with_context(|| format!("delete backup {}", b.id))?;
        removed += 1;
    }
    Ok(removed)
}

/// Backs up (when `keep` > 0) and prunes to `keep`. Errors are returned so
/// callers can decide whether a failed backup should stop them.
pub fn snapshot(inst: &Instance, reason: &str, keep: usize) -> Result<Option<Backup>> {
    if keep == 0 {
        return Ok(None);
    }
    let b = create(inst, reason)?;
    prune(inst, keep)?;
    Ok(b)
}

/// Restores one save folder, or every save in the backup when `save` is
/// `None`. The current saves are backed up first, so a restore can be
/// undone. Saves not in the backup are left alone.
pub fn restore(inst: &Instance, id: &str, save: Option<&str>) -> Result<()> {
    crate::running::refuse_if_running(inst)?;
    let b = list(inst)?
        .into_iter()
        .find(|b| b.id == id)
        .with_context(|| format!("no backup {id:?}"))?;
    let names: Vec<String> = match save {
        Some(s) if b.saves.iter().any(|n| n == s) => vec![s.to_owned()],
        Some(s) => bail!("backup {id} has no save {s:?}"),
        None => b.saves.clone(),
    };
    create(inst, "before restore")?;
    // The restored saves' craft are read afresh, not taken as edits or deletions.
    crate::crafts::forget_saves(&inst.dir, &names)?;
    let from = root(inst).join(&b.id).join("saves");
    let to = inst.game_dir().join("saves");
    for n in &names {
        let target = to.join(n);
        if target.exists() {
            fs::remove_dir_all(&target).with_context(|| format!("clear {}", target.display()))?;
        }
        for e in Source::Dir(from.join(n)).entries()? {
            let dest = target.join(&e.name);
            if e.is_dir {
                fs::create_dir_all(&dest).with_context(|| format!("create {}", dest.display()))?;
            } else {
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("create {}", parent.display()))?;
                }
                fs::copy(from.join(n).join(&e.name), &dest)
                    .with_context(|| format!("restore {}", e.name))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::{CopyMode, CreateOptions};
    use crate::paths::Paths;

    #[test]
    fn a_same_size_same_second_edit_is_backed_up() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ksp = tmp.path().join("ksp");
        fs::create_dir_all(ksp.join("GameData/Squad")).expect("mkdir");
        fs::write(ksp.join("KSP_x64.exe"), b"").expect("write");
        fs::write(ksp.join("readme.txt"), "Version 1.12.5\n").expect("write");
        let paths = Paths::new(Some(&tmp.path().join("orbis"))).expect("paths");
        let inst = crate::instance::create(
            &paths,
            &CreateOptions {
                name: "B".into(),
                source: ksp,
                stock_only: true,
                with_saves: false,
                mode: CopyMode::Link,
            },
            &mut |_, _| {},
        )
        .expect("create");
        let save = inst.game_dir().join("saves/Career");
        fs::create_dir_all(&save).expect("mkdir");
        fs::write(save.join("persistent.sfs"), b"day 1").expect("write");
        create(&inst, "first").expect("backup").expect("made");
        assert!(create(&inst, "again").expect("backup").is_none());
        let mtime = fs::metadata(save.join("persistent.sfs"))
            .and_then(|m| m.modified())
            .expect("mtime");
        fs::write(save.join("persistent.sfs"), b"day X").expect("write");
        fs::File::options()
            .write(true)
            .open(save.join("persistent.sfs"))
            .and_then(|f| f.set_modified(mtime))
            .expect("set mtime");
        assert!(
            create(&inst, "edited").expect("backup").is_some(),
            "a change in content is noticed"
        );
    }

    #[test]
    fn backs_up_dedups_prunes_and_restores() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ksp = tmp.path().join("ksp");
        fs::create_dir_all(ksp.join("GameData/Squad")).expect("mkdir");
        fs::write(ksp.join("KSP_x64.exe"), b"").expect("write");
        fs::write(ksp.join("readme.txt"), "Version 1.12.5\n").expect("write");
        let paths = Paths::new(Some(&tmp.path().join("orbis"))).expect("paths");
        let inst = crate::instance::create(
            &paths,
            &CreateOptions {
                name: "B".into(),
                source: ksp,
                stock_only: true,
                with_saves: false,
                mode: CopyMode::Link,
            },
            &mut |_, _| {},
        )
        .expect("create");
        assert!(create(&inst, "launch").expect("no saves").is_none());

        let save = inst.game_dir().join("saves/Career");
        fs::create_dir_all(save.join("Ships/VAB")).expect("mkdir");
        fs::write(save.join("persistent.sfs"), b"day 1").expect("write");
        fs::write(save.join("Ships/VAB/A.craft"), b"craft").expect("write");
        let first = create(&inst, "before launch")
            .expect("backup")
            .expect("made");
        assert_eq!(first.saves, ["Career"]);
        assert!(
            create(&inst, "again").expect("backup").is_none(),
            "unchanged saves are skipped"
        );

        fs::write(save.join("persistent.sfs"), b"day 2, corrupted").expect("write");
        restore(&inst, &first.id, Some("Career")).expect("restore");
        assert_eq!(
            fs::read(save.join("persistent.sfs")).expect("read"),
            b"day 1"
        );
        assert_eq!(
            fs::read(save.join("Ships/VAB/A.craft")).expect("read"),
            b"craft"
        );
        let all = list(&inst).expect("list");
        assert_eq!(all.len(), 2, "restore backed up the corrupted state first");
        assert_eq!(all[0].reason, "before restore");

        assert_eq!(prune(&inst, 1).expect("prune"), 1);
        assert_eq!(list(&inst).expect("list").len(), 1);
    }
}
