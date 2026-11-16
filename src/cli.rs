//! Command-line interface: every core operation, scriptable.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand};

use orbis::config::Config;
use orbis::download::DownloadEvent;
use orbis::instance::{self, CopyMode, CreateOptions, Instance};
use orbis::launch;
use orbis::manual::{self, AddOptions};
use orbis::metadata::InstallDirective;
use orbis::ops::{self, Event};
use orbis::paths::Paths;
use orbis::repo::{self, Index};
use orbis::resolver::{Plan, Request, ResolveError};
use orbis::steam::{self, SteamInfo};
use orbis::version::{GameVersion, ModVersion};

use crate::gui;

/// Shared state for commands and the GUI.
pub struct Context {
    pub paths: Paths,
    pub config: Config,
}

impl Context {
    pub fn new(root: Option<&Path>) -> Result<Self> {
        let paths = Paths::new(root)?;
        let config = Config::load(&paths)?;
        Ok(Self { paths, config })
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
    /// Start an instance through Proton.
    Launch {
        instance: Option<String>,
        /// Print the command instead of running it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show what orbis detected of Steam, KSP and Proton.
    Steam,
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
    /// Create an instance from the Steam install (stock game only).
    Create {
        name: String,
        /// Copy from this KSP directory instead of Steam's.
        #[arg(long, value_name = "DIR")]
        from: Option<PathBuf>,
        /// Copy the source's saves too.
        #[arg(long)]
        with_saves: bool,
        /// Copy non-stock GameData too (mods orbis won't know about).
        #[arg(long)]
        keep_mods: bool,
        /// Hard-link files instead of copying (for filesystems without reflinks).
        #[arg(long)]
        hardlink: bool,
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
pub enum ManualCmd {
    /// List the manual mod library.
    List,
    /// Add a .zip or folder to the library.
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
}

pub fn run(ctx: &Context, cmd: Command) -> Result<()> {
    match cmd {
        Command::Gui(_) => unreachable!("main handles the GUI"),
        Command::Update => {
            let s = repo::update(&ctx.paths, &ctx.config, &mut |msg| eprint!("\r\x1b[K{msg}"))?;
            eprintln!();
            if s.unchanged {
                println!("Index unchanged: {} mods", s.modules);
            } else {
                println!(
                    "Index updated: {} mods, {} releases ({} unreadable skipped)",
                    s.modules, s.releases, s.skipped
                );
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
                println!("Everything is up to date.");
                return Ok(());
            }
            let req = Request {
                upgrade: mods,
                ..Request::default()
            };
            plan_and_apply(ctx, &inst, req, &plan)
        }
        Command::Manual(c) => manual_cmd(ctx, c),
        Command::Launch { instance, dry_run } => {
            let mut inst = ctx.instance(instance.as_deref())?;
            let plan = launch::plan(&inst, &ctx.steam(), &ctx.config)?;
            if dry_run {
                println!("{}", plan.display());
                return Ok(());
            }
            let child = launch::spawn(&plan)?;
            inst.config.last_played = Some(orbis::paths::now_secs());
            inst.save()?;
            println!(
                "Started {} (pid {}); log: {}",
                inst.config.name,
                child.id(),
                plan.log.display()
            );
            Ok(())
        }
        Command::Steam => {
            for (k, v) in ctx.steam().report() {
                println!("{k:14} {v}");
            }
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
        println!("Nothing to do.");
        return Ok(());
    }
    println!("Changes to {}:", inst.config.name);
    for c in &plan.changes {
        println!("  {c}");
    }
    for s in &plan.suggestions {
        let kind = if s.recommended {
            "recommends"
        } else {
            "suggests"
        };
        println!(
            "  ({} {kind} {}: {})",
            s.by,
            s.relationship,
            s.candidates.join(", ")
        );
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
            println!("Cancelled.");
            return Ok(());
        }
    }
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
        let r = ops::apply(&ctx.paths, &ctx.config, inst, plan, &tx);
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
        println!(
            "{id:32} {:12} {:10} {}",
            m.version.to_string(),
            m.game_range_label(),
            m.summary
        );
    }
    if hits.len() > 50 {
        println!("... {} more", hits.len() - 50);
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
    println!("{} ({})", m.display_name(), m.identifier);
    println!("  {}", m.summary);
    if let Some(d) = &m.description {
        println!("\n{d}\n");
    }
    println!("  authors:   {}", m.author.join(", "));
    println!("  license:   {}", m.license.join(", "));
    println!("  downloads: {}", index.download_count(id));
    for key in ["homepage", "repository", "spacedock", "bugtracker"] {
        if let Some(r) = m.resource(key) {
            println!("  {key:10} {r}");
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
            println!("  {label:10} {}", list.join(", "));
        }
    }
    println!("  releases:");
    for v in versions.iter().take(15) {
        println!(
            "    {:16} KSP {}",
            v.version.to_string(),
            v.game_range_label()
        );
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
        let auto = if m.auto_installed { " (auto)" } else { "" };
        let up = upgrades
            .iter()
            .find(|(u, _)| u == id)
            .map_or(String::new(), |(_, n)| format!("  -> {}", n.version));
        println!("{id:36} {}{auto}{up}", m.version());
    }
    for dlc in inst.dlc() {
        println!(
            "{:36} {} (DLC)",
            dlc.identifier,
            dlc.version.map_or("?".into(), |v| v.to_string())
        );
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
                println!(
                    "{:20} {:24} KSP {:14} {n} mods",
                    i.id,
                    i.config.name,
                    i.config.game_version.to_string()
                );
            }
            Ok(())
        }
        InstanceCmd::Create {
            name,
            from,
            with_saves,
            keep_mods,
            hardlink,
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
                mode: if hardlink {
                    CopyMode::Hardlink
                } else {
                    CopyMode::Copy
                },
            };
            let inst = instance::create(&ctx.paths, &opts, progress)?;
            eprintln!();
            println!(
                "Created {} ({}) at {}",
                inst.config.name,
                inst.config.game_version,
                inst.dir.display()
            );
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
            println!(
                "Cloned {} into {} at {}",
                src.config.name,
                inst.config.name,
                inst.dir.display()
            );
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
                    println!("Cancelled.");
                    return Ok(());
                }
            }
            instance::delete(&inst)?;
            println!("Deleted {}", inst.config.name);
            Ok(())
        }
        InstanceCmd::Info { instance } => {
            let inst = ctx.instance(instance.as_deref())?;
            let c = &inst.config;
            println!("name:         {}", c.name);
            println!("id:           {}", inst.id);
            println!("directory:    {}", inst.dir.display());
            println!("game version: {}", c.game_version);
            let compat: Vec<String> = c.compat.iter().map(ToString::to_string).collect();
            println!("compatible:   {}", compat.join(", "));
            println!("testing:      {}", c.allow_testing);
            println!("prefix:       {:?}", c.launch.prefix);
            println!("source:       {}", c.source);
            let dlc: Vec<String> = inst.dlc().into_iter().map(|d| d.identifier).collect();
            println!("dlc:          {}", dlc.join(", "));
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
            println!(
                "{}: {} + {}",
                inst.config.name,
                inst.config.game_version,
                compat.join(", ")
            );
            Ok(())
        }
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
                println!("{:24} {:24} {}{provides}", m.id, m.name, m.version);
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
            println!("Added {} as {}", m.name, m.identifier());
            print_manual_plan(ctx, &m)
        }
        ManualCmd::Show { id } => {
            let m = manual::find(&ctx.paths, &id)?;
            println!("{} {} ({})", m.name, m.version, m.identifier());
            if !m.provides.is_empty() {
                println!("  provides:  {}", m.provides.join(", "));
            }
            if !m.depends.is_empty() {
                println!("  depends:   {}", m.depends.join(", "));
            }
            if !m.conflicts.is_empty() {
                println!("  conflicts: {}", m.conflicts.join(", "));
            }
            for d in &m.install {
                println!("  install:   {}", orbis::install::describe(d));
            }
            print_manual_plan(ctx, &m)
        }
        ManualCmd::Install { id, instance, plan } => {
            let inst = ctx.instance(instance.as_deref())?;
            let m = manual::find(&ctx.paths, &id)?;
            let index = Index::load(&ctx.paths)?.unwrap_or_default();
            let p = match ops::plan_manual(&inst, &index, &m)? {
                Ok(p) => p,
                Err(e) => return Err(explain(e)),
            };
            confirm_and_apply(ctx, &inst, &p, &plan)
        }
        ManualCmd::Delete { id } => {
            let m = manual::find(&ctx.paths, &id)?;
            manual::delete(&ctx.paths, &m)?;
            println!("Deleted {} from the library", m.name);
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
    println!("  {} files into: {}", files.len(), tops.join(", "));
    Ok(())
}
