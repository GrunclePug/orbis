//! Part names: which parts an instance's GameData provides and which parts a
//! craft uses.
//!
//! KSP turns `_` into `.` in part names when it loads them, and a craft names
//! each part as `<name>_<craftID>`. The index of an instance's parts comes
//! from the top-level `PART` nodes in its `.cfg` files, plus every part in
//! ModuleManager's cache (`GameData/ModuleManager.ConfigCache`, the patched
//! database from the last launch) when there is one. The raw scan can't see
//! parts a ModuleManager patch creates or deletes; the cache can, but only as
//! of the last launch. The index is `confirmed` when the cache is newer than
//! every `.cfg` file.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

/// One step through a KSP config file.
#[derive(Debug, PartialEq, Eq)]
pub enum Event<'a> {
    /// A node opens: its header, such as `PART` or `@PART[foo]:NEEDS[Bar]`.
    Open(&'a str),
    /// A `key = value` line.
    Value(&'a str, &'a str),
    /// The innermost open node closes.
    Close,
}

/// Walks a KSP config file, calling `f` with each event and the depth it
/// happens at (0 for top-level values and nodes). Comments are skipped and
/// unbalanced braces are tolerated.
pub fn scan(text: &str, mut f: impl FnMut(Event<'_>, usize)) {
    let mut depth = 0usize;
    let mut pending: Option<&str> = None;
    // Stock files start with a UTF-8 byte order mark.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for line in text.lines() {
        let line = match line.find("//") {
            Some(i) => &line[..i],
            None => line,
        };
        let mut rest = line;
        loop {
            let cut = rest.find(['{', '}']);
            let seg = cut.map_or(rest, |i| &rest[..i]).trim();
            if !seg.is_empty() {
                match seg.split_once('=') {
                    Some((k, v)) => {
                        if let Some(p) = pending.take() {
                            // A header followed by a value: it wasn't a header.
                            log::trace!("stray text {p:?}");
                        }
                        f(Event::Value(k.trim(), v.trim()), depth);
                    }
                    None => pending = Some(seg),
                }
            }
            let Some(i) = cut else { break };
            if rest.as_bytes()[i] == b'{' {
                f(Event::Open(pending.take().unwrap_or("")), depth);
                depth += 1;
            } else if depth > 0 {
                pending = None;
                depth -= 1;
                f(Event::Close, depth);
            }
            rest = &rest[i + 1..];
        }
    }
}

/// KSP's form of a part name.
pub fn normalize(name: &str) -> String {
    name.trim().replace('_', ".")
}

/// What a craft file says about itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CraftInfo {
    /// The `ship =` name.
    pub ship: String,
    /// The `type =` value (`VAB` or `SPH`), when present.
    pub kind: String,
    /// Part names, in KSP's form.
    pub parts: BTreeSet<String>,
}

/// Reads a craft file's name, type and parts.
pub fn read_craft(text: &str) -> CraftInfo {
    let mut info = CraftInfo::default();
    let mut in_part = false;
    scan(text, |e, depth| match e {
        Event::Value("ship", v) if depth == 0 && info.ship.is_empty() => info.ship = v.to_owned(),
        Event::Value("type", v) if depth == 0 && info.kind.is_empty() => info.kind = v.to_owned(),
        Event::Open(h) if depth == 0 => in_part = h == "PART",
        Event::Value("part", v) if depth == 1 && in_part => {
            let name = match v.rsplit_once('_') {
                Some((n, id)) if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) => n,
                _ => v,
            };
            info.parts.insert(normalize(name));
        }
        Event::Close if depth == 0 => in_part = false,
        _ => {}
    });
    info
}

/// The part names a config file defines (or creates or renames through
/// ModuleManager), in KSP's form.
pub fn parts_in_cfg(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut active = false;
    scan(text, |e, depth| match e {
        Event::Open(h) if depth == 0 => {
            let h = h.trim_start_matches(['@', '+', '$', '%']);
            let head = h.split([':', '[', ' ']).next().unwrap_or("");
            active = !h.starts_with('!') && head == "PART";
        }
        Event::Value(k, v) if depth == 1 && active => {
            if matches!(k, "name" | "@name" | "%name") && !v.is_empty() {
                out.push(normalize(v));
            }
        }
        Event::Close if depth == 0 => active = false,
        _ => {}
    });
    out
}

/// Every part in ModuleManager's cache, with the GameData folder its
/// definition came from.
pub fn parts_in_cache(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut url = String::new();
    let mut in_url = false;
    let mut in_part = false;
    scan(text, |e, depth| match e {
        Event::Open(h) if depth == 0 => {
            in_url = h == "UrlConfig";
            url.clear();
        }
        Event::Value("parentUrl", v) if depth == 1 && in_url => url = v.to_owned(),
        Event::Open(h) if depth == 1 => in_part = in_url && h == "PART",
        Event::Value("name", v) if depth == 2 && in_part => {
            let folder = url.split('/').next().unwrap_or("").to_owned();
            out.push((normalize(v), folder));
        }
        Event::Close if depth == 1 => in_part = false,
        _ => {}
    });
    out
}

/// The parts an instance provides.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartIndex {
    /// Fingerprint of the files the index was built from.
    pub fingerprint: u64,
    /// Built with a ModuleManager cache newer than every `.cfg` file.
    pub confirmed: bool,
    /// Part name -> the GameData folder that defines it.
    pub parts: BTreeMap<String, String>,
}

impl PartIndex {
    /// The parts in `parts` this index lacks.
    pub fn missing<'a>(&self, parts: impl IntoIterator<Item = &'a String>) -> Vec<String> {
        parts
            .into_iter()
            .filter(|p| !self.parts.contains_key(*p))
            .cloned()
            .collect()
    }
}

const CACHE_FILE: &str = "ModuleManager.ConfigCache";

/// Bumped whenever parsing changes, so cached indexes are rebuilt.
const INDEX_VERSION: u32 = 2;

/// The index of `game`'s parts, reusing `cache` (a JSON file) when nothing
/// changed since it was built.
pub fn index(game: &Path, cache: &Path) -> Result<PartIndex> {
    let gamedata = game.join("GameData");
    let mut cfgs = Vec::new();
    walk_cfgs(&gamedata, "", 0, &mut cfgs);
    cfgs.sort();
    let mm = gamedata.join(CACHE_FILE);
    let mm_meta = fs::metadata(&mm).ok().map(|m| (m.len(), mtime(&m)));
    let mut h = DefaultHasher::new();
    INDEX_VERSION.hash(&mut h);
    cfgs.hash(&mut h);
    mm_meta.hash(&mut h);
    let fingerprint = h.finish();
    if let Ok(text) = fs::read_to_string(cache)
        && let Ok(old) = serde_json::from_str::<PartIndex>(&text)
        && old.fingerprint == fingerprint
    {
        return Ok(old);
    }
    let mut parts = BTreeMap::new();
    for (rel, _, _) in &cfgs {
        let bytes = match fs::read(gamedata.join(rel)) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("read GameData/{rel}: {e}");
                continue;
            }
        };
        let folder = rel.split('/').next().unwrap_or("").to_owned();
        for p in parts_in_cfg(&String::from_utf8_lossy(&bytes)) {
            parts.entry(p).or_insert_with(|| folder.clone());
        }
    }
    let newest_cfg = cfgs.iter().map(|(_, _, t)| *t).max().unwrap_or(0);
    let mut confirmed = false;
    if let Some((_, mm_time)) = mm_meta {
        let bytes = fs::read(&mm).with_context(|| format!("read {}", mm.display()))?;
        for (p, folder) in parts_in_cache(&String::from_utf8_lossy(&bytes)) {
            parts.entry(p).or_insert(folder);
        }
        confirmed = mm_time >= newest_cfg;
    }
    let index = PartIndex {
        fingerprint,
        confirmed,
        parts,
    };
    let json = serde_json::to_vec(&index).context("serialize part index")?;
    paths::write_atomic(cache, &json)?;
    Ok(index)
}

fn mtime(m: &fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// Collects `(relative path, size, mtime)` of every `.cfg` file under `dir`,
/// following links (stock GameData is linked in).
fn walk_cfgs(dir: &Path, rel: &str, depth: usize, out: &mut Vec<(String, u64, u64)>) {
    if depth > 24 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let child = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let Ok(meta) = fs::metadata(e.path()) else {
            continue;
        };
        if meta.is_dir() {
            walk_cfgs(&e.path(), &child, depth + 1, out);
        } else if name.to_lowercase().ends_with(".cfg") {
            out.push((child, meta.len(), mtime(&meta)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_nested_and_inline_nodes() {
        let text =
            "PART\n{\n\tname = a_b // comment\n\tMODULE { name = M }\n}\nPART { name = c }\n}\n";
        let mut events = Vec::new();
        scan(text, |e, d| events.push(format!("{d}:{e:?}")));
        assert_eq!(
            events,
            [
                "0:Open(\"PART\")",
                "1:Value(\"name\", \"a_b\")",
                "1:Open(\"MODULE\")",
                "2:Value(\"name\", \"M\")",
                "1:Close",
                "0:Close",
                "0:Open(\"PART\")",
                "1:Value(\"name\", \"c\")",
                "0:Close",
            ]
        );
    }

    #[test]
    fn finds_parts_in_cfgs() {
        let cases = [
            (
                "PART\n{\n name = mk1pod_v2\n MODULE\n {\n  name = ModuleCommand\n }\n}",
                vec!["mk1pod.v2"],
            ),
            ("PART:NEEDS[Foo] { name = foo }", vec!["foo"]),
            (
                "\u{feff}PART\r\n{\r\n\tname = solarPanels4\r\n}\r\n",
                vec!["solarPanels4"],
            ),
            ("+PART[foo] { @name = foo2 }", vec!["foo2"]),
            ("@PART[foo] { @title = x }", vec![]),
            ("!PART[foo] { }", vec![]),
            ("PROP { name = notapart }", vec![]),
            ("PARTUPGRADE { name = up }", vec![]),
        ];
        for (text, want) in cases {
            assert_eq!(parts_in_cfg(text), want, "{text}");
        }
    }

    #[test]
    fn reads_craft_files() {
        let text = "ship = Orbiter I\nversion = 1.12.5\ntype = VAB\nPART\n{\n\tpart = mk1pod.v2_4294\n\tlink = tank_123\n\tMODULE\n\t{\n\t\tpart = notthis_1\n\t}\n}\nPART\n{\n\tpart = nfe-rtg-asrg-1_42\n}\n";
        let info = read_craft(text);
        assert_eq!(info.ship, "Orbiter I");
        assert_eq!(info.kind, "VAB");
        let parts: Vec<_> = info.parts.into_iter().collect();
        assert_eq!(parts, ["mk1pod.v2", "nfe-rtg-asrg-1"]);
    }

    #[test]
    fn reads_modulemanager_cache() {
        let text = "UrlConfig\n{\n\tparentUrl = NearFuturePropulsion/Parts/x.cfg\n\tPART\n\t{\n\t\tname = nfp_ion\n\t\tMODULE { name = y }\n\t}\n}\nUrlConfig\n{\n\tparentUrl = Squad/Resources/r.cfg\n\tRESOURCE_DEFINITION { name = LiquidFuel }\n}\n";
        assert_eq!(
            parts_in_cache(text),
            [("nfp.ion".to_owned(), "NearFuturePropulsion".to_owned())]
        );
    }

    #[test]
    fn indexes_and_caches() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let game = tmp.path().join("game");
        fs::create_dir_all(game.join("GameData/Squad/Parts")).expect("mkdir");
        fs::create_dir_all(game.join("GameData/Mod")).expect("mkdir");
        fs::write(
            game.join("GameData/Squad/Parts/pod.cfg"),
            "PART { name = pod_1 }",
        )
        .expect("w");
        fs::write(game.join("GameData/Mod/a.cfg"), "PART { name = modpart }").expect("w");
        let cache = tmp.path().join("parts.json");
        let idx = index(&game, &cache).expect("index");
        assert!(!idx.confirmed);
        assert_eq!(idx.parts.get("pod.1").map(String::as_str), Some("Squad"));
        assert_eq!(idx.parts.get("modpart").map(String::as_str), Some("Mod"));
        assert_eq!(index(&game, &cache).expect("cached"), idx);
        fs::remove_file(game.join("GameData/Mod/a.cfg")).expect("rm");
        let idx = index(&game, &cache).expect("reindex");
        assert!(!idx.parts.contains_key("modpart"));
        let parts = ["pod.1".to_owned(), "modpart".to_owned()];
        assert_eq!(idx.missing(&parts), ["modpart"]);
    }
}
