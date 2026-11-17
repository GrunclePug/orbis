//! Reading KSP's logs: levels per line, and exceptions grouped and traced
//! back to the mod whose DLL threw them.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::install::{self, Source};
use crate::instance::Instance;
use crate::launch::Resolved;
use crate::steam::SteamInfo;

/// Which log to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogKind {
    /// `KSP.log` in the game directory, written by KSP itself.
    Ksp,
    /// Unity's `Player.log` (full stack traces), in the Wine prefix or, for
    /// the Linux build, `~/.config/unity3d`.
    Player,
    /// Proton's output, captured by orbis at launch.
    Launch,
}

impl LogKind {
    pub const ALL: [LogKind; 3] = [LogKind::Ksp, LogKind::Player, LogKind::Launch];

    pub fn label(self) -> &'static str {
        match self {
            LogKind::Ksp => "KSP.log",
            LogKind::Player => "Player.log",
            LogKind::Launch => "Launch log",
        }
    }
}

/// Where a log lives for an instance. `Player.log` is per prefix (or per
/// user, for the Linux build), so when instances share one it belongs to
/// whichever ran last.
pub fn path(inst: &Instance, steam: &SteamInfo, kind: LogKind) -> Option<PathBuf> {
    const UNITY: &str = "Squad/Kerbal Space Program/Player.log";
    match kind {
        LogKind::Ksp => Some(inst.game_dir().join("KSP.log")),
        LogKind::Launch => Some(inst.dir.join("orbis-launch.log")),
        LogKind::Player => Some(match crate::launch::resolve(inst, steam).ok()? {
            Resolved::Proton => crate::launch::proton_prefix(inst, steam)
                .join("pfx/drive_c/users/steamuser/AppData/LocalLow")
                .join(UNITY),
            Resolved::Wine => crate::launch::wine_prefix(inst)
                .join("drive_c/users")
                .join(std::env::var_os("USER")?)
                .join("AppData/LocalLow")
                .join(UNITY),
            Resolved::Native => std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .or_else(|| crate::paths::home().ok().map(|h| h.join(".config")))?
                .join("unity3d")
                .join(UNITY),
        }),
    }
}

/// Severity of a log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Info,
    Warning,
    Error,
    Exception,
}

/// One log line.
#[derive(Clone, Debug)]
pub struct Line {
    pub level: Level,
    pub text: String,
}

/// Repeated exceptions, merged.
#[derive(Clone, Debug)]
pub struct ExceptionGroup {
    pub message: String,
    pub count: usize,
    /// Index into `Report::lines` of the first occurrence.
    pub first_line: usize,
    /// Mods whose code appears in the stack trace, innermost first.
    pub mods: Vec<String>,
}

/// A parsed log.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub path: PathBuf,
    pub lines: Vec<Line>,
    pub exceptions: Vec<ExceptionGroup>,
    pub warnings: usize,
    pub errors: usize,
}

/// The mod DLLs of an instance and who owns them.
#[derive(Clone, Debug, Default)]
pub struct Assemblies {
    /// DLL name (lowercased, no extension) -> owning mod.
    pub by_stem: HashMap<String, String>,
    /// Every mod DLL with its owner.
    pub dlls: Vec<(PathBuf, String)>,
}

/// Namespaces that belong to the game, Unity or .NET, never to a mod.
const NOT_MODS: [&str; 10] = [
    "unityengine",
    "system",
    "mono",
    "ksp",
    "expansions",
    "tmpro",
    "steamworks",
    "microsoft",
    "highlighting",
    "vehiclephysics",
];

/// Finds every DLL under the instance's own GameData folders (stock
/// folders are links and skipped) and the mod that installed it, or
/// `unmanaged: <folder>` for hand-added ones.
pub fn assemblies(inst: &Instance) -> Result<Assemblies> {
    let game = inst.game_dir();
    let reg = inst.registry()?;
    let owners = reg.owners();
    let mut out = Assemblies::default();
    let gamedata = game.join("GameData");
    let Ok(entries) = fs::read_dir(&gamedata) else {
        return Ok(out);
    };
    for e in entries.flatten() {
        if e.file_type().is_ok_and(|t| t.is_symlink()) || !e.path().is_dir() {
            continue;
        }
        let top = e.file_name().to_string_lossy().into_owned();
        for f in Source::Dir(e.path()).entries()? {
            if f.is_dir || !f.name.to_lowercase().ends_with(".dll") {
                continue;
            }
            let rel = format!("GameData/{top}/{}", f.name);
            let stem = Path::new(&f.name)
                .file_stem()
                .map(|s| s.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let owner = owners
                .get(&install::fold(&rel))
                .cloned()
                .unwrap_or_else(|| format!("unmanaged: {top}"));
            out.by_stem.insert(stem, owner.clone());
            out.dlls.push((game.join(&rel), owner));
        }
    }
    Ok(out)
}

impl Assemblies {
    /// Maps namespace roots that don't match a DLL name (MechJeb's code is
    /// `MuMech.*` in `MechJeb2.dll`) by finding the DLLs whose metadata
    /// string heap holds the name. Returns root (lowercased) -> owners.
    fn resolve_roots(&self, roots: &[String]) -> HashMap<String, String> {
        if roots.is_empty() {
            return HashMap::new();
        }
        let pattern = roots
            .iter()
            .map(|r| format!("\\x00({})\\x00", regex::escape(r)))
            .collect::<Vec<_>>()
            .join("|");
        let Ok(re) = regex::bytes::Regex::new(&pattern) else {
            return HashMap::new();
        };
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for (path, owner) in &self.dlls {
            let Ok(bytes) = fs::read(path) else { continue };
            for caps in re.captures_iter(&bytes) {
                if let Some(m) = caps.iter().skip(1).flatten().next() {
                    let root = String::from_utf8_lossy(m.as_bytes()).to_lowercase();
                    let owners = out.entry(root).or_default();
                    if !owners.contains(owner) {
                        owners.push(owner.clone());
                    }
                }
            }
        }
        out.into_iter().map(|(k, v)| (k, v.join(" or "))).collect()
    }
}

/// Namespace roots of stack frames that match no DLL name.
fn unknown_roots(text: &str, known: &HashMap<String, String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        if tag_level(line).is_some() {
            continue;
        }
        let Some(parts) = frame_parts(line) else {
            continue;
        };
        // Namespace.Type.Method at least; "Part.Update" is KSP's own code.
        if parts.len() < 3 {
            continue;
        }
        let root = parts[0];
        let lower = root.to_lowercase();
        if known.contains_key(&lower)
            || NOT_MODS.contains(&lower.as_str())
            || out.iter().any(|r| r == root)
        {
            continue;
        }
        out.push(root.to_owned());
        if out.len() >= 32 {
            break;
        }
    }
    out
}

/// Reads and analyses a log file, blaming exceptions on mods.
pub fn read(path: &Path, asm: &Assemblies) -> Result<Report> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let mut owners = asm.by_stem.clone();
    owners.extend(asm.resolve_roots(&unknown_roots(&text, &owners)));
    let mut report = analyse(&text, &owners);
    report.path = path.to_path_buf();
    Ok(report)
}

/// Analyses log text. KSP.log lines start with `[LOG|WRN|ERR|EXC hh:mm:ss]`;
/// Player.log has no tags, so exceptions are found by their wording.
/// Unprefixed lines after an exception are its stack trace.
pub fn analyse(text: &str, owners: &HashMap<String, String>) -> Report {
    let mut report = Report::default();
    let mut groups: BTreeMap<String, usize> = BTreeMap::new();
    let mut current: Option<usize> = None;
    for raw in text.lines() {
        let line = raw.trim_end();
        let tagged = tag_level(line);
        let level = tagged.unwrap_or_else(|| untagged_level(line));
        let index = report.lines.len();
        report.lines.push(Line {
            level,
            text: line.to_owned(),
        });
        match level {
            Level::Warning => report.warnings += 1,
            Level::Error => report.errors += 1,
            _ => {}
        }
        if level == Level::Exception {
            let message = strip_tag(line).trim().to_owned();
            let g = *groups.entry(message.clone()).or_insert_with(|| {
                report.exceptions.push(ExceptionGroup {
                    message,
                    count: 0,
                    first_line: index,
                    mods: vec![],
                });
                report.exceptions.len() - 1
            });
            report.exceptions[g].count += 1;
            current = Some(g);
            continue;
        }
        if tagged.is_some() || line.trim().is_empty() {
            current = None;
            continue;
        }
        // A stack frame of the current exception.
        if let Some(g) = current
            && let Some(owner) = frame_owner(line, owners)
            && !report.exceptions[g].mods.contains(&owner)
        {
            report.exceptions[g].mods.push(owner);
        }
    }
    report
        .exceptions
        .sort_by_key(|e| std::cmp::Reverse(e.count));
    report
}

fn tag_level(line: &str) -> Option<Level> {
    let rest = line.strip_prefix('[')?;
    let tag = rest.get(..3)?;
    if rest.as_bytes().get(3) != Some(&b' ') {
        return None;
    }
    Some(match tag {
        "LOG" => Level::Info,
        "WRN" => Level::Warning,
        "ERR" => Level::Error,
        "EXC" => Level::Exception,
        _ => return None,
    })
}

fn untagged_level(line: &str) -> Level {
    let t = line.trim_start();
    let first = t.split([':', ' ']).next().unwrap_or("");
    if first.ends_with("Exception") && t.contains(':') {
        Level::Exception
    } else {
        Level::Info
    }
}

fn strip_tag(line: &str) -> &str {
    if tag_level(line).is_some() {
        line.find(']').map_or(line, |i| &line[i + 1..])
    } else {
        line
    }
}

/// The mod that owns the code in a stack frame such as
/// `  at Kopernicus.Components.Foo.Bar () [0x0001]` or
/// `Kopernicus.Components.Foo:Bar()`.
fn frame_owner(line: &str, owners: &HashMap<String, String>) -> Option<String> {
    let mut parts = frame_parts(line)?;
    // Try the longest namespace prefix first: "Foo.Bar.Baz" -> foo.bar.baz, foo.bar, foo.
    while !parts.is_empty() {
        let candidate = parts.join(".").to_lowercase();
        if let Some(owner) = owners.get(&candidate) {
            return Some(owner.clone());
        }
        parts.pop();
    }
    None
}

/// The dotted symbol of a stack frame, split: `at A.B.C (..)` -> [A, B, C].
fn frame_parts(line: &str) -> Option<Vec<&str>> {
    let t = line.trim_start();
    let t = t.strip_prefix("at ").unwrap_or(t);
    let symbol = t.split([' ', '(']).next()?;
    let parts: Vec<&str> = symbol
        .split(['.', ':', '+'])
        .filter(|p| !p.is_empty())
        .collect();
    let ident = |p: &&str| p.chars().all(|c| c.is_alphanumeric() || "_`<>".contains(c));
    (parts.len() >= 2 && parts.iter().all(ident)).then_some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_exceptions_and_blames_mods() {
        let log = "\
[LOG 12:00:00.000] Loading
[WRN 12:00:01.000] Something odd
[EXC 12:00:02.000] NullReferenceException: Object reference not set to an instance of an object
\tKopernicus.Components.Ring.Update () (at <abc>:0)
\tUnityEngine.Debug:LogException(Exception)
[ERR 12:00:03.000] Cannot find texture
[EXC 12:00:04.000] NullReferenceException: Object reference not set to an instance of an object
  at Kopernicus.Components.Ring.Update () [0x00000] in <abc>:0
[LOG 12:00:05.000] done
";
        let mut owners = HashMap::new();
        owners.insert("kopernicus".to_owned(), "Kopernicus".to_owned());
        let r = analyse(log, &owners);
        assert_eq!(r.lines.len(), 9);
        assert_eq!(r.warnings, 1);
        assert_eq!(r.errors, 1);
        assert_eq!(r.exceptions.len(), 1);
        assert_eq!(r.exceptions[0].count, 2);
        assert_eq!(r.exceptions[0].mods, ["Kopernicus"]);
        assert_eq!(r.lines[r.exceptions[0].first_line].level, Level::Exception);

        // A namespace that isn't the DLL's name is found in the DLL's strings.
        let tmp = tempfile::tempdir().expect("tempdir");
        let dll = tmp.path().join("MechJeb2.dll");
        fs::write(&dll, b"MZ\x00junk\x00MuMech\x00MechJebCore\x00").expect("write");
        let asm = Assemblies {
            by_stem: [("mechjeb2".to_owned(), "MechJeb2".to_owned())].into(),
            dlls: vec![(dll, "MechJeb2".to_owned())],
        };
        let log_path = tmp.path().join("KSP.log");
        fs::write(
            &log_path,
            "[EXC 1] ArgumentOutOfRangeException: Index\n  at MuMech.MechJebCore.FixedUpdate () [0x0]\n  at Part.Update ()\n",
        )
        .expect("write");
        let r = read(&log_path, &asm).expect("read");
        assert_eq!(r.exceptions[0].mods, ["MechJeb2"]);

        let player = "ArgumentException: bad thing\n  at MechJeb2.Core.Tick () [0x0] in <x>:0\n";
        let mut owners = HashMap::new();
        owners.insert("mechjeb2".to_owned(), "MechJeb2".to_owned());
        let r = analyse(player, &owners);
        assert_eq!(r.exceptions.len(), 1);
        assert_eq!(r.exceptions[0].mods, ["MechJeb2"]);
    }
}
