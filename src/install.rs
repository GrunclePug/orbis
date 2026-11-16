//! Turning an archive into files in a game directory, using CKAN install
//! directives (ported from CKAN's `ModuleInstallDescriptor`).

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use regex::{Regex, RegexBuilder};

use crate::metadata::InstallDirective;

/// Folders CKAN strips from the front of an installed path, so installing a
/// `GameData` folder into `GameData` doesn't nest it.
const RESERVED: [&str; 3] = ["GameData", "Ships", "Missions"];

/// Directories that are part of the game and never removed on uninstall.
const KEEP_DIRS: [&str; 15] = [
    "GameData",
    "Ships",
    "Ships/VAB",
    "Ships/SPH",
    "Ships/@thumbs",
    "Ships/@thumbs/VAB",
    "Ships/@thumbs/SPH",
    "Ships/Script",
    "Missions",
    "saves",
    "saves/scenarios",
    "saves/training",
    "GameData/Squad",
    "GameData/SquadExpansion",
    "Internals",
];

/// Where mod content comes from: a zip archive or an unpacked folder.
#[derive(Clone, Debug)]
pub enum Source {
    Zip(PathBuf),
    Dir(PathBuf),
}

/// One archive entry: a `/`-separated relative path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
}

/// One file or folder to install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Planned {
    /// Path inside the archive.
    pub source: String,
    /// Path relative to the game directory.
    pub dest: String,
    pub is_dir: bool,
}

/// What an install put on disk, relative to the game directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstalledFiles {
    pub files: Vec<String>,
    pub dirs: Vec<String>,
}

impl Source {
    /// Every entry in the source.
    pub fn entries(&self) -> Result<Vec<Entry>> {
        match self {
            Source::Zip(path) => {
                let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
                let mut zip = zip::ZipArchive::new(file)
                    .with_context(|| format!("read zip {}", path.display()))?;
                let mut out = Vec::with_capacity(zip.len());
                for i in 0..zip.len() {
                    let f = zip
                        .by_index(i)
                        .with_context(|| format!("read zip entry {i}"))?;
                    let name = f
                        .name()
                        .with_context(|| format!("read zip entry {i} name"))?
                        .replace('\\', "/");
                    let is_dir = f.is_dir() || name.ends_with('/');
                    out.push(Entry {
                        name: name.trim_end_matches('/').to_owned(),
                        is_dir,
                    });
                }
                Ok(out)
            }
            Source::Dir(root) => {
                let mut out = Vec::new();
                walk(root, root, &mut out)?;
                Ok(out)
            }
        }
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<Entry>) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("read {}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        let rel = path
            .strip_prefix(root)
            .context("walked outside the root")?
            .to_string_lossy()
            .into_owned();
        let ft = e
            .file_type()
            .with_context(|| format!("stat {}", path.display()))?;
        if ft.is_dir() {
            out.push(Entry {
                name: rel,
                is_dir: true,
            });
            walk(root, &path, out)?;
        } else if ft.is_file() {
            out.push(Entry {
                name: rel,
                is_dir: false,
            });
        }
    }
    Ok(())
}

/// Normalizes a path the way CKAN does: forward slashes, no trailing slash.
fn normalize(p: &str) -> String {
    p.replace('\\', "/").trim_end_matches('/').to_owned()
}

fn dirname(p: &str) -> &str {
    p.rfind('/').map_or("", |i| &p[..i])
}

/// Maps a stanza's `install_to` to a directory relative to the game root.
fn install_dir(install_to: &str) -> Result<String> {
    let to = normalize(install_to);
    if to.contains("/..") || to.starts_with("..") {
        bail!("install_to {to:?} leaves the game directory");
    }
    if to == "GameData" || to.starts_with("GameData/") {
        return Ok(to);
    }
    Ok(match to.as_str() {
        "GameRoot" => String::new(),
        "Tutorial" => "saves/training".into(),
        "Scenarios" => "saves/scenarios".into(),
        "Missions" | "Ships" | "Ships/VAB" | "Ships/SPH" | "Ships/@thumbs"
        | "Ships/@thumbs/VAB" | "Ships/@thumbs/SPH" | "Ships/Script" => to,
        _ => bail!("unknown install_to {to:?}"),
    })
}

fn pattern(d: &InstallDirective) -> Result<Regex> {
    let src = match (&d.file, &d.find, &d.find_regexp) {
        (Some(f), None, None) => format!("^{}(/|$)", regex::escape(&normalize(f))),
        (None, Some(f), None) => format!("(?:^|/){}(/|$)", regex::escape(&normalize(f))),
        (None, None, Some(r)) => r.clone(),
        _ => bail!("install stanza needs exactly one of file, find or find_regexp"),
    };
    RegexBuilder::new(&src)
        .case_insensitive(true)
        .build()
        .with_context(|| format!("bad install pattern {src:?}"))
}

fn any_regex_matches(list: &[String], path: &str) -> Result<bool> {
    for r in list {
        if Regex::new(r)
            .with_context(|| format!("bad regexp {r:?}"))?
            .is_match(path)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_wanted(
    d: &InstallDirective,
    pat: &Regex,
    path: &str,
    match_where: Option<usize>,
) -> Result<bool> {
    let Some(m) = pat.find(path) else {
        return Ok(false);
    };
    if match_where.is_some_and(|w| m.start() != w) {
        return Ok(false);
    }
    let lower = path.to_lowercase();
    let segments: Vec<&str> = lower.split('/').collect();
    let has_segment = |list: &[String]| {
        list.iter()
            .any(|t| segments.contains(&t.to_lowercase().as_str()))
    };
    if d.filter.as_deref().is_some_and(has_segment) {
        return Ok(false);
    }
    if let Some(list) = &d.filter_regexp
        && any_regex_matches(list, path)?
    {
        return Ok(false);
    }
    if d.include_only.as_deref().is_some_and(has_segment) {
        return Ok(true);
    }
    if let Some(list) = &d.include_only_regexp
        && any_regex_matches(list, path)?
    {
        return Ok(true);
    }
    Ok(d.include_only.is_none() && d.include_only_regexp.is_none())
}

fn transform(pat: &Regex, name: &str, install_dir: &str, as_name: Option<&str>) -> Result<String> {
    // The shortest leading path that still matches the stanza.
    let mut shortest = name.to_owned();
    let mut p = name.trim_end_matches('/');
    while !p.is_empty() {
        if pat.is_match(p) {
            shortest = p.to_owned();
            p = dirname(p);
        } else {
            break;
        }
    }
    let mut out = name.to_owned();
    let lead = dirname(&shortest);
    if !lead.is_empty() {
        out = out
            .strip_prefix(&format!("{lead}/"))
            .with_context(|| format!("{name} does not start with {lead}"))?
            .to_owned();
    }
    if let Some(as_name) = as_name.filter(|s| !s.trim().is_empty()) {
        if as_name.contains('/') || as_name.contains('\\') {
            bail!("`as` must not contain path separators");
        }
        out = match out.split_once('/') {
            Some((_, rest)) => format!("{as_name}/{rest}"),
            None => as_name.to_owned(),
        };
    } else if let Some(prefix) = RESERVED.iter().find(|r| {
        out.len() > r.len()
            && out.as_bytes()[r.len()] == b'/'
            && out[..r.len()].eq_ignore_ascii_case(r)
    }) {
        out = out[prefix.len() + 1..].to_owned();
    }
    if out.contains("/../") || out.ends_with("/..") || out.starts_with("../") || out == ".." {
        bail!("{name} would install outside the game directory");
    }
    Ok(if install_dir.is_empty() {
        normalize(&out)
    } else {
        normalize(&format!("{install_dir}/{out}"))
    })
}

/// Works out where every entry goes under the given stanzas. Fails when a
/// stanza matches nothing, as CKAN does.
pub fn plan(directives: &[InstallDirective], entries: &[Entry]) -> Result<Vec<Planned>> {
    let mut out: Vec<Planned> = Vec::new();
    let mut seen = BTreeSet::new();
    for d in directives {
        let dir = install_dir(&d.install_to)?;
        let pat = pattern(d)?;
        let match_where = if d.find.is_some() {
            entries
                .iter()
                .filter_map(|e| pat.find(&e.name).map(|m| m.start()))
                .min()
        } else {
            None
        };
        let mut count = 0;
        for e in entries {
            if !is_wanted(d, &pat, &e.name, match_where)? {
                continue;
            }
            let dest = transform(&pat, &e.name, &dir, d.as_name.as_deref())?;
            count += 1;
            if dest.is_empty() || !seen.insert(dest.clone()) {
                continue;
            }
            out.push(Planned {
                source: e.name.clone(),
                dest,
                is_dir: e.is_dir,
            });
        }
        if count == 0 {
            bail!("install stanza {} matched no files", describe(d));
        }
    }
    Ok(out)
}

/// A short description of a stanza for messages.
pub fn describe(d: &InstallDirective) -> String {
    let what = match (&d.file, &d.find, &d.find_regexp) {
        (Some(f), _, _) => format!("file {f:?}"),
        (_, Some(f), _) => format!("find {f:?}"),
        (_, _, Some(r)) => format!("find_regexp {r:?}"),
        _ => "(empty)".into(),
    };
    format!("{what} -> {}", d.install_to)
}

/// Lowercased path, for comparing paths the way Wine sees them.
pub fn fold(path: &str) -> String {
    path.to_lowercase()
}

/// Copies planned files into `game`. `owners` maps folded paths to the mod
/// that owns them; a planned file that another mod owns is an error, and so
/// is one that exists on disk unowned unless `overwrite` is set. If anything
/// fails, the files written so far are removed again.
pub fn install(
    game: &Path,
    source: &Source,
    planned: &[Planned],
    owners: &HashMap<String, String>,
    overwrite: bool,
) -> Result<InstalledFiles> {
    let mut problems = Vec::new();
    for p in planned {
        if let Some(link) = through_link(game, &p.dest) {
            bail!(
                "{} would be written through {link}, which links to the base game install; \
                 orbis never changes that install",
                p.dest
            );
        }
    }
    for p in planned.iter().filter(|p| !p.is_dir) {
        check_safe(&p.dest)?;
        if let Some(owner) = owners.get(&fold(&p.dest)) {
            problems.push(format!("{} is owned by {owner}", p.dest));
        } else if !overwrite && game.join(&p.dest).exists() {
            problems.push(format!(
                "{} already exists and is not managed by orbis",
                p.dest
            ));
        }
    }
    if !problems.is_empty() {
        problems.truncate(20);
        bail!("file conflicts:\n  {}", problems.join("\n  "));
    }
    let mut done = InstalledFiles::default();
    let result = copy_all(game, source, planned, &mut done);
    if let Err(e) = result {
        uninstall(game, &done);
        return Err(e);
    }
    done.dirs = dirs_of(planned);
    Ok(done)
}

/// The directories a set of planned files lives in (parents included),
/// minus the game's own folders: what an uninstall may remove when empty.
pub fn dirs_of(planned: &[Planned]) -> Vec<String> {
    let mut dirs = BTreeSet::new();
    for p in planned {
        if p.is_dir {
            dirs.insert(p.dest.clone());
        }
        let mut d = dirname(&p.dest);
        while !d.is_empty() {
            dirs.insert(d.to_owned());
            d = dirname(d);
        }
    }
    dirs.into_iter().filter(|d| !is_kept_dir(d)).collect()
}

/// The first component of `rel` (under `game`) that is a symlink, if any.
/// Linked instances link stock files from the base install, and nothing may
/// be written or deleted through those links.
pub fn through_link(game: &Path, rel: &str) -> Option<String> {
    let mut prefix = String::new();
    for part in rel.split('/') {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(part);
        if fs::symlink_metadata(game.join(&prefix)).is_ok_and(|m| m.file_type().is_symlink()) {
            return Some(prefix);
        }
    }
    None
}

fn check_safe(dest: &str) -> Result<()> {
    if dest.starts_with('/') || dest.split('/').any(|s| s == ".." || s.is_empty()) {
        bail!("unsafe install path {dest:?}");
    }
    Ok(())
}

fn copy_all(
    game: &Path,
    source: &Source,
    planned: &[Planned],
    done: &mut InstalledFiles,
) -> Result<()> {
    let mut zip = match source {
        Source::Zip(path) => {
            let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
            Some(
                zip::ZipArchive::new(file)
                    .with_context(|| format!("read zip {}", path.display()))?,
            )
        }
        Source::Dir(_) => None,
    };
    let mut by_name = HashMap::new();
    if let Some(zip) = &mut zip {
        for i in 0..zip.len() {
            let f = zip
                .by_index(i)
                .with_context(|| format!("read zip entry {i}"))?;
            let name = f
                .name()
                .with_context(|| format!("read zip entry {i} name"))?;
            by_name.insert(normalize(&name), i);
        }
    }
    for p in planned {
        let dest = game.join(&p.dest);
        if p.is_dir {
            fs::create_dir_all(&dest).with_context(|| format!("create {}", dest.display()))?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        match (source, &mut zip) {
            (Source::Zip(_), Some(zip)) => {
                let i = *by_name
                    .get(&p.source)
                    .with_context(|| format!("{} missing from archive", p.source))?;
                let mut f = zip
                    .by_index(i)
                    .with_context(|| format!("read {}", p.source))?;
                let mut out =
                    File::create(&dest).with_context(|| format!("create {}", dest.display()))?;
                done.files.push(p.dest.clone());
                io::copy(&mut f, &mut out).with_context(|| format!("extract {}", p.source))?;
            }
            (Source::Dir(root), _) => {
                done.files.push(p.dest.clone());
                fs::copy(root.join(&p.source), &dest)
                    .with_context(|| format!("copy {}", p.source))?;
            }
            (Source::Zip(_), None) => unreachable!("zip source always opens an archive"),
        }
    }
    Ok(())
}

fn is_kept_dir(d: &str) -> bool {
    KEEP_DIRS.iter().any(|k| k.eq_ignore_ascii_case(d))
}

/// Removes installed files, then any of the recorded directories that are
/// now empty (deepest first). Missing files are ignored.
pub fn uninstall(game: &Path, installed: &InstalledFiles) {
    for f in &installed.files {
        if let Some(link) = through_link(game, f) {
            log::warn!("not removing {f}: {link} links to the base game install");
            continue;
        }
        let path = game.join(f);
        if let Err(e) = fs::remove_file(&path)
            && e.kind() != io::ErrorKind::NotFound
        {
            log::warn!("remove {}: {e}", path.display());
        }
    }
    let mut dirs: Vec<&String> = installed.dirs.iter().filter(|d| !is_kept_dir(d)).collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
    for d in dirs {
        if through_link(game, d).is_some() {
            continue;
        }
        // Fails harmlessly when the directory still has other files.
        let _ = fs::remove_dir(game.join(d));
    }
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

    fn dests(p: &[Planned]) -> Vec<&str> {
        p.iter()
            .filter(|p| !p.is_dir)
            .map(|p| p.dest.as_str())
            .collect()
    }

    fn stanza(json: &str) -> InstallDirective {
        serde_json::from_str(json).expect("stanza parses")
    }

    #[test]
    fn plans_like_ckan() {
        let zip = entries(&[
            "Mod-1.0/GameData/Mod/Plugins/Mod.dll",
            "Mod-1.0/GameData/Mod/Parts/part.cfg",
            "Mod-1.0/GameData/Mod/Thumbs.db",
            "Mod-1.0/GameData/ModuleManager.4.2.3.dll",
            "Mod-1.0/Ships/VAB/Craft.craft",
            "Mod-1.0/README.md",
        ]);
        let cases: Vec<(&str, Vec<&str>)> = vec![
            (
                r#"{"find": "Mod"}"#,
                vec![
                    "GameData/Mod/Plugins/Mod.dll",
                    "GameData/Mod/Parts/part.cfg",
                    "GameData/Mod/Thumbs.db",
                ],
            ),
            (
                r#"{"find": "Mod", "filter": "thumbs.db"}"#,
                vec![
                    "GameData/Mod/Plugins/Mod.dll",
                    "GameData/Mod/Parts/part.cfg",
                ],
            ),
            (
                r#"{"file": "Mod-1.0/GameData", "install_to": "GameData"}"#,
                vec![
                    "GameData/Mod/Plugins/Mod.dll",
                    "GameData/Mod/Parts/part.cfg",
                    "GameData/Mod/Thumbs.db",
                    "GameData/ModuleManager.4.2.3.dll",
                ],
            ),
            (
                r#"{"find": "VAB", "install_to": "Ships"}"#,
                vec!["Ships/VAB/Craft.craft"],
            ),
            (
                r#"{"find": "Mod", "as": "Renamed", "include_only": "Plugins"}"#,
                vec!["GameData/Renamed/Plugins/Mod.dll"],
            ),
            (
                r#"{"find_regexp": "ModuleManager.*\\.dll$", "install_to": "GameData"}"#,
                vec!["GameData/ModuleManager.4.2.3.dll"],
            ),
        ];
        for (json, want) in cases {
            let got = plan(&[stanza(json)], &zip).expect(json);
            assert_eq!(dests(&got), want, "{json}");
        }
        assert!(plan(&[stanza(r#"{"find": "Missing"}"#)], &zip).is_err());
        assert!(
            plan(
                &[stanza(r#"{"find": "Mod", "install_to": "Nowhere"}"#)],
                &zip
            )
            .is_err()
        );
    }

    #[test]
    fn installs_and_uninstalls_with_conflicts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let zip_path = tmp.path().join("m.zip");
        {
            let mut w = zip::ZipWriter::new(File::create(&zip_path).expect("create"));
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("GameData/Mod/a.cfg", opts).expect("start");
            w.write_all(b"a").expect("write");
            w.start_file("GameData/Mod/sub/b.cfg", opts).expect("start");
            w.write_all(b"b").expect("write");
            w.finish().expect("finish");
        }
        let game = tmp.path().join("game");
        fs::create_dir_all(game.join("GameData")).expect("mkdir");
        let src = Source::Zip(zip_path);
        let planned = plan(
            &[InstallDirective::default_for("Mod")],
            &src.entries().expect("entries"),
        )
        .expect("plan");

        let mut owners = HashMap::new();
        owners.insert(fold("GameData/Mod/a.cfg"), "Other".to_owned());
        assert!(install(&game, &src, &planned, &owners, false).is_err());

        let done = install(&game, &src, &planned, &HashMap::new(), false).expect("install");
        assert_eq!(
            fs::read(game.join("GameData/Mod/sub/b.cfg")).expect("read"),
            b"b"
        );
        assert_eq!(done.files.len(), 2);
        assert!(done.dirs.contains(&"GameData/Mod/sub".to_owned()));
        assert!(!done.dirs.contains(&"GameData".to_owned()));

        // A second install of the same files conflicts with what's on disk.
        assert!(install(&game, &src, &planned, &HashMap::new(), false).is_err());

        uninstall(&game, &done);
        assert!(!game.join("GameData/Mod").exists());
        assert!(game.join("GameData").is_dir());

        // Nothing is written through a link into a base install.
        let base = tmp.path().join("base");
        fs::create_dir_all(base.join("Mod")).expect("mkdir");
        fs::remove_dir(game.join("GameData")).expect("rmdir");
        std::os::unix::fs::symlink(&base, game.join("GameData")).expect("symlink");
        let err = install(&game, &src, &planned, &HashMap::new(), true).expect_err("refused");
        assert!(
            err.to_string().contains("links to the base game install"),
            "{err}"
        );
        assert!(
            fs::read_dir(base.join("Mod"))
                .expect("read")
                .next()
                .is_none()
        );
    }
}
