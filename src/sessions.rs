//! Per-session game logs. KSP rewrites `KSP.log` on every launch and a Wine
//! prefix keeps a single `Player.log`, so a problem from one session is gone
//! a launch or two later. With session logs on (`config.toml`
//! `session_logs`), each launch opens a session in
//! `<instance>/sessions/<millis>/`, and once the game exits its logs are
//! copied there, gzipped. Marks ("something broke") record when a problem
//! showed up, as an offset into the live `KSP.log`.
//!
//! A session is closed when orbis sees the game exit (the GUI, the Steam
//! wrapper) or, failing that, before the next launch (of any instance that
//! shares its `Player.log`) or the next look at the logs. A log counts as the
//! session's only when it changed since the launch: each session records the
//! size and modification time of every log it may keep before the game
//! starts. Changes to the sessions of an instance hold
//! `<instance>/sessions/.lock`, so the GUI, the CLI and the Steam wrapper
//! never close or prune the same session at once.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::instance::Instance;
use crate::logs::LogKind;
use crate::paths::{self, Paths};
use crate::steam::SteamInfo;

/// One game session.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Directory name: the start time in milliseconds, zero-padded.
    #[serde(skip)]
    pub id: String,
    /// Start time, Unix milliseconds.
    pub started: u64,
    /// When the game last wrote a log, Unix milliseconds; `None` while the
    /// session is open.
    #[serde(default)]
    pub ended: Option<u64>,
    /// Logs kept, by `LogKind::label`.
    #[serde(default)]
    pub logs: Vec<String>,
    /// Logs that were written but couldn't be kept, with the reason.
    #[serde(default)]
    pub failed: Vec<String>,
    #[serde(default)]
    pub marks: Vec<Mark>,
    /// Size and modification time (ms) of each log file before the launch,
    /// by file role (`SOURCES`); absent files are left out.
    #[serde(default)]
    pub before: BTreeMap<String, (u64, u64)>,
}

/// "Something broke": a moment in a session.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Mark {
    /// Unix milliseconds.
    pub at: u64,
    #[serde(default)]
    pub note: String,
    /// `KSP.log`'s size when the mark was made, so it can be placed in the log.
    #[serde(default)]
    pub ksp_offset: Option<u64>,
    /// The time on `KSP.log`'s last line then (the game's local time).
    #[serde(default)]
    pub ksp_time: Option<String>,
}

impl Session {
    pub fn is_open(&self) -> bool {
        self.ended.is_none()
    }

    /// Length in whole minutes, once closed.
    pub fn minutes(&self) -> Option<u64> {
        self.ended.map(|e| e.saturating_sub(self.started) / 60_000)
    }

    pub fn has(&self, kind: LogKind) -> bool {
        self.logs.iter().any(|l| l == kind.label())
    }
}

const META: &str = "session.toml";
const LOCK: &str = ".lock";

/// How far before a mark exceptions count as near it, in seconds.
pub const MARK_BEFORE_SECS: u32 = 300;
/// And how far after it (KSP.log lags a little behind the game).
pub const MARK_AFTER_SECS: u32 = 30;

fn root(inst: &Instance) -> PathBuf {
    inst.dir.join("sessions")
}

/// The archive file of a log in a session directory.
fn file_name(kind: LogKind) -> &'static str {
    match kind {
        LogKind::Ksp => "KSP.log.gz",
        LogKind::Player => "Player.log.gz",
        LogKind::Launch => "launch.log.gz",
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Size and modification time in milliseconds.
fn fingerprint(path: &Path) -> Option<(u64, u64)> {
    let m = fs::metadata(path).ok()?;
    let d = m
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some((m.len(), u64::try_from(d.as_millis()).ok()?))
}

/// The log files a session may keep, by role.
fn files(inst: &Instance, player: Option<&Path>) -> Vec<(&'static str, PathBuf)> {
    let mut out = vec![
        ("ksp", inst.game_dir().join("KSP.log")),
        ("launch", inst.dir.join("orbis-launch.log")),
    ];
    if let Some(p) = player {
        out.push(("player", p.to_path_buf()));
        out.push(("player-prev", p.with_file_name("Player-prev.log")));
    }
    out
}

/// Holds the instance's session lock until dropped.
fn lock(inst: &Instance) -> Result<fs::File> {
    let dir = root(inst);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(LOCK);
    let f = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    f.lock()
        .with_context(|| format!("lock {}", path.display()))?;
    Ok(f)
}

fn save(inst: &Instance, s: &Session) -> Result<()> {
    let text = toml::to_string(s).context("serialize session")?;
    paths::write_atomic(&root(inst).join(&s.id).join(META), text.as_bytes())
}

/// Every session of an instance, newest first.
pub fn list(inst: &Instance) -> Result<Vec<Session>> {
    let dir = root(inst);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let meta = e.path().join(META);
        let Ok(text) = fs::read_to_string(&meta) else {
            continue;
        };
        match toml::from_str::<Session>(&text) {
            Ok(mut s) => {
                s.id = e.file_name().to_string_lossy().into_owned();
                out.push(s);
            }
            Err(err) => log::warn!("skip {}: {err}", meta.display()),
        }
    }
    out.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(out)
}

/// Opens a session for a launch that is about to start: first closes any
/// session left open (its logs are still on disk), then keeps the newest
/// `keep` closed sessions. `player` is where this instance's `Player.log`
/// lives. Open sessions of a game that is still running stay open.
pub fn start(inst: &Instance, player: Option<&Path>, keep: usize) -> Result<Session> {
    let _lock = lock(inst)?;
    if !crate::running::is_running(&inst.dir) {
        finish_locked(inst, player, None)?;
    }
    let started = now_millis();
    let s = Session {
        id: format!("{started:013}"),
        started,
        before: files(inst, player)
            .into_iter()
            .filter_map(|(role, p)| Some((role.to_owned(), fingerprint(&p)?)))
            .collect(),
        ..Session::default()
    };
    save(inst, &s)?;
    prune_locked(inst, keep)?;
    Ok(s)
}

/// Closes open sessions (only session `only` when given), copying in the
/// logs the game wrote since each started, then keeps the newest `keep`.
/// Call it only when the session's game isn't running. A session that got
/// no logs and no marks (a launch that failed) is deleted.
pub fn finish(
    inst: &Instance,
    player: Option<&Path>,
    only: Option<&str>,
    keep: usize,
) -> Result<()> {
    let _lock = lock(inst)?;
    finish_locked(inst, player, only)?;
    prune_locked(inst, keep)
}

fn finish_locked(inst: &Instance, player: Option<&Path>, only: Option<&str>) -> Result<()> {
    let open = list(inst)?
        .into_iter()
        .filter(|s| s.is_open() && only.is_none_or(|id| s.id == id));
    for mut s in open {
        let dir = root(inst).join(&s.id);
        let mut ended = s.started;
        for (kind, src) in sources(inst, player, &s.before) {
            if let Some((_, t)) = fingerprint(&src) {
                ended = ended.max(t);
            }
            match archive(&src, &dir.join(file_name(kind))) {
                Ok(()) => s.logs.push(kind.label().to_owned()),
                Err(e) => {
                    log::warn!("keep {}: {e:#}", src.display());
                    s.failed.push(format!("{}: {e:#}", kind.label()));
                }
            }
        }
        if s.logs.is_empty() && s.failed.is_empty() && s.marks.is_empty() {
            fs::remove_dir_all(&dir).with_context(|| format!("delete {}", dir.display()))?;
            continue;
        }
        s.ended = Some(ended);
        save(inst, &s)?;
    }
    Ok(())
}

/// The logs the game wrote for a session: files that changed since the
/// launch (`before`). A shared prefix's `Player.log` counts only when it
/// names this instance's game; `Player-prev.log` stands in for it when
/// another launch rotated it, but not when it is the file that was
/// `Player.log` before this session (rotation keeps size and time).
fn sources(
    inst: &Instance,
    player: Option<&Path>,
    before: &BTreeMap<String, (u64, u64)>,
) -> Vec<(LogKind, PathBuf)> {
    let changed =
        |role: &str, p: &Path| fingerprint(p).is_some_and(|fp| before.get(role) != Some(&fp));
    let mut out = Vec::new();
    let ksp = inst.game_dir().join("KSP.log");
    if changed("ksp", &ksp) {
        out.push((LogKind::Ksp, ksp));
    }
    if let Some(p) = player {
        let game = inst.game_dir();
        let earlier: Vec<&(u64, u64)> = ["player", "player-prev"]
            .iter()
            .filter_map(|r| before.get(*r))
            .collect();
        let prev = p.with_file_name("Player-prev.log");
        if let Some(found) = [p.to_path_buf(), prev].into_iter().find(|c| {
            fingerprint(c).is_some_and(|fp| !earlier.contains(&&fp)) && names_game(c, &game)
        }) {
            out.push((LogKind::Player, found));
        }
    }
    let launch = inst.dir.join("orbis-launch.log");
    if changed("launch", &launch) {
        out.push((LogKind::Launch, launch));
    }
    out
}

/// Whether a `Player.log` was written by the game in `game_dir`: Unity's
/// first lines name the game's data folder (`Z:/home/...` under Wine).
fn names_game(player: &Path, game_dir: &Path) -> bool {
    let Ok(f) = fs::File::open(player) else {
        return false;
    };
    let mut head = Vec::new();
    if f.take(8192).read_to_end(&mut head).is_err() {
        return false;
    }
    let game = game_dir.to_string_lossy();
    let mut needle = game.trim_end_matches('/').to_owned();
    needle.push('/');
    String::from_utf8_lossy(&head).contains(&needle)
}

/// Gzips `src` into `dest`, streaming through a temporary file that is
/// renamed into place.
fn archive(src: &Path, dest: &Path) -> Result<()> {
    let mut input = fs::File::open(src).with_context(|| format!("open {}", src.display()))?;
    let tmp = dest.with_extension("gz.tmp");
    let result = (|| -> Result<()> {
        let out = fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        let mut gz = flate2::write::GzEncoder::new(
            std::io::BufWriter::new(out),
            flate2::Compression::fast(),
        );
        std::io::copy(&mut input, &mut gz).context("compress log")?;
        let mut buf = gz.finish().context("compress log")?;
        buf.flush().context("write log")?;
        buf.get_ref().sync_all().context("sync log")?;
        fs::rename(&tmp, dest).with_context(|| format!("rename to {}", dest.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn prune_locked(inst: &Instance, keep: usize) -> Result<()> {
    let closed: Vec<Session> = list(inst)?.into_iter().filter(|s| !s.is_open()).collect();
    for s in closed.iter().skip(keep) {
        let dir = root(inst).join(&s.id);
        fs::remove_dir_all(&dir).with_context(|| format!("delete {}", dir.display()))?;
    }
    Ok(())
}

/// Before a launch: closes the sessions left open by this instance and by
/// stopped instances that share its `Player.log` (the launch is about to
/// overwrite it), then opens a session when session logs are on. Returns
/// the new session's id. Errors only warn: they never stop a launch.
pub fn launching(
    paths: &Paths,
    inst: &Instance,
    steam: &SteamInfo,
    config: &Config,
) -> Option<String> {
    let player = crate::logs::path(inst, steam, LogKind::Player);
    if let Some(p) = &player {
        for other in crate::instance::list(paths).unwrap_or_default() {
            if other.id != inst.id
                && crate::logs::path(&other, steam, LogKind::Player).as_ref() == Some(p)
            {
                settle(&other, steam, config);
            }
        }
    }
    let keep = config.session_logs_keep;
    let result = if config.session_logs {
        start(inst, player.as_deref(), keep).map(|s| Some(s.id))
    } else {
        finish(inst, player.as_deref(), None, keep).map(|()| None)
    };
    result.unwrap_or_else(|e| {
        log::warn!("session logs for {}: {e:#}", inst.config.name);
        None
    })
}

/// After the game of session `id` exits: closes that session. Errors only
/// warn.
pub fn exited(inst: &Instance, steam: &SteamInfo, config: &Config, id: &str) {
    let player = crate::logs::path(inst, steam, LogKind::Player);
    if let Err(e) = finish(inst, player.as_deref(), Some(id), config.session_logs_keep) {
        log::warn!("session logs for {}: {e:#}", inst.config.name);
    }
}

/// When the instance's game isn't running: closes its open sessions (a
/// launch nobody watched exit). Errors only warn.
pub fn settle(inst: &Instance, steam: &SteamInfo, config: &Config) {
    if crate::running::is_running(&inst.dir) {
        return;
    }
    let player = crate::logs::path(inst, steam, LogKind::Player);
    if let Err(e) = finish(inst, player.as_deref(), None, config.session_logs_keep) {
        log::warn!("session logs for {}: {e:#}", inst.config.name);
    }
}

/// Marks the open session: something broke now.
pub fn mark(inst: &Instance, note: &str) -> Result<Mark> {
    let _lock = lock(inst)?;
    let Some(mut s) = list(inst)?.into_iter().find(Session::is_open) else {
        bail!(
            "{} has no open game session: start the game from orbis with session logs on",
            inst.config.name
        );
    };
    let ksp = inst.game_dir().join("KSP.log");
    let m = Mark {
        at: now_millis(),
        note: note.trim().to_owned(),
        ksp_offset: fs::metadata(&ksp).ok().map(|m| m.len()),
        ksp_time: last_time(&ksp),
    };
    s.marks.push(m.clone());
    save(inst, &s)?;
    Ok(m)
}

/// The `hh:mm:ss` of the last tagged line in a KSP.log.
fn last_time(ksp: &Path) -> Option<String> {
    use std::io::{Seek as _, SeekFrom};
    let mut f = fs::File::open(ksp).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(65_536))).ok()?;
    let mut tail = Vec::new();
    f.read_to_end(&mut tail).ok()?;
    let text = String::from_utf8_lossy(&tail);
    text.lines()
        .rev()
        .find_map(crate::logs::line_time)
        .map(|secs| {
            format!(
                "{:02}:{:02}:{:02}",
                secs / 3600,
                secs % 3600 / 60,
                secs % 60
            )
        })
}

/// A session's kept copy of a log, unpacked, with the path it was read from.
pub fn read_log(inst: &Instance, id: &str, kind: LogKind) -> Result<(PathBuf, String)> {
    let path = root(inst).join(id).join(file_name(kind));
    let f = fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(f)
        .read_to_end(&mut bytes)
        .with_context(|| format!("unpack {}", path.display()))?;
    Ok((path, String::from_utf8_lossy(&bytes).into_owned()))
}

/// The line index a mark falls on in its session's KSP.log text: the last
/// line that had started when the mark was made.
pub fn mark_line(text: &str, m: &Mark) -> Option<usize> {
    let offset = usize::try_from(m.ksp_offset?).ok()?;
    let upto = &text.as_bytes()[..offset.min(text.len())];
    if upto.is_empty() {
        return None;
    }
    let newlines = upto.iter().filter(|&&b| b == b'\n').count();
    // A prefix ending mid-line is on the line after its last newline.
    Some(if upto.ends_with(b"\n") {
        newlines - 1
    } else {
        newlines
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(dir: &Path) -> Instance {
        fs::create_dir_all(dir.join("game")).expect("game dir");
        Instance {
            id: "test".into(),
            dir: dir.to_path_buf(),
            config: toml::from_str("name = \"T\"\ngame_version = \"1.12.5\"\ncreated = 0\n")
                .expect("config"),
        }
    }

    fn player_for(inst: &Instance) -> String {
        format!(
            "Mono path[0] = 'Z:{}/KSP_x64_Data/Managed'\n",
            inst.game_dir().display()
        )
    }

    #[test]
    fn keeps_each_sessions_logs_and_marks() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let inst = instance(&tmp.path().join("inst"));
        let player = tmp.path().join("prefix/Player.log");
        fs::create_dir_all(player.parent().expect("parent")).expect("mkdir");

        let s = start(&inst, Some(&player), 2).expect("start");
        assert!(s.is_open());
        let ksp = inst.game_dir().join("KSP.log");
        fs::write(&ksp, "[LOG 21:45:31.554] hello\n[EXC 21:46:02.000] Boom\n").expect("write");
        let m = mark(&inst, " the rover flipped ").expect("mark");
        assert_eq!(m.note, "the rover flipped");
        assert_eq!(m.ksp_time.as_deref(), Some("21:46:02"));
        fs::write(&player, player_for(&inst)).expect("write");
        finish(&inst, Some(&player), Some(&s.id), 2).expect("finish");

        let all = list(&inst).expect("list");
        assert_eq!(all.len(), 1);
        assert!(!all[0].is_open());
        assert!(all[0].has(LogKind::Ksp) && all[0].has(LogKind::Player));
        assert!(!all[0].has(LogKind::Launch), "no launch log was written");
        let (_, text) = read_log(&inst, &all[0].id, LogKind::Ksp).expect("read");
        assert!(text.contains("Boom"));
        assert_eq!(mark_line(&text, &all[0].marks[0]), Some(1));
        assert!(mark(&inst, "").is_err(), "no open session");
    }

    #[test]
    fn keeps_only_logs_this_launch_wrote() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let inst = instance(&tmp.path().join("inst"));
        let player = tmp.path().join("Player.log");
        let ksp = inst.game_dir().join("KSP.log");
        // The previous run's logs, and another instance's Player.log.
        fs::write(&ksp, "[LOG 09:00:00.000] previous run\n").expect("write");
        fs::write(
            &player,
            "Mono path[0] = 'Z:/elsewhere/game/KSP_x64_Data/Managed'\n",
        )
        .expect("write");
        let s = start(&inst, Some(&player), 5).expect("start");
        // A launch that wrote nothing leaves no session behind, even right
        // after the previous run.
        finish(&inst, Some(&player), Some(&s.id), 5).expect("finish");
        assert!(list(&inst).expect("list").is_empty());

        // This instance's earlier Player.log is current at launch; the run
        // rotates it to Player-prev.log unchanged, and then another instance
        // overwrites Player.log. Neither file is this session's.
        fs::write(&player, player_for(&inst)).expect("write");
        let s = start(&inst, Some(&player), 5).expect("start");
        fs::write(&ksp, "[LOG 10:00:00.000] this run\n").expect("write");
        fs::rename(&player, tmp.path().join("Player-prev.log")).expect("rotate");
        fs::write(&player, "Mono path[0] = 'Z:/elsewhere/game/x'\n").expect("write");
        finish(&inst, Some(&player), Some(&s.id), 5).expect("finish");
        let all = list(&inst).expect("list");
        assert_eq!(all[0].logs, ["KSP.log"]);
    }

    #[test]
    fn closes_only_the_named_session_and_prunes_after_closing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let inst = instance(&tmp.path().join("inst"));
        let ksp = inst.game_dir().join("KSP.log");
        let mut ids = Vec::new();
        for i in 0..3 {
            let s = start(&inst, None, 1).expect("start");
            fs::write(&ksp, format!("[LOG 10:00:0{i}.000] run {i}\n")).expect("write");
            finish(&inst, None, Some(&s.id), 1).expect("finish");
            ids.push(s.id);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let all = list(&inst).expect("list");
        assert_eq!(all.len(), 1, "keep = 1 holds after closing");
        assert_eq!(all[0].id, ids[2]);

        // An old exit doesn't close a newer running session.
        let open = start(&inst, None, 1).expect("start");
        fs::write(&ksp, "[LOG 11:00:00.000] running\n").expect("write");
        finish(&inst, None, Some(&ids[2]), 1).expect("finish old");
        assert!(list(&inst).expect("list")[0].is_open());
        assert_eq!(list(&inst).expect("list")[0].id, open.id);
    }

    #[test]
    fn a_failed_copy_keeps_the_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let inst = instance(&tmp.path().join("inst"));
        let s = start(&inst, None, 5).expect("start");
        fs::write(inst.game_dir().join("KSP.log"), "[LOG 10:00:00.000] a\n").expect("write");
        // A directory where the archive should go makes the rename fail.
        fs::create_dir_all(root(&inst).join(&s.id).join("KSP.log.gz/x")).expect("mkdir");
        finish(&inst, None, Some(&s.id), 5).expect("finish");
        let all = list(&inst).expect("list");
        assert_eq!(all.len(), 1);
        assert!(all[0].logs.is_empty());
        assert_eq!(all[0].failed.len(), 1);
    }

    #[test]
    fn places_marks_on_lines() {
        let m = |offset| Mark {
            ksp_offset: Some(offset),
            ..Mark::default()
        };
        let text = "a\nb\nc";
        for (offset, want) in [
            (0, None),
            (2, Some(0)),
            (3, Some(1)),
            (4, Some(1)),
            (5, Some(2)),
        ] {
            assert_eq!(mark_line(text, &m(offset)), want, "offset {offset}");
        }
        assert_eq!(mark_line("", &m(0)), None);
    }
}
