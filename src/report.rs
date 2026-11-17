//! A plain-text bug report: what orbis knows about the system, Steam and an
//! instance, with the home folder and user name taken out, ready to paste
//! into a GitHub issue.

use std::fs;

use crate::config::Config;
use crate::game;
use crate::instance::{Instance, ModSource, PrefixMode};
use crate::launch;
use crate::logs::{self, LogKind};
use crate::steam::SteamInfo;

/// Exception groups listed per log.
const EXCEPTIONS: usize = 15;
/// Lines from the end of the launch log.
const LAUNCH_TAIL: usize = 25;

/// Builds the report for `inst` (or for orbis alone).
pub fn build(config: &Config, steam: &SteamInfo, inst: Option<&Instance>) -> String {
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push('\n');
    };
    line(format!("orbis {}", env!("CARGO_PKG_VERSION")));
    line(format!("System: {}", os_name()));
    line(format!(
        "Kernel: {}",
        read_trim("/proc/sys/kernel/osrelease").unwrap_or_else(|| "unknown".into())
    ));
    line(format!(
        "Session: {}",
        std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unknown".into())
    ));
    line(String::new());
    line("Steam".into());
    for (k, v) in steam.report() {
        line(format!("  {k}: {v}"));
    }
    for dir in game::other_installs() {
        line(format!("  Other KSP: {}", dir.display()));
    }
    if config.proton.is_some() || config.wine.is_some() || config.steam_root.is_some() {
        line(format!(
            "  Overrides: steam_root={:?} proton={:?} wine={:?}",
            config.steam_root, config.proton, config.wine
        ));
    }
    if let Some(inst) = inst {
        line(String::new());
        instance(inst, steam, &mut line);
    }
    scrub(&out)
}

fn instance(inst: &Instance, steam: &SteamInfo, line: &mut impl FnMut(String)) {
    let c = &inst.config;
    line(format!("Instance: {}", c.name));
    line(format!("  KSP {}", c.game_version));
    let builds: Vec<&str> = game::builds(&inst.game_dir())
        .iter()
        .map(|b| match b {
            game::Build::Windows => "Windows",
            game::Build::Linux => "Linux",
        })
        .collect();
    line(format!("  Builds: {}", builds.join(", ")));
    line(match &c.base {
        Some(b) => format!("  Linked to {}", b.display()),
        None => "  Full copy".into(),
    });
    let runs = if c.launch.via_steam {
        "through Steam".to_owned()
    } else {
        match launch::resolve(inst, steam) {
            Ok(launch::Resolved::Proton) => format!(
                "with Proton in {} prefix",
                match c.launch.prefix {
                    PrefixMode::Steam => "Steam's",
                    PrefixMode::Instance => "its own",
                }
            ),
            Ok(r) => format!("with {}", r.label()),
            Err(e) => format!("can't start: {e:#}"),
        }
    };
    line(format!("  Runs {runs} (runner {})", c.launch.runner.name()));
    if !c.launch.args.is_empty() {
        line(format!("  Arguments: {}", c.launch.args.join(" ")));
    }
    if !c.launch.env.is_empty() {
        // Names only: values can hold anything.
        let names: Vec<&str> = c.launch.env.keys().map(String::as_str).collect();
        line(format!("  Environment: {}", names.join(", ")));
    }
    let dlc: Vec<String> = inst.dlc().into_iter().map(|d| d.identifier).collect();
    line(format!(
        "  DLC: {}",
        if dlc.is_empty() {
            "none".into()
        } else {
            dlc.join(", ")
        }
    ));
    match inst.registry() {
        Ok(reg) => {
            line(format!("  Mods ({}):", reg.mods.len()));
            for (id, m) in &reg.mods {
                let mut tags = Vec::new();
                if m.source == ModSource::Manual {
                    tags.push("manual");
                }
                if m.auto_installed {
                    tags.push("dependency");
                }
                let tags = if tags.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", tags.join(", "))
                };
                line(format!("    {id} {}{tags}", m.version()));
            }
        }
        Err(e) => line(format!("  Mods: unreadable: {e:#}")),
    }
    let asm = logs::assemblies(inst).unwrap_or_default();
    for kind in [LogKind::Ksp, LogKind::Player] {
        let Some(path) = logs::path(inst, steam, kind) else {
            continue;
        };
        line(String::new());
        match logs::read(&path, &asm) {
            Ok(r) => {
                line(format!(
                    "{}: {} exceptions ({} kinds), {} errors, {} warnings",
                    kind.label(),
                    r.exceptions.iter().map(|e| e.count).sum::<usize>(),
                    r.exceptions.len(),
                    r.errors,
                    r.warnings
                ));
                for e in r.exceptions.iter().take(EXCEPTIONS) {
                    let mods = if e.mods.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", e.mods.join(", "))
                    };
                    line(format!("  {}x {}{mods}", e.count, e.message));
                }
            }
            Err(_) => line(format!("{}: none", kind.label())),
        }
    }
    if let Some(path) = logs::path(inst, steam, LogKind::Launch)
        && let Ok(text) = fs::read(&path)
    {
        let text = String::from_utf8_lossy(&text);
        let lines: Vec<&str> = text.lines().collect();
        line(String::new());
        line(format!("Launch log (last {LAUNCH_TAIL} lines):"));
        for l in &lines[lines.len().saturating_sub(LAUNCH_TAIL)..] {
            line(format!("  {l}"));
        }
    }
}

fn os_name() -> String {
    let text = fs::read_to_string("/etc/os-release").unwrap_or_default();
    text.lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim_matches('"').to_owned())
        .unwrap_or_else(|| "unknown".into())
}

fn read_trim(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

/// Takes the home folder and user name out of the report.
fn scrub(text: &str) -> String {
    let mut out = text.to_owned();
    if let Ok(home) = crate::paths::home() {
        let home = home.display().to_string();
        if home.len() > 1 {
            out = out.replace(&home, "~");
        }
    }
    if let Ok(user) = std::env::var("USER")
        && !user.is_empty()
    {
        out = scrub_user(&out, &user);
    }
    out
}

/// Replaces `user` where it is a whole path component.
fn scrub_user(text: &str, user: &str) -> String {
    let needle = format!("/{user}");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(&needle) {
        let after = &rest[i + needle.len()..];
        let whole = after
            .chars()
            .next()
            .is_none_or(|c| c == '/' || c.is_whitespace() || c == '"' || c == '\'');
        out.push_str(&rest[..i]);
        out.push_str(if whole { "/<user>" } else { &needle });
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_the_user_out_of_paths() {
        let cases = [
            ("/run/media/chad/Games/KSP", "/run/media/<user>/Games/KSP"),
            ("/mnt/chad", "/mnt/<user>"),
            ("/home/chadwick/x", "/home/chadwick/x"),
            ("chad wrote this", "chad wrote this"),
        ];
        for (input, want) in cases {
            assert_eq!(scrub_user(input, "chad"), want, "{input}");
        }
    }

    #[test]
    fn reports_an_instance() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let game = tmp.path().join("game");
        fs::create_dir_all(game.join("GameData")).expect("mkdir");
        fs::write(game.join(game::LINUX_EXE), b"").expect("write");
        fs::write(
            game.join("KSP.log"),
            "[EXC 12:00:00.000] NullReferenceException: boom\n",
        )
        .expect("write");
        let mut inst = Instance {
            id: "t".into(),
            dir: tmp.path().to_path_buf(),
            config: toml::from_str("name = \"Career\"\ngame_version = \"1.12.5\"\ncreated = 0\n")
                .expect("config"),
        };
        inst.config
            .launch
            .env
            .insert("SECRET".into(), "hunter2".into());
        let text = build(&Config::default(), &SteamInfo::default(), Some(&inst));
        assert!(text.contains("Instance: Career"), "{text}");
        assert!(text.contains("Builds: Linux"), "{text}");
        assert!(text.contains("Runs with the native Linux build"), "{text}");
        assert!(
            text.contains("Environment: SECRET") && !text.contains("hunter2"),
            "{text}"
        );
        assert!(text.contains("NullReferenceException"), "{text}");
    }
}
