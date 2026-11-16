//! Command-line interface: every core operation, scriptable.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand};

use orbis::cancel::Cancel;
use orbis::ckan_import;
use orbis::config::Config;
use orbis::crafts;
use orbis::download::DownloadEvent;
use orbis::flags;
use orbis::instance::{self, CopyMode, CreateOptions, Instance};
use orbis::launch;
use orbis::manual::{self, AddOptions};
use orbis::metadata::InstallDirective;
use orbis::modpack;
use orbis::ops::{self, Event};
use orbis::paths::Paths;
use orbis::repo::{self, Index};
use orbis::resolver::{Plan, Request, ResolveError};
use orbis::running;
use orbis::starred::Starred;
use orbis::steam::{self, SteamInfo};
use orbis::version::{GameVersion, ModVersion};
use orbis::{backup, history, logs, store};

use crate::gui;

/// Shared state for commands and the GUI.
pub struct Context {
    pub paths: Paths,
    pub config: Config,
    /// `--root`, when given (Steam's launch options must pass it on).
    pub root: Option<PathBuf>,
}

impl Context {
    pub fn new(root: Option<&Path>) -> Result<Self> {
        let paths = Paths::new(root)?;
        let config = Config::load(&paths)?;
        Ok(Self {
            paths,
            config,
            // Absolute, since Steam runs the launch options from elsewhere.
            root: root.map(std::path::absolute).transpose()?,
        })
    }

    pub fn steam(&self) -> SteamInfo {
        steam::detect(
            self.config.steam_root.as_deref(),
            self.config.proton.as_deref(),
        )
    }

    fn index(&self) -> Result<Index> {
        Index::load(&self.paths)?.context("no mod index yet: run `orbis update` first")
    }

    /// The named instance, or the only one when there's exactly one.
    fn instance(&self, key: Option<&str>) -> Result<Instance> {
        if let Some(key) = key {
            return instance::find(&self.paths, key);
        }
        let all = instance::list(&self.paths)?;
        match all.len() {
            1 => Ok(all.into_iter().next().expect("length checked")),
            0 => bail!("no instances yet: create one with `orbis instance create <name>`"),
            _ => bail!("several instances exist: pick one with -i <name>"),
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Open the graphical interface (the default with no command).
    Gui(gui::Options),
    /// Download the latest CKAN mod index.
    Update,
    /// Search mods by name, identifier, abstract, author or tag.
    Search {
        query: String,
        /// Only show mods compatible with this instance.
        #[arg(short, long)]
        instance: Option<String>,
    },
    /// Show details of a mod.
    Show { identifier: String },
    /// Manage instances.
    #[command(subcommand)]
    Instance(InstanceCmd),
    /// List the mods installed in an instance.
    List {
        #[arg(short, long)]
        instance: Option<String>,
    },
    /// Install mods (identifier, or identifier=version) with dependencies.
    Install {
        #[arg(short, long)]
        instance: Option<String>,
        #[arg(required = true)]
        mods: Vec<String>,
        #[command(flatten)]
        plan: PlanFlags,
        /// Also install every recommended mod.
        #[arg(long)]
        recommends: bool,
    },
    /// Remove mods, plus anything that depends on them.
    Remove {
        #[arg(short, long)]
        instance: Option<String>,
        #[arg(required = true)]
        mods: Vec<String>,
        #[command(flatten)]
        plan: PlanFlags,
    },
    /// Upgrade mods (all upgradable mods when none are named).
    Upgrade {
        #[arg(short, long)]
        instance: Option<String>,
        mods: Vec<String>,
        #[command(flatten)]
        plan: PlanFlags,
    },
    /// Manage the manual mod library.
    #[command(subcommand)]
    Manual(ManualCmd),
    /// Manage the craft library (shared by all instances).
    #[command(subcommand)]
    Craft(CraftCmd),
    /// Manage the custom flag library (shared by all instances).
    #[command(subcommand)]
    Flag(FlagCmd),
    /// Start an instance through Proton.
    Launch {
        instance: Option<String>,
        /// Print the command instead of running it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show what orbis detected of Steam, KSP and Proton, and the launch
    /// options KSP needs for launching instances through Steam.
    Steam,
    /// Steam's launch wrapper: set KSP's launch options to
    /// `orbis steam-run %command%`. Not meant to be run by hand.
    #[command(hide = true)]
    SteamRun {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, value_parser = clap::value_parser!(std::ffi::OsString))]
        command: Vec<std::ffi::OsString>,
    },
    /// Star mods so they're easy to find again (shared by all instances).
    Star {
        #[arg(required = true)]
        mods: Vec<String>,
    },
    /// Remove stars.
    Unstar {
        #[arg(required = true)]
        mods: Vec<String>,
    },
    /// List starred mods.
    Starred,
    /// Export and import modpacks.
    #[command(subcommand)]
    Pack(PackCmd),
    /// Save backups.
    #[command(subcommand)]
    Backup(BackupCmd),
    /// Change history and rollback.
    #[command(subcommand)]
    History(HistoryCmd),
    /// Show the mod store and download cache, or clean out what no instance uses.
    Cache {
        /// Remove stored mods and downloads that no instance uses.
        #[arg(long)]
        clean: bool,
        /// Make mods installed before the store existed share disk space
        /// with it (btrfs and XFS).
        #[arg(long)]
        share: bool,
    },
    /// Summarise a log: exceptions grouped, with the mods they came from.
    Logs {
        #[arg(short, long)]
        instance: Option<String>,
        /// ksp, player or launch.
        #[arg(long, default_value = "ksp")]
        log: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum PackCmd {
    /// Write an instance's mods (and optionally craft, saves and manual mod
    /// files) to an .orbispack file.
    Export {
        #[arg(short, long)]
        instance: Option<String>,
        /// Output file.
        out: PathBuf,
        /// Pack name (the instance name by default).
        #[arg(long, default_value = "")]
        name: String,
        #[arg(long, default_value = "")]
        description: String,
        /// Include the Ships folder (craft).
        #[arg(long)]
        craft: bool,
        /// Include these saves (comma-separated folder names, or "all").
        #[arg(long, value_delimiter = ',')]
        saves: Vec<String>,
        /// Include manual mod archives (only for mods you may share).
        #[arg(long)]
        manual_files: bool,
        /// Write a CKAN modpack (.ckan) instead; mods only.
        #[arg(long)]
        ckan: bool,
    },
    /// Create a new instance from an .orbispack or CKAN .ckan modpack.
    Import {
        file: PathBuf,
        /// Instance name (the pack's name by default).
        #[arg(long, default_value = "")]
        name: String,
        /// Link to this KSP directory instead of Steam's.
        #[arg(long, value_name = "DIR")]
        from: Option<PathBuf>,
        /// Install the newest compatible releases instead of the pack's versions.
        #[arg(long)]
        latest: bool,
    },
}

#[derive(Args, Debug)]
pub struct PlanFlags {
    /// Apply without asking.
    #[arg(short, long)]
    yes: bool,
    /// Show the plan and stop.
    #[arg(long)]
    dry_run: bool,
    /// Answer a provider choice: "<relationship>=<identifier>" or
    /// "<virtual name>=<identifier>". Repeatable.
    #[arg(long, value_name = "NAME=ID")]
    choose: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum InstanceCmd {
    /// List instances.
    List,
    /// Create an instance linked to the Steam install (or --from DIR). The
    /// instance holds its own mods, saves and settings; the stock game is
    /// linked and never changed.
    Create {
        name: String,
        /// Use this KSP directory instead of Steam's.
        #[arg(long, value_name = "DIR")]
        from: Option<PathBuf>,
        /// Copy the source's saves too.
        #[arg(long)]
        with_saves: bool,
        /// Make a full, independent copy that keeps its game version.
        #[arg(long)]
        copy: bool,
        /// With --copy: copy non-stock GameData too (mods orbis won't know about).
        #[arg(long, requires = "copy")]
        keep_mods: bool,
    },
    /// Choose how an instance starts: "direct" (Proton) or "steam" (through
    /// Steam, for the overlay and playtime).
    Launcher {
        instance: Option<String>,
        #[arg(long, value_parser = ["direct", "steam"])]
        via: String,
    },
    /// Re-link a linked instance to its base install (done before every launch).
    Sync { instance: Option<String> },
    /// Create an instance from a KSP install CKAN manages, tracking the mods
    /// CKAN installed.
    ImportCkan {
        /// The KSP directory (the one holding CKAN/registry.json).
        dir: PathBuf,
        #[arg(long, default_value = "")]
        name: String,
        /// Make a full, independent copy instead of linking the stock files.
        #[arg(long)]
        copy: bool,
    },
    /// Copy an instance with its mods.
    Clone {
        source: String,
        name: String,
        /// Leave the saves behind.
        #[arg(long)]
        no_saves: bool,
    },
    /// Rename an instance.
    Rename { instance: String, name: String },
    /// Delete an instance and all its files.
    Delete {
        instance: String,
        #[arg(short, long)]
        yes: bool,
    },
    /// Show an instance's settings.
    Info { instance: Option<String> },
    /// Change which game versions count as compatible.
    Compat {
        instance: Option<String>,
        #[arg(long, value_name = "VERSION")]
        add: Vec<String>,
        #[arg(long, value_name = "VERSION")]
        remove: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum CraftCmd {
    /// List the library, and whether each craft fits an instance.
    List {
        #[arg(short, long)]
        instance: Option<String>,
        /// List the trash instead.
        #[arg(long)]
        trash: bool,
    },
    /// Collect new and changed craft from every instance and update the others.
    Sync,
    /// Add a craft file to the library.
    Add { file: PathBuf },
    /// Add every craft under a folder (for example an old install's saves).
    Import { dir: PathBuf },
    /// Delete a craft from every instance (it goes to the trash).
    Delete { craft: String },
    /// Bring a craft back from the trash.
    Restore { craft: String },
    /// Delete everything in the trash for good.
    EmptyTrash,
    /// Show a craft's revisions.
    History {
        craft: String,
        #[arg(short, long)]
        instance: Option<String>,
    },
    /// Save a revision (numbered as `history` shows) as a new craft.
    Fork {
        craft: String,
        revision: usize,
        #[arg(long)]
        name: String,
    },
    /// List craft that went missing and wait for a decision, or settle one
    /// (numbered as listed).
    Pending {
        /// Delete this one everywhere.
        #[arg(long, value_name = "N", conflicts_with = "put_back")]
        delete: Option<usize>,
        /// Copy this one back into its save.
        #[arg(long, value_name = "N")]
        put_back: Option<usize>,
    },
}

#[derive(Subcommand, Debug)]
pub enum FlagCmd {
    /// List the flags in the library.
    List,
    /// Add a PNG to the library.
    Add {
        path: PathBuf,
        /// Name to give the flag (default: the file name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Delete a flag from the library.
    Remove { name: String },
}

#[derive(Subcommand, Debug)]
pub enum ManualCmd {
    /// List the manual mod library.
    List,
    /// Add a .zip, .7z or .rar archive, or a folder, to the library.
    Add {
        path: PathBuf,
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "1.0")]
        version: String,
        /// CKAN identifiers this mod replaces (comma-separated).
        #[arg(long, value_delimiter = ',')]
        provides: Vec<String>,
        /// CKAN identifiers this mod needs (comma-separated).
        #[arg(long, value_delimiter = ',')]
        depends: Vec<String>,
        /// CKAN identifiers this mod can't be installed with (comma-separated).
        #[arg(long, value_delimiter = ',')]
        conflicts: Vec<String>,
        /// Install only this folder (CKAN `find`) instead of guessing.
        #[arg(long, value_name = "FOLDER")]
        find: Option<String>,
        /// Where `--find` installs to.
        #[arg(long, default_value = "GameData")]
        install_to: String,
        /// Where the mod came from, for your reference.
        #[arg(long, default_value = "")]
        url: String,
        #[arg(long, default_value = "")]
        notes: String,
    },
    /// Show a manual mod and where its files go.
    Show { id: String },
    /// Install a manual mod into an instance.
    Install {
        id: String,
        #[arg(short, long)]
        instance: Option<String>,
        #[command(flatten)]
        plan: PlanFlags,
    },
    /// Delete a manual mod from the library (instances keep their copy).
    Delete { id: String },
    /// List GameData folders and files no installed mod owns (added by hand).
    Unmanaged {
        #[arg(short, long)]
        instance: Option<String>,
    },
    /// Track a hand-added GameData folder as a manual mod.
    Adopt {
        folder: String,
        #[arg(short, long)]
        instance: Option<String>,
        #[arg(long, default_value = "")]
        name: String,
        #[arg(long, default_value = "1.0")]
        version: String,
        /// CKAN identifiers this mod stands in for (comma-separated).
        #[arg(long, value_delimiter = ',')]
        provides: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum BackupCmd {
    /// List save backups, newest first.
    List {
        #[arg(short, long)]
        instance: Option<String>,
    },
    /// Back up the saves now.
    Create {
        #[arg(short, long)]
        instance: Option<String>,
    },
    /// Restore saves from a backup (the current saves are backed up first).
    Restore {
        id: String,
        #[arg(short, long)]
        instance: Option<String>,
        /// Restore only this save folder.
        #[arg(long)]
        save: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum HistoryCmd {
    /// List applied changes, newest first.
    List {
        #[arg(short, long)]
        instance: Option<String>,
    },
    /// Put the mod list back to how it was after a change (or before it,
    /// with --before). `undo` is `restore <newest id> --before`.
    Restore {
        id: u64,
        #[arg(short, long)]
        instance: Option<String>,
        #[arg(long)]
        before: bool,
    },
    /// Undo the newest change.
    Undo {
        #[arg(short, long)]
        instance: Option<String>,
    },
}

pub fn run(ctx: &Context, cmd: Command) -> Result<()> {
    match cmd {
        Command::Gui(_) => unreachable!("main handles the GUI"),
        Command::Update => {
            let s = repo::update(
                &ctx.paths,
                &ctx.config,
                &mut |msg| eprint!("\r\x1b[K{msg}"),
                &Cancel::default(),
            )?;
            eprintln!();
            if s.unchanged {
                writeln!(io::stdout(), "Index unchanged: {} mods", s.modules)?;
            } else {
                writeln!(
                    io::stdout(),
                    "Index updated: {} mods, {} releases ({} unreadable skipped)",
                    s.modules,
                    s.releases,
                    s.skipped
                )?;
            }
            Ok(())
        }
        Command::Search { query, instance } => search(ctx, &query, instance.as_deref()),
        Command::Show { identifier } => show(ctx, &identifier),
        Command::Instance(c) => instance_cmd(ctx, c),
        Command::List { instance } => list(ctx, instance.as_deref()),
        Command::Install {
            instance,
            mods,
            plan,
            recommends,
        } => {
            let inst = ctx.instance(instance.as_deref())?;
            let mut req = Request {
                install: mods
                    .iter()
                    .map(|m| match m.split_once('=') {
                        Some((id, v)) => (id.to_owned(), Some(ModVersion::new(v))),
                        None => (m.clone(), None),
                    })
                    .collect(),
                ..Request::default()
            };
            if recommends {
                let index = ctx.index()?;
                if let Ok(p) = ops::plan(&inst, &index, &with_choices(req.clone(), &plan.choose)?)?
                {
                    for s in p.suggestions.iter().filter(|s| s.recommended) {
                        req.install.push((s.candidates[0].clone(), None));
                    }
                }
            }
            plan_and_apply(ctx, &inst, req, &plan)
        }
        Command::Remove {
            instance,
            mods,
            plan,
        } => {
            let inst = ctx.instance(instance.as_deref())?;
            let reg = inst.registry()?;
            let mods = mods
                .into_iter()
                .map(|m| {
                    if reg.mods.contains_key(&m) {
                        m
                    } else if reg.mods.contains_key(&format!("{}{m}", manual::PREFIX)) {
                        format!("{}{m}", manual::PREFIX)
                    } else {
                        m
                    }
                })
                .collect();
            let req = Request {
                remove: mods,
                ..Request::default()
            };
            plan_and_apply(ctx, &inst, req, &plan)
        }
        Command::Upgrade {
            instance,
            mods,
            plan,
        } => {
            let inst = ctx.instance(instance.as_deref())?;
            let mods = if mods.is_empty() {
                let index = ctx.index()?;
                ops::upgradable(&inst, &index, &inst.registry()?)
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect()
            } else {
                mods
            };
            if mods.is_empty() {
                writeln!(io::stdout(), "Everything is up to date.")?;
                return Ok(());
            }
            let req = Request {
                upgrade: mods,
                ..Request::default()
            };
            plan_and_apply(ctx, &inst, req, &plan)
        }
        Command::Manual(c) => manual_cmd(ctx, c),
        Command::Flag(c) => flag_cmd(ctx, c),
        Command::Craft(c) => craft_cmd(ctx, c),
        Command::Launch { instance, dry_run } => {
            let mut inst = ctx.instance(instance.as_deref())?;
            if running::is_running(&inst.dir) {
                bail!("{} is already running", inst.config.name);
            }
            instance::sync(&ctx.paths, &mut inst)?;
            if !dry_run {
                sync_crafts(ctx);
            }
            if inst.config.launch.via_steam {
                let steam = ctx.steam();
                if dry_run {
                    writeln!(
                        io::stdout(),
                        "steam steam://rungameid/{}",
                        orbis::steam::KSP_APP_ID
                    )?;
                    writeln!(
                        io::stdout(),
                        "(Steam then runs: {})",
                        launch::steam_launch_options(ctx.root.as_deref())
                    )?;
                    return Ok(());
                }
                match backup::snapshot(&inst, "before launch", ctx.config.backups_keep) {
                    Ok(Some(b)) => eprintln!("Backed up {} save(s)", b.saves.len()),
                    Ok(None) => {}
                    Err(e) => eprintln!("warning: save backup failed: {e:#}"),
                }
                launch::launch_via_steam(&ctx.paths, &inst, &steam)?;
                inst.config.last_played = Some(orbis::paths::now_secs());
                inst.save()?;
                writeln!(io::stdout(), "Asked Steam to start {}", inst.config.name)?;
                return Ok(());
            }
            let plan = launch::plan(&inst, &ctx.steam(), &ctx.config)?;
            if !dry_run {
                match backup::snapshot(&inst, "before launch", ctx.config.backups_keep) {
                    Ok(Some(b)) => eprintln!("Backed up {} save(s)", b.saves.len()),
                    Ok(None) => {}
                    Err(e) => eprintln!("warning: save backup failed: {e:#}"),
                }
            }
            if dry_run {
                writeln!(io::stdout(), "{}", plan.display())?;
                return Ok(());
            }
            let child = launch::spawn(&plan)?;
            if let Err(e) = running::mark(&inst.dir, child.id(), "direct") {
                eprintln!("warning: {e:#}");
            }
            inst.config.last_played = Some(orbis::paths::now_secs());
            inst.save()?;
            writeln!(
                io::stdout(),
                "Started {} (pid {}); log: {}",
                inst.config.name,
                child.id(),
                plan.log.display()
            )?;
            Ok(())
        }
        Command::SteamRun { command } => {
            let code = launch::steam_run(&ctx.paths, &command)?;
            std::process::exit(code);
        }
        Command::Steam => {
            let steam = ctx.steam();
            let current = launch::current_steam_launch_options(&steam);
            let ready = current.as_deref().is_some_and(|o| o.contains("steam-run"));
            writeln!(
                io::stdout(),
                "{:14} {}",
                "Launch options",
                current.as_deref().unwrap_or("(none)")
            )?;
            if !ready {
                writeln!(
                    io::stdout(),
                    "{:14} to launch instances through Steam, set KSP's launch options to:\n{:14} {}",
                    "",
                    "",
                    launch::steam_launch_options(ctx.root.as_deref())
                )?;
            }
            for (k, v) in steam.report() {
                writeln!(io::stdout(), "{k:14} {v}")?;
            }
            Ok(())
        }
        Command::Star { mods } => {
            let mut s = Starred::load(&ctx.paths)?;
            s.mods.extend(mods);
            s.save(&ctx.paths)
        }
        Command::Unstar { mods } => {
            let mut s = Starred::load(&ctx.paths)?;
            for m in &mods {
                s.mods.remove(m);
            }
            s.save(&ctx.paths)
        }
        Command::Starred => {
            let index = Index::load(&ctx.paths)?.unwrap_or_default();
            for id in Starred::load(&ctx.paths)?.mods {
                let summary = index
                    .versions(&id)
                    .first()
                    .map_or("", |m| m.summary.as_str());
                writeln!(io::stdout(), "{id:36} {summary}")?;
            }
            Ok(())
        }
        Command::Pack(c) => pack_cmd(ctx, c),
        Command::Backup(c) => backup_cmd(ctx, c),
        Command::Cache { clean, share } => {
            let mib = |b: u64| b as f64 / 1_048_576.0;
            if share {
                let r = store::share(&ctx.paths, &Cancel::default(), &mut |m| eprintln!("{m}"))?;
                for s in &r.skipped {
                    eprintln!("skipped {s}");
                }
                writeln!(
                    io::stdout(),
                    "{} files ({:.1} MiB) now share disk space with the store",
                    r.files,
                    mib(r.bytes)
                )?;
                return Ok(());
            }
            if clean {
                let (n, bytes) = store::clean(&ctx.paths)?;
                writeln!(
                    io::stdout(),
                    "Removed {n} unused entries ({:.1} MiB)",
                    mib(bytes)
                )?;
                return Ok(());
            }
            let u = store::usage(&ctx.paths)?;
            writeln!(
                io::stdout(),
                "store:     {} mods, {:.1} MiB unpacked (shared with instances by reflink where possible)",
                u.store_entries,
                mib(u.store_bytes)
            )?;
            writeln!(
                io::stdout(),
                "downloads: {} archives, {:.1} MiB",
                u.download_files,
                mib(u.download_bytes)
            )?;
            writeln!(
                io::stdout(),
                "unused:    {} entries, {:.1} MiB (orbis cache --clean)",
                u.unused_entries,
                mib(u.unused_bytes)
            )?;
            Ok(())
        }
        Command::History(c) => history_cmd(ctx, c),
        Command::Logs { instance, log } => {
            let inst = ctx.instance(instance.as_deref())?;
            let kind = match log.as_str() {
                "ksp" => logs::LogKind::Ksp,
                "player" => logs::LogKind::Player,
                "launch" => logs::LogKind::Launch,
                other => bail!("unknown log {other:?}: use ksp, player or launch"),
            };
            let path = logs::path(&inst, &ctx.steam(), kind).context("can't locate that log")?;
            let report = logs::read(&path, &logs::assemblies(&inst)?)?;
            writeln!(
                io::stdout(),
                "{}: {} lines, {} exceptions ({} distinct), {} errors, {} warnings",
                path.display(),
                report.lines.len(),
                report.exceptions.iter().map(|e| e.count).sum::<usize>(),
                report.exceptions.len(),
                report.errors,
                report.warnings
            )?;
            for e in report.exceptions.iter().take(20) {
                let mods = if e.mods.is_empty() {
                    String::new()
                } else {
                    format!("  [{}]", e.mods.join(", "))
                };
                writeln!(io::stdout(), "{:5}x {}{mods}", e.count, e.message)?;
            }
            Ok(())
        }
    }
}

fn backup_cmd(ctx: &Context, cmd: BackupCmd) -> Result<()> {
    match cmd {
        BackupCmd::List { instance } => {
            let inst = ctx.instance(instance.as_deref())?;
            for b in backup::list(&inst)? {
                writeln!(
                    io::stdout(),
                    "{}  {:16} {:8.1} MiB  {}",
                    b.id,
                    b.reason,
                    b.bytes as f64 / 1_048_576.0,
                    b.saves.join(", ")
                )?;
            }
            Ok(())
        }
        BackupCmd::Create { instance } => {
            let inst = ctx.instance(instance.as_deref())?;
            match backup::create(&inst, "manual")? {
                Some(b) => writeln!(io::stdout(), "Backed up {} as {}", b.saves.join(", "), b.id)?,
                None => writeln!(
                    io::stdout(),
                    "Nothing to back up (no saves, or unchanged since the last backup)"
                )?,
            }
            Ok(())
        }
        BackupCmd::Restore { id, instance, save } => {
            let inst = ctx.instance(instance.as_deref())?;
            backup::restore(&inst, &id, save.as_deref())?;
            writeln!(
                io::stdout(),
                "Restored {} from {id}",
                save.as_deref().unwrap_or("all saves")
            )?;
            Ok(())
        }
    }
}

fn history_cmd(ctx: &Context, cmd: HistoryCmd) -> Result<()> {
    let restore = |inst: &Instance, target: &[history::SnapMod]| -> Result<()> {
        let index = ctx.index()?;
        let warnings = with_progress(|tx| {
            ops::restore(
                &ctx.paths,
                &ctx.config,
                inst,
                &index,
                target,
                tx,
                &Cancel::default(),
            )
        })?;
        for w in warnings {
            eprintln!("warning: {w}");
        }
        Ok(())
    };
    match cmd {
        HistoryCmd::List { instance } => {
            let inst = ctx.instance(instance.as_deref())?;
            for e in history::list(&inst)? {
                writeln!(io::stdout(), "{}  ({} mods after)", e.id, e.after.len())?;
                for line in &e.summary {
                    writeln!(io::stdout(), "    {line}")?;
                }
            }
            Ok(())
        }
        HistoryCmd::Restore {
            id,
            instance,
            before,
        } => {
            let inst = ctx.instance(instance.as_deref())?;
            let e = history::list(&inst)?
                .into_iter()
                .find(|e| e.id == id)
                .with_context(|| format!("no history entry {id}"))?;
            restore(&inst, if before { &e.before } else { &e.after })
        }
        HistoryCmd::Undo { instance } => {
            let inst = ctx.instance(instance.as_deref())?;
            let e = history::list(&inst)?
                .into_iter()
                .next()
                .context("nothing to undo")?;
            restore(&inst, &e.before)
        }
    }
}

fn pack_cmd(ctx: &Context, cmd: PackCmd) -> Result<()> {
    match cmd {
        PackCmd::Export {
            instance,
            out,
            name,
            description,
            craft,
            saves,
            manual_files,
            ckan,
        } => {
            let inst = ctx.instance(instance.as_deref())?;
            if ckan {
                modpack::export_ckan(&inst, &out, &name)?;
                writeln!(io::stdout(), "Wrote CKAN modpack {}", out.display())?;
                return Ok(());
            }
            let saves = if saves.iter().any(|s| s == "all") {
                modpack::save_names(&inst)
            } else {
                saves
            };
            let opts = modpack::ExportOptions {
                out: out.clone(),
                name,
                description,
                craft,
                saves,
                manual_files,
            };
            for w in modpack::export(&ctx.paths, &inst, &opts)? {
                eprintln!("warning: {w}");
            }
            writeln!(io::stdout(), "Wrote {}", out.display())?;
            Ok(())
        }
        PackCmd::Import {
            file,
            name,
            from,
            latest,
        } => {
            let index = ctx.index()?;
            let base = match from {
                Some(p) => p,
                None => ctx
                    .steam()
                    .ksp
                    .context("KSP not found in Steam; pass --from <dir>")?,
            };
            let opts = modpack::ImportOptions {
                path: file,
                name,
                base,
                exact: !latest,
            };
            let report = with_progress(|tx| {
                modpack::import(
                    &ctx.paths,
                    &ctx.config,
                    &index,
                    &opts,
                    tx,
                    &Cancel::default(),
                )
            })?;
            for w in &report.warnings {
                eprintln!("warning: {w}");
            }
            writeln!(
                io::stdout(),
                "Imported into {} at {}",
                report.instance.config.name,
                report.instance.dir.display()
            )?;
            Ok(())
        }
    }
}

fn with_choices(mut req: Request, choose: &[String]) -> Result<Request> {
    for c in choose {
        let (k, v) = c
            .rsplit_once('=')
            .with_context(|| format!("--choose {c:?} must look like NAME=IDENTIFIER"))?;
        req.choices.insert(k.trim().to_owned(), v.trim().to_owned());
    }
    Ok(req)
}

fn plan_and_apply(ctx: &Context, inst: &Instance, req: Request, flags: &PlanFlags) -> Result<()> {
    // Removing needs no index; installing and upgrading do.
    let index = if req.install.is_empty() && req.upgrade.is_empty() {
        Index::load(&ctx.paths)?.unwrap_or_default()
    } else {
        ctx.index()?
    };
    let req = with_choices(req, &flags.choose)?;
    let plan = match ops::plan(inst, &index, &req)? {
        Ok(p) => p,
        Err(e) => return Err(explain(e)),
    };
    confirm_and_apply(ctx, inst, &plan, flags)
}

fn explain(e: ResolveError) -> anyhow::Error {
    if let ResolveError::Choice {
        relationship,
        candidates,
        ..
    } = &e
    {
        return anyhow::anyhow!(
            "{e}\nPick one with: --choose '{relationship}={}'",
            candidates.first().map_or("", String::as_str)
        );
    }
    e.into()
}

fn confirm_and_apply(ctx: &Context, inst: &Instance, plan: &Plan, flags: &PlanFlags) -> Result<()> {
    if plan.is_empty() {
        writeln!(io::stdout(), "Nothing to do.")?;
        return Ok(());
    }
    writeln!(io::stdout(), "Changes to {}:", inst.config.name)?;
    for c in &plan.changes {
        writeln!(io::stdout(), "  {c}")?;
    }
    for s in &plan.suggestions {
        let kind = if s.recommended {
            "recommends"
        } else {
            "suggests"
        };
        writeln!(
            io::stdout(),
            "  ({} {kind} {}: {})",
            s.by,
            s.relationship,
            s.candidates.join(", ")
        )?;
    }
    if flags.dry_run {
        return Ok(());
    }
    if !flags.yes {
        print!("Proceed? [y/N] ");
        io::stdout().flush().ok();
        let mut line = String::new();
        io::stdin()
            .lock()
            .read_line(&mut line)
            .context("read answer")?;
        if !matches!(line.trim(), "y" | "Y" | "yes") {
            writeln!(io::stdout(), "Cancelled.")?;
            return Ok(());
        }
    }
    with_progress(|tx| ops::apply(&ctx.paths, &ctx.config, inst, plan, tx, &Cancel::default()))
}

/// Runs `work` while printing its progress events to stderr.
fn with_progress<T>(work: impl FnOnce(&mpsc::Sender<Event>) -> Result<T>) -> Result<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|s| {
        let printer = s.spawn(move || {
            for e in rx {
                match e {
                    Event::Status(msg) => eprintln!("\r\x1b[K{msg}"),
                    Event::Download(DownloadEvent::Progress {
                        identifier,
                        bytes,
                        total,
                    }) => {
                        let total = total
                            .map_or(String::new(), |t| format!(" / {:.1}", t as f64 / 1048576.0));
                        eprint!(
                            "\r\x1b[K  {identifier}: {:.1}{total} MiB",
                            bytes as f64 / 1048576.0
                        );
                    }
                    Event::Download(DownloadEvent::Done { identifier }) => {
                        eprintln!("\r\x1b[K  {identifier}: downloaded")
                    }
                }
            }
        });
        let r = work(&tx);
        drop(tx);
        let _ = printer.join();
        r
    })
}

fn search(ctx: &Context, query: &str, instance: Option<&str>) -> Result<()> {
    let index = ctx.index()?;
    let inst = instance
        .map(|i| instance::find(&ctx.paths, i))
        .transpose()?;
    let compat = inst.as_ref().map(Instance::compat_versions);
    let q = query.to_lowercase();
    let mut hits: Vec<(&String, &orbis::metadata::Module)> = index
        .iter()
        .filter_map(|(id, versions)| {
            let m = match (&compat, &inst) {
                (Some(c), Some(i)) => index.latest_compatible(id, c, i.config.allow_testing)?,
                _ => versions.first()?,
            };
            matches(m, &q).then_some((id, m))
        })
        .collect();
    hits.sort_by_key(|(id, _)| std::cmp::Reverse(index.download_count(id)));
    for (id, m) in hits.iter().take(50) {
        writeln!(
            io::stdout(),
            "{id:32} {:12} {:10} {}",
            m.version.to_string(),
            m.game_range_label(),
            m.summary
        )?;
    }
    if hits.len() > 50 {
        writeln!(io::stdout(), "... {} more", hits.len() - 50)?;
    }
    Ok(())
}

/// Whether a module matches a lowercased search query.
pub fn matches(m: &orbis::metadata::Module, q: &str) -> bool {
    q.is_empty()
        || m.identifier.to_lowercase().contains(q)
        || m.name.to_lowercase().contains(q)
        || m.summary.to_lowercase().contains(q)
        || m.author.iter().any(|a| a.to_lowercase().contains(q))
        || m.tags.iter().any(|t| t.to_lowercase() == q)
}

fn show(ctx: &Context, id: &str) -> Result<()> {
    let index = ctx.index()?;
    let versions = index.versions(id);
    let m = versions.first().with_context(|| format!("no mod {id:?}"))?;
    writeln!(io::stdout(), "{} ({})", m.display_name(), m.identifier)?;
    writeln!(io::stdout(), "  {}", m.summary)?;
    if let Some(d) = &m.description {
        writeln!(io::stdout(), "\n{d}\n")?;
    }
    writeln!(io::stdout(), "  authors:   {}", m.author.join(", "))?;
    writeln!(io::stdout(), "  license:   {}", m.license.join(", "))?;
    writeln!(io::stdout(), "  downloads: {}", index.download_count(id))?;
    for key in ["homepage", "repository", "spacedock", "bugtracker"] {
        if let Some(r) = m.resource(key) {
            writeln!(io::stdout(), "  {key:10} {r}")?;
        }
    }
    for (label, rels) in [
        ("depends", &m.depends),
        ("recommends", &m.recommends),
        ("suggests", &m.suggests),
        ("conflicts", &m.conflicts),
    ] {
        if !rels.is_empty() {
            let list: Vec<String> = rels.iter().map(ToString::to_string).collect();
            writeln!(io::stdout(), "  {label:10} {}", list.join(", "))?;
        }
    }
    writeln!(io::stdout(), "  releases:")?;
    for v in versions.iter().take(15) {
        writeln!(
            io::stdout(),
            "    {:16} KSP {}",
            v.version.to_string(),
            v.game_range_label()
        )?;
    }
    Ok(())
}

fn list(ctx: &Context, instance: Option<&str>) -> Result<()> {
    let inst = ctx.instance(instance)?;
    let reg = inst.registry()?;
    let index = Index::load(&ctx.paths)?;
    let upgrades: Vec<(String, orbis::metadata::Module)> = index
        .as_ref()
        .map(|i| ops::upgradable(&inst, i, &reg))
        .unwrap_or_default();
    for (id, m) in &reg.mods {
        let auto = if m.auto_installed {
            let by = reg.required_by(id);
            if by.is_empty() {
                " (dependency, no longer needed)".to_owned()
            } else {
                format!(" (dependency of {})", by.join(", "))
            }
        } else {
            String::new()
        };
        let up = upgrades
            .iter()
            .find(|(u, _)| u == id)
            .map_or(String::new(), |(_, n)| format!("  -> {}", n.version));
        writeln!(io::stdout(), "{id:36} {}{auto}{up}", m.version())?;
    }
    for dlc in inst.dlc() {
        writeln!(
            io::stdout(),
            "{:36} {} (DLC)",
            dlc.identifier,
            dlc.version.map_or("?".into(), |v| v.to_string())
        )?;
    }
    Ok(())
}

fn instance_cmd(ctx: &Context, cmd: InstanceCmd) -> Result<()> {
    let progress =
        &mut |done: usize, total: usize| eprint!("\r\x1b[KCopying files: {done}/{total}");
    match cmd {
        InstanceCmd::List => {
            for i in instance::list(&ctx.paths)? {
                let n = i.registry().map(|r| r.mods.len()).unwrap_or(0);
                writeln!(
                    io::stdout(),
                    "{:20} {:24} KSP {:14} {n} mods",
                    i.id,
                    i.config.name,
                    i.config.game_version.to_string()
                )?;
            }
            Ok(())
        }
        InstanceCmd::Create {
            name,
            from,
            with_saves,
            copy,
            keep_mods,
        } => {
            let source = match from {
                Some(p) => p,
                None => ctx
                    .steam()
                    .ksp
                    .context("KSP not found in Steam; pass --from <dir>")?,
            };
            let opts = CreateOptions {
                name,
                source,
                stock_only: !keep_mods,
                with_saves,
                mode: if copy { CopyMode::Copy } else { CopyMode::Link },
            };
            let inst = instance::create(&ctx.paths, &opts, progress)?;
            eprintln!();
            if let Some(base) = &inst.config.base {
                writeln!(io::stdout(), "Linked to {}", base.display())?;
            }
            writeln!(
                io::stdout(),
                "Created {} ({}) at {}",
                inst.config.name,
                inst.config.game_version,
                inst.dir.display()
            )?;
            Ok(())
        }
        InstanceCmd::Clone {
            source,
            name,
            no_saves,
        } => {
            let src = instance::find(&ctx.paths, &source)?;
            let inst = instance::clone_instance(&ctx.paths, &src, &name, !no_saves, progress)?;
            eprintln!();
            writeln!(
                io::stdout(),
                "Cloned {} into {} at {}",
                src.config.name,
                inst.config.name,
                inst.dir.display()
            )?;
            Ok(())
        }
        InstanceCmd::Launcher { instance, via } => {
            let mut inst = ctx.instance(instance.as_deref())?;
            inst.config.launch.via_steam = via == "steam";
            inst.save()?;
            writeln!(
                io::stdout(),
                "{} now launches {}",
                inst.config.name,
                if inst.config.launch.via_steam {
                    "through Steam"
                } else {
                    "directly through Proton"
                }
            )?;
            Ok(())
        }
        InstanceCmd::ImportCkan { dir, name, copy } => {
            let name = if name.trim().is_empty() {
                dir.file_name()
                    .map_or("CKAN install".into(), |n| n.to_string_lossy().into_owned())
            } else {
                name
            };
            let mode = if copy { CopyMode::Copy } else { CopyMode::Link };
            let report = ckan_import::import(&ctx.paths, &dir, &name, mode, progress)?;
            eprintln!();
            for w in &report.warnings {
                eprintln!("warning: {w}");
            }
            writeln!(
                io::stdout(),
                "Imported {} mods into {} at {}",
                report.imported,
                report.instance.config.name,
                report.instance.dir.display()
            )?;
            if !report.unmanaged.is_empty() {
                writeln!(
                    io::stdout(),
                    "Not tracked by CKAN (adopt with `orbis manual adopt <folder>`): {}",
                    report.unmanaged.join(", ")
                )?;
            }
            Ok(())
        }
        InstanceCmd::Sync { instance } => {
            let mut inst = ctx.instance(instance.as_deref())?;
            instance::sync(&ctx.paths, &mut inst)?;
            match &inst.config.base {
                Some(b) => writeln!(
                    io::stdout(),
                    "{} is linked to {} (KSP {})",
                    inst.config.name,
                    b.display(),
                    inst.config.game_version
                )?,
                None => writeln!(
                    io::stdout(),
                    "{} is a full copy; nothing to sync",
                    inst.config.name
                )?,
            }
            Ok(())
        }
        InstanceCmd::Rename { instance, name } => {
            let mut inst = instance::find(&ctx.paths, &instance)?;
            inst.config.name = name;
            inst.save()
        }
        InstanceCmd::Delete { instance, yes } => {
            let inst = instance::find(&ctx.paths, &instance)?;
            if !yes {
                print!(
                    "Delete {} and everything in {}? [y/N] ",
                    inst.config.name,
                    inst.dir.display()
                );
                io::stdout().flush().ok();
                let mut line = String::new();
                io::stdin()
                    .lock()
                    .read_line(&mut line)
                    .context("read answer")?;
                if !matches!(line.trim(), "y" | "Y" | "yes") {
                    writeln!(io::stdout(), "Cancelled.")?;
                    return Ok(());
                }
            }
            instance::delete(&inst)?;
            writeln!(io::stdout(), "Deleted {}", inst.config.name)?;
            Ok(())
        }
        InstanceCmd::Info { instance } => {
            let inst = ctx.instance(instance.as_deref())?;
            let c = &inst.config;
            writeln!(io::stdout(), "name:         {}", c.name)?;
            writeln!(io::stdout(), "id:           {}", inst.id)?;
            writeln!(io::stdout(), "directory:    {}", inst.dir.display())?;
            writeln!(io::stdout(), "game version: {}", c.game_version)?;
            let compat: Vec<String> = c.compat.iter().map(ToString::to_string).collect();
            writeln!(io::stdout(), "compatible:   {}", compat.join(", "))?;
            writeln!(io::stdout(), "testing:      {}", c.allow_testing)?;
            writeln!(io::stdout(), "prefix:       {:?}", c.launch.prefix)?;
            writeln!(io::stdout(), "source:       {}", c.source)?;
            match &c.base {
                Some(b) => writeln!(io::stdout(), "layout:       linked to {}", b.display())?,
                None => writeln!(io::stdout(), "layout:       full copy")?,
            }
            let dlc: Vec<String> = inst.dlc().into_iter().map(|d| d.identifier).collect();
            writeln!(io::stdout(), "dlc:          {}", dlc.join(", "))?;
            Ok(())
        }
        InstanceCmd::Compat {
            instance,
            add,
            remove,
        } => {
            let mut inst = ctx.instance(instance.as_deref())?;
            for v in add {
                let gv = GameVersion::parse(&v).with_context(|| format!("bad version {v:?}"))?;
                if !inst.config.compat.contains(&gv) {
                    inst.config.compat.push(gv);
                }
            }
            for v in remove {
                let gv = GameVersion::parse(&v).with_context(|| format!("bad version {v:?}"))?;
                inst.config.compat.retain(|c| *c != gv);
            }
            inst.config.compat.sort_by(|a, b| b.cmp(a));
            inst.save()?;
            let compat: Vec<String> = inst.config.compat.iter().map(ToString::to_string).collect();
            writeln!(
                io::stdout(),
                "{}: {} + {}",
                inst.config.name,
                inst.config.game_version,
                compat.join(", ")
            )?;
            Ok(())
        }
    }
}

/// Syncs craft before a launch; problems are warnings, never a reason not to launch.
fn sync_crafts(ctx: &Context) {
    match crafts::sync(&ctx.paths) {
        Ok(r) => {
            let summary = r.summary();
            if !summary.is_empty() {
                eprintln!("{summary}");
            }
            for n in &r.notes {
                eprintln!("{n}");
            }
        }
        Err(e) => eprintln!("warning: craft sync failed: {e:#}"),
    }
}

fn when_saved(secs: u64) -> String {
    let days = orbis::paths::now_secs().saturating_sub(secs) / 86_400;
    match days {
        0 => "today".into(),
        1 => "yesterday".into(),
        d => format!("{d} days ago"),
    }
}

fn craft_cmd(ctx: &Context, cmd: CraftCmd) -> Result<()> {
    let mut out = io::stdout();
    match cmd {
        CraftCmd::List { instance, trash } => {
            let lib = crafts::Library::load(&ctx.paths)?;
            let index = match &instance {
                Some(_) => Some(
                    crafts::cached_index(&ctx.instance(instance.as_deref())?)
                        .context("no part list for that instance yet; run `orbis craft sync`")?,
                ),
                None => None,
            };
            let providers = crafts::providers(&ctx.paths);
            for c in lib.crafts.iter().filter(|c| c.live() != trash) {
                let mut line = format!(
                    "{:4} {:3} {:36} {:3} parts  {:2} rev  saved {}",
                    c.id,
                    c.facility.dir(),
                    c.name,
                    c.head().parts.len(),
                    c.revisions.len(),
                    when_saved(c.head().saved)
                );
                if let Some(d) = &c.deleted {
                    line.push_str(&format!("  deleted in {} {}", d.from, when_saved(d.at)));
                }
                if let Some(idx) = &index {
                    let missing = idx.missing(&c.head().parts);
                    if missing.is_empty() {
                        line.push_str("  fits");
                    } else {
                        line.push_str(&format!(
                            "  needs {}",
                            crafts::explain(&missing, &providers)
                        ));
                    }
                }
                writeln!(out, "{line}")?;
            }
            Ok(())
        }
        CraftCmd::Sync => {
            let r = crafts::sync(&ctx.paths)?;
            let summary = r.summary();
            writeln!(
                out,
                "{}",
                if summary.is_empty() {
                    "Crafts: up to date"
                } else {
                    &summary
                }
            )?;
            for n in &r.notes {
                writeln!(out, "{n}")?;
            }
            if !r.running.is_empty() {
                writeln!(out, "Skipped while running: {}", r.running.join(", "))?;
            }
            Ok(())
        }
        CraftCmd::Add { file } => {
            let name = crafts::add_file(&ctx.paths, &file)?;
            writeln!(
                out,
                "Added {name}; `orbis craft sync` copies it into instances"
            )?;
            Ok(())
        }
        CraftCmd::Import { dir } => {
            let r = crafts::import(&ctx.paths, &dir)?;
            for n in &r.added {
                writeln!(out, "Added {n}")?;
            }
            writeln!(
                out,
                "{} added, {} already in the library; `orbis craft sync` copies them into instances",
                r.added.len(),
                r.skipped
            )?;
            Ok(())
        }
        CraftCmd::Delete { craft } => {
            let lib = crafts::Library::load(&ctx.paths)?;
            let c = lib.find(&craft)?;
            crafts::delete(&ctx.paths, &c.id)?;
            writeln!(
                out,
                "Deleted {}; the next sync removes it from instances",
                c.name
            )?;
            Ok(())
        }
        CraftCmd::Restore { craft } => {
            let lib = crafts::Library::load(&ctx.paths)?;
            let c = lib.find(&craft)?;
            let name = crafts::restore(&ctx.paths, &c.id)?;
            writeln!(out, "Restored {name}")?;
            Ok(())
        }
        CraftCmd::EmptyTrash => {
            let n = crafts::empty_trash(&ctx.paths)?;
            writeln!(out, "Deleted {n} craft for good")?;
            Ok(())
        }
        CraftCmd::History { craft, instance } => {
            let lib = crafts::Library::load(&ctx.paths)?;
            let c = lib.find(&craft)?;
            let index = match &instance {
                Some(_) => crafts::cached_index(&ctx.instance(instance.as_deref())?),
                None => None,
            };
            for (i, r) in c.revisions.iter().enumerate().rev() {
                let fit = match &index {
                    Some(idx) if crafts::fits(idx, r) => "  fits",
                    Some(_) => "  doesn't fit",
                    None => "",
                };
                writeln!(
                    out,
                    "{:3}  {:12} from {:28} {:3} parts{fit}",
                    i + 1,
                    when_saved(r.saved),
                    r.from,
                    r.parts.len()
                )?;
            }
            Ok(())
        }
        CraftCmd::Fork {
            craft,
            revision,
            name,
        } => {
            let lib = crafts::Library::load(&ctx.paths)?;
            let c = lib.find(&craft)?;
            let r = revision
                .checked_sub(1)
                .and_then(|i| c.revisions.get(i))
                .with_context(|| format!("{} has revisions 1 to {}", c.name, c.revisions.len()))?;
            let name = crafts::fork(&ctx.paths, &c.id, &r.hash, &name)?;
            writeln!(out, "Saved revision {revision} of {} as {name}", c.name)?;
            Ok(())
        }
        CraftCmd::Pending { delete, put_back } => {
            let list = crafts::pending(&ctx.paths)?;
            let pick = delete.or(put_back);
            let Some(n) = pick else {
                for (i, p) in list.iter().enumerate() {
                    writeln!(
                        out,
                        "{:3}  {} missing from {}/{}",
                        i + 1,
                        p.name,
                        p.instance_name,
                        p.location.save
                    )?;
                }
                return Ok(());
            };
            let p = n
                .checked_sub(1)
                .and_then(|i| list.get(i))
                .with_context(|| format!("no pending craft {n}"))?;
            crafts::resolve(&ctx.paths, p, delete.is_some())?;
            if delete.is_some() {
                writeln!(out, "Deleted {} everywhere", p.name)?;
            } else {
                writeln!(
                    out,
                    "{} goes back into {}/{} on the next sync",
                    p.name, p.instance_name, p.location.save
                )?;
            }
            Ok(())
        }
    }
}

fn flag_cmd(ctx: &Context, cmd: FlagCmd) -> Result<()> {
    match cmd {
        FlagCmd::List => {
            for f in flags::list(&ctx.paths)? {
                writeln!(
                    io::stdout(),
                    "{:32} {}x{}  {}/Flags/{}",
                    f.name,
                    f.width,
                    f.height,
                    flags::FOLDER,
                    f.name
                )?;
            }
            Ok(())
        }
        FlagCmd::Add { path, name } => {
            let f = flags::add(&ctx.paths, &path, name.as_deref())?;
            writeln!(
                io::stdout(),
                "Added {} ({}x{}); every instance shows it in the flag picker",
                f.name,
                f.width,
                f.height
            )?;
            Ok(())
        }
        FlagCmd::Remove { name } => flags::remove(&ctx.paths, &name),
    }
}

fn manual_cmd(ctx: &Context, cmd: ManualCmd) -> Result<()> {
    match cmd {
        ManualCmd::List => {
            for m in manual::list(&ctx.paths)? {
                let provides = if m.provides.is_empty() {
                    String::new()
                } else {
                    format!("  provides {}", m.provides.join(", "))
                };
                writeln!(
                    io::stdout(),
                    "{:24} {:24} {}{provides}",
                    m.id,
                    m.name,
                    m.version
                )?;
            }
            Ok(())
        }
        ManualCmd::Add {
            path,
            name,
            version,
            provides,
            depends,
            conflicts,
            find,
            install_to,
            url,
            notes,
        } => {
            let install = find
                .map(|f| {
                    vec![InstallDirective {
                        find: Some(f),
                        install_to,
                        ..InstallDirective::default()
                    }]
                })
                .unwrap_or_default();
            let m = manual::add(
                &ctx.paths,
                &AddOptions {
                    path,
                    name,
                    version,
                    provides,
                    depends,
                    conflicts,
                    install,
                    source_url: url,
                    notes,
                },
            )?;
            writeln!(io::stdout(), "Added {} as {}", m.name, m.identifier())?;
            print_manual_plan(ctx, &m)
        }
        ManualCmd::Show { id } => {
            let m = manual::find(&ctx.paths, &id)?;
            writeln!(
                io::stdout(),
                "{} {} ({})",
                m.name,
                m.version,
                m.identifier()
            )?;
            if !m.provides.is_empty() {
                writeln!(io::stdout(), "  provides:  {}", m.provides.join(", "))?;
            }
            if !m.depends.is_empty() {
                writeln!(io::stdout(), "  depends:   {}", m.depends.join(", "))?;
            }
            if !m.conflicts.is_empty() {
                writeln!(io::stdout(), "  conflicts: {}", m.conflicts.join(", "))?;
            }
            for d in &m.install {
                writeln!(io::stdout(), "  install:   {}", orbis::install::describe(d))?;
            }
            print_manual_plan(ctx, &m)
        }
        ManualCmd::Install { id, instance, plan } => {
            let inst = ctx.instance(instance.as_deref())?;
            let m = manual::find(&ctx.paths, &id)?;
            let index = Index::load(&ctx.paths)?.unwrap_or_default();
            let choices = with_choices(Request::default(), &plan.choose)?.choices;
            let p = match ops::plan_manual(&inst, &index, &m, &choices)? {
                Ok(p) => p,
                Err(e) => return Err(explain(e)),
            };
            confirm_and_apply(ctx, &inst, &p, &plan)
        }
        ManualCmd::Delete { id } => {
            let m = manual::find(&ctx.paths, &id)?;
            manual::delete(&ctx.paths, &m)?;
            writeln!(io::stdout(), "Deleted {} from the library", m.name)?;
            Ok(())
        }
        ManualCmd::Unmanaged { instance } => {
            let inst = ctx.instance(instance.as_deref())?;
            let (folders, loose) = manual::unmanaged(&inst.game_dir(), &inst.registry()?.owners())?;
            for u in &folders {
                writeln!(io::stdout(), "GameData/{:30} {} files", u.folder, u.files)?;
            }
            for f in &loose {
                writeln!(io::stdout(), "GameData/{f}")?;
            }
            if folders.is_empty() && loose.is_empty() {
                writeln!(io::stdout(), "Nothing unmanaged in {}", inst.config.name)?;
            }
            Ok(())
        }
        ManualCmd::Adopt {
            folder,
            instance,
            name,
            version,
            provides,
        } => {
            let inst = ctx.instance(instance.as_deref())?;
            let opts = AddOptions {
                name,
                version,
                provides,
                ..AddOptions::default()
            };
            let m = ops::adopt(&ctx.paths, &inst, &folder, &opts)?;
            writeln!(
                io::stdout(),
                "Adopted GameData/{folder} as {}",
                m.identifier()
            )?;
            Ok(())
        }
    }
}

fn print_manual_plan(ctx: &Context, m: &manual::ManualMod) -> Result<()> {
    let planned = m.plan(&ctx.paths)?;
    let files: Vec<&str> = planned
        .iter()
        .filter(|p| !p.is_dir)
        .map(|p| p.dest.as_str())
        .collect();
    let mut tops: Vec<String> = files
        .iter()
        .map(|f| f.splitn(3, '/').take(2).collect::<Vec<_>>().join("/"))
        .collect();
    tops.dedup();
    writeln!(
        io::stdout(),
        "  {} files into: {}",
        files.len(),
        tops.join(", ")
    )?;
    Ok(())
}
