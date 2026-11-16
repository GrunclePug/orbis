//! Which instances are running, recorded on disk so every orbis process (GUI,
//! CLI, the Steam wrapper) agrees, even after the one that launched the game
//! has exited.
//!
//! Launching writes `<instance>/running.toml` with the process that lives as
//! long as the game (Proton, or the Steam wrapper) and its start time from
//! `/proc`, so a recycled PID doesn't look like the game.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

/// The process that runs an instance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Running {
    pub pid: u32,
    /// The process start time from `/proc/<pid>/stat`, in clock ticks.
    pub start: u64,
    /// How it was started: `direct` or `steam`.
    pub via: String,
}

fn path(instance_dir: &Path) -> PathBuf {
    instance_dir.join("running.toml")
}

/// The start time of a live process, or `None` when it doesn't exist.
fn start_time(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the command name, which is in parentheses and may hold
    // spaces; starttime is field 22, the 20th after it.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Records that `pid` runs the instance in `instance_dir`.
pub fn mark(instance_dir: &Path, pid: u32, via: &str) -> Result<()> {
    let r = Running {
        pid,
        start: start_time(pid).with_context(|| format!("process {pid} is not running"))?,
        via: via.to_owned(),
    };
    let text = toml::to_string(&r).context("serialize running state")?;
    paths::write_atomic(&path(instance_dir), text.as_bytes())
}

/// Forgets the running state, if it is `pid`'s.
pub fn clear(instance_dir: &Path, pid: u32) {
    if current(instance_dir).is_some_and(|r| r.pid == pid) || state(instance_dir).is_none() {
        let _ = fs::remove_file(path(instance_dir));
    }
}

fn state(instance_dir: &Path) -> Option<Running> {
    let text = fs::read_to_string(path(instance_dir)).ok()?;
    toml::from_str(&text).ok()
}

/// The process running the instance, if it is still alive.
pub fn current(instance_dir: &Path) -> Option<Running> {
    state(instance_dir).filter(|r| start_time(r.pid) == Some(r.start))
}

/// Fails when the instance's game is running: mods, saves and the instance
/// itself must not change under KSP.
pub fn refuse_if_running(inst: &crate::instance::Instance) -> Result<()> {
    if let Some(r) = current(&inst.dir) {
        anyhow::bail!(
            "{} is running (process {}); close the game first",
            inst.config.name,
            r.pid
        );
    }
    Ok(())
}

/// Whether the instance's game is running.
pub fn is_running(instance_dir: &Path) -> bool {
    current(instance_dir).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_a_live_process() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(!is_running(tmp.path()));
        let me = std::process::id();
        mark(tmp.path(), me, "direct").expect("mark");
        assert!(is_running(tmp.path()));
        clear(tmp.path(), me + 1);
        assert!(is_running(tmp.path()), "another pid can't clear it");
        clear(tmp.path(), me);
        assert!(!is_running(tmp.path()));
    }

    #[test]
    fn a_dead_or_recycled_pid_is_not_running() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let r = Running {
            pid: std::process::id(),
            start: 1,
            via: "direct".into(),
        };
        fs::write(path(tmp.path()), toml::to_string(&r).expect("toml")).expect("write");
        assert!(!is_running(tmp.path()));
    }
}
