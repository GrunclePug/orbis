//! Starting an instance: the Windows build through Proton (the way Steam
//! does, but pointed at the instance's own game directory) or Wine, or the
//! native Linux build directly.
//!
//! KSP 1 has no DRM, so it runs fine outside Steam. For Proton, orbis uses
//! the build Steam has mapped to KSP and, when that Proton asks for it,
//! wraps it in the Steam Linux Runtime container exactly like Steam.

use std::ffi::OsString;
use std::fs::{self, File};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::game::{self, Build};
use crate::instance::{Instance, PrefixMode, Runner};
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

/// What runs an instance, once `Runner::Auto` is settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolved {
    Proton,
    Wine,
    Native,
}

impl Resolved {
    pub fn label(self) -> &'static str {
        match self {
            Resolved::Proton => "Proton",
            Resolved::Wine => "Wine",
            Resolved::Native => "the native Linux build",
        }
    }
}

/// Settles what runs the instance: its runner setting, checked against the
/// builds its game directory holds. `Auto` runs the Windows build through
/// Proton when one is found and Wine otherwise, and the Linux build when
/// that's the only one.
pub fn resolve(inst: &Instance, steam: &SteamInfo) -> Result<Resolved> {
    let builds = game::builds(&inst.game_dir());
    let windows = builds.contains(&Build::Windows);
    let linux = builds.contains(&Build::Linux);
    let no_windows = |how: &str| {
        anyhow::anyhow!(
            "{} has no {} (the Windows build) to run through {how}",
            inst.config.name,
            game::WINDOWS_EXE
        )
    };
    Ok(match inst.config.launch.runner {
        Runner::Proton if windows => Resolved::Proton,
        Runner::Proton => return Err(no_windows("Proton")),
        Runner::Wine if windows => Resolved::Wine,
        Runner::Wine => return Err(no_windows("Wine")),
        Runner::Native if linux => Resolved::Native,
        Runner::Native => bail!(
            "{} has no {} (the native Linux build). Steam installs it when KSP's \
             compatibility tool is off; GOG and the KSP store offer it as a download",
            inst.config.name,
            game::LINUX_EXE
        ),
        Runner::Auto if windows && steam.proton.is_some() => Resolved::Proton,
        Runner::Auto if windows => Resolved::Wine,
        Runner::Auto if linux => Resolved::Native,
        Runner::Auto => bail!(
            "{} has neither {} nor {}",
            inst.game_dir().display(),
            game::WINDOWS_EXE,
            game::LINUX_EXE
        ),
    })
}

/// Works out how to start an instance.
pub fn plan(inst: &Instance, steam: &SteamInfo, config: &Config) -> Result<LaunchPlan> {
    match resolve(inst, steam)? {
        Resolved::Proton => plan_proton(inst, steam, config),
        Resolved::Wine => plan_wine(inst, config),
        Resolved::Native => Ok(plan_native(inst)),
    }
}

/// The Wine prefix of an instance run through Wine, laid out like Proton's
/// (the Wine prefix proper in `pfx/`).
pub fn wine_prefix(inst: &Instance) -> PathBuf {
    inst.prefix_dir().join("pfx")
}

fn plan_wine(inst: &Instance, config: &Config) -> Result<LaunchPlan> {
    let game = inst.game_dir();
    let wine = match &config.wine {
        Some(w) if w.is_file() => w.clone(),
        Some(w) => bail!("{} (wine in config.toml) doesn't exist", w.display()),
        None => find_in_path("wine").context(
            "no Proton or Wine found: install Wine, set `wine` or `proton` in config.toml, or pick a Proton for KSP in Steam",
        )?,
    };
    let prefix = wine_prefix(inst);
    fs::create_dir_all(&prefix).with_context(|| format!("create {}", prefix.display()))?;
    let mut env: Vec<(String, OsString)> = vec![("WINEPREFIX".into(), prefix.into_os_string())];
    add_user_env(inst, &mut env);
    let mut args = vec![game.join(game::WINDOWS_EXE).into_os_string()];
    args.extend(inst.config.launch.args.iter().map(OsString::from));
    Ok(LaunchPlan {
        program: wine,
        args,
        env,
        cwd: game,
        log: inst.dir.join("orbis-launch.log"),
    })
}

fn plan_native(inst: &Instance) -> LaunchPlan {
    let game = inst.game_dir();
    // KSP reads numbers in config files with the system locale: where the
    // decimal separator is a comma, parts and mods load wrong.
    let mut env: Vec<(String, OsString)> = vec![("LC_ALL".into(), "C".into())];
    add_user_env(inst, &mut env);
    LaunchPlan {
        program: game.join(game::LINUX_EXE),
        args: inst.config.launch.args.iter().map(OsString::from).collect(),
        env,
        cwd: game,
        log: inst.dir.join("orbis-launch.log"),
    }
}

/// The instance's own environment variables, overriding orbis's.
fn add_user_env(inst: &Instance, env: &mut Vec<(String, OsString)>) {
    for (k, v) in &inst.config.launch.env {
        env.retain(|(ek, _)| ek != k);
        env.push((k.clone(), v.into()));
    }
}

/// The first executable called `name` on PATH.
fn find_in_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// The Proton prefix an instance runs in: Steam's KSP prefix when it shares
/// it, and its own when it doesn't or there's no Steam.
pub fn proton_prefix(inst: &Instance, steam: &SteamInfo) -> PathBuf {
    match (inst.config.launch.prefix, steam.ksp_prefix()) {
        (PrefixMode::Steam, Some(p)) => p,
        _ => inst.prefix_dir(),
    }
}

fn plan_proton(inst: &Instance, steam: &SteamInfo, config: &Config) -> Result<LaunchPlan> {
    let game = inst.game_dir();
    let exe = game.join(game::WINDOWS_EXE);
    let proton = steam.proton.clone().context(
        "no Proton found: set `proton` in config.toml or pick a Proton for KSP in Steam",
    )?;
    if !proton.join("proton").is_file() {
        bail!("{} has no proton script", proton.display());
    }
    // Proton runs without Steam too (KSP has no DRM); it only uses Steam's
    // folder for the Steam client libraries, which KSP doesn't need.
    let root = steam.root.clone().unwrap_or_default();
    let prefix = proton_prefix(inst, steam);
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
    add_user_env(inst, &mut env);

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

// ---- launching through Steam ---------------------------------------------
//
// Steam runs `orbis steam-run <its command>` when KSP's launch options are
// `orbis steam-run %command%`. Before asking Steam to start KSP, orbis
// leaves a short-lived note naming the instance; the wrapper consumes it and
// points Steam's command at that instance. Without a fresh note the command
// runs unchanged, so launching KSP from the Steam library stays vanilla.

/// How long a launch note stays valid.
const NOTE_TTL_SECS: u64 = 120;

/// The note orbis leaves for the wrapper.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SteamNote {
    pub created: u64,
    pub instance: String,
    pub game_dir: PathBuf,
    pub instance_dir: PathBuf,
    pub base: Option<PathBuf>,
    /// Set when the instance uses its own Wine prefix.
    pub prefix: Option<PathBuf>,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

fn note_path(paths: &crate::paths::Paths) -> PathBuf {
    paths.cache.join("steam-launch.toml")
}

/// The launch options KSP needs in Steam for `via_steam` instances.
pub fn steam_launch_options(root: Option<&std::path::Path>) -> String {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "orbis".into());
    let quote = |s: &str| {
        if s.contains(' ') {
            format!("\"{s}\"")
        } else {
            s.to_owned()
        }
    };
    match root {
        Some(r) => format!(
            "{} --root {} steam-run %command%",
            quote(&exe),
            quote(&r.display().to_string())
        ),
        None => format!("{} steam-run %command%", quote(&exe)),
    }
}

/// KSP's launch options as Steam has them, from the first Steam user that
/// has any (read-only).
pub fn current_steam_launch_options(steam: &SteamInfo) -> Option<String> {
    let root = steam.root.as_ref()?;
    let users = fs::read_dir(root.join("userdata")).ok()?;
    for user in users.flatten() {
        let path = user.path().join("config/localconfig.vdf");
        let Ok(bytes) = fs::read(&path) else { continue };
        let Ok(v) = crate::vdf::Vdf::parse(&String::from_utf8_lossy(&bytes)) else {
            continue;
        };
        let opts = v
            .path(&[
                "UserLocalConfigStore",
                "Software",
                "Valve",
                "Steam",
                "apps",
                KSP_APP_ID,
                "LaunchOptions",
            ])
            .and_then(crate::vdf::Vdf::as_str);
        if let Some(o) = opts {
            return Some(o.to_owned());
        }
    }
    None
}

/// Asks Steam to start KSP as the given instance.
pub fn launch_via_steam(
    paths: &crate::paths::Paths,
    inst: &Instance,
    steam: &SteamInfo,
) -> Result<()> {
    let game = inst.game_dir();
    if !game::is_game_dir(&game) {
        bail!("{} holds no KSP build", game.display());
    }
    let opts = current_steam_launch_options(steam).unwrap_or_default();
    if !opts.contains("steam-run") {
        bail!(
            "KSP's Steam launch options don't run orbis yet. In Steam, open KSP > Properties > Launch Options and enter:\n{}",
            steam_launch_options(None)
        );
    }
    let note = SteamNote {
        created: crate::paths::now_secs(),
        instance: inst.config.name.clone(),
        game_dir: game,
        instance_dir: inst.dir.clone(),
        base: inst.config.base.clone(),
        prefix: (inst.config.launch.prefix == PrefixMode::Instance).then(|| inst.prefix_dir()),
        args: inst.config.launch.args.clone(),
        env: inst
            .config
            .launch
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    };
    let text = toml::to_string_pretty(&note).context("serialize launch note")?;
    crate::paths::write_atomic(&note_path(paths), text.as_bytes())?;
    Command::new("steam")
        .arg(format!("steam://rungameid/{KSP_APP_ID}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("run steam")?;
    Ok(())
}

/// What the wrapper runs.
#[derive(Debug, PartialEq)]
pub struct Rewritten {
    pub args: Vec<OsString>,
    pub env: Vec<(String, OsString)>,
    pub cwd: Option<PathBuf>,
}

/// Points Steam's command at the instance in `note`, if it's fresh.
pub fn rewrite(command: &[OsString], note: Option<&SteamNote>, now: u64) -> Rewritten {
    let untouched = Rewritten {
        args: command.to_vec(),
        env: vec![],
        cwd: None,
    };
    let Some(note) = note.filter(|n| now.saturating_sub(n.created) <= NOTE_TTL_SECS) else {
        return untouched;
    };
    // Steam's command ends in the Windows build under Proton, or the Linux
    // build in the Steam Runtime: point whichever it is at the instance.
    let Some((i, exe)) = command.iter().enumerate().find_map(|(i, a)| {
        let name = std::path::Path::new(a).file_name()?.to_str()?;
        [game::WINDOWS_EXE, game::LINUX_EXE]
            .into_iter()
            .find(|e| e.eq_ignore_ascii_case(name))
            .map(|e| (i, e))
    }) else {
        return untouched;
    };
    let mut args = command.to_vec();
    args[i] = note.game_dir.join(exe).into_os_string();
    args.extend(note.args.iter().map(OsString::from));
    let mut mounts = std::env::var_os("STEAM_COMPAT_MOUNTS").unwrap_or_default();
    for p in note.base.iter().chain(std::iter::once(&note.instance_dir)) {
        if !mounts.is_empty() {
            mounts.push(":");
        }
        mounts.push(p);
    }
    let mut env = vec![
        (
            "STEAM_COMPAT_INSTALL_PATH".to_owned(),
            note.game_dir.clone().into_os_string(),
        ),
        ("STEAM_COMPAT_MOUNTS".to_owned(), mounts),
    ];
    if exe == game::LINUX_EXE {
        // As for a direct native launch (see `plan_native`); the
        // instance's own variables come after, so they still win.
        env.push(("LC_ALL".to_owned(), "C".into()));
    }
    if let Some(prefix) = &note.prefix {
        env.push((
            "STEAM_COMPAT_DATA_PATH".to_owned(),
            prefix.clone().into_os_string(),
        ));
    }
    env.extend(note.env.iter().map(|(k, v)| (k.clone(), OsString::from(v))));
    Rewritten {
        args,
        env,
        cwd: Some(note.game_dir.clone()),
    }
}

/// The `orbis steam-run` wrapper: runs Steam's command, pointed at the
/// instance orbis asked for when there is one, and returns its exit code.
/// Never refuses to run the game: any trouble with the note means vanilla.
pub fn steam_run(paths: &crate::paths::Paths, command: &[OsString]) -> Result<i32> {
    if command.is_empty() {
        bail!("steam-run needs Steam's command (launch options: orbis steam-run %command%)");
    }
    let path = note_path(paths);
    let note: Option<SteamNote> = fs::read_to_string(&path)
        .ok()
        .and_then(|t| toml::from_str(&t).ok());
    // One launch per note.
    let _ = fs::remove_file(&path);
    let r = rewrite(command, note.as_ref(), crate::paths::now_secs());
    let log_line = match (&note, r.cwd.is_some()) {
        (Some(n), true) => format!(
            "launching instance {} ({})",
            n.instance,
            n.game_dir.display()
        ),
        (Some(_), false) => {
            "launch note ignored (stale, or no KSP executable in Steam's command)".to_owned()
        }
        (None, _) => "no launch note: running KSP from the Steam install".to_owned(),
    };
    let log = paths.cache.join("steam-run.log");
    let _ = fs::create_dir_all(&paths.cache);
    let _ = fs::write(
        &log,
        format!(
            "{}\n{}\n",
            log_line,
            r.args
                .iter()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" ")
        ),
    );
    let mut cmd = Command::new(&r.args[0]);
    cmd.args(&r.args[1..])
        .envs(r.env.iter().map(|(k, v)| (k, v)));
    if let Some(cwd) = &r.cwd {
        cmd.current_dir(cwd);
    }
    // While this wrapper lives, the instance's game runs.
    let instance_dir = note
        .as_ref()
        .filter(|_| r.cwd.is_some())
        .map(|n| n.instance_dir.clone());
    if let Some(dir) = &instance_dir
        && let Err(e) = crate::running::mark(dir, std::process::id(), "steam")
    {
        log::warn!("record running instance: {e:#}");
    }
    let status = cmd
        .status()
        .with_context(|| format!("run {}", r.args[0].to_string_lossy()));
    if let Some(dir) = &instance_dir {
        crate::running::clear(dir, std::process::id());
        let line = match paths
            .check_format()
            .and_then(|()| crate::crafts::sync(paths))
        {
            Ok(rep) => format!("craft sync: {}\n", rep.summary()),
            Err(e) => format!("craft sync failed: {e:#}\n"),
        };
        if let Ok(mut f) = fs::OpenOptions::new().append(true).open(&log) {
            use std::io::Write as _;
            let _ = f.write_all(line.as_bytes());
        }
    }
    Ok(status?.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(created: u64) -> SteamNote {
        SteamNote {
            created,
            instance: "Career".into(),
            game_dir: PathBuf::from("/orbis/instances/career/game"),
            instance_dir: PathBuf::from("/orbis/instances/career"),
            base: Some(PathBuf::from("/steam/common/Kerbal Space Program")),
            prefix: None,
            args: vec!["-popupwindow".into()],
            env: vec![("DXVK_HUD".into(), "fps".into())],
        }
    }

    fn instance_with(dir: &std::path::Path, exes: &[&str], runner: Runner) -> Instance {
        let game = dir.join("game");
        fs::create_dir_all(game.join("GameData")).expect("mkdir");
        for e in exes {
            fs::write(game.join(e), b"").expect("write");
        }
        let mut inst = Instance {
            id: "t".into(),
            dir: dir.to_path_buf(),
            config: toml::from_str("name = \"T\"\ngame_version = \"1.12.5\"\ncreated = 0\n")
                .expect("config"),
        };
        inst.config.launch.runner = runner;
        inst
    }

    #[test]
    fn picks_what_runs_the_game() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let with_proton = SteamInfo {
            proton: Some(PathBuf::from("/proton")),
            ..SteamInfo::default()
        };
        let none = SteamInfo::default();
        let (win, lin) = (game::WINDOWS_EXE, game::LINUX_EXE);
        let cases: [(&[&str], Runner, &SteamInfo, Option<Resolved>); 9] = [
            (&[win], Runner::Auto, &with_proton, Some(Resolved::Proton)),
            (&[win], Runner::Auto, &none, Some(Resolved::Wine)),
            (&[lin], Runner::Auto, &with_proton, Some(Resolved::Native)),
            (
                &[win, lin],
                Runner::Auto,
                &with_proton,
                Some(Resolved::Proton),
            ),
            (
                &[win, lin],
                Runner::Native,
                &with_proton,
                Some(Resolved::Native),
            ),
            (&[win], Runner::Wine, &with_proton, Some(Resolved::Wine)),
            (&[lin], Runner::Proton, &with_proton, None),
            (&[win], Runner::Native, &with_proton, None),
            (&[], Runner::Auto, &with_proton, None),
        ];
        for (i, (exes, runner, steam, want)) in cases.into_iter().enumerate() {
            let inst = instance_with(&tmp.path().join(i.to_string()), exes, runner);
            assert_eq!(
                resolve(&inst, steam).ok(),
                want,
                "case {i}: {exes:?} {runner:?}"
            );
        }
    }

    #[test]
    fn plans_native_and_wine_launches() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut inst = instance_with(tmp.path(), &[game::LINUX_EXE], Runner::Native);
        inst.config.launch.args = vec!["-popupwindow".into()];
        let p = plan(&inst, &SteamInfo::default(), &Config::default()).expect("native");
        assert_eq!(p.program, inst.game_dir().join(game::LINUX_EXE));
        assert_eq!(p.args, [OsString::from("-popupwindow")]);
        assert!(p.env.contains(&("LC_ALL".into(), "C".into())));

        // The instance's own variables win.
        inst.config
            .launch
            .env
            .insert("LC_ALL".into(), "en_US.UTF-8".into());
        let p = plan(&inst, &SteamInfo::default(), &Config::default()).expect("native");
        assert_eq!(
            p.env,
            [("LC_ALL".to_owned(), OsString::from("en_US.UTF-8"))]
        );

        let wine = tmp.path().join("bin/wine");
        fs::create_dir_all(wine.parent().expect("parent")).expect("mkdir");
        fs::write(&wine, b"").expect("write");
        let inst = instance_with(&tmp.path().join("w"), &[game::WINDOWS_EXE], Runner::Wine);
        let config = Config {
            wine: Some(wine.clone()),
            ..Config::default()
        };
        let p = plan(&inst, &SteamInfo::default(), &config).expect("wine");
        assert_eq!(p.program, wine);
        assert_eq!(
            p.args[0],
            inst.game_dir().join(game::WINDOWS_EXE).into_os_string()
        );
        assert!(
            p.env
                .contains(&("WINEPREFIX".into(), wine_prefix(&inst).into_os_string()))
        );
        assert!(wine_prefix(&inst).is_dir());
    }

    #[test]
    fn rewrites_the_native_build_too() {
        let steam: Vec<OsString> = [
            "/steam/ubuntu12_32/reaper",
            "SteamLaunch",
            "AppId=220200",
            "--",
            "/steam/common/SteamLinuxRuntime_sniper/_v2-entry-point",
            "--verb=waitforexitandrun",
            "--",
            "/steam/common/Kerbal Space Program/KSP.x86_64",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        let r = rewrite(&steam, Some(&note(1000)), 1010);
        assert_eq!(
            r.args[7],
            OsString::from("/orbis/instances/career/game/KSP.x86_64")
        );
        assert!(r.env.contains(&("LC_ALL".to_owned(), OsString::from("C"))));
    }

    #[test]
    fn rewrites_only_with_a_fresh_note() {
        let steam: Vec<OsString> = [
            "/steam/ubuntu12_32/reaper",
            "SteamLaunch",
            "AppId=220200",
            "--",
            "/steam/common/SteamLinuxRuntime_sniper/_v2-entry-point",
            "--verb=waitforexitandrun",
            "--",
            "/steam/compatibilitytools.d/GE/proton",
            "waitforexitandrun",
            "/steam/common/Kerbal Space Program/KSP_x64.exe",
        ]
        .iter()
        .map(OsString::from)
        .collect();

        let r = rewrite(&steam, Some(&note(1000)), 1010);
        assert_eq!(
            r.args[9],
            OsString::from("/orbis/instances/career/game/KSP_x64.exe")
        );
        assert_eq!(r.args.last(), Some(&OsString::from("-popupwindow")));
        assert_eq!(
            r.cwd.as_deref(),
            Some(std::path::Path::new("/orbis/instances/career/game"))
        );
        assert!(r.env.iter().any(|(k, v)| {
            k == "STEAM_COMPAT_MOUNTS"
                && v.to_string_lossy()
                    .contains("/steam/common/Kerbal Space Program")
        }));
        assert!(r.env.iter().any(|(k, _)| k == "DXVK_HUD"));

        // Stale or missing notes leave Steam's command alone.
        assert_eq!(
            rewrite(&steam, Some(&note(1000)), 1000 + NOTE_TTL_SECS + 1).args,
            steam
        );
        assert_eq!(rewrite(&steam, None, 1010).args, steam);
    }
}
