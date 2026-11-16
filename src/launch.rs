//! Starting an instance through Proton, the way Steam does but pointed at
//! the instance's own game directory.
//!
//! KSP 1 has no DRM, so it runs fine outside Steam. Orbis uses the Proton
//! build Steam has mapped to KSP and, when that Proton asks for it, wraps
//! it in the Steam Linux Runtime container exactly like Steam.

use std::ffi::OsString;
use std::fs::{self, File};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::instance::{Instance, PrefixMode};
use crate::steam::{self, KSP_APP_ID, SteamInfo};

/// A fully worked-out launch command.
#[derive(Clone, Debug)]
pub struct LaunchPlan {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(String, OsString)>,
    pub cwd: PathBuf,
    pub log: PathBuf,
}

impl LaunchPlan {
    /// A shell-like rendering for `--dry-run` and the GUI.
    pub fn display(&self) -> String {
        let quote = |s: &str| {
            if s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c))
            {
                s.to_owned()
            } else {
                format!("'{}'", s.replace('\'', r"'\''"))
            }
        };
        let mut parts: Vec<String> = vec![format!("cd {} &&", quote(&self.cwd.to_string_lossy()))];
        for (k, v) in &self.env {
            parts.push(format!("{k}={}", quote(&v.to_string_lossy())));
        }
        parts.push(quote(&self.program.to_string_lossy()));
        parts.extend(self.args.iter().map(|a| quote(&a.to_string_lossy())));
        parts.join(" ")
    }
}

/// Works out how to start an instance.
pub fn plan(inst: &Instance, steam: &SteamInfo, config: &Config) -> Result<LaunchPlan> {
    let game = inst.game_dir();
    let exe = game.join("KSP_x64.exe");
    if !exe.is_file() {
        bail!("{} is missing", exe.display());
    }
    let proton = steam.proton.clone().context(
        "no Proton found: set `proton` in config.toml or pick a Proton for KSP in Steam",
    )?;
    if !proton.join("proton").is_file() {
        bail!("{} has no proton script", proton.display());
    }
    let root = steam
        .root
        .clone()
        .context("Steam not found: set `steam_root` in config.toml")?;
    let prefix = match inst.config.launch.prefix {
        PrefixMode::Steam => steam.ksp_prefix().context(
            "Steam's KSP prefix not found; run KSP from Steam once or use an instance prefix",
        )?,
        PrefixMode::Instance => inst.prefix_dir(),
    };
    fs::create_dir_all(&prefix).with_context(|| format!("create {}", prefix.display()))?;

    let mut tool_paths = vec![proton.clone()];
    let sniper = steam
        .sniper
        .clone()
        .filter(|_| config.use_steam_runtime && steam::proton_needs_sniper(&proton));
    if let Some(s) = &sniper {
        tool_paths.push(s.clone());
    }
    let join = |v: &[PathBuf]| -> OsString {
        let mut s = OsString::new();
        for (i, p) in v.iter().enumerate() {
            if i > 0 {
                s.push(":");
            }
            s.push(p);
        }
        s
    };
    let mut env: Vec<(String, OsString)> = vec![
        (
            "STEAM_COMPAT_CLIENT_INSTALL_PATH".into(),
            root.into_os_string(),
        ),
        ("STEAM_COMPAT_DATA_PATH".into(), prefix.into_os_string()),
        (
            "STEAM_COMPAT_INSTALL_PATH".into(),
            game.clone().into_os_string(),
        ),
        ("STEAM_COMPAT_TOOL_PATHS".into(), join(&tool_paths)),
        ("STEAM_COMPAT_APP_ID".into(), KSP_APP_ID.into()),
        ("SteamAppId".into(), KSP_APP_ID.into()),
        ("SteamGameId".into(), KSP_APP_ID.into()),
    ];
    // Let the Steam Runtime container see the linked base install, which
    // may live outside the home directory.
    if let Some(base) = &inst.config.base {
        env.push((
            "STEAM_COMPAT_MOUNTS".into(),
            join(&[base.clone(), inst.dir.clone()]),
        ));
    }
    for (k, v) in &inst.config.launch.env {
        env.retain(|(ek, _)| ek != k);
        env.push((k.clone(), v.into()));
    }

    let mut args: Vec<OsString> = Vec::new();
    let program = match &sniper {
        Some(s) => {
            args.push("--verb=waitforexitandrun".into());
            args.push("--".into());
            args.push(proton.join("proton").into_os_string());
            s.join("_v2-entry-point")
        }
        None => proton.join("proton"),
    };
    args.push("waitforexitandrun".into());
    args.push(exe.into_os_string());
    args.extend(inst.config.launch.args.iter().map(OsString::from));
    Ok(LaunchPlan {
        program,
        args,
        env,
        cwd: game,
        log: inst.dir.join("orbis-launch.log"),
    })
}

/// Starts the game in its own process group, logging Proton's output to
/// `orbis-launch.log` in the instance directory.
pub fn spawn(plan: &LaunchPlan) -> Result<Child> {
    let log = File::create(&plan.log).with_context(|| format!("create {}", plan.log.display()))?;
    let log2 = log.try_clone().context("duplicate log handle")?;
    Command::new(&plan.program)
        .args(&plan.args)
        .envs(plan.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&plan.cwd)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log2)
        .process_group(0)
        .spawn()
        .with_context(|| format!("start {}", plan.program.display()))
}
